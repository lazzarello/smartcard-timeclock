//! Standard SNI/DBusMenu adapter; the only tracker is `watch` in main.rs.
use super::*;
use gtk4::{gio, glib, prelude::*};
use ksni::{
    MenuItem,
    blocking::TrayMethods,
    menu::{StandardItem, SubMenu},
};
use std::{
    cell::Cell,
    io::{Read, Write},
    process::{Child, Command as ProcessCommand, Stdio},
    rc::Rc,
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
            Self::Idle => "Select project".into(),
            Self::Watching(WatchState::Waiting) => "Waiting for card".into(),
            Self::Watching(WatchState::Tracking) => "Tracking".into(),
            Self::Watching(WatchState::CardAbsent) => "Paused".into(),
            Self::Watching(WatchState::Reconnecting) => "Reader unavailable — reconnecting".into(),
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
    Select(String),
    Entry(Entry),
    Scan,
    Status,
    Report,
    Quit,
}

#[derive(Clone)]
struct Tray {
    tx: Sender<Action>,
    settings: Settings,
    projects: Vec<String>,
    project_seconds: i64,
    status: Status,
    dialog_open: bool,
}
fn label(text: &str) -> String {
    text.replace('_', "__")
}
impl Tray {
    fn status_text(&self) -> String {
        if !self.settings.project.is_empty() {
            let state = match self.status {
                Status::Watching(WatchState::Waiting) => Some("Waiting for card"),
                Status::Watching(WatchState::Tracking) => Some("Tracking"),
                Status::Watching(WatchState::CardAbsent) => Some("Paused"),
                Status::Watching(WatchState::Reconnecting) => {
                    Some("Reader unavailable — reconnecting")
                }
                _ => None,
            };
            if let Some(state) = state {
                return format!(
                    "{state} {} ({})",
                    self.settings.project,
                    store::format_duration(self.project_seconds)
                );
            }
        }
        self.status.text()
    }
    fn editable(&self) -> bool {
        self.status != Status::Watching(WatchState::Tracking)
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
}
impl ksni::Tray for Tray {
    const MENU_ON_ACTIVATE: bool = false;
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(Action::Status);
    }
    fn id(&self) -> String {
        "smartcard-timeclock".into()
    }
    fn title(&self) -> String {
        self.status_text()
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
            SubMenu {
                label: "Projects".into(),
                enabled: self.editable(),
                submenu: projects,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            self.item("Scan readers…", Action::Scan, !self.dialog_open),
            self.item("Report…", Action::Report, !self.dialog_open),
            SubMenu {
                label: "Settings".into(),
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
}
impl Dialog {
    fn spawn(entry: Option<Entry>, title: &str, text: String) -> Result<Self> {
        let mut command = ProcessCommand::new("zenity");
        command.args(["--title", title, "--width=640"]);
        if entry.is_some() {
            command.args(["--entry", "--text", title, "--entry-text", &text]);
        } else {
            command.args(["--text-info", "--font=monospace", "--height=480"]);
        }
        Self::launch(command, entry, text)
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

const APPEARANCE_NAMESPACE: &str = "org.freedesktop.appearance";
const COLOR_SCHEME_KEY: &str = "color-scheme";

fn prefer_dark(mut value: glib::Variant, fallback: bool) -> bool {
    // Older portals wrap Settings.Read's value in an extra variant.
    while value.is::<glib::Variant>() {
        value = value.as_variant().unwrap();
    }
    match value.get::<u32>() {
        Some(1) => true,
        Some(2) => false,
        _ => fallback,
    }
}

async fn follow_system_theme() {
    let Some(settings) = gtk4::Settings::default() else {
        return;
    };
    let fallback = settings.is_gtk_application_prefer_dark_theme();
    let connection = match gio::bus_get_future(gio::BusType::Session).await {
        Ok(connection) => connection,
        Err(error) => {
            eprintln!("Cannot follow desktop color scheme; keeping GTK theme: {error}");
            return;
        }
    };
    let changed = Rc::new(Cell::new(false));
    let signal_changed = changed.clone();
    let signal_settings = settings.clone();
    let _subscription = connection.subscribe_to_signal(
        Some("org.freedesktop.portal.Desktop"),
        Some("org.freedesktop.portal.Settings"),
        Some("SettingChanged"),
        Some("/org/freedesktop/portal/desktop"),
        Some(APPEARANCE_NAMESPACE),
        gio::DBusSignalFlags::NONE,
        move |signal| {
            if signal.parameters.is::<(String, String, glib::Variant)>()
                && let Some((namespace, key, value)) =
                    signal.parameters.get::<(String, String, glib::Variant)>()
                && namespace == APPEARANCE_NAMESPACE
                && key == COLOR_SCHEME_KEY
            {
                signal_changed.set(true);
                signal_settings.set_gtk_application_prefer_dark_theme(prefer_dark(value, fallback));
            }
        },
    );
    match connection
        .call_future(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings",
            "Read",
            Some(&(APPEARANCE_NAMESPACE, COLOR_SCHEME_KEY).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            3000,
        )
        .await
    {
        Ok(reply) => {
            // A newer SettingChanged signal must win over an in-flight initial read.
            if !changed.get()
                && reply.is::<(glib::Variant,)>()
                && let Some((value,)) = reply.get::<(glib::Variant,)>()
            {
                settings.set_gtk_application_prefer_dark_theme(prefer_dark(value, fallback));
            }
        }
        Err(error) => eprintln!("Cannot read desktop color scheme; keeping GTK theme: {error}"),
    }
    // Keep the subscription alive until the status window's task is aborted.
    std::future::pending::<()>().await;
}

struct StatusWindow {
    window: gtk4::Window,
    label: gtk4::Label,
    theme: glib::JoinHandle<()>,
}
impl StatusWindow {
    fn new(view: &Tray) -> Result<Self> {
        gtk4::init().context("Cannot open status window; run in a graphical session")?;
        let label = gtk4::Label::builder()
            .label(view.status_text())
            .selectable(true)
            .margin_top(16)
            .margin_bottom(16)
            .margin_start(16)
            .margin_end(16)
            .build();
        let titlebar = gtk4::HeaderBar::new();
        titlebar.add_css_class("default-decoration");
        let style = gtk4::CssProvider::new();
        // Alacritty's Wayland title bar is 35 logical pixels, including the bottom border.
        style.load_from_data("headerbar { min-height: 34px; padding-top: 0; padding-bottom: 0; }");
        titlebar
            .style_context()
            .add_provider(&style, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
        let window = gtk4::Window::builder()
            .title("Timeclock status")
            .resizable(false)
            .hide_on_close(true)
            .child(&label)
            .build();
        window.set_titlebar(Some(&titlebar));
        let theme = glib::spawn_future_local(follow_system_theme());
        Ok(Self {
            window,
            label,
            theme,
        })
    }
    fn update(&self, view: &Tray) {
        let text = view.status_text();
        if self.label.text() != text {
            self.label.set_text(&text);
        }
    }
}
impl Drop for StatusWindow {
    fn drop(&mut self) {
        self.theme.abort();
        self.window.destroy();
    }
}

struct Controller {
    view: Tray,
    worker: Option<Worker>,
    spawn_worker: fn(Settings) -> Worker,
    dialog: Option<Dialog>,
    status_window: Option<StatusWindow>,
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
        Ok(())
    }
    fn projects(&mut self) -> Result<()> {
        let rows = if self.view.settings.db.try_exists()? {
            store::report_rows(&self.view.settings.db)?
        } else {
            Vec::new()
        };
        self.view.project_seconds = rows
            .iter()
            .find(|row| row.project == self.view.settings.project)
            .map_or(0, |row| row.seconds);
        self.view.projects = rows.into_iter().map(|row| row.project).collect();
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
        self.view.project_seconds = 0;
        self.projects()?;
        if !self.view.settings.project.is_empty() {
            self.start_watching()?;
        }
        Ok(())
    }
    fn action(&mut self, action: Action) -> Result<()> {
        self.poll_worker()?;
        match action {
            Action::Select(project) => self.apply(Entry::Project, project)?,
            Action::Status => {
                if self.status_window.is_none() {
                    self.status_window = Some(StatusWindow::new(&self.view)?);
                }
                let status = self.status_window.as_ref().unwrap();
                status.update(&self.view);
                status.window.present();
            }
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
        let mut updated = false;
        if let Some(worker) = &self.worker {
            for state in worker.events.try_iter() {
                self.view.status = Status::Watching(state);
                updated = true;
            }
            if worker.thread.is_finished() {
                self.stop()?;
                updated = true;
            }
        }
        if updated {
            self.projects().context("Cannot refresh project time")?;
        }
        Ok(())
    }
    fn poll(&mut self) -> Result<()> {
        self.poll_worker()?;
        if let Some(status) = &self.status_window {
            status.update(&self.view);
            let context = gtk4::glib::MainContext::default();
            while context.pending() {
                context.iteration(false);
            }
        }
        if let Some(dialog) = &mut self.dialog {
            let result = dialog.poll();
            if !matches!(result, Ok(None)) {
                let dialog = self.dialog.take().unwrap();
                if let Some(entry) = dialog.entry {
                    if let Some(Some(text)) = result? {
                        self.apply(entry, text)?;
                    }
                } else {
                    result?;
                }
            }
        }
        Ok(())
    }
    fn error(&mut self, error: anyhow::Error) {
        let text = format!("{error:#}");
        eprintln!("{text}");
        if self.worker.is_none() {
            self.view.status = Status::Error(text);
        }
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.status_window.take();
        self.dialog.take();
        if let Err(error) = self.stop() {
            eprintln!("Cannot stop watcher cleanly: {error:#}");
        }
    }
}

pub fn run(db: PathBuf, reader: Option<String>, atr: Vec<u8>) -> Result<()> {
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
        project_seconds: 0,
        status: Status::Idle,
        dialog_open: false,
    };
    let mut controller = Controller {
        view,
        worker: None,
        spawn_worker: Worker::watch,
        dialog: None,
        status_window: None,
    };
    if let Err(error) = controller.projects() {
        controller.error(error);
    }
    let handle = controller.view.clone().spawn().context("Cannot register system tray; a session bus and StatusNotifier host (such as DMS) are required")?;
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
                project_seconds: 0,
                status: Status::Idle,
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
            status_window: None,
        }
    }

    fn status_label(view: &Tray) -> String {
        let title = ksni::Tray::title(view);
        assert_eq!(ksni::Tray::tool_tip(view).title, title);
        assert_eq!(title, view.status_text());
        title
    }

    #[test]
    fn desktop_color_scheme_handles_preferences_and_wrapped_read_values() {
        for fallback in [false, true] {
            for (scheme, expected) in [(0u32, fallback), (1, true), (2, false), (99, fallback)] {
                let value = scheme.to_variant();
                assert_eq!(prefer_dark(value.clone(), fallback), expected);
                assert_eq!(prefer_dark(value.to_variant(), fallback), expected);
            }
            assert_eq!(prefer_dark("invalid".to_variant(), fallback), fallback);
        }
    }

    #[test]
    fn left_click_requests_status_in_any_tracking_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = controller(dir.path().join("unused.db"));
        let (tx, rx) = mpsc::channel();
        c.view.tx = tx;
        for status in [
            Status::Idle,
            Status::Watching(WatchState::Waiting),
            Status::Watching(WatchState::Tracking),
            Status::Watching(WatchState::CardAbsent),
            Status::Watching(WatchState::Reconnecting),
            Status::Error("Reader unavailable".into()),
        ] {
            c.view.status = status.clone();
            ksni::Tray::activate(&mut c.view, 0, 0);
            assert!(matches!(rx.try_recv().unwrap(), Action::Status));
            assert!(rx.try_recv().is_err());
            assert_eq!(c.view.status, status);
        }
        assert!(c.worker.is_none());
        assert!(!c.view.settings.db.exists());
    }

    #[test]
    fn menu_routes_only_scan_report_and_quit_top_level_actions() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = controller(dir.path().join("unused.db"));
        let (tx, rx) = mpsc::channel();
        c.view.tx = tx;
        assert!(ksni::Tray::menu(&c.view).iter().all(|item| {
            !matches!(item, MenuItem::Standard(item) if item.label.starts_with("Database:"))
        }));
        let actions: Vec<_> = ksni::Tray::menu(&c.view)
            .into_iter()
            .filter_map(|item| match item {
                MenuItem::Standard(item) if item.enabled => Some(item),
                _ => None,
            })
            .collect();
        assert_eq!(
            actions
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["Scan readers…", "Report…", "Quit"]
        );
        for item in actions {
            (item.activate)(&mut c.view);
        }
        assert!(matches!(rx.try_recv().unwrap(), Action::Scan));
        assert!(matches!(rx.try_recv().unwrap(), Action::Report));
        assert!(matches!(rx.try_recv().unwrap(), Action::Quit));
        assert!(rx.try_recv().is_err());
        assert!(c.worker.is_none());
        assert!(!c.view.settings.db.exists());
    }

    #[test]
    fn title_time_follows_checkpoints_pause_and_reinsertion() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("clock.db");
        let mut c = controller(db.clone());
        let mut store = store::Store::open(&db).unwrap();
        let project = c.view.settings.project.clone();
        store.record("OLD_project <&>", "3B00", "fake", 0).unwrap();
        store.stop("removed", Some(99999)).unwrap();
        store.record(&project, "3B00", "fake", 10).unwrap();
        store.stop("removed", Some(3733)).unwrap();
        store.record(&project, "3B00", "fake", 10000).unwrap();

        let (updates, events) = mpsc::channel();
        let (ready, wait) = mpsc::channel();
        c.worker = Some(Worker::spawn(move |running, tx| {
            while running.load(Ordering::SeqCst) {
                if let Ok(state) = events.recv_timeout(Duration::from_millis(5)) {
                    tx.send(state)?;
                    ready.send(())?;
                }
            }
            Ok(())
        }));
        let update = |c: &mut Controller, state| {
            updates.send(state).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            c.poll().unwrap();
        };
        update(&mut c, WatchState::Tracking);
        assert_eq!(status_label(&c.view), "Tracking old_project <&> (01:02:03)");
        store.record(&project, "3B00", "fake", 10002).unwrap();
        update(&mut c, WatchState::Tracking);
        assert_eq!(status_label(&c.view), "Tracking old_project <&> (01:02:05)");
        store.stop("removed", Some(10003)).unwrap();
        update(&mut c, WatchState::CardAbsent);
        for _ in 0..3 {
            c.poll().unwrap();
            assert_eq!(status_label(&c.view), "Paused old_project <&> (01:02:06)");
        }
        update(&mut c, WatchState::Reconnecting);
        assert_eq!(
            status_label(&c.view),
            "Reader unavailable — reconnecting old_project <&> (01:02:06)"
        );
        assert!(c.view.editable());
        store.record(&project, "3B00", "fake", 20000).unwrap();
        update(&mut c, WatchState::Tracking);
        assert_eq!(status_label(&c.view), "Tracking old_project <&> (01:02:06)");
        store.record(&project, "3B00", "fake", 20001).unwrap();
        update(&mut c, WatchState::Tracking);
        assert_eq!(status_label(&c.view), "Tracking old_project <&> (01:02:07)");
    }

    #[test]
    fn title_time_resets_on_selection_and_idle_does_not_create_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("unused.db");
        let mut c = controller(db.clone());
        c.view.settings.project.clear();
        c.projects().unwrap();
        assert_eq!(status_label(&c.view), "Select project");
        assert!(!db.exists());
        c.apply(Entry::Project, "new".into()).unwrap();
        assert_eq!(status_label(&c.view), "Waiting for card new (00:00:00)");
        c.view.status = Status::Watching(WatchState::Tracking);
        assert_eq!(status_label(&c.view), "Tracking new (00:00:00)");
        assert!(!db.exists());
        c.stop().unwrap();

        let mut store = store::Store::open(&db).unwrap();
        store.record("new", "3B00", "fake", 0).unwrap();
        store.stop("removed", Some(360000)).unwrap();
        c.projects().unwrap();
        c.view.status = Status::Watching(WatchState::CardAbsent);
        assert_eq!(status_label(&c.view), "Paused new (100:00:00)");
        c.apply(Entry::Project, "NEW".into()).unwrap();
        assert_eq!(c.view.project_seconds, 0);
        c.apply(Entry::Project, "new".into()).unwrap();
        assert_eq!(c.view.project_seconds, 360000);

        let other_db = dir.path().join("other.db");
        let mut other = store::Store::open(&other_db).unwrap();
        other.record("new", "3B00", "fake", 10).unwrap();
        other.stop("removed", Some(70)).unwrap();
        c.apply(Entry::Database, other_db.to_str().unwrap().into())
            .unwrap();
        assert_eq!(c.view.project_seconds, 60);
        c.view.status = Status::Watching(WatchState::CardAbsent);
        assert_eq!(status_label(&c.view), "Paused new (00:01:00)");
        let missing = dir.path().join("missing.db");
        c.apply(Entry::Database, missing.to_str().unwrap().into())
            .unwrap();
        assert_eq!(c.view.project_seconds, 0);
        assert!(!missing.exists());
    }

    #[test]
    fn selection_arms_watcher_and_switch_joins_before_mutation() {
        for state in [WatchState::CardAbsent, WatchState::Reconnecting] {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("clock.db");
            let mut c = controller(db.clone());
            let (ready, wait) = mpsc::channel();
            c.worker = Some(Worker::spawn(move |running, tx| {
                let mut store = store::Store::open(&db)?;
                let _lock = watcher_lock(&db)?;
                store.record("old_project <&>", "3B00", "fake", 10)?;
                store.stop("removed_or_changed", Some(15))?;
                tx.send(state)?;
                ready.send(())?;
                while running.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
                tx.send(WatchState::Tracking)?; // Must be discarded when joining the old worker.
                store.stop("shutdown", None)?;
                Ok(())
            }));
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            let id = c.worker.as_ref().unwrap().thread.thread().id();
            c.start_watching().unwrap();
            c.start_watching().unwrap();
            assert_eq!(id, c.worker.as_ref().unwrap().thread.thread().id());
            c.poll().unwrap();
            assert_eq!(c.view.status, Status::Watching(state));
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
            anyhow::bail!("close failed")
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
        anyhow::bail!("Fake dialog timed out")
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
    #[ignore = "requires GTK4, Zenity and a graphical session; briefly opens status/report windows"]
    fn status_and_report_without_card_or_active_project_opens_and_preserves_history() {
        for has_history in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("report.db");
            let mut c = controller(db.clone());
            c.view.settings.project.clear();
            c.action(Action::Status).unwrap();
            c.poll().unwrap();
            assert!(!db.exists());
            assert!(c.worker.is_none());
            let mut store = store::Store::open(&db).unwrap();
            if has_history {
                store.record("work", "3B00", "fake", 10).unwrap();
                store.stop("removed_or_changed", Some(15)).unwrap();
            }
            drop(store);
            let before = store::report_text(&store::report_rows(&db).unwrap());

            let (tx, rx) = mpsc::channel();
            c.view.tx = tx;
            ksni::Tray::activate(&mut c.view, 0, 0);
            c.action(rx.try_recv().unwrap()).unwrap();
            let window = c.status_window.as_ref().unwrap().window.clone();
            assert!(c.dialog.is_none());
            assert_eq!(
                c.status_window.as_ref().unwrap().label.text(),
                "Select project"
            );
            ksni::Tray::activate(&mut c.view, 0, 0);
            c.action(rx.try_recv().unwrap()).unwrap();
            assert_eq!(c.status_window.as_ref().unwrap().window, window);
            c.view.settings.project = "My_Project <&> 日本語".into();
            c.worker = Some(Worker::spawn(|running, _| {
                while running.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            }));
            let worker_id = c.worker.as_ref().unwrap().thread.thread().id();
            c.view.project_seconds = 7080;
            c.view.status = Status::Watching(WatchState::Tracking);
            c.poll().unwrap();
            assert_eq!(
                c.status_window.as_ref().unwrap().label.text(),
                "Tracking My_Project <&> 日本語 (01:58:00)"
            );
            c.view.project_seconds += 1;
            c.view.status = Status::Watching(WatchState::CardAbsent);
            for _ in 0..3 {
                c.poll().unwrap();
                assert_eq!(
                    c.status_window.as_ref().unwrap().label.text(),
                    "Paused My_Project <&> 日本語 (01:58:01)"
                );
            }
            window.close();
            c.poll().unwrap();
            assert!(!window.is_visible());
            c.action(Action::Status).unwrap();
            assert!(window.is_visible());
            assert_eq!(c.worker.as_ref().unwrap().thread.thread().id(), worker_id);
            assert_eq!(c.view.status, Status::Watching(WatchState::CardAbsent));
            c.stop().unwrap();
            c.view.status = Status::Error("Reader unavailable".into());
            c.poll().unwrap();
            assert_eq!(
                c.status_window.as_ref().unwrap().label.text(),
                "Error — Reader unavailable"
            );
            c.view.settings.project.clear();
            c.view.status = Status::Idle;
            c.action(Action::Report).unwrap();
            let dialog_id = c.dialog.as_ref().unwrap().child.id();
            c.action(Action::Report).unwrap();
            assert_eq!(c.dialog.as_ref().unwrap().child.id(), dialog_id);
            // Poll the real process so unsupported Zenity options cannot pass as a successful spawn.
            for _ in 0..100 {
                thread::sleep(Duration::from_millis(5));
                c.poll().unwrap();
                assert!(c.dialog.is_some(), "Report dialog exited unexpectedly");
            }
            assert!(c.worker.is_none());
            assert!(c.view.settings.project.is_empty());
            assert_eq!(c.view.status, Status::Idle);
            drop(c);
            assert!(!window.is_visible());
            assert_eq!(
                store::report_text(&store::report_rows(&db).unwrap()),
                before
            );
        }
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
    fn settings_ok_and_cancel_close_the_dialog() {
        for accepted in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut c = controller(dir.path().join("unused.db"));
            c.view.settings.reader = Some("old reader".into());
            c.start_watching().unwrap();
            let id = c.worker.as_ref().unwrap().thread.thread().id();
            let mut dialog = fake_dialog(if accepted {
                "printf 'new reader  \\n'"
            } else {
                "printf 'discarded reader\\n'; exit 1"
            });
            dialog.entry = Some(Entry::Reader);
            c.dialog = Some(dialog);
            for _ in 0..200 {
                c.poll().unwrap();
                if c.dialog.is_none() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            assert!(c.dialog.is_none());
            assert_eq!(
                c.view.settings.reader.as_deref(),
                Some(if accepted {
                    "new reader  "
                } else {
                    "old reader"
                })
            );
            let current_id = c.worker.as_ref().unwrap().thread.thread().id();
            if accepted {
                assert_ne!(id, current_id);
            } else {
                assert_eq!(id, current_id);
            }
            assert_eq!(c.view.settings.project, "old_project <&>");
            assert!(!c.view.settings.db.exists());
        }
    }

    struct Appearance(Arc<std::sync::atomic::AtomicU32>);
    #[zbus::interface(name = "org.freedesktop.portal.Settings")]
    impl Appearance {
        #[zbus(property, name = "version")]
        fn version(&self) -> u32 {
            1
        }

        fn read(
            &self,
            namespace: &str,
            key: &str,
        ) -> zbus::fdo::Result<zbus::zvariant::OwnedValue> {
            if namespace != APPEARANCE_NAMESPACE || key != COLOR_SCHEME_KEY {
                return Err(zbus::fdo::Error::InvalidArgs("Unknown setting".into()));
            }
            Ok(self.0.load(Ordering::SeqCst).into())
        }
    }

    #[test]
    #[ignore = "requires TIMECLOCK_ISOLATED_BUS=1, an isolated DBus and a graphical session"]
    fn isolated_theme_follows_portal_initial_value_changes_and_window_lifetime() {
        assert_eq!(std::env::var("TIMECLOCK_ISOLATED_BUS").as_deref(), Ok("1"));
        let scheme = Arc::new(std::sync::atomic::AtomicU32::new(1));
        let bus = zbus::blocking::connection::Builder::session()
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .serve_at(
                "/org/freedesktop/portal/desktop",
                Appearance(scheme.clone()),
            )
            .unwrap()
            .build()
            .unwrap();
        gtk4::init().unwrap();
        let settings = gtk4::Settings::default().unwrap();
        settings.set_gtk_application_prefer_dark_theme(false);
        settings.set_gtk_theme_name(Some("Adwaita"));
        settings.set_gtk_font_name(Some("Sans 10"));
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("unused.db");
        let mut c = controller(db.clone());
        c.action(Action::Status).unwrap();
        let wait_for_theme = |c: &mut Controller, dark| {
            for _ in 0..400 {
                c.poll().unwrap();
                if settings.is_gtk_application_prefer_dark_theme() == dark {
                    let titlebar = c.status_window.as_ref().unwrap().window.titlebar().unwrap();
                    let (minimum, natural, _, _) =
                        titlebar.measure(gtk4::Orientation::Vertical, -1);
                    assert_eq!((minimum, natural), (35, 35));
                    assert!(
                        titlebar
                            .downcast_ref::<gtk4::HeaderBar>()
                            .unwrap()
                            .shows_title_buttons()
                    );
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            panic!("Desktop theme did not change to dark={dark}");
        };
        wait_for_theme(&mut c, true);
        let change = |namespace: &str, key: &str, value: u32| {
            scheme.store(value, Ordering::SeqCst);
            bus.emit_signal(
                None::<&str>,
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Settings",
                "SettingChanged",
                &(namespace, key, zbus::zvariant::Value::from(value)),
            )
            .unwrap();
        };
        change(APPEARANCE_NAMESPACE, COLOR_SCHEME_KEY, 2);
        wait_for_theme(&mut c, false);
        c.status_window.as_ref().unwrap().window.close();
        change(APPEARANCE_NAMESPACE, COLOR_SCHEME_KEY, 1);
        wait_for_theme(&mut c, true);
        c.action(Action::Status).unwrap();
        assert!(settings.is_gtk_application_prefer_dark_theme());
        change(APPEARANCE_NAMESPACE, "unrelated", 2);
        change("org.example.unrelated", COLOR_SCHEME_KEY, 2);
        for _ in 0..20 {
            c.poll().unwrap();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(settings.is_gtk_application_prefer_dark_theme());
        change(APPEARANCE_NAMESPACE, COLOR_SCHEME_KEY, 0);
        wait_for_theme(&mut c, false);
        settings.set_gtk_font_name(Some("Sans 32"));
        let titlebar = c.status_window.as_ref().unwrap().window.titlebar().unwrap();
        assert!(titlebar.measure(gtk4::Orientation::Vertical, -1).0 > 35);
        settings.set_gtk_font_name(Some("Sans 10"));
        assert!(c.worker.is_none());
        assert!(!db.exists());
        drop(c);
        change(APPEARANCE_NAMESPACE, COLOR_SCHEME_KEY, 1);
        let context = glib::MainContext::default();
        for _ in 0..20 {
            while context.pending() {
                context.iteration(false);
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!settings.is_gtk_application_prefer_dark_theme());
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
        c.view.settings.project.clear();
        let (actions, received) = mpsc::channel();
        c.view.tx = actions;
        c.projects().unwrap();
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
                .contains("Select project")
        );
        assert!(!item.get_property::<bool>("ItemIsMenu").unwrap());
        let menu: zbus::zvariant::OwnedObjectPath = item.get_property("Menu").unwrap();
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
        // Hosts render right-click menus; requesting one must not open an app window.
        let context_menu: zbus::Result<()> = item.call("ContextMenu", &(0i32, 0i32));
        assert!(matches!(
            context_menu,
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod"
        ));
        let needs_update: bool = menu_proxy.call("AboutToShow", &(0i32,)).unwrap();
        assert!(!needs_update);
        let (_, idle_layout): (u32, Layout) = menu_proxy
            .call("GetLayout", &(0i32, -1i32, Vec::<&str>::new()))
            .unwrap();
        assert!(!idle_layout.2.is_empty());
        for event in ["opened", "closed"] {
            let _: () = menu_proxy
                .call(
                    "Event",
                    &(0i32, event, zbus::zvariant::Value::from(0i32), 0u32),
                )
                .unwrap();
            handle.update(|tray| *tray = c.view.clone());
        }
        c.poll().unwrap();
        assert!(matches!(
            received.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert_eq!(c.view.status, Status::Idle);
        assert!(c.view.settings.project.is_empty());
        assert!(c.worker.is_none());
        assert!(c.status_window.is_none());
        assert!(c.dialog.is_none());
        assert!(!c.view.settings.db.exists());

        c.view.settings.project = "old_project <&>".into();
        c.projects().unwrap();
        handle.update(|tray| *tray = c.view.clone());
        let _: () = item.call("Activate", &(0i32, 0i32)).unwrap();
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            Action::Status
        ));
        let icons: Vec<(i32, i32, Vec<u8>)> = item.get_property("IconPixmap").unwrap();
        assert_eq!(icons[0].2.len(), 32 * 32 * 4);
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
        assert!(!text.contains("Keyboard controls"));
        for label in [
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
