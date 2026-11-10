# TwinSync: instructions for Claude Code

One-way "Update Copy" from a Mac folder to two external drives, in Rust (egui). Read `docs/PROJECT_NOTES.md` first:
it holds the current status, the decisions and why, known gaps, and the change log.

## Standing rule from the user: always keep the notes updated

**Whenever you change anything in this repository (code, behaviour, config, scripts, docs), update
`docs/PROJECT_NOTES.md` in the same commit**: the relevant section, the change-log row, the "Last updated" line and
the test/gate numbers. Update `README.md` as well when users can see the change. Follow the checklist at the bottom
of the notes file. If the notes are wrong, fix them rather than appending a contradiction.

## Workflow

- Commit and push as you go. Subjects start with a gitmoji; end the message with the `Co-Authored-By` trailer the
  harness specifies.
- Language: talk to the user in Thai; keep code, commit messages and docs in English.
- Be honest about verification: say what was run, what used mocks, what was not tested. Never claim real-hardware
  testing that did not happen.

## Commands

```sh
export PATH="$HOME/.cargo/bin:$PATH"     # cargo is not on the default PATH
cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test && cargo build --release
scripts/fs-compat-test.sh                # real filesystems on throwaway disk images
scripts/bundle-macos.sh                  # TwinSync.app
TWINSYNC_DATA_DIR=/tmp/twinsync-dev cargo run
```

## Safety rules

- Tests and experiments must never touch real drives or the real data directory
  (`~/Library/Application Support/TwinSync`). Use temp folders, fakes, `TWINSYNC_DATA_DIR`, or the disk-image script,
  which detaches only what it attached.
- Do not launch or restart the user's running app unless asked.
- Update Copy never deletes, never overwrites a file it did not create, and never writes to the source. The planner has
  no delete action on purpose; keep it that way.
- Never write file contents, file names or folder paths to logs or the Activity page.
- If the harness refuses an action (for example publishing), stop and ask; do not work around it.
