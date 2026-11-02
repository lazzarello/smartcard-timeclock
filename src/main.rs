mod store;

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

use anyhow::{Context as _, Result, bail, ensure};
use clap::{Parser, Subcommand};
use pcsc::{Context, ReaderState, Scope, State};

// shortcut: ATRs identify card types; use card-specific identity if physical-card uniqueness is needed.
const TARGET_ATR: &str = "3B 68 00 00 99 02 00 D1 03 04 22 01";
const POLL_INTERVAL: Duration = Duration::from_secs(1);

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

fn scan() -> Result<()> {
    let ctx = context()?;
    let mut states: Vec<_> = readers(&ctx)?
        .into_iter()
        .map(|name| ReaderState::new(name, State::UNAWARE))
        .collect();
    ctx.get_status_change(POLL_INTERVAL, &mut states)?;
    for state in states {
        println!("Reader: {}", state.name().to_string_lossy());
        if state.event_state().contains(State::PRESENT) {
            println!("  ATR: {}", format_atr(state.atr()));
        } else {
            println!("  No card present ({:?})", state.event_state());
        }
    }
    Ok(())
}

fn matches_target(state: State, atr: &[u8], target: &[u8]) -> bool {
    state.contains(State::PRESENT)
        && !state.intersects(
            State::EMPTY | State::UNKNOWN | State::UNAVAILABLE | State::MUTE | State::IGNORE,
        )
        && atr == target
}

fn now() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .try_into()?)
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

fn watch(path: &Path, project: &str, requested: Option<&str>, atr: &[u8]) -> Result<()> {
    ensure!(!project.trim().is_empty(), "Project name cannot be blank");
    let running = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&running);
    ctrlc::set_handler(move || flag.store(false, Ordering::SeqCst))?;

    let mut store = store::Store::open(path).context("Cannot initialize SQLite database")?;
    let _lock = watcher_lock(path)?;
    let recovered = store.recover()?;
    if recovered > 0 {
        eprintln!(
            "Recovered {recovered} interrupted session at its last checkpoint (no downtime counted)."
        );
    }

    let result = monitor(&mut store, project, requested, atr, &running);
    // On errors, do not count time after the last confirmed presence.
    let stop_time = if result.is_ok() { Some(now()?) } else { None };
    if store.stop(
        if result.is_ok() {
            "shutdown"
        } else {
            "reader_error"
        },
        stop_time,
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
) -> Result<()> {
    let ctx = context()?;
    let name = select_reader(readers(&ctx)?, requested)?;
    let reader = name.to_string_lossy().into_owned();
    let atr = format_atr(target);
    println!("Watching {reader:?} for ATR {atr}; project {project:?}.");
    println!("Waiting for target card insertion to start or resume {project:?}.");
    eprintln!("ATR matching identifies a card type, not a unique card. Press Ctrl-C to stop.");
    let mut states = [ReaderState::new(name, State::UNAWARE)];
    let mut previous = None;
    while running.load(Ordering::SeqCst) {
        match ctx.get_status_change(POLL_INTERVAL, &mut states) {
            Ok(()) | Err(pcsc::Error::Timeout) => {}
            Err(error) => {
                return Err(error)
                    .context("PC/SC monitoring failed; reconnect the reader and restart watch");
            }
        }
        let state = &mut states[0];
        let flags = state.event_state();
        if flags.intersects(State::UNKNOWN | State::UNAVAILABLE | State::IGNORE) {
            bail!("Reader unavailable; reconnect it and restart watch");
        }
        let matching = matches_target(flags, state.atr(), target);
        let count = state.event_count();
        // Catch removal/reinsertion that happened between observations, where supported by pcscd.
        if matching
            && previous.is_some_and(|(was_matching, old_count)| was_matching && old_count != count)
        {
            store.stop("card_changed", None)?;
        }
        if matching {
            if store.record(project, &atr, &reader, now()?)? {
                println!("Started {project:?}.");
            }
        } else if store.stop("removed_or_changed", Some(now()?))? {
            println!("Paused {project:?}; waiting for target card insertion to resume.");
        }
        previous = Some((matching, count));
        state.sync_current_state();
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan => scan(),
        Command::Watch {
            project,
            reader,
            atr,
        } => {
            let atr = parse_atr(&atr).map_err(anyhow::Error::msg)?;
            watch(&cli.db, &project, reader.as_deref(), &atr)
        }
        Command::Report => {
            store::report(&cli.db).context("Cannot read report; run watch first or check --db")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
