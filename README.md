# Smartcard time clock

A local Rust CLI with an optional system-tray menu: insert the target card to track time on a named project; remove it
to pause. Reinsert it to resume the same project. Sessions are stored in SQLite. No network or privileged access to the card is needed. The CLI does not require a GUI.

## Card identity: ATR is not a serial number

The ATR (Answer To Reset) describes a card's communication parameters and often its
card type. **It is not generally unique to a physical card.** Two cards with the
same ATR will both trigger this app. There is no universal PC/SC command for a
unique ID on arbitrary contact smartcards; stronger identification would require
knowing the card's application and supported commands.

The default is the ATR in `target-card-id.txt`:

```text
3B 68 00 00 99 02 00 D1 03 04 22 01
```

This is a convenience trigger, not authentication. The app only observes PC/SC
presence and ATR data; it sends no application commands and reads no bank-account
data from your card.

## Plan / implementation

1. Use the native PC/SC API to discover readers and wait for insertion/removal.
   USB `058f:9540` is your **reader's** ID, not the card's ID. PC/SC exposes reader
   names rather than USB IDs; the app selects the sole reader automatically or
   accepts an exact `--reader` name when several are connected.
2. On matching insertion, create one project session. Repeated presence events
   update its checkpoint rather than starting duplicate sessions. Removal or an
   unmatched/unresponsive card closes the timed session, not the project; the watcher
   stays paused until the target card is reinserted. A matching card already inserted
   at startup starts tracking immediately, with no attempt to backdate its insertion.
3. Persist project, ATR, reader, UTC Unix start/checkpoint/end timestamps (seconds),
   and stop reason in `sessions`. A partial unique index permits only one open
   session. A file lock permits only one watcher per database; reports remain usable.
4. Checkpoint once per second while present. Removal and Ctrl-C/SIGTERM close at
   the last confirmed-presence checkpoint, never after a blocked SQLite write.
   Reader/PC/SC failure closes at the last checkpoint and exits with an error; reconnect
   the reader and restart the watcher. On the next watch run after a crash or power
   loss, close any unfinished session at its last checkpoint, not at restart time.
5. Provide project totals and tests for matching, event persistence, recovery,
   validation, and locking. No daemon, project registry, or automatic billing.

## Run

The host must have a working `pcscd` service and reader driver (as it already does
if `pcsc_scan` works). On NixOS, enable `services.pcscd.enable = true;` in the host
configuration if needed. Do not start a second pcscd inside the development shell.

`devenv.nix` supplies Rust, pkg-config, pcsclite, pcsc-tools, SQLite, Zenity, and DBus tools:

```sh
devenv shell
cargo run -- scan
cargo run -- watch "My project"
```

Leave `watch` running, then insert/remove the card. Press Ctrl-C to exit. Run totals
in another terminal (using the same database path):

```sh
cargo run -- report
```

Optional overrides:

```sh
cargo run -- --db ./work.sqlite3 watch "Client A" \
  --reader "Alcor Micro AU9540 00 00" \
  --atr "3B 68 00 00 99 02 00 D1 03 04 22 01"
cargo run -- --db ./work.sqlite3 report
```

`--atr` also accepts compact or colon-separated hex. Change projects by stopping
`watch` and starting it with another project name. There is no enrollment step.

### System tray (DMS / DankMaterialShell)

From the development shell, in your graphical session:

```sh
cargo run -- --db /absolute/path/to/work.sqlite3 tray
# The same reader/ATR overrides are available at launch:
cargo run -- --db /absolute/path/to/work.sqlite3 tray \
  --reader "Alcor Micro AU9540 00 00" --atr "3B:00"
# Or build once and launch target/debug/smartcard-timeclock --db ... tray
```

The Rust process exports standard **StatusNotifierItem + DBusMenu** using `ksni`,
like Discord/Signal. DMS's System Tray widget consumes these protocols; no DMS
plugin, service, autostart entry, or desktop configuration change is installed.
A session DBus and a running tray host are required. Zenity must be on PATH for
dialogs (`devenv shell` supplies it). Keep the launching terminal open, or launch
the built binary from your desktop launcher with an explicit absolute `--db`.

**Launch is idle and does not create or recover a database.** A single left-click
on the clock icon opens the report; a single right-click opens the full menu.
Choose an existing project under
**Projects**, or **New project…** to enter a name. Selection automatically starts
watching: insertion starts timing and removal stops timing. If the target card is
already inserted, timing starts immediately. There are no manual Play/Pause or
Start/Stop timer controls. Names are exact and case-sensitive. Projects come from the selected database's
history plus the currently selected name. Unrecorded names disappear when you select
another name; they are not a saved project registry.

The tray menu header and tooltip show **Timeclock: tracking (HH:MM:SS)** or
**Timeclock: paused (HH:MM:SS)** after card removal (reinsertion resumes).
Time is the selected project's cumulative total through the last recorded checkpoint,
excluding pauses. No time is shown without an active project or while waiting for
its first insertion. Waiting, idle, and error explanations appear in the header
and tooltip; there is no duplicate status row in the menu.
The icon also indicates status: green clock hands mean tracking,
an orange dot means waiting for the card, a grey clock means no project is armed,
and a red cross means error. The menu always shows the selected project.
The current database path is shown when opening **Settings → Database path…**.

**Project selection and settings become read-only while tracking.** Remove the
target card to unlock them; selecting another project while absent automatically
arms that project. A dialog opened before insertion cannot change the project or
settings after tracking starts. Reports and reader scanning remain available.
After a reader/lock/database error, inspect the error, fix its cause, and reselect
the project to retry. Only one watcher runs at a time.

**Scan readers…** shows exact reader names and ATRs without opening SQLite.
**Report…** shows the same totals as the CLI in a selectable text dialog.
**Settings** provides database path, exact reader name (empty = automatic), and
ATR entry. OK applies the change and closes the dialog.
Cancel closes it without changing settings or tracking; invalid entries are
rejected. Settings are editable only when not tracking. Accepting a valid change
joins the previous watcher and automatically rearms the selected project; without
a selected project, the tray stays idle. The watcher reuses the CLI's PC/SC,
locking, checkpoint, recovery and SQLite logic, not a separate timer. There is no
fallback to another database if the selected path fails. A missing database is
created when a project is armed. Settings/project selection are in-memory for this run; use launch
options to repeat nondefault database/reader/ATR choices.

**Quit**, Ctrl-C and SIGTERM close the active session and reap any open dialogs.
Switching and quitting may take roughly one polling interval (plus SQLite's busy
wait), but that wait is not counted. Removal and shutdown close at the
last confirmed-presence checkpoint, conservatively dropping the unobserved tail
(normally up to one polling interval). Timing retains whole-second precision. Hard kill or power
loss uses checkpoint recovery when a project is next armed. Keep the database `.lock` file
in place; hard-linked database aliases and deleting a live lock file can defeat
lock identity.

#### Tray acceptance check (temporary data only)

```sh
test_dir=$(mktemp -d)
cargo run -- --db "$test_dir/acceptance.sqlite3" tray
# After quitting, inspect if desired, then remove only this test directory:
rm -r -- "$test_dir"
```

1. Confirm a visible clock icon in DMS. Right-click to open the full menu; open
   **Settings → Database path…** and verify the current absolute path.
   Verify the menu actions and Zenity text entry/cancellation. After recording a
   session, left-click to open the report; repeated clicks must not open duplicate dialogs.
   Check screen-reader labels/focus manually.
2. Use Scan readers; set the exact reader and target ATR. Enter/select a project
   (also try underscores, spaces and Unicode). It must start watching automatically.
3. Select with no card: Waiting. Insert the target: Tracking. Project selection and
   settings must become read-only in the tray menu. Remove:
   card-absent pause; selection becomes editable. Wait, reinsert, remove and check
   Report: absent time must be excluded; reinsertion adds another session. A
   different-ATR card is ignored. There must be no manual timer controls.
4. Open a project-entry dialog while absent, insert the target, then submit the
   dialog: the active project must not change. Remove the card and select another
   project; insertion must start a session on that project without a Resume action.
5. Try blank project/invalid ATR and Cancel settings: no unintended changes.
   Change databases and confirm no writes go to the previous path. Test an invalid
   path and a missing reader: an actionable error, not tracking or fallback.
6. While tray watching is active, start a second CLI watcher **only against this
   temporary database**: it must fail to acquire the lock. Quit the tray with a
   dialog open, verify its dialog closes and reports show zero open sessions.
   Repeat with Ctrl-C/SIGTERM. Ensure no test watcher remains before removing data.

These visible-desktop, accessibility and physical-card checks cannot be replaced
by the automated fake-observation and isolated-DBus tests.

### Pause and resume an unfinished project

Remove the card to pause: no time is counted while it is absent. Leave the watcher
running and reinsert the card to resume automatically. Removal does **not** mark the
project complete.

If you exited the app, select the same project by name:

```sh
cargo run -- resume "My project"
```

This waits for the target card without creating a timed session until it is present.
If the card is already inserted, tracking starts immediately. Each insertion adds a
new session to the same project's total; the pause between sessions is excluded.
Use exactly the same name (case-sensitive) and database path to retain the totals.

`resume` is an explicit alias for `watch`, so it accepts the same `--db`, `--reader`,
and `--atr` options and can also select a new project name. Repeat any custom reader
or ATR options from the earlier run; those options are not restored from history.
No project-completion state or separate `pause` command is needed: removing the card
pauses timing, and Ctrl-C exits without deleting the project's recorded time.

The default database is `timeclock.sqlite3` **in the current directory**. Use an
absolute `--db` path if launching from different directories. Keep the `.lock` file
in place while watching; locks are released by the OS on exit, so a leftover lock
file is normal. Use a local filesystem. SQLite data and its WAL/SHM files are ignored
by Git. For a simple backup, stop the watcher before copying the database, or use
SQLite's backup command while it is running.

Reports show hours/minutes/seconds, session counts, and unclosed-session counts.
`SESSIONS` increases once per target-card insertion for that project, including a
currently open session; removal closes it without resetting the count. `OPEN` is
normally zero after removal and one while tracking.
An open session contributes time only through its last persisted checkpoint;
`OPEN` does not prove a watcher is still alive. Inspect individual sessions with:

```sh
sqlite3 timeclock.sqlite3 \
  "SELECT project, datetime(started_at, 'unixepoch') AS start_utc,
          datetime(ended_at, 'unixepoch') AS end_utc, stop_reason
   FROM sessions ORDER BY id;"
```

## Checks and limits

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
# Real Zenity regression test (graphical session; briefly opens report dialogs):
cargo test report_without_card_or_active_project -- --ignored
# Isolated protocol test (never put its fake watcher on the desktop bus):
TIMECLOCK_ISOLATED_BUS=1 timeout 40s dbus-run-session -- cargo test isolated_protocol -- --ignored
```

Sessions use whole-second wall-clock timestamps. Keep the system clock accurate;
backward adjustments are clamped to prevent negative durations, but large clock
changes can distort elapsed time. After a crash, the uncheckpointed tail is lost
(normally up to about one second). While the watcher is stopped, no time is tracked.
Fast removal/reinsertion between observations splits sessions if the reader/PC/SC
stack reports an updated event counter; the exact unobserved transition time cannot
be reconstructed.

Hardware acceptance check: run `watch`, insert the target, leave it for several
seconds, remove it, and check the report. Repeat insertion to confirm a second
session; try a different-ATR card to confirm it is ignored. Automated tests do not
require a reader; actual card insertion/removal still needs this manual check.
