# Development

Binary installation, commands, and hardware requirements live in [README.md](README.md).
Agent-specific instructions live in [AGENTS.md](AGENTS.md).

## Setup

Use Rust **1.89+**, Cargo, rustfmt, Clippy, pkg-config, and the PC/SC and SQLite
headers/libraries, plus GTK4 development libraries for the tray status window.
The CLI needs no display (GTK is initialized only when opening the status window).
Tray dialogs additionally need Zenity and a graphical session with a session DBus
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
fake observations and temporary databases. Three tests are ignored by default:

- `isolated_protocol…` registers a fake StatusNotifierWatcher and exercises the
  real SNI/DBusMenu protocols. **Never run it on the desktop session bus.** The
  command above provides an isolated bus and requires `dbus-run-session`,
  `dbus-send`, and `timeout`.
- `status_and_report_without_card_or_active_project…` opens a real GTK4 status
  window and Zenity report dialogs. It checks live text, literal project names,
  singleton/reopen behavior, and unchanged report history. Run it separately in a
  graphical session:

  ```sh
  cargo test --locked status_and_report_without_card_or_active_project -- --ignored
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

### CI and GitHub Releases

[.github/workflows/ci.yml](.github/workflows/ci.yml) is the single GitHub Actions
pipeline. Pushes (including tags), pull requests, and manual runs execute the
checks above in `rust:1.89-bookworm`, including the isolated-bus test. CI installs
the native headers and rustfmt/Clippy, but needs no pcscd service, reader, or
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
the session bus, with GTK4 for status and Zenity for dialogs. CI's headless test
does not certify any particular desktop.

#### Publish a release

1. Update `version` in `Cargo.toml` and regenerate `Cargo.lock` with
   `cargo check`. Commit both along with the intended changes.
2. Push the commit and wait for CI. Download its Actions artifact, verify its
   checksums, and test the binary on the intended host using the manual card/tray
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
- `src/store.rs`: SQLite schema, transactional checkpoints, recovery, read-only
  report queries, and report formatting.
- `src/tray.rs`: SNI/DBusMenu presentation, controller, worker ownership, and
  Zenity dialog lifecycle. It uses the same watcher/store as the CLI, not a
  separate timer.
- Tests live next to their implementation under `#[cfg(test)]`. `Cargo.lock` is
  committed for reproducible application builds.

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
or SQLite waits. Reader failure exits the CLI with an error. Recovery on the
next watch closes unfinished sessions at their last checkpoints, never at
restart time. Whole-second UTC Unix timestamps use wall time; backwards updates
are clamped, but large clock adjustments still distort elapsed time. Shutdown or
switching may wait for one poll plus SQLite's busy timeout; this wait is not
billable time.

### Storage and locking

`sessions` records project, ATR, reader, start/checkpoint/end timestamps, and stop
reason. A partial unique index permits only one open session. Database constraints
validate names, timestamp ordering, and end/reason consistency. Use parameterized
SQL and keep transactions/error handling intact.

A canonical-path `.lock` file permits one watcher per database while reports stay
usable. Keep the lock for the watcher's lifetime and leave the lock file in place.
Hard-linked database aliases or deleting a live lock file can defeat lock
identity. Use a local filesystem. Do not change database paths implicitly or
silently fall back to another file.

Reports are read-only: they do not create databases or recover sessions. An open
session contributes only through its checkpoint; `OPEN` is not a liveness check.
`SESSIONS` increases per insertion, not per heartbeat, and includes an open
session. Removal does not complete/delete a project. Names are exact and
case-sensitive, with spaces and Unicode preserved. There is no project registry,
completion state, daemon, or automatic billing.

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

Launch is idle: it does not create/recover a database or arm a project. Selecting
a project automatically starts watching; a missing database is created then.
Projects come from database history plus the currently selected name. Unrecorded
names disappear when another name is selected; settings and selection are
in-memory only.

The card alone controls tracking: no manual Start/Stop or Play/Pause controls.
Project/settings actions are read-only during tracking, including submissions
from dialogs opened before insertion. Recheck worker events after potentially
blocking validation and before changing settings. Validate before mutation; Cancel
must change nothing. Join the previous watcher before switching and discard its
event queue so stale events cannot affect the new selection.

Left-click opens/reuses a compact read-only GTK4 status window; right-click opens
the unchanged action menu, including Report. The status line is, for example,
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
subscription and pending read are cancelled when the window is destroyed. GTK
events are serviced by the tray controller; there is no separate timer. Reports and reader scanning remain
available while tracking. The selected project's total includes all persisted
sessions/checkpoints and excludes pauses; titles and tooltips reflect the same
project/status/time. Errors must be actionable, never shown as successful tracking.
After fixing a watcher error, reselecting the project retries.

Every Zenity child and pipe thread is owned and reaped on completion, Quit,
Ctrl-C, or SIGTERM. Closing a report early may break its input pipe without making
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
   lock. Check reader disconnect errors, graceful shutdown, and recovery after
   terminating an active watcher; no downtime may be added.

### Tray / visible desktop

```sh
test_dir=$(mktemp -d)
cargo run --locked -- --db "$test_dir/acceptance.sqlite3" tray
# After quitting all test watchers/dialogs, inspect if desired, then:
rm -r -- "$test_dir"
```

1. Confirm a visible clock icon in DMS (or the intended host). Right-click for
   the full menu; **Settings → Database path…** shows the absolute path. Verify
   Zenity entry/cancellation and screen-reader labels/focus. Left-click opens a
   compact read-only status window, even before recording a session; repeated
   clicks must reuse it. Verify keyboard focus, text selection/screen-reader
   output, closing, and reopening without changing tracking. Toggle the desktop's
   light/dark preference with the window open and hidden: reopening must match the
   current mode, with readable text and decorations. Compare the title-bar height
   against Alacritty at the same display scale; verify dragging, the close button,
   and unclipped text at larger accessibility font sizes. No toolbar clock should appear.
   Open **Report…**
   from the right-click menu; repeated requests must not duplicate that dialog.
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
6. While tray watching is active, start a second CLI watcher against **only this
   temporary database**: it must fail the lock. Quit with a dialog open: it must
   close, and reports must show zero open sessions. Repeat with Ctrl-C/SIGTERM.
   Ensure no test watcher remains before removing the directory.

Automated fake-observation and isolated-DBus tests cannot replace physical-card,
visible-desktop, or accessibility acceptance.
