use super::*;

pub(super) struct EntryWindow {
    pub(super) window: gtk4::Dialog,
    input: gtk4::Entry,
    hint: gtk4::Label,
}

impl EntryWindow {
    pub(super) fn new(
        entry: Entry,
        settings: &Settings,
        id: u64,
        tx: Sender<Action>,
    ) -> Result<Self> {
        gtk4::init().context("Cannot open entry dialog; run in a graphical session")?;
        let (title, label, value, hint) = match entry {
            Entry::Project => (
                "New project",
                "_Project name",
                settings.project.clone(),
                "Project names are exact and case-sensitive.",
            ),
            Entry::Database => (
                "Database path",
                "_Database path",
                settings.db.to_string_lossy().into_owned(),
                "Use an existing database or a path in an existing directory.",
            ),
            Entry::Reader => (
                "Reader",
                "_Exact reader name",
                settings.reader.clone().unwrap_or_default(),
                "Leave empty for automatic selection (one reader only). Use Scan readers for exact names.",
            ),
            Entry::Atr => (
                "Card-type ATR",
                "_ATR",
                format_atr(&settings.atr),
                "ATR identifies a card type, not a unique card. Accepts spaced, compact or colon-separated hex.",
            ),
        };
        let window = gtk4::Dialog::builder()
            .title(title)
            // Keep responses in the action area beneath our shared compact header.
            .use_header_bar(0)
            .default_width(480)
            .build();
        window.set_titlebar(Some(&compact_header()));
        window.add_button("_Cancel", gtk4::ResponseType::Cancel);
        window.add_button("_OK", gtk4::ResponseType::Ok);
        window.set_default_response(gtk4::ResponseType::Ok);
        let input = gtk4::Entry::builder()
            .text(value)
            .activates_default(true)
            .hexpand(true)
            .build();
        let label = gtk4::Label::builder()
            .label(label)
            .use_underline(true)
            .xalign(0.0)
            .mnemonic_widget(&input)
            .build();
        let help = gtk4::Label::builder()
            .label(hint)
            .wrap(true)
            .xalign(0.0)
            .build();
        let hint = gtk4::Label::builder().wrap(true).xalign(0.0).build();
        let content = window.content_area();
        content.set_spacing(12);
        content.set_margin_top(16);
        content.set_margin_bottom(16);
        content.set_margin_start(16);
        content.set_margin_end(16);
        content.append(&label);
        content.append(&input);
        content.append(&help);
        content.append(&hint);
        let responded = Rc::new(Cell::new(false));
        let done = responded.clone();
        let value = input.clone();
        let response_tx = tx.clone();
        window.connect_response(move |window, response| {
            if !done.replace(true) {
                let text = (response == gtk4::ResponseType::Ok).then(|| value.text().to_string());
                let _ = response_tx.send(Action::EntryResponse(id, text));
                window.set_visible(false);
            }
        });
        window.connect_close_request(move |_| {
            if !responded.replace(true) {
                let _ = tx.send(Action::EntryResponse(id, None));
            }
            glib::Propagation::Proceed
        });
        input.grab_focus();
        input.select_region(0, -1);
        Ok(Self {
            window,
            input,
            hint,
        })
    }

    pub(super) fn update(&self, editable: bool) {
        self.input.set_sensitive(editable);
        self.window
            .set_response_sensitive(gtk4::ResponseType::Ok, editable);
        let text = if editable {
            ""
        } else {
            "Remove the target card before changing the project or settings."
        };
        if self.hint.text() != text {
            self.hint.set_text(text);
        }
    }
}

impl Drop for EntryWindow {
    fn drop(&mut self) {
        self.window.destroy();
    }
}

pub(super) struct ScanWindow {
    pub(super) window: gtk4::Window,
    text: gtk4::Label,
    status: gtk4::Label,
    refresh: gtk4::Button,
}

impl ScanWindow {
    pub(super) fn new(tx: Sender<Action>) -> Result<Self> {
        gtk4::init().context("Cannot open reader scan; run in a graphical session")?;
        let text = gtk4::Label::builder()
            .selectable(true)
            .xalign(0.0)
            .yalign(0.0)
            .build();
        text.update_property(&[gtk4::accessible::Property::Description(
            "Reader names, card presence and ATRs",
        )]);
        let status = gtk4::Label::builder()
            .wrap(true)
            .selectable(true)
            .xalign(0.0)
            .build();
        let refresh = gtk4::Button::with_mnemonic("_Refresh");
        refresh.connect_clicked(move |_| {
            let _ = tx.send(Action::RefreshScan);
        });
        let scroll = gtk4::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&text)
            .build();
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        content.set_margin_top(16);
        content.set_margin_bottom(16);
        content.set_margin_start(16);
        content.set_margin_end(16);
        content.append(&scroll);
        content.append(&status);
        content.append(&refresh);
        let window = gtk4::Window::builder()
            .title("Scan readers")
            .default_width(600)
            .default_height(360)
            .hide_on_close(true)
            .child(&content)
            .build();
        window.set_titlebar(Some(&compact_header()));
        refresh.grab_focus();
        Ok(Self {
            window,
            text,
            status,
            refresh,
        })
    }

    pub(super) fn scanning(&self) {
        self.refresh.set_sensitive(false);
        self.status.remove_css_class("error");
        self.status.set_text("Scanning readers…");
    }

    pub(super) fn finish(&self, result: &Result<String>) {
        self.refresh.set_sensitive(true);
        match result {
            Ok(text) => {
                self.text.set_text(text);
                self.status.remove_css_class("error");
                self.status.set_text(
                    "Snapshot updated. Refresh after inserting a card or changing readers.",
                );
            }
            Err(error) => {
                self.status.add_css_class("error");
                self.status.set_text(&format!(
                    "Scan failed: {error:#}. Refresh to retry. Any previous snapshot is unchanged."
                ));
            }
        }
    }
}

impl Drop for ScanWindow {
    fn drop(&mut self) {
        self.window.destroy();
    }
}

pub(super) struct ErrorWindow(pub(super) gtk4::Window);
impl ErrorWindow {
    pub(super) fn new(text: &str) -> Self {
        let label = gtk4::Label::builder()
            .label(text)
            .wrap(true)
            .selectable(true)
            .xalign(0.0)
            .margin_top(16)
            .margin_bottom(16)
            .margin_start(16)
            .margin_end(16)
            .build();
        let scroll = gtk4::ScrolledWindow::builder().child(&label).build();
        let window = gtk4::Window::builder()
            .title("Timeclock error")
            .default_width(480)
            .default_height(180)
            .hide_on_close(true)
            .child(&scroll)
            .build();
        window.set_titlebar(Some(&compact_header()));
        window.present();
        Self(window)
    }
}
impl Drop for ErrorWindow {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a real graphical session; uses temporary data only"]
    fn gtk_entries_scan_and_errors_service_events_without_status_or_reports() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("unused.sqlite3");
        let mut c = super::super::tests::controller(db.clone());
        c.view.settings.project.clear();
        let (tx, rx) = mpsc::channel();
        c.view.tx = tx;
        c.action(Action::Entry(Entry::Reader)).unwrap();
        assert!(c.theme.is_some());
        assert!(c.status_window.is_none());
        assert!(c.reports_window.is_none());
        let serviced = Rc::new(Cell::new(false));
        let observed = serviced.clone();
        glib::idle_add_local_once(move || observed.set(true));
        c.poll().unwrap();
        assert!(serviced.get());
        let entry = c.entry_window.as_ref().unwrap();
        entry.input.set_text(" exact_reader 项目 '<&>  ");
        c.view.status = Status::Watching(WatchState::Tracking);
        c.poll().unwrap();
        let entry = c.entry_window.as_ref().unwrap();
        assert!(!entry.input.is_sensitive());
        assert!(
            !entry
                .window
                .widget_for_response(gtk4::ResponseType::Ok)
                .unwrap()
                .is_sensitive()
        );
        c.view.status = Status::Idle;
        c.poll().unwrap();
        let entry = c.entry_window.as_ref().unwrap();
        assert!(entry.input.is_sensitive());
        entry.window.response(gtk4::ResponseType::Ok);
        entry.window.response(gtk4::ResponseType::Ok);
        c.action(rx.try_recv().unwrap()).unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(
            c.view.settings.reader.as_deref(),
            Some(" exact_reader 项目 '<&>  ")
        );
        assert!(c.entry_window.is_none());
        assert!(!c.view.dialog_open);
        for response in [gtk4::ResponseType::Cancel, gtk4::ResponseType::DeleteEvent] {
            c.action(Action::Entry(Entry::Project)).unwrap();
            c.entry_window
                .as_ref()
                .unwrap()
                .input
                .set_text("must not apply");
            c.entry_window.as_ref().unwrap().window.response(response);
            c.action(rx.try_recv().unwrap()).unwrap();
            assert!(c.view.settings.project.is_empty());
            assert!(c.worker.is_none());
        }
        c.action(Action::Entry(Entry::Database)).unwrap();
        assert_eq!(
            c.entry_window.as_ref().unwrap().input.text(),
            db.to_string_lossy().as_ref()
        );
        c.entry_window
            .as_ref()
            .unwrap()
            .window
            .response(gtk4::ResponseType::Cancel);
        c.action(rx.try_recv().unwrap()).unwrap();
        c.action(Action::Entry(Entry::Atr)).unwrap();
        c.entry_window.as_ref().unwrap().window.close();
        c.poll().unwrap();
        c.action(rx.try_recv().unwrap()).unwrap();
        assert!(c.entry_window.is_none());
        assert!(!db.exists());
        drop(c);

        // Scan first must also initialize the shared theme and service GTK independently.
        let mut c = super::super::tests::controller(db.clone());
        let (tx, rx) = mpsc::channel();
        c.view.tx = tx;
        c.scan = || anyhow::bail!("test reader unavailable");
        c.action(Action::Scan).unwrap();
        assert!(c.theme.is_some());
        assert!(c.status_window.is_none() && c.reports_window.is_none());
        let serviced = Rc::new(Cell::new(false));
        let observed = serviced.clone();
        glib::idle_add_local_once(move || observed.set(true));
        while !c.scan_worker.as_ref().unwrap().is_finished() {
            thread::yield_now();
        }
        assert!(c.poll().is_err());
        assert!(serviced.get());
        assert!(
            c.scan_window
                .as_ref()
                .unwrap()
                .status
                .text()
                .contains("test reader unavailable")
        );
        assert!(c.scan_window.as_ref().unwrap().refresh.is_sensitive());
        c.scan = || Ok("exact_reader 项目 <&>\nATR: 3B00".into());
        c.scan_window.as_ref().unwrap().refresh.emit_clicked();
        c.action(rx.try_recv().unwrap()).unwrap();
        while !c.scan_worker.as_ref().unwrap().is_finished() {
            thread::yield_now();
        }
        c.poll().unwrap();
        assert_eq!(
            c.scan_window.as_ref().unwrap().text.text(),
            "exact_reader 项目 <&>\nATR: 3B00"
        );
        let window = c.scan_window.as_ref().unwrap().window.clone();
        window.close();
        c.poll().unwrap();
        c.action(Action::Scan).unwrap();
        assert_eq!(c.scan_window.as_ref().unwrap().window, window);
        assert!(c.scan_worker.is_none());
        c.error(anyhow::anyhow!("literal error <&> 项目"));
        assert!(c.error_window.as_ref().unwrap().0.is_visible());
        c.scan_window.take();
        let serviced = Rc::new(Cell::new(false));
        let observed = serviced.clone();
        glib::idle_add_local_once(move || observed.set(true));
        c.poll().unwrap();
        assert!(serviced.get());
        assert!(c.worker.is_none());
        drop(c);
        assert!(!window.is_visible());
        assert!(!db.exists());
    }
}
