mod store;
mod tray;

use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, ensure};
use clap::{Parser, Subcommand};
use pcsc::{Context, ReaderState, Scope, State};

// shortcut: ATRs identify card types; use card-specific identity if physical-card uniqueness is needed.
const TARGET_ATR: &str = "3B 68 00 00 99 02 00 D1 03 04 22 01";
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_OBSERVATION_GAP: Duration = Duration::from_secs(3);

#[derive(Parser)]
#[command(
    version,
    about = "Track project time while a matching smartcard is inserted"
)]
struct Cli {
    /// SQLite database path (relative to the current directory by default)
    #[arg(long, global = true, default_value = "timeclock.sqlite3")]
    db: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show PC/SC reader names and currently inserted cards' ATRs
    Scan,
    /// Start or resume a project; card removal pauses until reinsertion
    #[command(visible_alias = "resume")]
    Watch {
        /// Project name to record for this run
        project: String,
        /// Exact PC/SC reader name; required if multiple readers are attached
        #[arg(long)]
        reader: Option<String>,
        /// ATR to match (card type, NOT a unique physical-card ID)
        #[arg(long, default_value = TARGET_ATR)]
        atr: String,
    },
    /// Show totals per project, including open sessions through their last checkpoint
    Report,
    /// Show a hardware-controlled system tray (select a project, then insert/remove the card)
    Tray {
        /// Exact PC/SC reader name; empty in the tray settings restores automatic selection
        #[arg(long)]
        reader: Option<String>,
        /// ATR to match (card type, NOT a unique physical-card ID)
        #[arg(long, default_value = TARGET_ATR)]
        atr: String,
    },
}

fn parse_atr(input: &str) -> Result<Vec<u8>, String> {
    let hex: String = input
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != ':')
        .collect();
    if !(4..=66).contains(&hex.len())
        || !hex.len().is_multiple_of(2)
        || !hex.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err(
            "ATR must be 2–33 bytes of hexadecimal, optionally separated by spaces or colons"
                .into(),
        );
    }
    let bytes: Vec<_> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    if !matches!(bytes[0], 0x3B | 0x3F) {
        return Err("ATR must start with 3B (direct) or 3F (inverse convention)".into());
    }
    Ok(bytes)
}

fn format_atr(atr: &[u8]) -> String {
    atr.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn context() -> Result<Context> {
    Context::establish(Scope::User)
        .context("Cannot reach PC/SC; ensure the host pcscd service is running")
}

fn readers(ctx: &Context) -> Result<Vec<CString>> {
    let readers = ctx
        .list_readers_owned()
        .context("Cannot list PC/SC readers")?;
    ensure!(
        !readers.is_empty(),
        "No PC/SC readers found; connect the reader and check pcscd"
    );
    Ok(readers)
}

fn select_reader(mut readers: Vec<CString>, requested: Option<&str>) -> Result<CString> {
    if let Some(name) = requested {
        return readers
            .into_iter()
            .find(|r| r.to_bytes() == name.as_bytes())
            .with_context(|| format!("Reader {name:?} not found; use `scan` to list readers"));
    }
    ensure!(
        readers.len() == 1,
        "Multiple readers found; use `scan`, then pass --reader with an exact name"
    );
    Ok(readers.remove(0))
}

fn scan_text() -> Result<String> {
    use std::fmt::Write;
    let mut output = String::new();
    let ctx = context()?;
    let mut states: Vec<_> = readers(&ctx)?
        .into_iter()
        .map(|name| ReaderState::new(name, State::UNAWARE))
        .collect();
    ctx.get_status_change(POLL_INTERVAL, &mut states)?;
    for state in states {
        writeln!(output, "Reader: {}", state.name().to_string_lossy())?;
        if state.event_state().contains(State::PRESENT) {
            writeln!(output, "  ATR: {}", format_atr(state.atr()))?;
        } else {
            writeln!(output, "  No card present ({:?})", state.event_state())?;
        }
    }
    Ok(output)
}

fn matches_target(state: State, atr: &[u8], target: &[u8]) -> bool {
    state.contains(State::PRESENT)
        && !state.intersects(
            State::EMPTY
                | State::UNKNOWN
                | State::UNAVAILABLE
                | State::MUTE
                | State::UNPOWERED
                | State::IGNORE,
        )
        && atr == target
}

fn now() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .try_into()?)
}

fn log_timestamp(timestamp: i64) -> Result<gtk4::glib::GString> {
    Ok(gtk4::glib::DateTime::from_unix_utc(timestamp)?.format_iso8601()?)
}

fn watcher_lock(path: &Path) -> Result<File> {
    let mut name = path.canonicalize()?.into_os_string();
    name.push(".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(name)?;
    file.try_lock()
        .context("Cannot lock database; another watcher may already be using it")?;
    Ok(file)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatchState {
    Waiting,
    Tracking,
    CardAbsent,
    Reconnecting,
}

fn watch(
    path: &Path,
    project: &str,
    requested: Option<&str>,
    atr: &[u8],
    running: &AtomicBool,
    mut status: impl FnMut(WatchState),
) -> Result<()> {
    ensure!(!project.trim().is_empty(), "Project name cannot be blank");
    let mut store = store::Store::open(path).context("Cannot initialize SQLite database")?;
    let _lock = watcher_lock(path)?;
    let recovered = store.recover()?;
    if recovered > 0 {
        eprintln!(
            "Recovered {recovered} interrupted session at its last checkpoint (no downtime counted)."
        );
    }

    let result = monitor(&mut store, project, requested, atr, running, &mut status);
    // Never count shutdown/SQLite wait time after the last confirmed presence.
    if store.stop(
        if result.is_ok() {
            "shutdown"
        } else {
            "reader_error"
        },
        None,
    )? {
        println!("Stopped {project:?}.");
    }
    result
}

fn monitor(
    store: &mut store::Store,
    project: &str,
    requested: Option<&str>,
    target: &[u8],
    running: &AtomicBool,
    status: &mut impl FnMut(WatchState),
) -> Result<()> {
    let ctx = context()?;
    let name = select_reader(readers(&ctx)?, requested)?;
    let reader = name.to_string_lossy().into_owned();
    let atr = format_atr(target);
    println!("Watching {reader:?} for ATR {atr}; project {project:?}.");
    println!("Waiting for target card insertion to start or resume {project:?}.");
    eprintln!("ATR matching identifies a card type, not a unique card. Press Ctrl-C to stop.");
    let mut input = PcscInput {
        ctx: Some(ctx),
        states: [ReaderState::new(name.clone(), State::UNAWARE)],
        name,
        fresh: true,
    };
    monitor_input(
        store,
        project,
        &reader,
        target,
        running,
        status,
        &mut input,
        &mut std::io::stdout(),
    )
}

fn recoverable(error: pcsc::Error) -> bool {
    use pcsc::Error::*;
    matches!(
        error,
        UnknownReader
            | ReaderUnavailable
            | NoReadersAvailable
            | NoService
            | ServiceStopped
            | InvalidHandle
            | Shutdown
            | SystemCancelled
            | CommError
            | CommDataLost
            | NotReady
            | ServerTooBusy
    )
}

// /proc/uptime includes Linux suspend time, unlike Instant's monotonic clock.
fn boot_elapsed() -> Result<Duration> {
    let text = std::fs::read_to_string("/proc/uptime")?;
    let seconds: f64 = text
        .split_whitespace()
        .next()
        .context("Missing boot time")?
        .parse()?;
    Ok(Duration::try_from_secs_f64(seconds)?)
}

fn retry_wait(running: &AtomicBool) {
    for _ in 0..20 {
        if !running.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Observation {
    flags: State,
    atr: Vec<u8>,
    count: u32,
}

// The same loop accepts fake PC/SC observations and clocks without hardware or real data.
trait WatchInput {
    fn poll(&mut self, reconnect: bool) -> Result<Option<Observation>, pcsc::Error>;
    fn elapsed(&mut self) -> Result<Duration> {
        boot_elapsed()
    }
    fn timestamp(&mut self) -> Result<i64> {
        now()
    }
    fn wait(&mut self, running: &AtomicBool) {
        retry_wait(running);
    }
}

struct PcscInput {
    ctx: Option<Context>,
    name: CString,
    states: [ReaderState; 1],
    fresh: bool,
}

fn pinned_reader_present(names: &[CString], pinned: &std::ffi::CStr) -> Result<(), pcsc::Error> {
    if names.iter().any(|name| name.as_c_str() == pinned) {
        Ok(())
    } else {
        Err(pcsc::Error::UnknownReader)
    }
}

impl WatchInput for PcscInput {
    fn poll(&mut self, reconnect: bool) -> Result<Option<Observation>, pcsc::Error> {
        if reconnect {
            // Cloning a Context shares its handle; recovery needs a new establishment.
            self.ctx = None;
            self.fresh = true;
            self.states = [ReaderState::new(self.name.clone(), State::UNAWARE)];
            let ctx = Context::establish(Scope::User)?;
            pinned_reader_present(&ctx.list_readers_owned()?, &self.name)?;
            self.ctx = Some(ctx);
        }
        match self
            .ctx
            .as_ref()
            .unwrap()
            .get_status_change(POLL_INTERVAL, &mut self.states)
        {
            Ok(()) => self.fresh = false,
            Err(pcsc::Error::Timeout) if self.fresh => return Ok(None),
            Err(pcsc::Error::Timeout) => {}
            Err(error) => return Err(error),
        }
        let state = &mut self.states[0];
        let observation = Observation {
            flags: state.event_state(),
            atr: state.atr().to_vec(),
            count: state.event_count(),
        };
        state.sync_current_state();
        Ok(Some(observation))
    }
}

#[allow(clippy::too_many_arguments)]
fn monitor_input(
    store: &mut store::Store,
    project: &str,
    reader: &str,
    target: &[u8],
    running: &AtomicBool,
    status: &mut impl FnMut(WatchState),
    input: &mut impl WatchInput,
    output: &mut impl std::io::Write,
) -> Result<()> {
    let atr = format_atr(target);
    let mut previous = None;
    let mut seen_target = false;
    let mut reconnect = false;
    let mut last_observation = None;
    status(WatchState::Waiting);
    while running.load(Ordering::SeqCst) {
        let before = input.elapsed()?;
        let gap_before = last_observation.is_some_and(|last| observation_gap(last, before));
        let result = if gap_before {
            Ok(None)
        } else {
            input.poll(reconnect)
        };
        if !running.load(Ordering::SeqCst) {
            break;
        }
        // Capture wall time before the gap check, so suspend here cannot advance an old session.
        let timestamp = input.timestamp()?;
        let observed = input.elapsed()?;
        let gap = gap_before || observation_gap(last_observation.unwrap_or(before), observed);
        let interruption = match result {
            Err(error) if recoverable(error) => Some("reader_unavailable"),
            Err(error) => return Err(error).context("PC/SC monitoring failed"),
            _ if gap => Some("observation_gap"),
            Ok(Some(ref observation))
                if observation
                    .flags
                    .intersects(State::UNKNOWN | State::UNAVAILABLE | State::IGNORE) =>
            {
                Some("reader_unavailable")
            }
            _ => None,
        };
        if let Some(reason) = interruption {
            status(WatchState::Reconnecting);
            if !reconnect {
                writeln!(
                    output,
                    "[{}] Reader unavailable — reconnecting to {reader:?} ({reason}).",
                    log_timestamp(timestamp)?
                )?;
            }
            store.stop(reason, None)?;
            previous = None;
            last_observation = None;
            seen_target = true; // Fresh absence after recovery is Paused, even if initially empty.
            reconnect = true;
            input.wait(running);
            continue;
        }
        let Some(observation) = result.unwrap() else {
            if reconnect {
                input.wait(running);
            }
            continue;
        };
        let matching = matches_target(observation.flags, &observation.atr, target);
        let count = observation.count;
        if matching
            && previous.is_some_and(|(was_matching, old_count)| was_matching && old_count != count)
        {
            store.stop("card_changed", None)?;
        }
        if !running.load(Ordering::SeqCst) {
            break;
        }
        if matching {
            if store.record(project, &atr, reader, timestamp)? {
                writeln!(
                    output,
                    "[{}] Started {project:?}.",
                    log_timestamp(timestamp)?
                )?;
            }
        } else {
            let stopped = store.stop("removed_or_changed", None)?;
            if stopped || reconnect {
                writeln!(
                    output,
                    "[{}] Paused {project:?}; waiting for target card insertion to resume.",
                    log_timestamp(timestamp)?
                )?;
            }
        }
        // Keep the pre-write baseline: a blocked SQLite write is also an observation gap.
        last_observation = Some(observed);
        reconnect = false;
        seen_target |= matching;
        status(if matching {
            WatchState::Tracking
        } else if seen_target {
            WatchState::CardAbsent
        } else {
            WatchState::Waiting
        });
        previous = Some((matching, count));
    }
    Ok(())
}

fn observation_gap(previous: Duration, current: Duration) -> bool {
    current
        .checked_sub(previous)
        .is_none_or(|gap| gap > MAX_OBSERVATION_GAP)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan => {
            print!("{}", scan_text()?);
            Ok(())
        }
        Command::Watch {
            project,
            reader,
            atr,
        } => {
            let atr = parse_atr(&atr).map_err(anyhow::Error::msg)?;
            let running = Arc::new(AtomicBool::new(true));
            let flag = Arc::clone(&running);
            ctrlc::set_handler(move || flag.store(false, Ordering::SeqCst))?;
            watch(&cli.db, &project, reader.as_deref(), &atr, &running, |_| {})
        }
        Command::Tray { reader, atr } => {
            let atr = parse_atr(&atr).map_err(anyhow::Error::msg)?;
            tray::run(cli.db, reader, atr)
        }
        Command::Report => {
            store::report(&cli.db).context("Cannot read report; run watch first or check --db")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeInput<'a> {
        running: &'a AtomicBool,
        steps: std::collections::VecDeque<(Result<Option<Observation>, pcsc::Error>, u64, i64)>,
        boot: u64,
        wall: i64,
        resets: Vec<bool>,
        waits: usize,
        cancel_on_wait: bool,
        write_delay: u64,
        elapsed_reads: usize,
        lock_path: Option<PathBuf>,
    }

    impl<'a> FakeInput<'a> {
        fn new(running: &'a AtomicBool) -> Self {
            Self {
                running,
                steps: Default::default(),
                boot: 0,
                wall: 0,
                resets: vec![],
                waits: 0,
                cancel_on_wait: false,
                write_delay: 0,
                elapsed_reads: 0,
                lock_path: None,
            }
        }
        fn card(&mut self, flags: State, atr: &[u8], boot_delta: u64, wall: i64) {
            self.steps.push_back((
                Ok(Some(Observation {
                    flags,
                    atr: atr.to_vec(),
                    count: 0,
                })),
                boot_delta,
                wall,
            ));
        }
        fn error(&mut self, error: pcsc::Error) {
            self.steps.push_back((Err(error), 1, self.wall));
        }
    }

    impl WatchInput for FakeInput<'_> {
        fn poll(&mut self, reconnect: bool) -> Result<Option<Observation>, pcsc::Error> {
            if let Some(path) = &self.lock_path {
                assert!(watcher_lock(path).is_err());
            }
            let Some((result, delta, wall)) = self.steps.pop_front() else {
                self.running.store(false, Ordering::SeqCst);
                return Ok(None);
            };
            self.resets.push(reconnect);
            self.boot += delta;
            self.wall = wall;
            result
        }
        fn elapsed(&mut self) -> Result<Duration> {
            self.elapsed_reads += 1;
            let elapsed = Duration::from_secs(self.boot);
            // Simulate a write stall after capturing the first observation's baseline.
            if self.elapsed_reads == 2 {
                self.boot += std::mem::take(&mut self.write_delay);
            }
            Ok(elapsed)
        }
        fn timestamp(&mut self) -> Result<i64> {
            Ok(self.wall)
        }
        fn wait(&mut self, running: &AtomicBool) {
            self.waits += 1;
            if self.cancel_on_wait {
                running.store(false, Ordering::SeqCst);
            }
        }
    }

    fn run_fake(store: &mut store::Store, input: &mut FakeInput<'_>) -> Result<Vec<WatchState>> {
        let mut states = vec![];
        monitor_input(
            store,
            "work",
            "pinned",
            &[0x3b, 0],
            input.running,
            &mut |state| states.push(state),
            input,
            &mut std::io::sink(),
        )?;
        Ok(states)
    }

    #[test]
    fn recovery_repeated_context_loss_requires_fresh_observations_and_keeps_lock() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("clock.db");
        let mut store = store::Store::open(&db).unwrap();
        let lock = watcher_lock(&db).unwrap();
        let running = AtomicBool::new(true);
        let mut input = FakeInput::new(&running);
        input.lock_path = Some(db.clone());
        input.card(State::PRESENT, &[0x3b, 0], 1, 10);
        input.card(State::PRESENT, &[0x3b, 0], 1, 12);
        input.card(State::UNAVAILABLE, &[], 1, 13);
        input.error(pcsc::Error::ServiceStopped);
        input.error(pcsc::Error::InvalidHandle);
        // A timeout from a fresh UNAWARE reader is not confirmation of presence.
        input.steps.push_back((Ok(None), 1, 50));
        input.card(State::PRESENT, &[0x3b, 0], 1, 100);
        input.card(State::PRESENT, &[0x3b, 0], 1, 102);
        input.error(pcsc::Error::UnknownReader);
        input.card(State::PRESENT, &[0x3b, 0], 1, 200);
        let states = run_fake(&mut store, &mut input).unwrap();
        assert_eq!(
            states
                .iter()
                .filter(|s| **s == WatchState::Reconnecting)
                .count(),
            4
        );
        assert_eq!(
            input.resets,
            [
                false, false, false, true, true, true, true, false, false, true
            ]
        );
        store.stop("shutdown", None).unwrap();
        let rows = store::report_rows(&db).unwrap();
        assert_eq!((rows[0].seconds, rows[0].sessions, rows[0].open), (4, 3, 0));
        assert!(watcher_lock(&db).is_err());
        drop(lock);
        assert!(watcher_lock(&db).is_ok());
    }

    #[test]
    fn recovery_absent_changed_and_unusable_cards_never_accrue() {
        for flags in [
            State::EMPTY,
            State::PRESENT,
            State::PRESENT | State::MUTE,
            State::PRESENT | State::UNPOWERED,
        ] {
            for initially_matching in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let db = dir.path().join("clock.db");
                let mut store = store::Store::open(&db).unwrap();
                let running = AtomicBool::new(true);
                let mut input = FakeInput::new(&running);
                input.card(
                    if initially_matching {
                        State::PRESENT
                    } else {
                        State::EMPTY
                    },
                    &[0x3b, 0],
                    1,
                    10,
                );
                input.card(State::UNKNOWN, &[], 1, 11);
                input.card(
                    flags,
                    if flags == State::PRESENT {
                        &[0x3b, 1]
                    } else {
                        &[0x3b, 0]
                    },
                    1,
                    100,
                );
                let mut states = vec![];
                let mut output = Vec::new();
                // A second absent observation must not repeat the recovery announcement.
                input.card(State::EMPTY, &[], 1, 101);
                monitor_input(
                    &mut store,
                    "work",
                    "pinned",
                    &[0x3b, 0],
                    &running,
                    &mut |state| states.push(state),
                    &mut input,
                    &mut output,
                )
                .unwrap();
                assert_eq!(*states.last().unwrap(), WatchState::CardAbsent);
                let output = String::from_utf8(output).unwrap();
                assert_eq!(
                    output
                        .matches("Paused \"work\"; waiting for target card insertion to resume.")
                        .count(),
                    1,
                    "{output}"
                );
                let rows = store::report_rows(&db).unwrap();
                assert!(rows.iter().all(|row| row.seconds == 0 && row.open == 0));
            }
        }
    }

    #[test]
    fn transition_logs_include_utc_observation_timestamps_on_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("clock.db");
        let mut store = store::Store::open(&db).unwrap();
        let running = AtomicBool::new(true);
        let mut input = FakeInput::new(&running);
        input.card(State::PRESENT, &[0x3b, 0], 1, 10);
        input.card(State::PRESENT, &[0x3b, 0], 1, 11);
        input.card(State::EMPTY, &[], 1, 12);
        input.card(State::UNAVAILABLE, &[], 1, 13);
        input.card(State::EMPTY, &[], 1, 14);
        input.card(State::PRESENT, &[0x3b, 0], 1, 15);
        let mut output = Vec::new();
        monitor_input(
            &mut store,
            "work",
            "pinned",
            &[0x3b, 0],
            &running,
            &mut |_| {},
            &mut input,
            &mut output,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            concat!(
                "[1970-01-01T00:00:10Z] Started \"work\".\n",
                "[1970-01-01T00:00:12Z] Paused \"work\"; waiting for target card insertion to resume.\n",
                "[1970-01-01T00:00:13Z] Reader unavailable — reconnecting to \"pinned\" (reader_unavailable).\n",
                "[1970-01-01T00:00:14Z] Paused \"work\"; waiting for target card insertion to resume.\n",
                "[1970-01-01T00:00:15Z] Started \"work\".\n",
            )
        );
    }

    #[test]
    fn successful_poll_after_suspend_or_write_gap_cannot_bridge_sessions() {
        for gap_kind in ["successful_poll", "timeout", "write"] {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("clock.db");
            let mut store = store::Store::open(&db).unwrap();
            let running = AtomicBool::new(true);
            let mut input = FakeInput::new(&running);
            input.card(State::PRESENT, &[0x3b, 0], 1, 10);
            if gap_kind == "write" {
                input.write_delay = 30;
            } else if gap_kind == "timeout" {
                input.steps.push_back((Ok(None), 30, 40));
            } else {
                input.card(State::PRESENT, &[0x3b, 0], 30, 40);
            }
            input.card(State::PRESENT, &[0x3b, 0], 1, 100);
            input.card(State::PRESENT, &[0x3b, 0], 1, 102);
            let states = run_fake(&mut store, &mut input).unwrap();
            assert!(states.contains(&WatchState::Reconnecting));
            store.stop("shutdown", None).unwrap();
            let rows = store::report_rows(&db).unwrap();
            assert_eq!((rows[0].seconds, rows[0].sessions), (2, 2));
            let conn = rusqlite::Connection::open(&db).unwrap();
            let end: i64 = conn
                .query_row(
                    "SELECT ended_at FROM sessions ORDER BY id LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(end, 10);
        }
        assert!(!observation_gap(Duration::ZERO, MAX_OBSERVATION_GAP));
        assert!(observation_gap(
            Duration::from_secs(5),
            Duration::from_secs(4)
        ));
        assert!(boot_elapsed().unwrap() > Duration::ZERO);
    }

    #[test]
    fn recovery_only_accepts_the_pinned_exact_reader() {
        let pinned = CString::new("exact reader 00").unwrap();
        for names in [
            vec![],
            vec![CString::new("exact reader 01").unwrap()],
            vec![CString::new("other").unwrap()],
        ] {
            assert_eq!(
                pinned_reader_present(&names, &pinned),
                Err(pcsc::Error::UnknownReader)
            );
        }
        assert!(
            pinned_reader_present(&[CString::new("other").unwrap(), pinned.clone()], &pinned)
                .is_ok()
        );
        for error in [
            pcsc::Error::UnknownReader,
            pcsc::Error::ReaderUnavailable,
            pcsc::Error::NoReadersAvailable,
            pcsc::Error::NoService,
            pcsc::Error::ServiceStopped,
            pcsc::Error::InvalidHandle,
            pcsc::Error::Shutdown,
            pcsc::Error::SystemCancelled,
            pcsc::Error::CommError,
            pcsc::Error::CommDataLost,
            pcsc::Error::NotReady,
            pcsc::Error::ServerTooBusy,
        ] {
            assert!(recoverable(error));
        }
        assert!(!recoverable(pcsc::Error::InvalidParameter));
        assert!(!recoverable(pcsc::Error::Timeout));
    }

    #[test]
    fn cancellation_joins_a_retrying_watcher_and_releases_its_lock() {
        struct Unavailable(std::sync::mpsc::Sender<()>);
        impl WatchInput for Unavailable {
            fn poll(&mut self, _: bool) -> Result<Option<Observation>, pcsc::Error> {
                Err(pcsc::Error::NoService)
            }
            fn wait(&mut self, running: &AtomicBool) {
                self.0.send(()).unwrap();
                retry_wait(running);
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("clock.db");
        let path = db.clone();
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let (ready, wait) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut store = store::Store::open(&path).unwrap();
            let _lock = watcher_lock(&path).unwrap();
            store.record("work", "3B00", "pinned", 10).unwrap();
            store.record("work", "3B00", "pinned", 12).unwrap();
            monitor_input(
                &mut store,
                "work",
                "pinned",
                &[0x3b, 0],
                &flag,
                &mut |_| {},
                &mut Unavailable(ready),
                &mut std::io::sink(),
            )
            .unwrap();
            store.stop("shutdown", None).unwrap();
        });
        let ready = wait.recv_timeout(Duration::from_secs(2));
        let locked = watcher_lock(&db).is_err();
        let start = std::time::Instant::now();
        running.store(false, Ordering::SeqCst);
        thread.join().unwrap();
        assert!(ready.is_ok());
        assert!(locked);
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(watcher_lock(&db).is_ok());
        let rows = store::report_rows(&db).unwrap();
        assert_eq!((rows[0].seconds, rows[0].open), (2, 0));
    }

    #[test]
    fn retry_cancellation_and_fatal_errors_do_not_loop() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("clock.db");
        let mut store = store::Store::open(&db).unwrap();
        let running = AtomicBool::new(true);
        let mut input = FakeInput::new(&running);
        input.error(pcsc::Error::NoService);
        input.cancel_on_wait = true;
        assert_eq!(
            run_fake(&mut store, &mut input).unwrap().last(),
            Some(&WatchState::Reconnecting)
        );
        assert_eq!(input.waits, 1);
        let start = std::time::Instant::now();
        retry_wait(&running);
        assert!(start.elapsed() < Duration::from_millis(100));

        running.store(true, Ordering::SeqCst);
        let mut input = FakeInput::new(&running);
        input.error(pcsc::Error::InvalidParameter);
        assert!(run_fake(&mut store, &mut input).is_err());
        assert_eq!(input.waits, 0);

        // Real SQLite failure must propagate, not become a reader retry.
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TRIGGER fail_write BEFORE INSERT ON sessions BEGIN SELECT RAISE(FAIL, 'test storage failure'); END;").unwrap();
        let mut input = FakeInput::new(&running);
        input.card(State::PRESENT, &[0x3b, 0], 1, 10);
        assert!(
            run_fake(&mut store, &mut input)
                .unwrap_err()
                .to_string()
                .contains("test storage failure")
        );
        assert_eq!(input.waits, 0);

        conn.execute_batch(
            "DROP TRIGGER fail_write;
            CREATE TRIGGER fail_stop BEFORE UPDATE ON sessions WHEN NEW.ended_at IS NOT NULL
            BEGIN SELECT RAISE(FAIL, 'test closure failure'); END;",
        )
        .unwrap();
        let mut input = FakeInput::new(&running);
        input.card(State::PRESENT, &[0x3b, 0], 1, 10);
        input.error(pcsc::Error::NoService);
        assert!(
            run_fake(&mut store, &mut input)
                .unwrap_err()
                .to_string()
                .contains("test closure failure")
        );
        assert_eq!(input.waits, 0);
        // Failed closure leaves the persisted checkpoint available for crash recovery.
        conn.execute_batch("DROP TRIGGER fail_stop;").unwrap();
        store.recover().unwrap();
        let rows = store::report_rows(&db).unwrap();
        assert_eq!((rows[0].seconds, rows[0].open), (0, 0));
    }

    #[test]
    fn atr_formats_and_validation() {
        let expected = parse_atr(TARGET_ATR).unwrap();
        assert_eq!(expected, parse_atr("3b680000990200d103042201").unwrap());
        assert_eq!(
            expected,
            parse_atr("3b:68:00:00:99:02:00:d1:03:04:22:01").unwrap()
        );
        assert_eq!(format_atr(&expected), TARGET_ATR);
        for invalid in [
            "",
            "3",
            "3B0",
            "GG00",
            "0000",
            "3B🙂",
            "ATR: 3B00",
            &"3B".repeat(34),
        ] {
            assert!(parse_atr(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn only_present_usable_matching_card_counts() {
        let target = parse_atr(TARGET_ATR).unwrap();
        assert!(matches_target(
            State::PRESENT | State::CHANGED | State::INUSE,
            &target,
            &target
        ));
        assert!(!matches_target(State::PRESENT, &[0x3B, 0x00], &target));
        for flags in [
            State::EMPTY,
            State::UNKNOWN,
            State::UNAVAILABLE,
            State::MUTE,
            State::UNPOWERED,
            State::IGNORE,
        ] {
            assert!(!matches_target(flags, &target, &target));
            assert!(!matches_target(flags | State::PRESENT, &target, &target));
        }
    }

    #[test]
    fn reader_selection_requires_unambiguous_exact_match() {
        let names = || {
            vec![
                CString::new("Alcor Micro AU9540 00 00").unwrap(),
                CString::new("other").unwrap(),
            ]
        };
        assert!(select_reader(names(), None).is_err());
        assert!(select_reader(names(), Some("Alcor")).is_err());
        assert_eq!(
            select_reader(names(), Some("other")).unwrap().to_bytes(),
            b"other"
        );
        assert_eq!(
            select_reader(vec![CString::new("only").unwrap()], None)
                .unwrap()
                .to_bytes(),
            b"only"
        );
    }

    #[test]
    fn watcher_lock_is_exclusive_and_released_on_drop() {
        let dir = std::env::temp_dir().join(format!(
            "timeclock-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let db = dir.join("test.sqlite3");
        File::create(&db).unwrap();
        let lock = watcher_lock(&db).unwrap();
        assert!(watcher_lock(&dir.join(".").join("test.sqlite3")).is_err());
        drop(lock);
        drop(watcher_lock(&db).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tray_launch_does_not_offer_keyboard_controls() {
        let cli = Cli::try_parse_from(["timeclock", "tray"]).unwrap();
        assert!(matches!(cli.command, Command::Tray { .. }));
        assert!(Cli::try_parse_from(["timeclock", "tray", "--controls"]).is_err());
    }

    #[test]
    fn watch_and_resume_select_named_project_with_same_defaults() {
        for command in ["watch", "resume"] {
            let cli = Cli::try_parse_from(["timeclock", command, "My project", "--db", "work.db"])
                .unwrap();
            assert_eq!(cli.db, PathBuf::from("work.db"));
            let Command::Watch { project, atr, .. } = cli.command else {
                panic!("wrong command")
            };
            assert_eq!(project, "My project");
            assert_eq!(parse_atr(&atr).unwrap(), parse_atr(TARGET_ATR).unwrap());
        }
    }
}
