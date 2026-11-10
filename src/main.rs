use std::path::Path;
use std::sync::Arc;
use twinsync::app::TwinSyncApp;
use twinsync::config::{APP_NAME, AppPaths, acquire_instance_lock};
use twinsync::devices::{MacEjector, MacResolver};
use twinsync::engine::{Engine, EngineConfig};

fn init_logging(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("twinsync.log"))
    {
        // Metadata only: file contents are never logged.
        tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .init();
    }
}

fn main() -> eframe::Result {
    // Development override. Read here only, never inside the library.
    let paths = match std::env::var_os("TWINSYNC_DATA_DIR") {
        Some(d) => AppPaths::under(Path::new(&d)),
        None => AppPaths::macos_default().expect("HOME is not set"),
    };
    init_logging(&paths.log_dir);
    let _instance = match acquire_instance_lock(&paths.lock_path()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{APP_NAME} is already running, or its data folder is not writable ({e}).");
            std::process::exit(1);
        }
    };
    tracing::info!("starting {APP_NAME} {}", env!("CARGO_PKG_VERSION"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([1120.0, 780.0])
            .with_min_inner_size([780.0, 540.0]),
        ..Default::default()
    };
    let ui_paths = paths.clone();
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| {
            let resolver = Arc::new(MacResolver::new());
            let ejector = Arc::new(MacEjector(resolver.clone()));
            let mut cfg = EngineConfig::new(paths, resolver, ejector);
            let ctx = cc.egui_ctx.clone();
            cfg.on_change = Arc::new(move || ctx.request_repaint());
            let engine = Engine::start(cfg)?;
            Ok(Box::new(TwinSyncApp::new(engine, &ui_paths)))
        }),
    )
}
