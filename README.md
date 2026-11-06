# Smartcard time clock

A local Linux time tracker with a CLI and optional system tray. Select a project,
insert a matching smartcard to track time, and remove it to pause. Reinsert it to
resume. Records stay in a local SQLite database; no account or network is needed.

## Deployment targets

- **Binary:** Linux x86-64, Debian 12 (glibc 2.36) or compatible newer systems,
  with PC/SC, SQLite, and GTK4 runtime libraries. Not a static binary.
- **Tray:** [`ksni`](https://github.com/iovxw/ksni) uses StatusNotifierItem (SNI)
  and DBusMenu, requiring a session DBus and a compatible host, such as KDE Plasma
  or DMS / DankMaterialShell. GNOME needs an AppIndicator extension. X11 and
  Wayland work with a compatible host; legacy XEmbed-only trays do not. Dialogs
  require Zenity. Windows and macOS are not supported.
- **CLI:** no desktop or tray host required; the same runtime libraries are needed.

## Install

Download `smartcard-timeclock-linux-x86_64.tar.gz` from
[GitHub Releases](https://github.com/lazzarello/smartcard-timeclock/releases/latest).
On Debian/Ubuntu, install dependencies and the extracted binary:

```sh
sudo apt-get install libpcsclite1 libsqlite3-0 libgtk-4-1 pcscd libccid pcsc-tools zenity
sudo systemctl enable --now pcscd.socket
tar -xzf smartcard-timeclock-linux-x86_64.tar.gz
sudo install -m 755 smartcard-timeclock /usr/local/bin/smartcard-timeclock
```

Other distributions need equivalent packages and a running PC/SC service. For
source builds and NixOS, see [DEVELOPMENT.md](DEVELOPMENT.md#setup).

## Run

```sh
smartcard-timeclock scan
smartcard-timeclock --db "$HOME/timeclock.sqlite3" watch "My project"
# In another terminal:
smartcard-timeclock --db "$HOME/timeclock.sqlite3" report
```

Leave `watch` running. Insertion starts timing, removal pauses it, and Ctrl-C
exits. A matching card already inserted at startup starts immediately. Run
`resume "My project"` (an alias for `watch`) to continue after exiting. Project
names are exact and case-sensitive; switching projects means stopping the watcher
and starting it with another name. No time is counted while the app is stopped.
`Started`, `Paused`, and `Reader unavailable` messages go to stdout with UTC
observation timestamps, for example `[2026-11-06T17:43:15Z] Started "My project".`
Pause/outage timestamps mark detection, not the last billed checkpoint.

If needed, override the reader and target ATR using values from `scan`:

```sh
smartcard-timeclock --db "$HOME/timeclock.sqlite3" watch "Client A" \
  --reader "Alcor Micro AU9540 00 00" \
  --atr "3B 68 00 00 99 02 00 D1 03 04 22 01"
```

A sole reader is selected automatically; multiple readers require an exact
`--reader` name. `--atr` accepts spaced, compact, or colon-separated hex. Use
`--help` or `watch --help` for options.

### Optional system tray

In a graphical session meeting the tray requirements above:

```sh
smartcard-timeclock --db "$HOME/timeclock.sqlite3" tray
```

Right-click for **Projects → New project…** or an existing project, then use the
card to start/pause timing. Left-click opens a compact, read-only live status
window, for example `Tracking My Project (01:58:00)`. Removing the card changes it
to `Paused My Project (01:58:01)`; the cumulative time stays fixed until reinsertion.
The floating window follows the desktop's light/dark preference, including changes
while open, via the desktop settings portal. If that preference is unavailable,
it keeps the configured GTK theme. Its compact title bar matches Alacritty's
35-logical-pixel Wayland title bar, growing if larger fonts need more space.
Repeated clicks reuse the window; closing it does not stop tracking. Reports remain
available via **Report…** in the right-click menu. The title/tooltip shows the same
project, status, and checkpointed time; green means tracking, orange waiting/paused,
grey idle, and red error. Remove the card before changing projects or settings.

**Settings** changes the database, reader, or ATR. Choices last only for this run;
repeat launch options to reuse them. The tray also accepts `--reader` and `--atr`.
Temporary reader/PC/SC interruptions recover automatically; terminal errors (including
database errors) require fixing the error and reselecting the project. Keep the
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
- Temporary reader/PC/SC loss shows **Reader unavailable — reconnecting** and
  automatically retries the same exact reader, retaining the project and ATR.
  No other reader is substituted. Fresh usable matching presence resumes Tracking;
  absence, a different ATR, or an unusable card shows Paused after recovery.
- Outages end at the last checkpoint. Linux suspend-inclusive observation gaps
  over three seconds also split sessions and force fresh presence confirmation,
  even if PC/SC succeeds. Suspend/outage time is not backfilled. This conservative
  guard may split sessions during scheduling or database stalls; it does not
  detect every shorter suspend. Unexpected PC/SC or storage errors remain terminal.
- Stop the watcher before copying the database for backup, or use SQLite's online
  backup command. Do not discard live WAL/SHM files. Local databases are ignored
  by Git, but contain project names and timing history: keep them private.

Developer checks and hardware/desktop acceptance steps are in
[DEVELOPMENT.md](DEVELOPMENT.md); coding-agent rules are in [AGENTS.md](AGENTS.md).
