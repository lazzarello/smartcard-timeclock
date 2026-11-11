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

struct DatabaseLock(File);

impl Drop for DatabaseLock {
    fn drop(&mut self) {
        // Do not let a subprocess's inherited descriptor prolong the lock after shutdown.
        let _ = self.0.unlock();
    }
}

fn watcher_lock(path: &Path) -> Result<DatabaseLock> {
    // Create only the file before locking; schema initialization must hold the lock.
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
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
    Ok(DatabaseLock(file))
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
    store.create_project(project)?;
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
            if std::env::var_os("GSK_RENDERER").is_none() {
                // These text-only windows do not need cold GPU/driver initialization.
                // SAFETY: main is single-threaded, before GTK, PC/SC, or signal/tray workers.
                unsafe { std::env::set_var("GSK_RENDERER", "cairo") };
            }
            tray::run(cli.db, reader, atr)
        }
        Command::Report => {
            store::report(&cli.db).context("Cannot read report; run watch first or check --db")
        }
    }
}

#[cfg(test)]
mod tests;
