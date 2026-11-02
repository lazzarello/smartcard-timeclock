//! Standard SNI/DBusMenu adapter; the only tracker is `watch` in main.rs.
use super::*;
use ksni::{
    MenuItem,
    blocking::TrayMethods,
    menu::{StandardItem, SubMenu},
};
use std::{
    io::{Read, Write},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    Idle,
    Watching(WatchState),
    Error(String),
}
impl Status {
    fn text(&self) -> String {
        match self {
            Self::Idle => "Select a project to watch for the card".into(),
            Self::Watching(WatchState::Waiting) => "Waiting for target card".into(),
            Self::Watching(WatchState::Tracking) => "Tracking".into(),
            Self::Watching(WatchState::CardAbsent) => {
                "Paused — card absent; watching for reinsertion".into()
            }
            Self::Error(error) => format!("Error — {error}"),
        }
    }
}

#[derive(Clone)]
struct Settings {
    db: PathBuf,
    project: String,
    reader: Option<String>,
    atr: Vec<u8>,
}

// A worker owns its event queue. Joining and discarding it prevents stale events after switching.
struct Worker {
    running: Arc<AtomicBool>,
    thread: JoinHandle<Result<()>>,
    events: Receiver<WatchState>,
}
impl Worker {
    fn spawn(
        run: impl FnOnce(&AtomicBool, Sender<WatchState>) -> Result<()> + Send + 'static,
    ) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let (tx, events) = mpsc::channel();
        Self {
            running,
            events,
            thread: thread::spawn(move || run(&flag, tx)),
        }
    }
    fn watch(settings: Settings) -> Self {
        Self::spawn(move |running, tx| {
            watch(
                &settings.db,
                &settings.project,
                settings.reader.as_deref(),
                &settings.atr,
                running,
                |state| {
                    let _ = tx.send(state);
                },
            )
        })
    }
    fn stop(self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        self.thread
            .join()
            .map_err(|_| anyhow::anyhow!("Watcher thread panicked"))?
    }
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Project,
    Database,
    Reader,
    Atr,
}
#[derive(Clone)]
enum Action {
    Controls,
    Select(String),
    Entry(Entry),
    Scan,
    Report,
    Quit,
}

#[derive(Clone)]
struct Tray {
    tx: Sender<Action>,
    settings: Settings,
    projects: Vec<String>,
    status: Status,
    notice: String,
    dialog_open: bool,
}
fn label(text: &str) -> String {
    text.replace('_', "__")
}
impl Tray {
    fn editable(&self) -> bool {
        self.status != Status::Watching(WatchState::Tracking)
    }
    fn control_choices(&self) -> Vec<(String, Action)> {
        let mut choices = vec![
            ("Scan readers…".into(), Action::Scan),
            ("Report…".into(), Action::Report),
            ("Quit timeclock".into(), Action::Quit),
        ];
        if self.editable() {
            choices.extend([
                ("New project…".into(), Action::Entry(Entry::Project)),
                ("Database path…".into(), Action::Entry(Entry::Database)),
                ("Exact reader…".into(), Action::Entry(Entry::Reader)),
                ("Target ATR…".into(), Action::Entry(Entry::Atr)),
            ]);
            choices.extend(
                self.projects
                    .iter()
                    .map(|p| (format!("Select project: {p}"), Action::Select(p.clone()))),
            );
        }
        choices
    }
    fn item(&self, text: &str, action: Action, enabled: bool) -> MenuItem<Self> {
        StandardItem {
            label: label(text),
            enabled,
            activate: Box::new(move |tray: &mut Self| {
                let _ = tray.tx.send(action.clone());
            }),
            ..Default::default()
        }
        .into()
    }
    fn info(text: &str) -> MenuItem<Self> {
        StandardItem {
            label: label(text),
            enabled: false,
            ..Default::default()
        }
        .into()
    }
}
impl ksni::Tray for Tray {
    const MENU_ON_ACTIVATE: bool = true;
    fn id(&self) -> String {
        "smartcard-timeclock".into()
    }
    fn title(&self) -> String {
        format!("Timeclock: {}", self.status.text())
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.title(),
            ..Default::default()
        }
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        // Status only: clock hands while tracking, a waiting dot, or an error cross.
        let (color, shape) = match self.status {
            Status::Watching(WatchState::Tracking) => ([255, 70, 210, 130], 0),
            Status::Watching(_) => ([255, 245, 190, 60], 1),
            Status::Idle => ([255, 150, 150, 150], 2),
            Status::Error(_) => ([255, 245, 90, 90], 3),
        };
        let mut data = vec![0; 32 * 32 * 4];
        for y in 0i32..32 {
            for x in 0i32..32 {
                let r = (x - 16).pow(2) + (y - 16).pow(2);
                let mark = match shape {
                    0 => {
                        ((15..=16).contains(&x) && (9..=16).contains(&y))
                            || ((15..=16).contains(&y) && (16..=22).contains(&x))
                    }
                    1 => (x - 16).pow(2) + (y - 16).pow(2) <= 12,
                    2 => false,
                    _ => (10..=22).contains(&x) && ((x - y).abs() <= 1 || (x + y - 32).abs() <= 1),
                };
                if (155..=215).contains(&r) || mark {
                    let offset = ((y * 32 + x) * 4) as usize;
                    data[offset..offset + 4].copy_from_slice(&color);
                }
            }
        }
        vec![ksni::Icon {
            width: 32,
            height: 32,
            data,
        }]
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut projects: Vec<_> = self
            .projects
            .iter()
            .map(|p| self.item(p, Action::Select(p.clone()), self.editable()))
            .collect();
        projects.push(self.item(
            "New project…",
            Action::Entry(Entry::Project),
            self.editable() && !self.dialog_open,
        ));
        vec![
            self.item("Keyboard controls…", Action::Controls, !self.dialog_open),
            Self::info(&self.status.text()),
            Self::info(&format!(
                "Project: {}",
                if self.settings.project.is_empty() {
                    "(select one)"
                } else {
                    &self.settings.project
                }
            )),
            Self::info(&format!("Database: {}", self.settings.db.display())),
            Self::info(&self.notice),
            SubMenu {
                label: "Projects (read-only while tracking)".into(),
                enabled: self.editable(),
                submenu: projects,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            self.item("Scan readers…", Action::Scan, !self.dialog_open),
            self.item("Report…", Action::Report, !self.dialog_open),
            SubMenu {
                label: "Settings (read-only while tracking)".into(),
                enabled: self.editable(),
                submenu: vec![
                    self.item(
                        "Database path…",
                        Action::Entry(Entry::Database),
                        self.editable() && !self.dialog_open,
                    ),
                    self.item(
                        &format!(
                            "Reader: {}…",
                            self.settings.reader.as_deref().unwrap_or("automatic")
                        ),
                        Action::Entry(Entry::Reader),
                        self.editable() && !self.dialog_open,
                    ),
                    self.item(
                        &format!("ATR: {}…", format_atr(&self.settings.atr)),
                        Action::Entry(Entry::Atr),
                        self.editable() && !self.dialog_open,
                    ),
                ],
                ..Default::default()
            }
            .into(),
            self.item("Quit", Action::Quit, true),
        ]
    }
}

// Own and reap every dialog, including pipe readers/writers, even on signals or Quit.
struct Dialog {
    child: Child,
    output: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
    input: Option<JoinHandle<std::io::Result<()>>>,
    entry: Option<Entry>,
    actions: Vec<Action>,
}
impl Dialog {
    fn spawn(entry: Option<Entry>, title: &str, text: String) -> Result<Self> {
        let mut command = ProcessCommand::new("zenity");
        command.args(["--title", title, "--width=640"]);
        if entry.is_some() {
            command.args(["--entry", "--text", title, "--entry-text", &text]);
        } else {
            command.args(["--text-info", "--height=480"]);
        }
        Self::launch(command, entry, text)
    }
    fn controls(view: &Tray) -> Result<Self> {
        let choices = view.control_choices();
        let mut command = ProcessCommand::new("zenity");
        command.args([
            "--list", "--title=Timeclock controls", "--width=720", "--height=600",
            "--column=ID", "--column=Action", "--hide-column=1", "--print-column=1",
            "--text",
            &format!("{}\nProject: {}\nDatabase: {}\n{}\nSelect an action with arrows and Enter. Cancel closes controls, not the tracker. Status refreshes after each action.",
                view.status.text(), view.settings.project, view.settings.db.display(), view.notice)
                .replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"),
        ]);
        for (index, (label, _)) in choices.iter().enumerate() {
            command.arg(index.to_string()).arg(label);
        }
        let mut dialog = Self::launch(command, None, String::new())?;
        dialog.actions = choices.into_iter().map(|(_, action)| action).collect();
        Ok(dialog)
    }
    fn launch(mut command: ProcessCommand, entry: Option<Entry>, text: String) -> Result<Self> {
        let mut child = command
            .stdin(if entry.is_some() {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .spawn()
            .context("Cannot open dialog; install Zenity and run in a graphical session")?;
        let mut stdout = child.stdout.take().unwrap();
        let output = Some(thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes)?;
            Ok(bytes)
        }));
        let input = child
            .stdin
            .take()
            .map(|mut stdin| thread::spawn(move || stdin.write_all(text.as_bytes())));
        Ok(Self {
            child,
            output,
            input,
            entry,
            actions: Vec::new(),
        })
    }
    fn poll(&mut self) -> Result<Option<Option<String>>> {
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        let bytes = self
            .output
            .take()
            .unwrap()
            .join()
            .map_err(|_| anyhow::anyhow!("Dialog reader panicked"))??;
        if let Some(input) = self.input.take() {
            let _ = input.join();
        } // Closing a report early may break its pipe.
        if status.code() == Some(1) {
            return Ok(Some(None));
        }
        ensure!(
            status.success(),
            "Zenity failed ({status}); check the graphical session"
        );
        let mut text = String::from_utf8(bytes)?;
        // Remove only Zenity's line terminator, not whitespace in project/reader names.
        if text.ends_with('\n') {
            text.pop();
        }
        Ok(Some(Some(text)))
    }
}
impl Drop for Dialog {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(output) = self.output.take() {
            let _ = output.join();
        }
        if let Some(input) = self.input.take() {
            let _ = input.join();
        }
    }
}

struct Controller {
    view: Tray,
    worker: Option<Worker>,
    spawn_worker: fn(Settings) -> Worker,
    dialog: Option<Dialog>,
    controls_open: bool,
}
impl Controller {
    fn stop(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            worker.stop()?;
        }
        self.view.status = Status::Idle;
        Ok(())
    }
    fn start_watching(&mut self) -> Result<()> {
        if self.worker.is_some() {
            return Ok(());
        }
        ensure!(
            !self.view.settings.project.trim().is_empty(),
            "Select a nonblank project first"
        );
        self.worker = Some((self.spawn_worker)(self.view.settings.clone()));
        self.view.status = Status::Watching(WatchState::Waiting);
        self.view.notice.clear();
        Ok(())
    }
    fn projects(&mut self) -> Result<()> {
        self.view.projects = if self.view.settings.db.try_exists()? {
            store::report_rows(&self.view.settings.db)?
                .into_iter()
                .map(|row| row.project)
                .collect()
        } else {
            Vec::new()
        };
        if !self.view.settings.project.is_empty()
            && !self.view.projects.contains(&self.view.settings.project)
        {
            self.view.projects.push(self.view.settings.project.clone());
        }
        Ok(())
    }
    fn apply(&mut self, entry: Entry, text: String) -> Result<()> {
        self.poll_worker()?;
        ensure!(
            self.view.editable(),
            "Remove the target card before changing the project or settings"
        );
        let mut settings = self.view.settings.clone();
        match entry {
            Entry::Project => {
                ensure!(!text.trim().is_empty(), "Project name cannot be blank");
                settings.project = text;
            }
            Entry::Database => {
                ensure!(!text.trim().is_empty(), "Database path cannot be blank");
                settings.db = std::path::absolute(text)?;
                if settings.db.try_exists()? {
                    store::report_rows(&settings.db).context("Cannot use selected database")?;
                } else {
                    ensure!(
                        settings.db.parent().is_some_and(Path::is_dir),
                        "Database parent directory does not exist"
                    );
                }
            }
            Entry::Reader => settings.reader = if text.is_empty() { None } else { Some(text) },
            Entry::Atr => settings.atr = parse_atr(&text).map_err(anyhow::Error::msg)?,
        }
        // Validation may block on SQLite; recheck events before changing a live project.
        self.poll_worker()?;
        ensure!(
            self.view.editable(),
            "Remove the target card before changing the project or settings"
        );
        self.stop()?;
        self.view.settings = settings;
        self.projects()?;
        if !self.view.settings.project.is_empty() {
            self.start_watching()?;
        }
        Ok(())
    }
    fn action(&mut self, action: Action) -> Result<()> {
        self.poll_worker()?;
        match action {
            Action::Controls if self.dialog.is_none() => {
                self.dialog = Some(Dialog::controls(&self.view)?);
                self.controls_open = true;
            }
            Action::Select(project) => self.apply(Entry::Project, project)?,
            Action::Entry(entry) if self.dialog.is_none() => {
                ensure!(
                    self.view.editable(),
                    "Remove the target card before changing the project or settings"
                );
                let (title, text) = match entry {
                    Entry::Project => (
                        "Project name (case-sensitive)",
                        self.view.settings.project.clone(),
                    ),
                    Entry::Database => (
                        "SQLite database path",
                        self.view.settings.db.to_string_lossy().into_owned(),
                    ),
                    Entry::Reader => (
                        "Exact reader name (empty = automatic; use Scan readers)",
                        self.view.settings.reader.clone().unwrap_or_default(),
                    ),
                    Entry::Atr => (
                        "Target ATR (card type, not unique identity)",
                        format_atr(&self.view.settings.atr),
                    ),
                };
                self.dialog = Some(Dialog::spawn(Some(entry), title, text)?);
            }
            Action::Scan if self.dialog.is_none() => {
                self.dialog = Some(Dialog::spawn(None, "PC/SC readers", scan_text()?)?);
            }
            Action::Report if self.dialog.is_none() => {
                let rows = store::report_rows(&self.view.settings.db).context(
                    "Cannot read selected database; record a session first or check the path",
                )?;
                self.dialog = Some(Dialog::spawn(
                    None,
                    "Project totals (open time through last checkpoint)",
                    store::report_text(&rows),
                )?);
                self.projects()?;
            }
            _ => {}
        }
        Ok(())
    }
    fn poll_worker(&mut self) -> Result<()> {
        if let Some(worker) = &self.worker {
            for state in worker.events.try_iter() {
                self.view.status = Status::Watching(state);
            }
            if worker.thread.is_finished() {
                self.stop()?;
                self.projects()?;
            }
        }
        Ok(())
    }
    fn poll(&mut self) -> Result<()> {
        self.poll_worker()?;
        if let Some(dialog) = &mut self.dialog {
            let result = dialog.poll();
            if !matches!(result, Ok(None)) {
                let dialog = self.dialog.take().unwrap();
                if !dialog.actions.is_empty() {
                    self.controls_open = false; // Cancel or failure must not reopen a dialog.
                    if let Some(Some(text)) = result? {
                        let action = dialog
                            .actions
                            .get(text.parse::<usize>()?)
                            .context("Invalid controls selection")?
                            .clone();
                        self.controls_open = true;
                        if matches!(action, Action::Quit) {
                            self.controls_open = false;
                            self.view.tx.send(action)?;
                        } else {
                            self.action(action)?;
                        }
                    }
                } else if let (Some(entry), Some(Some(text))) = (dialog.entry, result?) {
                    self.apply(entry, text)?;
                }
            }
        }
        if self.controls_open && self.dialog.is_none() {
            self.dialog = Some(Dialog::controls(&self.view)?);
        }
        Ok(())
    }
    fn error(&mut self, error: anyhow::Error) {
        let text = format!("{error:#}");
        eprintln!("{text}");
        if self.worker.is_none() {
            self.view.status = Status::Error(text.clone());
        }
        self.view.notice = text;
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.dialog.take();
        if let Err(error) = self.stop() {
            eprintln!("Cannot stop watcher cleanly: {error:#}");
        }
    }
}

pub fn run(db: PathBuf, reader: Option<String>, atr: Vec<u8>, controls: bool) -> Result<()> {
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    ctrlc::set_handler(move || flag.store(false, Ordering::SeqCst))?;
    let (tx, rx) = mpsc::channel();
    let view = Tray {
        tx,
        settings: Settings {
            db: std::path::absolute(db)?,
            project: String::new(),
            reader,
            atr,
        },
        projects: Vec::new(),
        status: Status::Idle,
        notice: "Select or enter a project; the card controls timing.".into(),
        dialog_open: false,
    };
    let mut controller = Controller {
        view,
        worker: None,
        spawn_worker: Worker::watch,
        dialog: None,
        controls_open: false,
    };
    if let Err(error) = controller.projects() {
        controller.error(error);
    }
    let handle = controller.view.clone().spawn().context("Cannot register system tray; a session bus and StatusNotifier host (such as DMS) are required")?;
    if controls {
        controller.view.tx.send(Action::Controls)?;
    }
    while running.load(Ordering::SeqCst) && !handle.is_closed() {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Action::Quit) => break,
            Ok(action) => {
                if let Err(error) = controller.action(action) {
                    controller.error(error);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if let Err(error) = controller.poll() {
            controller.error(error);
        }
        controller.view.dialog_open = controller.dialog.is_some();
        handle.update(|view| *view = controller.view.clone());
    }
    controller.dialog.take();
    let result = controller.stop();
    handle.shutdown().wait();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller(db: PathBuf) -> Controller {
        let (tx, _) = mpsc::channel();
        Controller {
            view: Tray {
                tx,
                settings: Settings {
                    db,
                    project: "old_project <&>".into(),
                    reader: None,
                    atr: parse_atr(TARGET_ATR).unwrap(),
                },
                projects: Vec::new(),
                status: Status::Idle,
                notice: String::new(),
                dialog_open: false,
            },
            worker: None,
            spawn_worker: |_| {
                Worker::spawn(|running, tx| {
                    tx.send(WatchState::Waiting)?;
                    while running.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Ok(())
                })
            },
            dialog: None,
            controls_open: false,
        }
    }

    #[test]
    fn selection_arms_watcher_and_switch_joins_before_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("clock.db");
        let mut c = controller(db.clone());
        let (ready, wait) = mpsc::channel();
        c.worker = Some(Worker::spawn(move |running, tx| {
            let mut store = store::Store::open(&db)?;
            let _lock = watcher_lock(&db)?;
            store.record("old_project <&>", "3B00", "fake", 10)?;
            store.stop("removed_or_changed", Some(15))?;
            tx.send(WatchState::CardAbsent)?;
            ready.send(())?;
            while running.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
            store.stop("shutdown", None)?;
            Ok(())
        }));
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        let id = c.worker.as_ref().unwrap().thread.thread().id();
        c.start_watching().unwrap();
        c.start_watching().unwrap();
        assert_eq!(id, c.worker.as_ref().unwrap().thread.thread().id());
        c.poll().unwrap();
        assert_eq!(c.view.status, Status::Watching(WatchState::CardAbsent));
        c.action(Action::Select("new_项目".into())).unwrap();
        assert_ne!(id, c.worker.as_ref().unwrap().thread.thread().id());
        assert_eq!(c.view.status, Status::Watching(WatchState::Waiting));
        assert_eq!(c.view.settings.project, "new_项目");
        let rows = store::report_rows(&c.view.settings.db).unwrap();
        assert_eq!((rows[0].seconds, rows[0].open), (5, 0));
        // Selecting another project arms it without recording any absent-card time.
        c.poll().unwrap();
        assert!(c.worker.is_some());
        c.stop().unwrap();
        let mut store = store::Store::open(&c.view.settings.db).unwrap();
        let _lock = watcher_lock(&c.view.settings.db).unwrap();
        store
            .record("old_project <&>", "3B00", "fake", 1000)
            .unwrap();
        store.stop("shutdown", Some(1005)).unwrap();
        let rows = store::report_rows(&c.view.settings.db).unwrap();
        assert_eq!(
            (rows[0].seconds, rows[0].sessions, rows[0].open),
            (10, 2, 0)
        );
    }

    #[test]
    fn validation_does_not_mutate_settings_or_create_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("unused.db");
        let mut c = controller(db.clone());
        c.projects().unwrap();
        for (entry, text) in [
            (Entry::Project, "  "),
            (Entry::Atr, "bad"),
            (Entry::Database, ""),
        ] {
            assert!(c.apply(entry, text.into()).is_err());
        }
        assert_eq!(c.view.settings.project, "old_project <&>");
        assert_eq!(c.view.settings.db, db);
        assert!(!db.exists());
        c.apply(Entry::Reader, "exact reader  ".into()).unwrap();
        assert_eq!(c.view.settings.reader.as_deref(), Some("exact reader  "));
        c.apply(Entry::Reader, String::new()).unwrap();
        assert!(c.view.settings.reader.is_none());
        assert_eq!(label("a_b__c"), "a__b____c");
    }

    #[test]
    fn tracking_locks_menus_settings_and_stale_dialog_submissions() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = controller(dir.path().join("unused.db"));
        c.projects().unwrap();
        let (updates, events) = mpsc::channel();
        let (ready, wait) = mpsc::channel();
        c.worker = Some(Worker::spawn(move |running, tx| {
            tx.send(WatchState::Tracking)?;
            ready.send(())?;
            while running.load(Ordering::SeqCst) {
                if let Ok(state) = events.recv_timeout(Duration::from_millis(5)) {
                    tx.send(state)?;
                    ready.send(())?;
                }
            }
            Ok(())
        }));
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        let id = c.worker.as_ref().unwrap().thread.thread().id();
        // The view is still idle: queued worker events must be checked before actions.
        assert!(c.action(Action::Select("other".into())).is_err());
        assert_eq!(c.view.settings.project, "old_project <&>");
        assert!(!c.view.editable());
        for entry in [Entry::Project, Entry::Database, Entry::Reader, Entry::Atr] {
            assert!(c.action(Action::Entry(entry)).is_err());
            assert!(c.apply(entry, "other".into()).is_err());
        }
        assert!(
            c.view.control_choices().iter().all(|(_, action)| {
                matches!(action, Action::Scan | Action::Report | Action::Quit)
            })
        );
        for item in ksni::Tray::menu(&c.view) {
            if let MenuItem::SubMenu(menu) = item {
                assert!(!menu.enabled);
            }
        }
        // An entry dialog opened before insertion cannot change the active project.
        c.dialog = Some(fake_dialog("printf 'other\\n'"));
        let mut rejected = false;
        for _ in 0..200 {
            if c.poll().is_err() {
                rejected = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(rejected);
        assert!(c.dialog.is_none());
        assert_eq!(id, c.worker.as_ref().unwrap().thread.thread().id());
        assert_eq!(c.view.settings.project, "old_project <&>");

        updates.send(WatchState::CardAbsent).unwrap();
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        c.poll().unwrap();
        assert!(c.view.editable());
        assert!(
            c.view
                .control_choices()
                .iter()
                .any(|(_, action)| matches!(action, Action::Select(_)))
        );
        for item in ksni::Tray::menu(&c.view) {
            if let MenuItem::SubMenu(menu) = item {
                assert!(menu.enabled);
            }
        }
        c.action(Action::Select("other".into())).unwrap();
        assert_eq!(c.view.settings.project, "other");
        assert_ne!(id, c.worker.as_ref().unwrap().thread.thread().id());
        assert!(!c.view.settings.db.exists());
    }

    #[test]
    fn setup_stays_idle_until_project_selection_then_watches_automatically() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = controller(dir.path().join("unused.db"));
        c.view.settings.project.clear();
        c.apply(Entry::Reader, "reader".into()).unwrap();
        assert!(c.worker.is_none());
        c.apply(Entry::Project, "My project".into()).unwrap();
        assert!(c.worker.is_some());
        assert_eq!(c.view.status, Status::Watching(WatchState::Waiting));
        assert!(!c.view.settings.db.exists());
    }

    #[test]
    fn worker_errors_are_not_reported_as_idle_and_events_do_not_leak() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = controller(dir.path().join("unused.db"));
        c.worker = Some(Worker::spawn(|_, tx| {
            tx.send(WatchState::Tracking)?;
            bail!("close failed")
        }));
        while !c.worker.as_ref().unwrap().thread.is_finished() {
            thread::sleep(Duration::from_millis(1));
        }
        let error = c.poll().unwrap_err();
        c.error(error);
        assert!(matches!(c.view.status, Status::Error(_)));
        assert!(c.worker.is_none());
        c.stop().unwrap();
        c.poll().unwrap();
        assert_eq!(c.view.status, Status::Idle);
    }

    fn fake_dialog(script: &str) -> Dialog {
        let mut command = ProcessCommand::new("sh");
        command.args(["-c", script]);
        Dialog::launch(command, Some(Entry::Project), String::new()).unwrap()
    }
    fn wait_dialog(dialog: &mut Dialog) -> Result<Option<String>> {
        for _ in 0..200 {
            if let Some(result) = dialog.poll()? {
                return Ok(result);
            }
            thread::sleep(Duration::from_millis(5));
        }
        bail!("Fake dialog timed out")
    }
    #[test]
    fn dialogs_preserve_input_distinguish_cancel_and_reap_on_drop() {
        assert_eq!(
            wait_dialog(&mut fake_dialog("printf '  name_;$()  \\n'")).unwrap(),
            Some("  name_;$()  ".into())
        );
        assert_eq!(wait_dialog(&mut fake_dialog("exit 1")).unwrap(), None);
        assert!(wait_dialog(&mut fake_dialog("exit 2")).is_err());
        let start = std::time::Instant::now();
        drop(fake_dialog("exec sleep 30"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn cancelled_dialog_leaves_selection_and_active_worker_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = controller(dir.path().join("unused.db"));
        c.worker = Some(Worker::spawn(|running, tx| {
            tx.send(WatchState::Waiting)?;
            while running.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        }));
        let id = c.worker.as_ref().unwrap().thread.thread().id();
        c.dialog = Some(fake_dialog("exit 1"));
        for _ in 0..200 {
            c.poll().unwrap();
            if c.dialog.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(c.dialog.is_none());
        assert_eq!(c.view.settings.project, "old_project <&>");
        assert_eq!(c.worker.as_ref().unwrap().thread.thread().id(), id);
        assert!(!c.view.settings.db.exists());
        c.stop().unwrap();
    }

    #[test]
    fn controls_cancel_and_quit_route_without_starting_watchers() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = controller(dir.path().join("unused.db"));
        let (tx, rx) = mpsc::channel();
        c.view.tx = tx;
        for (script, cancelled) in [("exit 1", true), ("printf '0\\n'", false)] {
            let mut dialog = fake_dialog(script);
            dialog.actions = vec![Action::Quit];
            c.dialog = Some(dialog);
            c.controls_open = true;
            for _ in 0..200 {
                c.poll().unwrap();
                if c.dialog.is_none() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            assert!(c.dialog.is_none());
            assert!(!c.controls_open);
            if cancelled {
                assert!(rx.try_recv().is_err());
            } else {
                assert!(matches!(rx.try_recv().unwrap(), Action::Quit));
            }
            assert!(c.worker.is_none());
            assert!(!c.view.settings.db.exists());
        }
    }

    struct Watcher(Sender<String>);
    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl Watcher {
        fn register_status_notifier_item(&self, service: &str) {
            self.0.send(service.into()).unwrap();
        }
        #[zbus(property)]
        fn is_status_notifier_host_registered(&self) -> bool {
            true
        }
    }

    // Run only under an isolated bus: never register a fake watcher on the desktop bus.
    #[test]
    #[ignore = "requires TIMECLOCK_ISOLATED_BUS=1 dbus-run-session and dbus-send"]
    fn isolated_protocol_registration_menu_updates_and_shutdown() {
        assert_eq!(std::env::var("TIMECLOCK_ISOLATED_BUS").as_deref(), Ok("1"));
        let (tx, rx) = mpsc::channel();
        let bus = zbus::blocking::connection::Builder::session()
            .unwrap()
            .name("org.kde.StatusNotifierWatcher")
            .unwrap()
            .serve_at("/StatusNotifierWatcher", Watcher(tx))
            .unwrap()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut c = controller(dir.path().join("untouched.db"));
        let (actions, received) = mpsc::channel();
        c.view.tx = actions;
        let handle = c.view.clone().spawn().unwrap();
        let name = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        // SNI uses NewTitle, not PropertiesChanged; do not use zbus's property cache.
        let item = zbus::blocking::proxy::Builder::<zbus::blocking::Proxy>::new(&bus)
            .destination(name.as_str())
            .unwrap()
            .path("/StatusNotifierItem")
            .unwrap()
            .interface("org.kde.StatusNotifierItem")
            .unwrap()
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .unwrap();
        assert_eq!(item.get_property::<String>("Status").unwrap(), "Active");
        assert!(
            item.get_property::<String>("Title")
                .unwrap()
                .contains("Select a project")
        );
        let icons: Vec<(i32, i32, Vec<u8>)> = item.get_property("IconPixmap").unwrap();
        assert_eq!(icons[0].2.len(), 32 * 32 * 4);
        let menu: zbus::zvariant::OwnedObjectPath = item.get_property("Menu").unwrap();
        let output = ProcessCommand::new("dbus-send")
            .args([
                "--session",
                "--print-reply",
                "--reply-timeout=3000",
                &format!("--dest={name}"),
                menu.as_str(),
                "com.canonical.dbusmenu.GetLayout",
                "int32:0",
                "int32:-1",
                "array:string:label",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        for label in [
            "Keyboard controls",
            "New project",
            "Scan readers",
            "Report",
            "Database path",
            "old__project",
        ] {
            assert!(text.contains(label), "missing {label}: {text}");
        }
        assert!(!text.contains("Resume selected project"));
        assert!(!text.contains("Pause / stop watching"));
        let menu_proxy = zbus::blocking::Proxy::new(
            &bus,
            name.as_str(),
            menu.as_str(),
            "com.canonical.dbusmenu",
        )
        .unwrap();
        type Layout = (
            i32,
            std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
            Vec<zbus::zvariant::OwnedValue>,
        );
        let (_, layout): (u32, Layout) = menu_proxy
            .call("GetLayout", &(0i32, -1i32, Vec::<&str>::new()))
            .unwrap();
        for child in layout.2 {
            let (id, properties, _): Layout = child.try_into().unwrap();
            if properties
                .get("label")
                .and_then(|v| <&str>::try_from(v).ok())
                == Some("Quit")
            {
                let _: () = menu_proxy
                    .call(
                        "Event",
                        &(id, "clicked", zbus::zvariant::Value::from(0i32), 0u32),
                    )
                    .unwrap();
            }
        }
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            Action::Quit
        ));
        handle.update(|tray| tray.status = Status::Watching(WatchState::Tracking));
        assert!(
            item.get_property::<String>("Title")
                .unwrap()
                .contains("Tracking")
        );
        handle.update(|tray| tray.status = Status::Error("fake error".into()));
        assert!(
            item.get_property::<String>("Title")
                .unwrap()
                .contains("fake error")
        );
        handle.shutdown().wait();
        // ksni acknowledges shutdown just before dropping the service receiver.
        for _ in 0..200 {
            if handle.is_closed() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(handle.is_closed());
        assert!(!c.view.settings.db.exists());
    }
}
