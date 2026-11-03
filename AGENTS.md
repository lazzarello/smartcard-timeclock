# Development guidelines for coding agents

Read [README.md](README.md) for the user contract and
[DEVELOPMENT.md](DEVELOPMENT.md) for setup, internals, checks, and acceptance.
Keep those documents in sync with user-visible changes.

## Scope and structure

- Keep changes small and complete. Reuse existing code and dependencies; avoid
  new abstractions, configuration, or features without a concrete need.
- `src/main.rs` owns CLI/PCSC/watcher logic, `src/store.rs` owns persistence and
  reports, and `src/tray.rs` adapts the shared watcher to SNI/DBusMenu and Zenity.
  Do not create a separate tray timer or add manual tracking controls.
- Rust edition 2024, minimum Rust 1.89. Preserve that minimum and commit
  `Cargo.lock` changes when dependencies intentionally change.
- Keep the single container CI pipeline in `.github/workflows/ci.yml`; the
  distributable is `cargo build --locked --release --bin smartcard-timeclock`.
  Preserve the stripped, debug-info-free release profile.

## Safety and behavior

- ATR is a card-type trigger, not a serial number or authentication. The app
  observes PC/SC presence/ATR only; do not add application/card-data commands or
  claim unique identity. Keep exact reader selection and ATR validation.
- Only a present, usable, matching card may accrue time. Removal pauses; insertion
  resumes. An already inserted card starts now, never at an assumed earlier time.
- Preserve one open session, transactional checkpoints, timestamp constraints,
  parameterized SQL, and the lifetime-held database lock. Removal/shutdown/error
  and crash recovery end at the last confirmed checkpoint, not after a blocked
  write or at restart. Never count pauses or downtime.
- Reports stay read-only and must not create databases or recover sessions.
  Project names are exact/case-sensitive; preserve spaces, Unicode, and literal
  text. Do not silently change/fall back to another database.
- Tray launch stays idle. Selection arms the shared watcher. Project/settings
  changes are forbidden while tracking, including stale dialog submissions.
  Validate before mutation, recheck worker events after blocking validation,
  join the old worker before switching, and discard stale events.
- Cancel changes nothing; OK applies once and closes the dialog. Reap all child
  processes/pipe threads on completion and shutdown. Surface errors honestly and
  preserve accessible labels, focus, status, and menu escaping.
- Use temporary databases for tests and manual checks. Never touch real time
  records, delete a live lock, discard live WAL/SHM files, or commit private data.
  Do not alter host services/desktop configuration without explicit permission.

## Verification

For changed nontrivial behavior, add/update a focused regression test alongside
existing tests. Run:

```sh
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
TIMECLOCK_ISOLATED_BUS=1 timeout 40s dbus-run-session -- cargo test --locked isolated_protocol -- --ignored
cargo build --locked --release --bin smartcard-timeclock
./target/release/smartcard-timeclock --help
```

Never run the fake StatusNotifierWatcher test on the desktop bus or use blanket
`--ignored` there. The real Zenity test, card insertion/removal, desktop tray,
and accessibility checks require the manual steps in `DEVELOPMENT.md`. State
which checks ran and which remain unverified; do not claim hardware/GUI success
from headless tests.
