# TwinSync: project notes

The living record of this project: where it stands, why things are the way they are, what is known to be
missing, and what changed when. **Update this file in the same commit as any change** (see
[Keeping this current](#keeping-this-current)). The README describes the product for users; this file is for whoever
works on it next.

Last updated: 2026-11-11 (CI re-run: still blocked by the account billing lock).

## 1. Status at a glance

| | |
|---|---|
| What it is | macOS app in Rust (egui). One-way "Update Copy" from a Mac folder to two external drives. |
| Repo | https://github.com/rapeeza1598/twinsync (**public** since 2026-11-10; it was private before), branch `main`, pushed after every commit. GitHub About set: description "One Mac. Two drives. Files in sync. …", topics `rust macos backup file-sync egui external-drive` |
| Size | about 9k lines in `src/`, about 4.3k in `tests/` |
| Tests | **153 pass, 0 fail, 2 ignored** (the 2 are the real-filesystem tests, run by `scripts/fs-compat-test.sh`) |
| CI | `.github/workflows/ci.yml` on a macOS runner: fmt, clippy, tests, release build; `bundle` job (app zip artifact) on manual run or `v*` tag. **Not running: every run ends `startup_failure`**, see section 5 |
| Gates | `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`, `cargo build --release` all green |
| Real hardware | One session (USB SSD + USB HDD, exFAT): clean. Harder cases still untested, see [section 6](#6-not-yet-tested-on-real-hardware) |
| Packaging | `scripts/bundle-macos.sh` builds an ad-hoc signed `TwinSync.app`; bundle id is a placeholder; not notarized; no icon |

Where things live on the user's Mac:

- State: `~/Library/Application Support/TwinSync/` (`twinsync.sqlite3`, `twinsync.lock`). Logs: `~/Library/Logs/TwinSync/twinsync.log`.
- `TWINSYNC_DATA_DIR=<dir>` redirects both (read only in `main.rs`, never in the library). Use it for any experiment.
- The user's real source is `~/Documents/backup-all`; drives were `/Volumes/MD202` (SSD) and `/Volumes/HDD`, folder `backup-all` on each.

## 2. Working agreements with the user

- The user writes in Thai; answer in Thai. Keep code, commit messages and docs in English.
- **Commit and push as the work goes** (asked explicitly: "ระหว่างทำเอาขึ้น git ไปเลย"). Commit subjects start with a gitmoji
  (global rule); end with the `Co-Authored-By` trailer the harness gives.
- **Always update this file when anything changes** (asked explicitly: "ถ้าแก้อะไรให้มาupdateเสมอ").
- Plan with Opus: `~/.claude/settings.json` has `"model": "opusplan"` (Opus in plan mode, Sonnet otherwise).
- The user wants honest reporting: say what was tested, what used mocks, what was not tested. Never claim hardware testing that did not happen.
- Never touch the user's real drives or real data directory from tests or experiments. Real-filesystem tests use throwaway disk images only.
- Creating the GitHub repo was refused once by the harness's safety check and worked when the user said "retry". Do not try to work around such a refusal; stop and ask.

## 3. Decisions and the reasons behind them

| Decision | Why |
|---|---|
| Single crate `twinsync` (lib + bin), no `twinsync-core` | Integration tests use the lib; a second crate added nothing. |
| Threads and channels, **not tokio** (spec said tokio; user confirmed the change) | All drive I/O is blocking and already lives on one worker thread per drive. The coordinator is one `recv_timeout` loop. |
| One worker thread per destination | Structural guarantee that two writers never meet on one target, and a stalled drive stalls only itself. |
| The coordinator never touches a drive | Scans run in a short-lived thread, drive I/O in workers, so UI and coordinator stay responsive on a sleeping HDD. |
| `Fingerprint` = size + mtime + inode; **no `st_dev`, no `ctime`** | `st_dev` changes on every replug and ctime is bumped by Spotlight/Finder; either would make a reconnect look like mass change and re-hash the whole drive. (An advisor review caught this.) |
| Inode is **not compared on FAT/exFAT** (`FsCapabilities::inode_stable`) | Measured on real filesystems: inode changed for 1 of 11 files across a remount on exFAT and FAT32. APFS and HFS+ were 12/12 stable. |
| Source change check uses metadata; content hash only when needed | `hash_all_source_files` is an opt-in. For `Update`, the source is hashed first and compared with the last synced hash, so a bare `touch` does not rewrite the drive. |
| Everything on a drive goes through directory fds and `O_NOFOLLOW` (`transfer/fsops.rs`) | Canonicalizing once does not stop a swapped folder or a planted symlink; walking component by component does. |
| Commit by `renameatx_np(RENAME_EXCL)`; plain rename only where unsupported (FAT) | A file that appears at the target meanwhile is never replaced. |
| Recovery decides from the **real state on the drive**, not from the journal step | The database and the filesystem are not one transaction; a crash can land between an action and its journal write. |
| Journal records the new hash **before** the previous version is moved aside | Recovery can then recognise "the new file is already in place". |
| Planner is a pure function with no delete action in its vocabulary | Update Copy must never delete; making deletion unrepresentable is the strongest guard. |
| Identity = volume UUID plus a marker file; never name or mount path; duplicate or missing UUID stops and asks | Spec requirement; a lookalike drive at the same path is never written to (tested). |
| Media type only from macOS `SolidState`; otherwise `Unknown` with conservative defaults | Spec forbids guessing from names or sizes. Disk images report no `SolidState` (so `Unknown`); the real USB drives did report it. |
| Destination folder is never created by TwinSync once configured | A vanished drive must not leave a folder under `/Volumes` on the internal disk. |
| "Prepare to Disconnect" is not "safe to unplug"; Eject succeeds only if `diskutil` exits 0 **and** the volume left the mount table | Spec requirement; tested with a lying fake ejector. |
| `UiSnapshot` is an `Arc` swapped by the coordinator; big lists are `Arc`'d inside | The UI clones a pointer per frame; a 100k-row preview must not be deep-copied. |
| Legacy "SSD A / SSD B" import expects `{"source","ssd_a","ssd_b"}` (**assumed format**, user confirmed no real file exists) | Imported drives have no UUID, so they need confirmation before any write. Original file is renamed `.migrated`, never deleted. |
| About is a sidebar button that opens a window, not a fourth page; build info comes from `src/about.rs` and `build.rs` embeds the short git commit | The spec fixes the sidebar at Overview/Activity/Settings. The commit makes "which build is this?" answerable (it came up when the running app was an old binary). Falls back to `unknown` outside a git checkout. |
| CI runs only on macOS and is frugal (skips docs-only pushes, cancels superseded runs, bundle job only on request or tag) | The crate refuses to compile elsewhere, and private-repo macOS minutes are billed at 10x. |
| The UI is smoke-tested headlessly with `egui::Context::run_ui` and the output text is checked | The GUI previously had only a screenshot check. Texture deltas must be cleared (`out.textures_delta.clear()`) because there is no renderer. |
| Event-triggered scans identical to the previous one do not read drives that were already up to date | Added after the first real run (see section 5). User actions, the timer, wake and mount changes always read the drives. |

## 4. Where the implementation differs from, or falls short of, the original spec

Deviations the user agreed to or that follow from the spec's own "avoid unnecessary abstractions":

- tokio replaced by threads (above).
- Traits only for `DeviceResolver` and `Ejector` (they need fakes). `FileScanner`, `SyncPlanner`, `TransferExecutor`,
  `StateRepository` and `FilesystemCapabilities` are plain functions or structs.
- Added `engine/` and `scanner`-level tests beyond the listed test files (`engine_tests`, `scanner_tests`, `fs_compat_tests`).
- Per-drive performance profile is on the drive card, not on the Settings page (Settings explains this).

Gaps against the spec (not implemented or only partly):

- **No hard per-operation deadline.** Stall detection (20 s without progress shows "Waiting for drive"), bounded
  retries with exponential backoff, and cooperative cancel exist; a blocked syscall is never killed, by design.
- **No bulk resolution.** "Adopt if identical" is per item. A first sync onto a drive that already holds a copy
  would benefit from "adopt all identical".
- The shared-physical-disk gate (`WorkerEnv::gate`) is tested only for being set up and for a sync completing, not for
  actually serialising the two drives.
- Wake-from-sleep detection (wall clock vs monotonic clock gap) and watcher-overflow rescans are implemented but were
  never triggered on a real sleep or a real FSEvents overflow.
- Activity "Retry" re-runs the whole sync, not a single item.
- Pause is enabled even when nothing is configured (cosmetic, seen in the first screenshot).
- No app icon, no notarization, no menu bar, no launch at login (the last two are "future phases" in the spec).
- **GitHub Actions does not run at all** (see section 5). The workflow is written and its YAML is valid, but no CI has ever executed, so it is untested: the `bundle` job, the `rust-cache` and `dtolnay/rust-toolchain` steps, and whether the FSEvents tests and `live_read_only_query_of_the_boot_volume_works` pass on a hosted macOS VM are all unknown.
- The About window and the other pages are verified by the headless text test only; nobody has looked at the About window on screen yet (the sidebar button cannot be clicked from automation).
- CI has no job for `scripts/fs-compat-test.sh` (it needs disk images and `hdiutil` attach, which a hosted runner may not permit).

## 5. Findings log (what testing and real use taught us)

Found by automated tests while building:

- **Bug, fixed:** after importing a legacy config, confirming a drive computed the destination folder wrongly (volume root
  instead of `…/Backup`), so files would have gone to the wrong place. Caught by `old_ssd_a_b_config…` in `engine_tests`.
- **Bug, fixed:** a stale preview ("plan changed") was detected but the user was never told. Notice added.
- APFS rejects non-UTF-8 file names (`EILSEQ`), so the scanner can never see one there. Byte-exact paths are tested in
  `domain`/`migration` tests instead.
- On exFAT and FAT32, macOS keeps extended attributes in `._name` AppleDouble sidecars and moves them with renamed
  files. They show up inside version history. They are macOS's files, not TwinSync's; the compat test ignores them.
- A headless egui test panics with "Dropped TexturesDelta" unless `FullOutput::textures_delta` is cleared.
- macOS temp dirs are `/var/folders` symlinked to `/private/var`; `canonicalize` returns the real path, so test
  fixtures must use canonical paths.
- Disk images report `BusProtocol = "Disk Image"` and no `SolidState` (so media `Unknown`).

GitHub Actions (2026-11-10):

- The first push of `ci.yml` and a manual `workflow_dispatch` both ended `startup_failure` (0-2 s, no jobs, `path: BuildFailed`). GitHub says "likely a workflow file issue", but the file parses (the workflow is registered as `CI`, dispatch is accepted, `ruby -ryaml` loads it).
- A throwaway branch with a trivial workflow (`echo` on `ubuntu-latest` and on `macos-latest`) also ended `startup_failure`. So the cause is not the workflow. Repository settings look fine (`actions/permissions`: enabled, all actions allowed, read-only default token).
- **Cause found (after making the repo public, which did not help):** once the run could be created, the job's annotation read "The job was not started because your account is locked due to a billing issue." (read with `gh api repos/<owner>/<repo>/check-runs/<job id>/annotations`; `gh run view --log-failed` shows nothing because no step ever ran). The lock is on the **GitHub account**, so it affects public and private repositories alike. An earlier note here guessed it was private-repo billing and that going public would fix it; that was wrong.
- The billing endpoints themselves need the `user` token scope, which was not granted, so the exact reason (failed payment, spending limit, ...) is unknown. **Next step for the user:** fix it under GitHub Settings → Billing and plans, then re-run the workflow (Actions → CI → Run workflow) and record the result here. Everything in the `ci.yml` is still untested until it runs.
- **Re-run on 2026-11-11** (`workflow_dispatch`, run 43049947916): same annotation, job ended after 3 s with 0 steps. Still locked; nothing to change in the repo. Retry with `gh workflow run ci.yml --ref main` and read `gh api repos/rapeeza1598/twinsync/check-runs/<job id>/annotations`.
- Lesson: `gh workflow run` only accepts workflow files that exist on the default branch; to test a throwaway workflow give it a `push` trigger on its own branch. The probe branch and its runs were deleted afterwards.

Found in the first real-hardware session (2026-11-10, read back from the log and database, read-only):

- Drive A `MD202` = USB SSD, Drive B `HDD` = USB HDD, both exFAT; macOS reported `SolidState` for both.
- 57 files / 10.46 GB copied to both; 114 transfers all committed; 0 failures; Full Verify 0 mismatches on both.
  About 100 MB/s per drive. After about 16 h open: 50-90 MB RAM, about 0.1% average CPU.
- Not checked: whether files stay open on a connected drive while idle (drives were already unplugged when looked at).
- **Problem 1, fixed:** an empty `untitled folder` (created in Finder, then renamed in the source) stayed on both drives
  and the app said nothing. Folders that leave the source are now reported like files (`SourceDeleted`, dismissible).
- **Problem 2, fixed:** 15 of 41 syncs did nothing but re-read both drives (would wake a sleeping HDD); Finder rewriting
  `.DS_Store` is the likely trigger (file touched 22:39:47 local, no-op sync 22:39:49). Now: watcher ignores excluded paths;
  an event-triggered scan identical to the previous one skips drives that were already up to date.
- **Problem 3, fixed:** nothing recorded a drive disconnecting, Prepare to Disconnect, eject, setup, pause/resume, or why a
  scan ran, so the user's eject could not be audited. Activity and the log now record them (no paths).
- The user disconnected with Prepare to Disconnect then Eject in the app; both volumes left `/Volumes`.

## 6. Not yet tested on real hardware

Do these and then record the result here and tick the README checklist:

- Unplug a drive in the middle of a large copy; reconnect; check recovery and the Activity page.
- Let the HDD spin down, then Sync; check that "Waiting for the drive to respond" appears instead of an error.
- Change a file after the first sync and check version history on the real drive.
- Prepare to Disconnect **during** a copy (only done while idle so far).
- Collision with a file placed by hand; Full Verify compared against `shasum -a 256`.
- Whether a connected, idle drive has files held open by TwinSync (`lsof -p <pid> | grep /Volumes`).
- Sleep/wake with a drive attached; NTFS; FAT32 files near 4 GiB; a USB bridge that hides media type.

## 7. How to work on it

```sh
export PATH="$HOME/.cargo/bin:$PATH"     # cargo is not on the default PATH on this Mac
cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test && cargo build --release
scripts/fs-compat-test.sh                # real APFS/HFS+/exFAT/FAT32 on throwaway disk images
scripts/bundle-macos.sh                  # TwinSync.app (set BUNDLE_ID=... for a real identifier)
TWINSYNC_DATA_DIR=/tmp/twinsync-dev cargo run
```

- `clippy` and `rustfmt` were installed with `rustup component add clippy rustfmt` (the toolchain lacked them).
- egui/eframe 0.37 API: `App::ui(&mut Ui, ..)`, `Panel::left(id).show(ui, ..)`, `CentralPanel::default_margins()`,
  `egui_extras::TableBuilder` with `body.rows(..)`. `egui` is a direct dependency alongside `eframe`.
- Edition 2024: `unsafe extern`, `unsafe_op_in_unsafe_fn`, `env::set_var` is unsafe. Tests therefore pass data dirs
  explicitly instead of using environment variables.
- `cargo fmt` reflows code, so line numbers quoted in older notes drift. Search by name.
- Tests that depend on real FSEvents (`auto_sync_*`, `changes_to_excluded_*`, `an_unchanged_*`) were run four times in a
  row without flakes. A fake-event hook would make them deterministic if they ever flake.
- Mutation checks used on safety guards: temporarily break the guard, confirm a test fails, restore, `cmp` with the backup.
- macOS `sed -i` differs from GNU; multi-line edits were done with small Python scripts. PyYAML is not installed; validate workflow YAML with `ruby -ryaml -e 'YAML.load_file(".github/workflows/ci.yml")'`.

Architecture overview and the per-folder map are in the README (section "Architecture"); not repeated here.

## 8. Change log (newest first)

| Date | Commit | Change |
|---|---|---|
| 2026-11-11 | (this commit) | Re-ran CI: still `account is locked due to a billing issue`; recorded in the notes. |
| 2026-11-10 | `6a9ccb0` | Repository made public at the user's request (history scanned first: no secrets). Corrected the CI diagnosis: the account is billing-locked, public does not fix it. README, notes and memory updated. |
| 2026-11-10 | `4ac44de` | Set the GitHub repository About description and topics (no file changes besides this note). |
| 2026-11-10 | `f471510` | Record that GitHub Actions cannot start (account-level `startup_failure`, probe workflow also fails); README note; next steps for the user. |
| 2026-11-10 | `a4fd456` | About window (sidebar button; version, git commit, data/log folders, promises), `build.rs`, headless UI render tests, GitHub Actions CI workflow, README CI section and badge. |
| 2026-11-10 | `5f7cc4d` | Add this notes file, repo `CLAUDE.md` with the "always update the notes" rule. |
| 2026-11-10 | `30e7bdf` | README: record the first real-hardware session; tick only observed checklist items; document that source events no longer read up-to-date drives. |
| 2026-11-10 | `1b5f874` | Ignore watcher events for excluded files; skip drive reads after an identical event-triggered scan; record drive/eject/setup/control events and the trigger of each scan. Added `scans_completed` to the snapshot. |
| 2026-11-10 | `69e325a` | Report managed folders that left the source (`SourceDeleted` for dirs). |
| 2026-11-09 | `7035bd8` | README, macOS packaging (`Info.plist`, `bundle-macos.sh`), engine logging. |
| 2026-11-09 | `06aea4b` | Real-filesystem compat test and `fs-compat-test.sh` (APFS, HFS+, exFAT, FAT32 disk images). |
| 2026-11-09 | `3f788f2` | egui front end: overview, activity, settings. |
| 2026-11-09 | `a4477af` | Engine: coordinator, per-drive workers, monitor; fixed legacy-import root path bug. |
| 2026-11-09 | `067ec6e` | Device identity, destination validation, macOS integration. |
| 2026-11-09 | `508124d` | Safe transfer pipeline, journal, crash recovery. |
| 2026-11-09 | `a3b5d81` | Scanner and pure planner. |
| 2026-11-09 | `632ec74` | Domain types, config, SQLite storage and migrations. |
| 2026-11-09 | `d4414b9` | Cargo project scaffold. |

## Keeping this current

Whenever you change anything (code, behaviour, config, scripts, docs):

1. Update the relevant section above (status, decisions, gaps, findings, not-yet-tested) so it matches reality.
2. Add a row to the change log. A change-log row for a commit may name it as "this commit" until the next commit
   replaces that with the hash.
3. Update the "Last updated" line and the test/gate numbers if they moved.
4. If the change is visible to users, update the README too.
5. Commit with a gitmoji subject and push.

If something here turns out to be wrong, correct it rather than adding a contradicting note.
