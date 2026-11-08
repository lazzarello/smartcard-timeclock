# Development

Binary installation, commands, and hardware requirements live in [README.md](README.md).
Agent-specific instructions live in [AGENTS.md](AGENTS.md).

## Setup

Use Rust **1.89+**, Cargo, rustfmt, Clippy, pkg-config, and the PC/SC and SQLite
headers/libraries, plus GTK4 development libraries for the tray status window.
The CLI needs no display (GTK is initialized only when opening the status window).
Tray settings and reader-scan dialogs additionally need Zenity. GTK4 status/reports
windows and tray dialogs need a graphical session; the tray needs a session DBus
and StatusNotifierItem/DBusMenu host.

The existing Nix environment supplies Rust and the native/development tools:

```sh
devenv shell
cargo run --locked -- scan
```

The **host** must run `pcscd` and have a suitable reader driver. On NixOS, enable
`services.pcscd.enable = true;`. Do not start a second pcscd inside the development
shell. Confirm hardware independently with `pcsc_scan`. Build on NixOS in this
environment rather than using the generic Linux release binary.

For Debian/Ubuntu:

```sh
sudo apt-get install build-essential pkg-config libpcsclite-dev libsqlite3-dev libgtk-4-dev \
  pcscd libccid pcsc-tools dbus dbus-bin sqlite3 zenity
sudo systemctl enable --now pcscd.socket
rustup component add rustfmt clippy
```

Clone the repository (or extract its source archive), then build and install:

```sh
cargo build --locked --release --bin smartcard-timeclock
./target/release/smartcard-timeclock --help
install -Dm755 target/release/smartcard-timeclock "$HOME/.local/bin/smartcard-timeclock"
```

Ensure `~/.local/bin` is on `PATH`, or run the binary from `target/release` directly.
The release profile optimizes the binary and strips symbols and debug information.

Use temporary databases for development; never point test watchers at real time
records. For example:

```sh
test_dir=$(mktemp -d)
cargo run --locked -- --db "$test_dir/dev.sqlite3" watch "Test project"
# After stopping the watcher:
cargo run --locked -- --db "$test_dir/dev.sqlite3" report
rm -r -- "$test_dir"
```

## Checks

Run from the repository root:

```sh
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
# Headless protocol test: always use a fresh, isolated bus.
TIMECLOCK_ISOLATED_BUS=1 timeout 40s dbus-run-session -- cargo test --locked isolated_protocol -- --ignored
cargo build --locked --release --bin smartcard-timeclock
./target/release/smartcard-timeclock --help
```

The normal tests require no physical reader or desktop. They cover ATR parsing,
reader selection, matching, session persistence/recovery, clock rollback, write
contention, locking, reports, tray state, settings, and dialog lifecycle using
fake observations and temporary databases. Store regressions also cover v0→v1
migration, rollback/reopen, exact names, schema-version rejection, foreign keys,
archive/restore/delete, cascading rollback, name reuse, zero-session discovery,
archived tracking rejection, writer-lock ordering, and read-only legacy queries.
Project-management tests cover archived menu filtering, disarming/rearming,
deletion confirmation, stale database/revision targets, tracking guards, and
failed writes without recovery. Watcher regression tests inject PC/SC results
and elapsed/wall clocks: repeated context/reader outages, pinned identity,
fresh recovery/card changes, absence before/after recovery, successful polls across
suspend or write gaps, lock ownership, retry cancellation, and terminal storage
failures. Four tests are ignored by default:

- `isolated_protocol…` registers a fake StatusNotifierWatcher and exercises the
  real SNI/DBusMenu protocols, including idle menu requests that must not open
  app windows, arm a watcher, or create a database. **Never run it on the desktop
  session bus.** The command above provides an isolated bus and requires
  `dbus-run-session`, `dbus-send`, and `timeout`.
- `status_and_report_without_card_or_active_project…` opens real GTK4 status
  and reports windows. It checks live text, literal project names,
  singleton/reopen behavior, and unchanged report history. Run it separately in a
  graphical session:

  ```sh
  cargo test --locked status_and_report_without_card_or_active_project -- --ignored
  ```

- `reports_archive_restore_delete_and_independent_window_lifecycle` opens the
  real GTK4 overview without first opening Status. It checks missing-database
  reads, singleton/reopen behavior, literal names, archive/restore and failed-write
  checkbox state, tracking sensitivity, cancellation, and confirmed deletion.
  Run it separately in a graphical session, using only its temporary database:

  ```sh
  cargo test --locked reports_archive_restore_delete_and_independent_window_lifecycle -- --ignored
  ```

- `isolated_theme…` registers a fake desktop settings portal and checks initial
  dark mode, live light/dark/no-preference changes, unrelated signals, and cleanup.
  It also checks the 35-logical-pixel title bar in both modes, native title buttons,
  and expansion for larger fonts.
  It needs a graphical session (or a private headless compositor) **and an isolated
  bus**; never register the fake portal on the desktop bus:

  ```sh
  TIMECLOCK_ISOLATED_BUS=1 timeout 40s dbus-run-session -- cargo test --locked isolated_theme -- --ignored
  ```

Do not use a blanket `cargo test -- --ignored` on the desktop bus.

### Test organization and everyday use

Rust distinguishes tests by how they are compiled, not just by their directory:

- **Unit tests** are modules inside the application crate. They can test private
  functions without making those functions public. The CLI/watcher tests and
  their fake input live in `src/tests/mod.rs`; `#[cfg(test)] mod tests;` in
  `src/main.rs` loads that file only for test builds. `use super::*;` imports
  items from the parent module (`main.rs`). Store and tray unit tests remain in
  their respective source files. Inline and separate-file unit tests are both
  normal Rust layouts.
- **Integration tests** go in the repository-root `tests/` directory. Cargo
  builds each top-level `.rs` file there as a separate test crate. They cannot
  access this binary's private functions; use them for black-box CLI checks,
  launching `env!("CARGO_BIN_EXE_smartcard-timeclock")` with `std::process::Command`.
  This project does not currently have integration tests. Do not expose internals
  or include `main.rs` in another crate just to move unit tests there.

Enter `devenv shell` first when using the Nix development environment. Useful
commands (in addition to the full checks above):

```sh
# Run the normal suite.
cargo test --locked --all-targets
# Run one test by name (the filter matches substrings).
cargo test --locked atr_formats_and_validation
# Select exactly one relocated test by its full module path.
cargo test --locked tests::atr_formats_and_validation -- --exact
# Show stdout/stderr even for passing tests.
cargo test --locked atr_formats_and_validation -- --nocapture
# List tests without running them.
cargo test --locked --all-targets -- --list
```

Add a `#[test] fn descriptive_behavior_name()` to the relevant unit-test module
and assert the observable result with `assert!` or `assert_eq!`. Keep tests
independent: Cargo runs them in parallel by default, so use a fresh temporary
folder/database per test and fake card observations/clocks rather than real
hardware or sleeps. Keep test-only dependencies under `[dev-dependencies]` in
`Cargo.toml`. Preserve the explicit ignored-test commands and bus isolation
above for desktop/protocol checks.

### CI and GitHub Releases

[.github/workflows/ci.yml](.github/workflows/ci.yml) is the single GitHub Actions
pipeline. Pull requests, pushes to `main` or `v*` tags, and manual runs execute
the checks above in `rust:1.89-bookworm`, including the isolated-bus test. Feature
branch pushes do not trigger separate runs; open or update a PR to run CI.
CI installs the native headers and rustfmt/Clippy, but needs no pcscd service, reader, or
display. Visible desktop and physical hardware checks remain manual.

Every successful run uploads the `smartcard-timeclock-linux-x86_64` Actions
artifact containing:

- `smartcard-timeclock-linux-x86_64.tar.gz`: stripped release binary and
  [GPL-3.0-or-later license](LICENSE.txt), preserving executable permissions.
- `smartcard-timeclock-source.tar.gz`: matching tracked source from the same
  commit, including `Cargo.lock` and the license.
- `SHA256SUMS`: SHA-256 checksums for both archives; verify after downloading with
  `sha256sum --check SHA256SUMS` in their directory.

The only prebuilt target is Linux x86-64, dynamically linked against Debian 12
libraries (glibc 2.36 baseline, PC/SC, SQLite, GTK4). Newer compatible distributions
need those runtime libraries; NixOS uses a source build. Tray compatibility is
protocol-based, not distribution-based: `ksni` requires an SNI/DBusMenu host on
the session bus, with GTK4 for status/reports and Zenity for settings/reader scanning. CI's headless test
does not certify any particular desktop.

#### Publish a release

1. Update `version` in `Cargo.toml` and regenerate `Cargo.lock` with
   `cargo check`. Commit both along with the intended changes.
2. Merge or push the commit to `main` and wait for CI. Download its Actions artifact,
   verify its checksums, and test the binary on the intended host using the manual card/tray
   acceptance steps below. Review tracked files for private data.
3. Tag that exact accepted commit with `v` plus the package version, then push
   the tag. For example, for package version `0.1.0`:

   ```sh
   git tag -a v0.1.0 <accepted-commit> -m "Release v0.1.0"
   git push origin v0.1.0
   ```

4. The tag pipeline rejects a version mismatch, reruns all checks, and rebuilds
   the archives. Only after success does its `release` job publish a GitHub
   Release with generated notes, both archives, and `SHA256SUMS`. Only that job
   receives `contents: write`; builds and pull requests remain read-only.
5. Review the published release notes and download/install the public asset to
   confirm the user path. The README's Releases link becomes usable after the
   first release is published.

Do not move published tags or replace assets; fix mistakes with a new version.
If publishing fails before a release is created, rerun the failed job after
fixing repository permissions. A failed check must be fixed and revalidated
before tagging another commit. Manual workflow runs produce artifacts only;
only a `v*` tag triggers publishing. No cross-build matrix or package repository
is maintained.

## Code map and behavior to preserve

- `src/main.rs`: clap commands, ATR parsing/formatting, PC/SC reader discovery and
  polling, watcher lock, shutdown signals, and the shared watcher.
- `src/store.rs`: versioned SQLite schema/migration, project registry and lifecycle,
  transactional checkpoints, recovery, read-only discovery/reports, and report formatting.
- `src/tray.rs`: SNI/DBusMenu presentation, controller, worker ownership, and
  Zenity dialog lifecycle. `src/tray/reports.rs` owns the GTK4 project overview
  and delete confirmation widgets; callbacks send actions to the same controller.
  It uses the same watcher/store as the CLI, not a separate timer.
- `src/tests/mod.rs`: CLI/watcher unit tests and fake observations, loaded by
  `#[cfg(test)] mod tests;` in `src/main.rs`. Store and tray unit tests remain
  inline under `#[cfg(test)]` in their implementation modules.
- `Cargo.lock` is committed for reproducible application builds.

### Card observation and timing

ATR matching identifies a **card type**, not a physical card or authenticated
user. Do not describe it as a unique identifier or silently add card commands.
The default ATR is the `TARGET_ATR` constant in `src/main.rs`, not an external
configuration file. Reader selection is automatic only with one reader; an
explicit selection must match an exact PC/SC name.

A matching card already present at startup starts timing immediately, without
backdating insertion. Repeated matching observations checkpoint a single open
session. Removal, a different ATR, or an unresponsive card closes that session
and leaves the project armed for reinsertion. Where the PC/SC event counter
supports it, a removal/reinsertion between polls splits sessions; its exact
unobserved transition time cannot be reconstructed.

Checkpoints are roughly once per second. Removal, Ctrl-C, SIGTERM, and reader
failure close at the **last confirmed-presence checkpoint**, not after polling
or SQLite waits. Temporary reader/PC/SC interruptions keep the shared watcher and
its database lock alive, publish `Reconnecting` (nontracking), and retry about
once per second with cancellation-aware waits. Reestablish a new PC/SC context
and `UNAWARE` reader state, pinning the exact initially resolved name (even with
automatic initial selection). Never fall back to another reader. Initial reader
validation and unexpected PC/SC/storage errors remain terminal. Recovery requires
fresh usable matching presence; absent/different/muted/unpowered cards show Paused,
even when no target card had been observed before the outage.

Linux `/proc/uptime` supplies a suspend-inclusive elapsed clock. An observation gap
over three seconds, including time spent in SQLite, closes at the old checkpoint
and discards any poll result spanning that gap. Clear cached state/counters and
require fresh observation before starting a new session; do not infer presence
from before suspend. Clock-read failure is terminal, not a disabled guard. This
conservative threshold can split sessions during scheduling stalls and cannot
detect every shorter suspend. Observation timestamps/baselines are captured before
SQLite writes. Unfinished sessions on the next watch close at their last
checkpoints, never at restart time. Whole-second UTC Unix timestamps use wall time;
backwards updates are clamped, but large clock adjustments still distort elapsed time. Shutdown or
switching may wait for one poll plus SQLite's busy timeout; this wait is not
billable time. Retry waits check cancellation every 50 ms. PC/SC establishment,
listing, and context release have no native timeout argument; a wedged native call
can still delay shutdown.

### Storage and locking

`sessions` records project, ATR, reader, start/checkpoint/end timestamps, and stop
reason. A partial unique index permits only one open session. Database constraints
validate names, timestamp ordering, and end/reason consistency. Use parameterized
SQL and keep transactions/error handling intact.

A canonical-path `.lock` file permits one writer per database while reports stay
usable. `Store::open` acquires it before SQLite initialization or migration and
holds it for the connection's lifetime, including project lifecycle operations.
Only the empty database file may be created before locking so its canonical path
can be resolved. The connection closes before its lock guard explicitly unlocks,
so a descriptor briefly inherited by a subprocess cannot prolong ownership.
In-memory unit-test stores need no filesystem lock. Keep the lock file in place.
Hard-linked database aliases or deleting a live lock file can defeat lock
identity. Use a local filesystem. Do not change database paths implicitly or
silently fall back to another file.

CLI reports and overview reads are read-only: they do not create databases or
recover sessions. Only explicit project lifecycle actions write from Reports. An open
session contributes only through its checkpoint; `OPEN` is not a liveness check.
`SESSIONS` increases per insertion, not per heartbeat, and includes an open
session. Removal does not complete/delete a project. Names are exact and
case-sensitive, with spaces and Unicode preserved. Archived projects retain their
history in reports; permanently deleting a project removes its sessions as well.
There is no completion state, daemon, or automatic billing.

`PRAGMA user_version = 0` denotes the legacy sessions-only schema; version 1 adds
`projects(name TEXT PRIMARY KEY COLLATE BINARY, archived INTEGER DEFAULT 0)` with
nonblank-name and boolean constraints. Each session references its project with
`ON DELETE CASCADE`; writable connections enable foreign keys. Initialization
backfills exact names, rebuilds sessions without changing IDs/metadata/timestamps,
recreates the single-open-session index, checks foreign keys, and sets version 1
in one transaction. Failure rolls back schema, data, and version. Migration does
not recover sessions; watcher recovery remains separate. Unknown newer versions
are rejected for writes before changing the journal mode. Do not use older
binaries to write upgraded databases.

`Store::create_project` registers/validates an active name without recording time;
watcher startup calls it before recovery/hardware access. `record` also registers
new names transactionally, rejects archived names, and cannot checkpoint a different
project's open session. `archive_project`/`unarchive_project` retain history;
`delete_project` requires an archived project and cascades to all its sessions.
Lifecycle validation and mutation share a transaction, missing names fail explicitly,
and a project's open session blocks lifecycle changes. Set-archive operations are
idempotent; deletion is not. A deleted name may be created again with no history.
Deletion is permanent in the application, not forensic secure erasure of backups or
SQLite/WAL bytes. No archive timestamps or numeric project IDs are stored.

`project_rows` is read-only and includes archived and zero-session projects; it
also discovers legacy session names without migrating them. `report_rows` remains
session-based and works on both schema versions: archived history is included,
deleted history is absent, and zero-session projects add no rows. Both queries
fail on a missing database without creating it or recovering open sessions.
Controller mutations stop/join the database's watcher before opening another
writer, recheck tracking/stale actions, and never restart archived projects.

Before removing a temporary test directory, inspect its **test** database with:

```sh
sqlite3 "$test_dir/dev.sqlite3" \
  "SELECT project, datetime(started_at, 'unixepoch') AS start_utc,
          datetime(ended_at, 'unixepoch') AS end_utc, stop_reason
   FROM sessions ORDER BY id;"
```

Back up only with the watcher stopped or via SQLite's backup command. Do not
commit databases, WAL/SHM files, lock files, or private project history.

### Tray and dialogs

The GTK4 Reports window is independent of Status and reuses its compact header
and desktop theme behavior. One controller-owned theme subscription serves both
windows, including when hidden. GTK events are serviced when either window exists.
Reports reads every registered project, joining read-only totals with zero-session
and archived rows. Only active projects populate the tray Projects menu; a stale
selected name must not reintroduce an archived project.

Archive changes apply once, with checkbox state reflecting committed data only.
Lifecycle actions recheck queued worker observations after blocking validation,
join the existing watcher before acquiring the writer store, and retain database
path/revision targets to reject stale actions. Archiving the selected project clears
selection and leaves Idle; unrelated changes rearm a previously armed watcher after
releasing the store lock. A failed write leaves the selection intact but stops the
watcher honestly. No lifecycle action implicitly recovers stale open sessions.

Delete is available only for archived rows and requires a parented, modal GTK4
confirmation with literal name, session count, total, Cancel as default, and a
separately labeled destructive action. Cancel does not stop the watcher or mutate
data. Switching databases cancels confirmation and invalidates queued report
actions, including after switching away and back to the same path. Closing Reports
cancels confirmation but does not stop tracking. Quit destroys both GTK windows.

Launch is idle: it does not create/recover a database or arm a project. Selecting
a project automatically starts watching; a missing database is created then.
Projects come from the active-project registry plus an as-yet-unregistered
selected name (legacy databases use distinct session names). Archived selections
are never appended back into the menu. Watcher startup registers selected names,
so projects with no sessions persist across selections/restarts. Settings and
selection themselves remain in-memory only. Archived names cannot be selected or
implicitly restored. Changing to a database where the selected name is archived
clears selection.

The card alone controls tracking: no manual Start/Stop or Play/Pause controls.
Project/settings actions are read-only during tracking, including submissions
from dialogs opened before insertion. Recheck worker events after potentially
blocking validation and before changing settings. Validate before mutation; Cancel
must change nothing. Join the previous watcher before switching and discard its
event queue so stale events cannot affect the new selection.

Left-click opens/reuses a compact read-only GTK4 status window; right-click opens
the host-rendered DBusMenu, including Report. Menu layout requests and opened/closed
events must not open GTK/Zenity windows or arm a watcher. For a menu-opening flash,
distinguish the host's surface (such as DMS's `dms:tray-menu-window` layer) from the
app's `Timeclock status` window before changing app window code.
The status line is, for example,
`Tracking My Project (01:58:00)` or `Paused My Project (01:58:01)`. It updates while
open, freezes during pauses, and shows selection/waiting/error states honestly.
Closing the window does not stop the watcher. The native GTK header bar uses
widget-local sizing CSS for a 35-logical-pixel minimum height (including its bottom
border), matching Alacritty's Wayland decoration. It retains themed colors, native
window controls/dragging, display scaling, and room to expand for larger fonts.
Its light/dark appearance follows
`org.freedesktop.portal.Settings`'s `org.freedesktop.appearance/color-scheme`, read
asynchronously and monitored via `SettingChanged`. No preference or an unavailable
portal preserves the configured GTK theme; no shell settings are changed. The
subscription and pending read are shared by both windows and cancelled on controller
shutdown. GTK
events are serviced by the tray controller; there is no separate timer. Reports and reader scanning remain
available while tracking. The selected project's total includes all persisted
sessions/checkpoints and excludes pauses; titles and tooltips reflect the same
project/status/time. Errors must be actionable, never shown as successful tracking.
Temporary reader interruptions show “Reader unavailable — reconnecting” in the
status window/title/tooltip, with a nontracking icon and frozen checkpointed total.
Recovery keeps the worker alive; switching while reconnecting still joins it and
discards its queued events. After fixing a terminal watcher error, reselecting the
project retries.

Every Zenity child and pipe thread is owned and reaped on completion, Quit,
Ctrl-C, or SIGTERM. Closing scan output early may break its input pipe without making
shutdown fail. Preserve literal names and escape menu underscores. Do not install
DMS plugins, autostart entries, or desktop configuration as a side effect.

## Manual acceptance (temporary data only)

### CLI / physical card

1. Run `scan` and confirm exact reader names and ATRs match `pcsc_scan`.
2. Start `watch` against a temporary database, insert the target for several
   seconds, remove it, and check `report`. Reinsert/remove: a second session must
   add time without counting the pause. A different-ATR card must be ignored.
3. Stop and run `resume` with the same name/database; totals must persist. Also
   test startup with the matching card already inserted.
4. With only test data, check a second watcher fails to acquire the same database
   lock. With and without a card, unplug/replug the same reader: unavailable/
   reconnecting must retain the project, reader/ATR and lock; fresh matching presence
   must resume, fresh absence must show Paused. A different or unusable card must
   not track; another reader alone must not substitute. Repeat interruptions and
   quit during retry. Check recovery after terminating an active watcher; no
   downtime may be added.
5. When explicitly permitted on the test host, repeat suspend/resume and a Nix
   rebuild with and without the card using only temporary data. Check separate
   sessions ending at the old checkpoint, no suspend/outage backfill, and fresh
   post-recovery status. A successful PC/SC poll after a long gap must also split.
   These host/hardware operations are manual acceptance only, not normal tests.
   For totals checks, capture stdout's UTC-timestamped `Started`, `Paused`, and
   `Reader unavailable` lines and compare against the temporary database's session
   timestamps (query above). These logs use pre-write observation timestamps;
   pause/outage detection can be later than the last billed checkpoint. Do not
   treat the detection timestamp as the session's end.

### Tray / visible desktop

```sh
test_dir=$(mktemp -d)
cargo run --locked -- --db "$test_dir/acceptance.sqlite3" tray
# After quitting all test watchers/dialogs, inspect if desired, then:
rm -r -- "$test_dir"
```

1. Confirm a visible clock icon in DMS (or the intended host). With no card and
   no selected project, right-click and dismiss the menu repeatedly: no extra
   window or top-left flash should appear, and tracking must remain idle. Check
   both the first opening and subsequent openings. **Settings → Database path…**
   shows the absolute path. Verify Zenity entry/cancellation and screen-reader
   labels/focus. Left-click opens a
   compact read-only status window, even before recording a session; repeated
   clicks must reuse it. Verify keyboard focus, text selection/screen-reader
   output, closing, and reopening without changing tracking. Toggle the desktop's
   light/dark preference with the window open and hidden: reopening must match the
   current mode, with readable text and decorations. Compare the title-bar height
   against Alacritty at the same display scale; verify dragging, the close button,
   and unclipped text at larger accessibility font sizes. No toolbar clock should appear.
   Open **Report…**
   from the right-click menu before opening Status as well as afterward; repeated
   requests must raise the same GTK4 overview. Check resize/scroll behavior, literal
   long/Unicode project names, keyboard focus, and screen-reader labels for each
   Archived checkbox and Delete button. Totals-only updates must preserve focus.
   Missing databases must show an empty state without creating a file.
2. Scan readers; set the exact reader and ATR. Enter/select projects with spaces,
   underscores, and Unicode. Selection must automatically arm the watcher.
3. Keep the status window open. Without a card: waiting for card, project, and
   total. Insert: `Tracking My Project (HH:MM:SS)`, with cumulative time updating and
   project/settings controls locked. Remove: `Paused My Project (HH:MM:SS)`, time
   fixed and controls editable. Reinsert: time resumes without counting absence. No manual timer
   controls should appear; a different-ATR card must not track.
4. Open project entry while absent, insert the card, then submit: the active
   project must not change. Remove and select another project; insertion must
   track that project without an extra Resume action.
5. Try blank projects, invalid ATRs, and Cancel: no unintended mutations. Valid
   settings OK closes the dialog and applies once. Change databases: no further
   writes to the old path. Invalid paths/missing readers must show errors, not
   tracking or fallback. With no selected project, settings must leave the tray idle.
6. In Reports, archive a project: its row/history stays visible but its tray menu
   entry disappears. Restore it: the entry returns without starting its watcher.
   Archive the selected, paused project: selection clears and reinsertion must not
   restart it. Confirm that all lifecycle controls disable while tracking, including
   a delete confirmation opened before insertion. Delete requires archive first;
   verify Cancel/Escape/window close change nothing, then confirm deletion and check
   both registry and session history are gone. Reuse the name and verify no old time.
   Switch databases while a confirmation is open: it must close and never affect a
   same-named project in the new database. Close/reopen Reports without stopping
   tracking, and check write failures leave saved checkbox state and visible errors.
7. While tray watching is active, start a second CLI watcher against **only this
   temporary database**: it must fail the lock. Quit with a dialog open: it must
   close, and reports must show zero open sessions. Repeat with Ctrl-C/SIGTERM.
   Ensure no test watcher remains before removing the directory.

Automated fake-observation and isolated-DBus tests cannot replace physical-card,
visible-desktop, or accessibility acceptance.
