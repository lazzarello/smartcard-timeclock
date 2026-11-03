# Smartcard time clock

A local Linux time tracker with a CLI and optional system tray. Select a project,
insert a matching smartcard to track time, and remove it to pause. Reinsert it to
resume. Records stay in a local SQLite database; no account or network is needed.

## Download

Open this repository's **Actions → CI**, choose a successful run, and download the
`smartcard-timeclock-linux-x86_64` artifact (GitHub sign-in required). Unzip the
artifact, then extract the binary:

```sh
tar -xzf smartcard-timeclock-linux-x86_64.tar.gz
```

This is a stripped, optimized Linux x86-64 build for Debian 12 / glibc 2.36 or
compatible newer systems, not a static or cross-platform binary. Install the host
libraries and reader support first. For Debian/Ubuntu:

```sh
sudo apt-get install libpcsclite1 libsqlite3-0 pcscd libccid pcsc-tools
sudo systemctl enable --now pcscd.socket
# Optional, for tray dialogs:
sudo apt-get install zenity
```

Other distributions need equivalent packages and a running PC/SC service. On
NixOS, enable `services.pcscd.enable = true;` and build in the development shell
below rather than using the generic Linux binary.

## Build

Download the source with **Code → Download ZIP**, or clone this repository. Install
Rust **1.89 or newer** with Cargo. On Debian/Ubuntu, also install:

```sh
sudo apt-get install build-essential pkg-config libpcsclite-dev libsqlite3-dev
```

From the source directory:

```sh
cargo build --locked --release --bin smartcard-timeclock
./target/release/smartcard-timeclock --help
```

The release binary is optimized, with debug information and symbols stripped.
Nix/devenv users can run `devenv shell` before building; see
[DEVELOPMENT.md](DEVELOPMENT.md) for development setup and checks.

## Run

These examples use the downloaded binary; when building locally, substitute
`./target/release/smartcard-timeclock`.

```sh
./smartcard-timeclock scan
./smartcard-timeclock --db "$HOME/timeclock.sqlite3" watch "My project"
# In another terminal:
./smartcard-timeclock --db "$HOME/timeclock.sqlite3" report
```

Leave `watch` running. Insertion starts timing, removal pauses it, and Ctrl-C
exits. A matching card already inserted at startup starts immediately. Run
`resume "My project"` (an alias for `watch`) to continue after exiting. Project
names are exact and case-sensitive; switching projects means stopping the watcher
and starting it with another name. No time is counted while the app is stopped.

If needed, override the reader and target ATR using values from `scan`:

```sh
./smartcard-timeclock --db "$HOME/timeclock.sqlite3" watch "Client A" \
  --reader "Alcor Micro AU9540 00 00" \
  --atr "3B 68 00 00 99 02 00 D1 03 04 22 01"
```

A sole reader is selected automatically; multiple readers require an exact
`--reader` name. `--atr` accepts spaced, compact, or colon-separated hex. Use
`--help` or `watch --help` for options.

### Optional system tray

In a graphical session with Zenity, a session DBus, and a StatusNotifierItem /
DBusMenu tray host (such as DMS / DankMaterialShell):

```sh
./smartcard-timeclock --db "$HOME/timeclock.sqlite3" tray
```

Right-click for **Projects → New project…** or an existing project, then use the
card to start/pause timing. Left-click opens the report. The title/tooltip shows
tracking or paused time; green means tracking, orange waiting/paused, grey idle,
and red error. Remove the card before changing projects or settings.

**Settings** changes the database, reader, or ATR. Choices last only for this run;
repeat launch options to reuse them. The tray also accepts `--reader` and `--atr`.
After fixing a reader/database error, reselect the project to retry. Keep the
launching terminal open, or launch the binary from your desktop with an absolute
`--db` path. No desktop configuration or autostart entry is installed.

## Card hardware and identity

Use a PC/SC-compatible smartcard reader and a card whose presence and ATR the
reader exposes. A USB CCID contact reader is the simplest starting point. Check
that `pcsc_scan` sees insertions/removals before running the app; some readers need
vendor drivers. Reader USB ID `058f:9540` identifies a reader, **not a card**.
Compatibility with arbitrary NFC/RFID readers or cards is not guaranteed.

The app matches the **ATR (Answer To Reset)**, which describes communication
parameters and often a card type. **It is not a unique serial number:** two cards
with the same ATR both trigger timing. The built-in default is:

```text
3B 68 00 00 99 02 00 D1 03 04 22 01
```

Use `scan` and `--atr` for your card. This is a convenience trigger, **not
authentication**. The app only observes PC/SC presence and ATR data: it sends no
application commands and reads no bank-account data. Unique physical-card
identification would require card-specific commands; there is no universal ID
command for arbitrary contact smartcards.

## Data and limits

- The default database is `timeclock.sqlite3` in the **current directory**. Use an
  absolute `--db` path consistently, especially with desktop launchers.
- Only one watcher may use a database; reports remain available. Use a local
  filesystem and leave its `.lock` file in place (a leftover file is normal).
- Reports show cumulative time, insertion/session counts, and open sessions.
  Open time ends at the last checkpoint; `OPEN` does not prove a watcher is alive.
- Timing uses whole-second wall-clock timestamps and roughly one-second
  checkpoints. Removal/shutdown stop at the last confirmed checkpoint. Crash
  recovery excludes downtime but loses the uncheckpointed tail. Keep the system
  clock accurate; large clock changes can distort totals.
- Reader/PC/SC failure stops tracking; reconnect and restart/reselect the project.
- Stop the watcher before copying the database for backup, or use SQLite's online
  backup command. Do not discard live WAL/SHM files. Local databases are ignored
  by Git, but contain project names and timing history: keep them private.

Developer checks and hardware/desktop acceptance steps are in
[DEVELOPMENT.md](DEVELOPMENT.md); coding-agent rules are in [AGENTS.md](AGENTS.md).
