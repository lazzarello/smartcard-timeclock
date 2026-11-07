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
        pinned_reader_present(&[CString::new("other").unwrap(), pinned.clone()], &pinned).is_ok()
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
        let cli =
            Cli::try_parse_from(["timeclock", command, "My project", "--db", "work.db"]).unwrap();
        assert_eq!(cli.db, PathBuf::from("work.db"));
        let Command::Watch { project, atr, .. } = cli.command else {
            panic!("wrong command")
        };
        assert_eq!(project, "My project");
        assert_eq!(parse_atr(&atr).unwrap(), parse_atr(TARGET_ATR).unwrap());
    }
}
