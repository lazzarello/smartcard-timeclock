use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProjectSummary {
    pub(super) project: String,
    pub(super) seconds: i64,
    pub(super) sessions: i64,
    pub(super) open: i64,
    pub(super) archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProjectTarget {
    pub(super) db: PathBuf,
    pub(super) revision: u64,
    pub(super) project: String,
}

struct ProjectWidgets {
    project: String,
    time: gtk4::Label,
    sessions: gtk4::Label,
    open: gtk4::Label,
    archived: gtk4::CheckButton,
    saved_archive: Rc<Cell<bool>>,
    delete: gtk4::Button,
}

pub(super) struct ReportsWindow {
    pub(super) window: gtk4::Window,
    database: gtk4::Label,
    grid: gtk4::Grid,
    hint: gtk4::Label,
    error: gtk4::Label,
    rows: Vec<ProjectWidgets>,
    db: PathBuf,
    revision: Rc<Cell<u64>>,
    updating: Rc<Cell<bool>>,
    confirmation: Option<gtk4::Dialog>,
}

impl ReportsWindow {
    pub(super) fn new(tx: Sender<Action>) -> Result<Self> {
        gtk4::init().context("Cannot open reports; run in a graphical session")?;
        let database = gtk4::Label::builder()
            .xalign(0.0)
            .selectable(true)
            .wrap(true)
            .build();
        let grid = gtk4::Grid::builder()
            .column_spacing(20)
            .row_spacing(10)
            .build();
        let scroll = gtk4::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&grid)
            .build();
        let hint = gtk4::Label::builder().xalign(0.0).wrap(true).build();
        let error = gtk4::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .build();
        error.add_css_class("error");
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        content.set_margin_top(16);
        content.set_margin_bottom(16);
        content.set_margin_start(16);
        content.set_margin_end(16);
        content.append(&database);
        content.append(&scroll);
        content.append(&hint);
        content.append(&error);
        let window = gtk4::Window::builder()
            .title("Timeclock reports")
            .default_width(760)
            .default_height(420)
            .hide_on_close(true)
            .child(&content)
            .build();
        window.set_titlebar(Some(&compact_header()));
        window.connect_close_request(move |_| {
            let _ = tx.send(Action::CancelDelete);
            glib::Propagation::Proceed
        });
        Ok(Self {
            window,
            database,
            grid,
            hint,
            error,
            rows: Vec::new(),
            db: PathBuf::new(),
            revision: Rc::new(Cell::new(0)),
            updating: Rc::new(Cell::new(false)),
            confirmation: None,
        })
    }

    pub(super) fn update(&mut self, view: &Tray, revision: u64, projects: &[ProjectSummary]) {
        self.database.set_text(&view.settings.db.to_string_lossy());
        if self.db != view.settings.db
            || self
                .rows
                .iter()
                .map(|row| &row.project)
                .ne(projects.iter().map(|row| &row.project))
        {
            self.db = view.settings.db.clone();
            // Removed widgets must retain their old target, even after switching away and back.
            self.revision = Rc::new(Cell::new(revision));
            self.close_confirmation();
            self.rows.clear();
            while let Some(child) = self.grid.first_child() {
                self.grid.remove(&child);
            }
            for (column, heading) in [
                "Project",
                "Time (HH:MM:SS)",
                "Sessions",
                "Open",
                "Archived",
                "",
            ]
            .iter()
            .enumerate()
            {
                let label = gtk4::Label::new(Some(heading));
                label.set_xalign(0.0);
                label.add_css_class("heading");
                self.grid.attach(&label, column as i32, 0, 1, 1);
            }
            for (index, summary) in projects.iter().enumerate() {
                let name = gtk4::Label::builder()
                    .label(&summary.project)
                    .selectable(true)
                    .xalign(0.0)
                    .hexpand(true)
                    .wrap(true)
                    .build();
                let time = gtk4::Label::new(None);
                let sessions = gtk4::Label::new(None);
                let open = gtk4::Label::new(None);
                let archived = gtk4::CheckButton::new();
                let archive_label = format!("Archived — {}", summary.project);
                archived.update_property(&[gtk4::accessible::Property::Label(&archive_label)]);
                archived.set_tooltip_text(Some(&archive_label));
                let saved_archive = Rc::new(Cell::new(summary.archived));
                let saved = saved_archive.clone();
                let updating = self.updating.clone();
                let revision = self.revision.clone();
                let db = self.db.clone();
                let project = summary.project.clone();
                let tx = view.tx.clone();
                archived.connect_toggled(move |button| {
                    if updating.get() {
                        return;
                    }
                    let desired = button.is_active();
                    // The displayed value is committed state, never an optimistic write.
                    updating.set(true);
                    button.set_active(saved.get());
                    updating.set(false);
                    let _ = tx.send(Action::Archive(
                        ProjectTarget {
                            db: db.clone(),
                            revision: revision.get(),
                            project: project.clone(),
                        },
                        desired,
                    ));
                });
                let delete = gtk4::Button::with_label("Delete…");
                let delete_label =
                    format!("Delete {} permanently (archive first)", summary.project);
                delete.set_tooltip_text(Some(&delete_label));
                delete.update_property(&[gtk4::accessible::Property::Label(&delete_label)]);
                let revision = self.revision.clone();
                let db = self.db.clone();
                let project = summary.project.clone();
                let tx = view.tx.clone();
                delete.connect_clicked(move |_| {
                    let _ = tx.send(Action::RequestDelete(ProjectTarget {
                        db: db.clone(),
                        revision: revision.get(),
                        project: project.clone(),
                    }));
                });
                let row = index as i32 + 1;
                self.grid.attach(&name, 0, row, 1, 1);
                self.grid.attach(&time, 1, row, 1, 1);
                self.grid.attach(&sessions, 2, row, 1, 1);
                self.grid.attach(&open, 3, row, 1, 1);
                self.grid.attach(&archived, 4, row, 1, 1);
                self.grid.attach(&delete, 5, row, 1, 1);
                self.rows.push(ProjectWidgets {
                    project: summary.project.clone(),
                    time,
                    sessions,
                    open,
                    archived,
                    saved_archive,
                    delete,
                });
            }
        }
        self.revision.set(revision);
        self.updating.set(true);
        for (widgets, summary) in self.rows.iter().zip(projects) {
            widgets
                .time
                .set_text(&store::format_duration(summary.seconds));
            widgets.sessions.set_text(&summary.sessions.to_string());
            widgets.open.set_text(&summary.open.to_string());
            widgets.saved_archive.set(summary.archived);
            widgets.archived.set_active(summary.archived);
            widgets.archived.set_sensitive(view.editable());
            widgets
                .delete
                .set_sensitive(view.editable() && summary.archived);
        }
        self.updating.set(false);
        if let Some(dialog) = &self.confirmation
            && let Some(delete) = dialog.widget_for_response(gtk4::ResponseType::Accept)
        {
            delete.set_sensitive(view.editable());
        }
        self.hint.set_text(if !view.editable() {
            "Remove the target card before changing projects. Reports remain available while tracking."
        } else if projects.is_empty() {
            "No projects yet. Select New project… from the tray to get started."
        } else {
            "Archived projects remain in reports but cannot track time. Archive a project before deleting it."
        });
    }

    pub(super) fn show_error(&self, message: &str) {
        self.error.set_text(message);
    }

    pub(super) fn confirm_delete(
        &mut self,
        target: ProjectTarget,
        summary: &ProjectSummary,
        tx: Sender<Action>,
    ) {
        self.close_confirmation();
        let dialog = gtk4::Dialog::builder()
            .title("Delete project permanently?")
            .transient_for(&self.window)
            .modal(true)
            .destroy_with_parent(true)
            .build();
        let text = gtk4::Label::builder()
            .label(format!("Permanently delete “{}”?\n\nThis will delete the project and all {} sessions, totaling {}.\nThis cannot be undone.",
                summary.project, summary.sessions, store::format_duration(summary.seconds)))
            .wrap(true).selectable(true).margin_top(16).margin_bottom(16)
            .margin_start(16).margin_end(16).build();
        dialog.content_area().append(&text);
        dialog.add_button("Cancel", gtk4::ResponseType::Cancel);
        let delete = dialog.add_button("Delete permanently", gtk4::ResponseType::Accept);
        delete.add_css_class("destructive-action");
        dialog.set_default_response(gtk4::ResponseType::Cancel);
        if let Some(cancel) = dialog.widget_for_response(gtk4::ResponseType::Cancel) {
            cancel.grab_focus();
        }
        dialog.connect_response(move |dialog, response| {
            let _ = tx.send(Action::DeleteResponse(
                target.clone(),
                response == gtk4::ResponseType::Accept,
            ));
            dialog.hide();
        });
        dialog.present();
        self.confirmation = Some(dialog);
    }

    pub(super) fn close_confirmation(&mut self) {
        if let Some(dialog) = self.confirmation.take() {
            dialog.destroy();
        }
    }
}

impl Drop for ReportsWindow {
    fn drop(&mut self) {
        self.close_confirmation();
        self.window.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires GTK4 and a graphical session; opens reports and delete confirmations"]
    fn reports_archive_restore_delete_and_independent_window_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("reports.db");
        let mut c = super::super::tests::controller(db.clone());
        c.view.settings.project.clear();
        let (tx, rx) = mpsc::channel();
        c.view.tx = tx;
        c.action(Action::Report).unwrap();
        c.poll().unwrap();
        assert!(c.status_window.is_none());
        assert!(c.worker.is_none());
        assert!(!db.exists());
        let window = c.reports_window.as_ref().unwrap().window.clone();
        c.action(Action::Report).unwrap();
        assert_eq!(c.reports_window.as_ref().unwrap().window, window);

        let project = "Client_项目 '<&>";
        let mut store = store::Store::open(&db).unwrap();
        store.record(project, "3B00", "fake", 10).unwrap();
        store.stop("removed", Some(15)).unwrap();
        store.create_project("zero sessions").unwrap();
        drop(store);
        c.projects().unwrap();
        let archive = c.reports_window.as_ref().unwrap().rows[0].archived.clone();
        archive.grab_focus();
        c.poll().unwrap();
        assert_eq!(c.reports_window.as_ref().unwrap().rows[0].archived, archive);
        archive.set_active(true);
        assert!(
            !archive.is_active(),
            "Do not display an uncommitted archive state"
        );
        c.action(rx.try_recv().unwrap()).unwrap();
        assert!(archive.is_active());
        assert!(!c.view.projects.contains(&project.into()));
        assert_eq!(c.reports_window.as_ref().unwrap().rows.len(), 2);
        assert_eq!(
            c.reports_window.as_ref().unwrap().rows[0].time.text(),
            "00:00:05"
        );

        // Failure leaves the committed checkbox value and reports an actionable error.
        let store = store::Store::open(&db).unwrap();
        archive.set_active(false);
        let error = c.action(rx.try_recv().unwrap()).unwrap_err();
        c.error(error);
        assert!(archive.is_active());
        assert!(!c.reports_window.as_ref().unwrap().error.text().is_empty());
        drop(store);
        archive.set_active(false);
        c.action(rx.try_recv().unwrap()).unwrap();
        assert!(!archive.is_active());
        assert!(c.view.projects.contains(&project.into()));
        archive.set_active(true);
        c.action(rx.try_recv().unwrap()).unwrap();

        c.reports_window.as_ref().unwrap().rows[0]
            .delete
            .emit_clicked();
        c.action(rx.try_recv().unwrap()).unwrap();
        let stale_confirmation = c.pending_delete.clone().unwrap();
        let other = dir.path().join("other.db");
        c.apply(Entry::Database, other.to_str().unwrap().into())
            .unwrap();
        assert!(c.pending_delete.is_none());
        assert!(c.reports_window.as_ref().unwrap().confirmation.is_none());
        assert!(!other.exists());
        c.apply(Entry::Database, db.to_str().unwrap().into())
            .unwrap();
        assert!(
            c.action(Action::DeleteResponse(stale_confirmation, true))
                .is_err()
        );
        // Even an old widget retained across an away-and-back switch carries an expired target.
        archive.set_active(false);
        assert!(c.action(rx.try_recv().unwrap()).is_err());
        let archive = c.reports_window.as_ref().unwrap().rows[0].archived.clone();
        assert!(archive.is_active());
        c.view.status = Status::Watching(WatchState::Tracking);
        c.poll().unwrap();
        assert!(!archive.is_sensitive());
        assert!(
            !c.reports_window.as_ref().unwrap().rows[0]
                .delete
                .is_sensitive()
        );
        c.view.status = Status::Idle;
        c.poll().unwrap();
        for response in [gtk4::ResponseType::Cancel, gtk4::ResponseType::Accept] {
            c.reports_window.as_ref().unwrap().rows[0]
                .delete
                .emit_clicked();
            c.action(rx.try_recv().unwrap()).unwrap();
            let dialog = c
                .reports_window
                .as_ref()
                .unwrap()
                .confirmation
                .as_ref()
                .unwrap()
                .clone();
            assert!(dialog.is_modal());
            c.view.status = Status::Watching(WatchState::Tracking);
            c.poll().unwrap();
            assert!(
                !dialog
                    .widget_for_response(gtk4::ResponseType::Accept)
                    .unwrap()
                    .is_sensitive()
            );
            c.view.status = Status::Idle;
            c.poll().unwrap();
            assert!(
                dialog
                    .widget_for_response(gtk4::ResponseType::Accept)
                    .unwrap()
                    .is_sensitive()
            );
            dialog.response(response);
            c.action(rx.try_recv().unwrap()).unwrap();
            assert!(c.pending_delete.is_none());
            if response == gtk4::ResponseType::Cancel {
                assert_eq!(c.project_rows.len(), 2);
            }
        }
        assert_eq!(c.project_rows.len(), 1);
        assert_eq!(c.project_rows[0].project, "zero sessions");
        assert!(store::report_rows(&db).unwrap().is_empty());
        window.close();
        c.action(rx.try_recv().unwrap()).unwrap();
        c.poll().unwrap();
        assert!(!window.is_visible());
        c.action(Action::Report).unwrap();
        assert!(window.is_visible());
        assert!(c.worker.is_none());
        drop(c);
        assert!(!window.is_visible());
    }
}
