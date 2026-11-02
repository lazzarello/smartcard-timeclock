# Smartcard time clock

A local Rust CLI: insert the target card to track time on a named project; remove it
to pause. Reinsert it to resume the same project. Sessions are stored in SQLite. No network, GUI, or privileged access to the
card is needed.

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
4. Checkpoint once per second while present. Ctrl-C/SIGTERM closes the session.
   Reader/PC/SC failure closes at the last checkpoint and exits with an error; reconnect
   the reader and restart the watcher. On the next watch run after a crash or power
   loss, close any unfinished session at its last checkpoint, not at restart time.
5. Provide project totals and tests for matching, event persistence, recovery,
   validation, and locking. No daemon, project registry, or automatic billing.

## Run

The host must have a working `pcscd` service and reader driver (as it already does
if `pcsc_scan` works). On NixOS, enable `services.pcscd.enable = true;` in the host
configuration if needed. Do not start a second pcscd inside the development shell.

`devenv.nix` supplies Rust, pkg-config, pcsclite, pcsc-tools, and SQLite:

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
