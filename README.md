# TwinSync

**One Mac. Two drives. Files in sync.**

[![CI](https://github.com/rapeeza1598/twinsync/actions/workflows/ci.yml/badge.svg)](https://github.com/rapeeza1598/twinsync/actions/workflows/ci.yml)

TwinSync is a macOS app, written in Rust, that watches a folder on your Mac and copies it one way to
**two external drives**. It is built around not losing data: it never changes your source folder, never
deletes anything on a drive, never overwrites a file it did not put there, and keeps the previous version of
anything it replaces.

> **Status: MVP.** Everything in this README is implemented and covered by automated tests unless it is
> listed under [Known limitations](#known-limitations). It has been exercised on throwaway disk images
> (APFS, HFS+, exFAT, FAT32), with fakes for drive identity, and in **one real session with a USB SSD and a
> USB HDD** (see [Real-hardware observations](#real-hardware-observations)). That session covered the happy
> path only; the harder cases (unplugging mid-copy, a sleeping HDD, version history on real drives) are still
> open in the [manual checklist](#manual-ssd--hdd-checklist). Use it before trusting TwinSync with data you cannot replace.

## Requirements

- macOS on Apple silicon or Intel. Built and tested only on **macOS 27.2 (arm64)**; `Info.plist` declares a
  minimum of 11.0 but older versions are untested.
- Rust stable (developed with 1.99.1) and the Xcode command line tools (`codesign`, `hdiutil`, `plutil`).
- Non-macOS builds are refused at compile time: the transfer layer uses `renameatx_np`, `F_FULLFSYNC`
  and `diskutil`.

## Development commands

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test                               # unit + integration tests, temp folders only
cargo run                                # runs against ~/Library/Application Support/TwinSync
TWINSYNC_DATA_DIR=/tmp/twinsync-dev cargo run   # throwaway state, for trying things out
scripts/fs-compat-test.sh                # real filesystems on disk images (see below)
```

### Continuous integration

`.github/workflows/ci.yml` runs on every push to `main` and on pull requests (docs-only changes are skipped): `cargo fmt --check`,
`clippy -D warnings`, `cargo test` and a release build, all on a **macOS** runner because TwinSync only builds on macOS.
The real-filesystem tests are ignored there; run `scripts/fs-compat-test.sh` yourself. A manual run (**Actions → CI → Run workflow**)
or a `v*` tag also builds `TwinSync.app` and attaches it to the run as an artifact (ad-hoc signed, not notarized).

The repository is public, so hosted runners are free. If it ever goes private again, macOS minutes count 10x against the
allowance; the workflow already cancels superseded runs and skips documentation-only pushes to stay cheap.

> **Status (2026-11-10): not running yet.** GitHub accepts the run but never starts a job: the run's annotation reads
> *"The job was not started because your account is locked due to a billing issue."* That is a lock on the GitHub **account**,
> not on this repository or this workflow: a two-line probe workflow failed the same way, and making the repository public did
> not change it. Resolve it under GitHub → Settings → Billing and plans (a failed payment or a spending limit is the usual cause),
> then re-run it (Actions → CI → Run workflow). Until then the badge stays red and the same checks have to be run locally
> (`cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test`).

State lives in `~/Library/Application Support/TwinSync/` (SQLite database and a lock file that prevents two
copies of the app writing the same project). Logs are in `~/Library/Logs/TwinSync/twinsync.log` and contain
metadata such as counts and status, never file contents.

## Release build and `TwinSync.app`

```sh
scripts/bundle-macos.sh                                   # target/bundle/TwinSync.app
BUNDLE_ID=com.yourcompany.twinsync scripts/bundle-macos.sh
```

The script builds `--release`, assembles the bundle, lints `Info.plist` and signs ad hoc.

- The default bundle identifier `com.example.twinsync` is a **development placeholder**. Set `BUNDLE_ID` before
  you distribute anything.
- An ad-hoc signature runs on the Mac that built it. Giving the app to other people needs a Developer ID
  certificate and notarization (`codesign --options runtime` plus `xcrun notarytool`); this repository does
  not do that. Without it Gatekeeper will warn on other Macs.
- There is no app icon yet.
- Running the bundled binary was checked (it starts, creates its data folder, exits cleanly); the signed
  bundle was **not** notarized or launched through Finder.

## Setting up

1. **Source folder**: *Choose folder…* on the Overview page. Symlinks are skipped, not followed.
2. **Drive A and Drive B**: *Choose folder…* on each card, then pick a folder **on the external drive**.
   TwinSync checks the choice and tells you plainly if it cannot be used (overlapping folders, same volume,
   read-only, network share) or needs your confirmation (untested filesystem, both on one physical disk, volume
   with no UUID, destination on the same volume as the source).
3. **Preview Sync** shows every planned action and why. **Sync Now** runs it. If the plan changed since you
   reviewed it, nothing runs and you get a new preview.
4. Optionally enable **Auto Sync** (Settings controls the debounce and the periodic re-check).

You can use one drive at a time. A drive that is not connected shows its last known state; the other keeps working.

The **About** button at the bottom of the sidebar shows the version and the exact git commit of the running build,
where your data and logs are, and what TwinSync promises and does not promise.

## Update Copy, and how it differs from Mirror

| Situation | Update Copy (this app) | Mirror (not implemented) |
|---|---|---|
| New file in the source | copied to the drive | copied |
| Source file changed | drive copy updated, **old version kept** | drive copy replaced |
| Source file deleted | **copy kept**, listed for you to review | copy deleted |
| Renamed or moved in the source | treated as a new file; old copy kept | old copy deleted |
| File on the drive that TwinSync did not copy | never touched | deleted |
| Drive copy edited or deleted outside TwinSync | shown as a conflict / deletion, waits for you | overwritten |

There is no two-way sync and no sync between the drives. Choosing "Replace with source" never copies anything
back to the Mac.

## Version history and conflict handling

Before TwinSync replaces a file it moves the existing one to
`<drive folder>/.twinsync/<project id>/versions/<UTC timestamp>_<id>/<original path>` **on the same drive**.
If it cannot keep the old version it does not replace the file. Version history is never deleted
automatically and is never synced; the card shows how much space it uses.

Items that need a decision appear under **Needs Attention**:

| Item | Choices |
|---|---|
| A file already exists where a new one should go (collision) | Adopt if identical · Replace with source (existing goes to version history) · Ignore |
| Copy edited on the drive (conflict) | Replace with source · Ignore |
| Copy deleted from the drive | Restore from source · Don't recreate |
| File or folder gone from the source | Keep copy, dismiss |

"Ignore" stays quiet until the source file changes again.

## Stopping work and ejecting

**Prepare to Disconnect** stops new work for that drive, asks the running operation to stop at the next safe
point, waits until the worker really has stopped, and tells you TwinSync has closed its files. **That is not an
eject.** Eject in Finder, or use **Eject…**, which runs `diskutil eject` without force, warns first if other
volumes on the same disk would also unmount, and reports success only when macOS exits successfully **and** the
volume has left the mount table. The app never says "safe to unplug" before that.

## Testing with temporary folders

`cargo test` never touches a real drive. Tests use temp folders, a real SQLite file, fault injection at every
step of the transfer (`transfer::FaultPoint`) and fakes for drive discovery (`devices::FakeResolver`,
`FakeEjector`). Highlights: crash at every point of the pipeline then recover, disconnect mid-copy, full disk,
source or destination changing mid-copy, symlink escape, stale previews, wrong drive in the right place,
a slow drive not blocking the coordinator, pause, eject failures, legacy import. A headless render of every page and of
the About window checks that the important text really appears; it cannot judge layout or looks.

### Filesystem compatibility (real filesystems, disk images)

`scripts/fs-compat-test.sh` creates 96 MB APFS, HFS+, exFAT and FAT32 images in a scratch folder, mounts them
hidden from Finder, runs `tests/fs_compat_tests.rs`, detaches and re-attaches each (new device number) and
verifies that nothing looks changed. It detaches only what it attached. Last run: all four passed.

What was **measured** (on disk images, macOS 27.2):

| | APFS | HFS+ | exFAT | FAT32 |
|---|---|---|---|---|
| Unicode (NFC/NFD, Thai, emoji), 200-char names, empty file, 5 MB file, empty folder | ok | ok | ok | ok |
| Update keeps previous version; collision never overwritten | ok | ok | ok | ok |
| Names with `:` | stored | stored | **blocked by TwinSync** | **blocked by TwinSync** |
| Inode unchanged after replug | 12/12 | 12/12 | **10/11** | **10/11** |
| Free from false "changed" alarms after replug | yes | yes | yes (inode not compared) | yes (inode not compared) |
| macOS writes `._name` AppleDouble sidecars | no | no | **yes** | **yes** |

Also observed: APFS rejects non-UTF-8 file names outright (`EILSEQ`), so such names cannot occur there; paths are
stored as raw bytes anyway.

Not tested: NTFS (and other filesystems) at all, FAT32 files near 4 GiB (the limit is checked from the
filesystem type, and tested with mocked capabilities), any real SSD or HDD, USB-bridge behaviour, spin-up of a
sleeping disk.

### Manual SSD / HDD checklist

Do this with data you can afford to lose, and write down what you see.

- [x] Connect an SSD. Set it up as Drive A. Media shows **SSD** (or **Unknown** behind a bridge that does not say; that is fine). *Observed: SSD.*
- [x] Connect an HDD as Drive B. The card never guesses SSD/HDD from the name or size. *Observed: HDD, reported by macOS.*
- [ ] Preview, Sync, then confirm file counts and a few checksums match (`shasum -a 256`). *Counts and TwinSync's own hash check matched; not yet compared with `shasum`.*
- [ ] Change a file; sync; check the old version is under `.twinsync/.../versions/`.
- [ ] Unplug Drive B in the middle of a large copy (yank the cable). Reconnect. TwinSync should report
      the interruption, clean up its temp file, and finish on the next sync. Look in the Activity page.
- [ ] Let the HDD spin down, then Sync. The card should show *Waiting for the drive to respond* rather than an error.
- [ ] Prepare to Disconnect during a copy: the state goes to *Stopping…*, then the note appears. Check that
      the note does not say it is safe to unplug. Then Eject… and check Finder agrees.
      *Observed once while idle: Prepare to Disconnect, then Eject in the app; both drives left `/Volumes`. Not yet during a copy.*
- [ ] Plug both drives, then try one drive at a time.
- [ ] Put a file in a destination folder by hand with the same name as a source file: expect a collision, not an overwrite.
- [x] Run **Full Verify**. *Observed: 0 mismatches on both drives. Not yet compared with your own checksums.*

### Real-hardware observations

One session, 2026-11-10, read back from `~/Library/Logs/TwinSync/twinsync.log` and the app's database
(read-only). Treat it as a single data point, not a test campaign.

- **Drives:** Drive A a USB SSD (`MD202`), Drive B a USB HDD, both exFAT. macOS reported `SolidState` for both, so the
  cards showed SSD and HDD rather than Unknown.
- **Source:** 57 files, 10.46 GB. Both drives hold the same 57 files; 114 transfers, **all committed, 0 failures**,
  no warnings or errors in the log, no unfinished operations.
- **Full Verify** (content hashes) on both drives: 0 mismatches, 0 missing.
- **Speed:** the first 10.1 GB took about 93 s to the SSD and 104 s to the HDD (roughly 100 MB/s each; the timestamps
  suggest the two drives were written in parallel). Observed, not a benchmark.
- **App health:** after about 16 hours open, about 50-90 MB of memory and about 0.1% average CPU. Whether files are
  held open on a connected drive while idle was **not** checked (the drives were already unplugged by then).
- **What it taught us** (and what was changed afterwards): an empty folder TwinSync created and the user then renamed
  stayed on both drives with no sign of it, so folders that leave the source are now reported like files; 15 of 41
  syncs did nothing yet re-read both drives, so changes to excluded files such as `.DS_Store` are now ignored and an
  event-triggered scan that finds the source unchanged no longer reads drives that were already up to date; and the
  log could not say why a scan ran or that a drive had been ejected, so triggers and drive events are now recorded.

## Recovery procedures

TwinSync records each copy in a journal and reconciles it with what is really on the drive whenever a drive
is connected, before anything new is written.

- **App crashed or power was lost during a copy**: just start it again. A copy that had not replaced anything is
  discarded; a replacement that was interrupted after the old version was set aside is rolled back (the old
  file is put back) or, if the new file was already in place, finished.
- **A drive was unplugged mid-copy**: reconnect it. If leftover temp files cannot be cleaned while it is gone they
  are removed on reconnect. Temp files live in `.twinsync/<project id>/tmp/` and only files that TwinSync can
  prove it created are removed.
- **"Needs attention" after recovery**: TwinSync found something it will not guess about (for example a different
  file at the target path). Nothing is deleted; both files are kept. Look in `.twinsync/<project id>/versions/`.
- **A drive reports the wrong identity**: it is never written to. Check that it is the right drive and the
  destination folder still exists. TwinSync never recreates a missing destination folder.
- **Lost the database**: your copies and version history are ordinary files on the drives. A fresh setup will report
  existing files as collisions; *Adopt if identical* re-attaches byte-identical files without copying.

## Privacy and permissions

TwinSync is **not sandboxed**, runs locally, has no telemetry and sends nothing over the network. The log and the
Activity page hold counts, labels, volume names and status only (drive connected or disconnected, Prepare to
Disconnect, eject, setup, pause and resume, and why each scan ran), never file contents or folder paths.
macOS may ask once for access to **Documents / Desktop / Downloads** (if your source is there) and to
**removable volumes** (when it first touches a drive); the `Info.plist` usage strings explain why. It does not
ask for Full Disk Access and does not need it. It does not use security-scoped bookmarks, so it makes no
sandbox-persistent-access claims.

## Architecture

```
src/
  domain/     RelPath (raw bytes), Fingerprint, statuses            no I/O
  planner/    pure plan(): scan + manifest + observations -> actions
  scanner/    read-only source walk, exclusions, unreadable tracking
  transfer/   directory-fd filesystem ops, safe pipeline, journal recovery
  storage/    SQLite schema + migrations (+ legacy SSD A/B import)
  devices/    volume identity, destination validation, capabilities, fakes
  platform/   macOS only: mount table, diskutil, eject
  monitor/    file-system events, mount polling, wake detection (signals only)
  engine/     coordinator + one worker per drive; UiSnapshot / Command
  ui/ app.rs  egui front end (no SQLite, no FFI, no blocking I/O)
```

The coordinator never touches a drive. Each destination has exactly one worker thread, so two writers can never
meet on one target and a stalled drive stalls only itself. Concurrency is threads and channels rather than an
async runtime (all drive I/O is blocking).

## Known limitations

- **Not a full-fidelity backup.** It copies file contents and modification time and permission bits only.
  Extended attributes, ACLs, resource forks, creation dates and Finder metadata are not copied. Hard links become
  separate files, sparse files are expanded, package directories (`.app`, `.photoslibrary`) are copied as plain
  folders, and iCloud placeholder files are skipped (reported, never downloaded).
- **Not consistent for live databases.** A file that changes while being read is detected and retried, but
  metadata checks are not a snapshot. Do not rely on it to back up a database that is being written to.
- **Not power-loss proof.** It flushes with `F_FULLFSYNC` where the drive supports it and recovers from
  interruptions, but a cable pull or power cut during a write can still lose the in-flight file (never a
  previously completed copy, by design) and some drives lie about flushing. The read-back verification may be
  served from the OS cache, so it detects write-path errors but is not a guarantee about the physical media;
  use **Full Verify** for that.
- **Atomic replace differs per filesystem.** `RENAME_EXCL` is used where available; on FAT-family volumes there is a
  small check-then-rename window.
- **Inode numbers are not compared on FAT/exFAT** (measured unstable across remounts). A file edited without
  changing size or modification time on such a drive is only noticed by Full Verify.
- **Source changes that keep size, modification time and inode** are only found with *Re-hash every source file*
  (Settings), which is slower.
- **One project, two destinations on different volumes**, local volumes only (no NAS, no network shares). The
  data model allows more projects later.
- **NTFS and other filesystems**: not tested; read-only ones are refused, writable ones need confirmation and
  carry no support claim. TwinSync never formats a drive.
- **Eject** was tested against fakes and `diskutil` argument validation, not against a real drive.
- **The first version of the legacy importer is a guess**: no real "SSD A / SSD B" file was available, so it
  expects `config.json` with `source`, `ssd_a`, `ssd_b`. The original is renamed, never deleted, and imported drives
  must be confirmed before any write.
- The GUI was verified by running it and capturing the window, plus engine-level tests of everything it
  triggers. Individual clicks and the native folder dialog are not automated.
- Periodic re-checks and Auto Sync read the drive and may wake a sleeping HDD; with Auto Sync off only the source
  is scanned. File-system events that change nothing in the source, or only touch excluded files, do not read the
  drives; so a change made **on a drive** (say a file deleted there) is noticed by the periodic re-check, a mount or
  wake event, or when you press Preview or Sync Now, not by source events alone.
- ETA is not shown at all; transfer speed appears only after a couple of seconds of data.

## Future phases (not started, and not blocking the MVP)

Menu bar integration, launch at login, Mirror mode, automatic version retention.
