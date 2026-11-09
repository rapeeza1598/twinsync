//! Signals only. File system events, mount-table changes and wake-from-sleep
//! tell the engine *when to look*; they are never the source of truth.

use crate::devices::{DeviceResolver, VolumeInfo};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsSignal {
    Changed,
    /// The watcher dropped events or failed: only a full rescan can be trusted.
    Overflow,
}

#[derive(Clone)]
pub struct Callbacks {
    pub on_fs: Arc<dyn Fn(FsSignal) + Send + Sync>,
    pub on_mounts: Arc<dyn Fn(Vec<VolumeInfo>) + Send + Sync>,
    pub on_wake: Arc<dyn Fn() + Send + Sync>,
}

pub struct Monitor {
    stop: Arc<AtomicBool>,
    watcher: Option<RecommendedWatcher>,
    poll: Option<JoinHandle<()>>,
    cb: Callbacks,
}

impl Monitor {
    /// Polls the mount table (cached by the kernel, so it never wakes a drive).
    pub fn start(resolver: Arc<dyn DeviceResolver>, every: Duration, cb: Callbacks) -> Monitor {
        let stop = Arc::new(AtomicBool::new(false));
        let poll = {
            let (stop, cb) = (stop.clone(), cb.clone());
            std::thread::Builder::new()
                .name("twinsync-mounts".into())
                .spawn(move || poll_loop(resolver, every, stop, cb))
                .ok()
        };
        Monitor {
            stop,
            watcher: None,
            poll,
            cb,
        }
    }

    /// Replace the watched folder. Errors are reported as an overflow so the
    /// engine falls back to periodic reconciliation instead of trusting silence.
    pub fn watch_source(&mut self, path: Option<&Path>) {
        self.watcher = None;
        let Some(path) = path else { return };
        let on_fs = self.cb.on_fs.clone();
        let handler = move |res: notify::Result<notify::Event>| match res {
            Ok(ev) if ev.need_rescan() => on_fs(FsSignal::Overflow),
            Ok(ev) if matches!(ev.kind, EventKind::Access(_)) => {}
            Ok(_) => on_fs(FsSignal::Changed),
            Err(_) => on_fs(FsSignal::Overflow),
        };
        match notify::recommended_watcher(handler) {
            Ok(mut w) => {
                if w.watch(path, RecursiveMode::Recursive).is_err() {
                    (self.cb.on_fs)(FsSignal::Overflow);
                } else {
                    self.watcher = Some(w);
                }
            }
            Err(_) => (self.cb.on_fs)(FsSignal::Overflow),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.watcher = None;
        if let Some(h) = self.poll.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn poll_loop(
    resolver: Arc<dyn DeviceResolver>,
    every: Duration,
    stop: Arc<AtomicBool>,
    cb: Callbacks,
) {
    let mut last: Option<Vec<VolumeInfo>> = None;
    let mut mark = (Instant::now(), SystemTime::now());
    while !stop.load(Ordering::Relaxed) {
        // `Instant` does not advance while the Mac sleeps but the wall clock
        // does; a large gap between the two means we just woke up.
        let mono = mark.0.elapsed().as_secs_f64();
        let wall = SystemTime::now()
            .duration_since(mark.1)
            .map_or(0.0, |d| d.as_secs_f64());
        if wall - mono > 5.0 {
            (cb.on_wake)();
        }
        mark = (Instant::now(), SystemTime::now());

        if let Ok(now) = resolver.mounted_volumes()
            && last.as_ref() != Some(&now)
        {
            last = Some(now.clone());
            (cb.on_mounts)(now);
        }
        let until = Instant::now() + every;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
