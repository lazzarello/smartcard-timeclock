use std::{path::Path, time::Duration};

use anyhow::{Result, ensure};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};

// The lock covers initialization, migration, and every write for this connection's lifetime.
pub struct Store(Connection, #[allow(dead_code)] Option<crate::DatabaseLock>);

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let lock = if path == Path::new(":memory:") {
            None
        } else {
            Some(crate::watcher_lock(path)?)
        };
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            matches!(version, 0 | 1),
            "Unsupported database schema version {version}"
        );
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
        if version == 0 {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS sessions (
                     id INTEGER PRIMARY KEY,
                     project TEXT NOT NULL CHECK(length(trim(project)) > 0),
                     atr TEXT NOT NULL,
                     reader TEXT NOT NULL,
                     started_at INTEGER NOT NULL,
                     last_seen_at INTEGER NOT NULL CHECK(last_seen_at >= started_at),
                     ended_at INTEGER CHECK(ended_at >= last_seen_at),
                     stop_reason TEXT,
                     CHECK((ended_at IS NULL) = (stop_reason IS NULL))
                 );
                 CREATE TABLE projects (
                     name TEXT NOT NULL PRIMARY KEY COLLATE BINARY
                         CHECK(length(trim(name)) > 0),
                     archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0, 1))
                 );
                 INSERT INTO projects (name) SELECT DISTINCT project FROM sessions;
                 CREATE TABLE sessions_v1 (
                     id INTEGER PRIMARY KEY,
                     project TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE
                         CHECK(length(trim(project)) > 0),
                     atr TEXT NOT NULL,
                     reader TEXT NOT NULL,
                     started_at INTEGER NOT NULL,
                     last_seen_at INTEGER NOT NULL CHECK(last_seen_at >= started_at),
                     ended_at INTEGER CHECK(ended_at >= last_seen_at),
                     stop_reason TEXT,
                     CHECK((ended_at IS NULL) = (stop_reason IS NULL))
                 );
                 INSERT INTO sessions_v1
                     (id, project, atr, reader, started_at, last_seen_at, ended_at, stop_reason)
                 SELECT id, project, atr, reader, started_at, last_seen_at, ended_at, stop_reason
                     FROM sessions;
                 DROP TABLE sessions;
                 ALTER TABLE sessions_v1 RENAME TO sessions;
                 CREATE UNIQUE INDEX one_open_session
                     ON sessions ((1)) WHERE ended_at IS NULL;",
            )?;
            ensure!(
                !tx.prepare("PRAGMA foreign_key_check")?.exists([])?,
                "Foreign key violation during migration"
            );
            tx.pragma_update(None, "user_version", 1)?;
            tx.commit()?;
        }
        Ok(Self(conn, lock))
    }

    /// Register a new active project, or validate an existing active project.
    pub fn create_project(&mut self, project: &str) -> Result<()> {
        let tx = self.0.transaction()?;
        ensure_active_project(&tx, project)?;
        tx.commit()?;
        Ok(())
    }

    pub fn archive_project(&mut self, project: &str) -> Result<()> {
        self.set_archived(project, true)
    }

    pub fn unarchive_project(&mut self, project: &str) -> Result<()> {
        self.set_archived(project, false)
    }

    fn set_archived(&mut self, project: &str, archived: bool) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_lifecycle(&tx, project)?;
        tx.execute(
            "UPDATE projects SET archived = ?2 WHERE name = ?1",
            params![project, archived],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_project(&mut self, project: &str) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_lifecycle(&tx, project)?;
        ensure!(
            tx.execute(
                "DELETE FROM projects WHERE name = ?1 AND archived = 1",
                [project]
            )? == 1,
            "Project {project:?} must be archived before deletion"
        );
        tx.commit()?;
        Ok(())
    }

    pub fn recover(&self) -> Result<usize> {
        Ok(self.0.execute(
            "UPDATE sessions SET ended_at = last_seen_at, stop_reason = 'interrupted'
             WHERE ended_at IS NULL",
            [],
        )?)
    }

    // Returns true only when this observation starts a new session.
    pub fn record(&mut self, project: &str, atr: &str, reader: &str, now: i64) -> Result<bool> {
        let tx = self.0.transaction()?;
        ensure_active_project(&tx, project)?;
        ensure!(
            !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE ended_at IS NULL AND project != ?1)",
                [project],
                |row| row.get::<_, bool>(0),
            )?,
            "Another project already has an open session"
        );
        let started = tx.execute(
            "INSERT INTO sessions (project, atr, reader, started_at, last_seen_at)
             SELECT ?1, ?2, ?3, ?4, ?4
             WHERE NOT EXISTS (SELECT 1 FROM sessions WHERE ended_at IS NULL)",
            params![project, atr, reader, now],
        )? > 0;
        tx.execute(
            "UPDATE sessions SET last_seen_at = MAX(last_seen_at, ?1)
             WHERE ended_at IS NULL",
            [now],
        )?;
        tx.commit()?;
        Ok(started)
    }

    pub fn stop(&self, reason: &str, now: Option<i64>) -> Result<bool> {
        Ok(self.0.execute(
            "UPDATE sessions
             SET ended_at = MAX(last_seen_at, COALESCE(?1, last_seen_at)), stop_reason = ?2
             WHERE ended_at IS NULL",
            params![now, reason],
        )? > 0)
    }
}

fn ensure_active_project(conn: &Connection, project: &str) -> Result<()> {
    ensure!(!project.trim().is_empty(), "Project name cannot be blank");
    conn.execute(
        "INSERT INTO projects (name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
        [project],
    )?;
    let archived: bool = conn.query_row(
        "SELECT archived FROM projects WHERE name = ?1",
        [project],
        |row| row.get(0),
    )?;
    ensure!(
        !archived,
        "Project {project:?} is archived; restore it before tracking"
    );
    Ok(())
}

fn validate_lifecycle(conn: &Connection, project: &str) -> Result<()> {
    ensure!(
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE name = ?1)",
            [project],
            |row| row.get::<_, bool>(0)
        )?,
        "Project {project:?} does not exist"
    );
    ensure!(
        !conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE project = ?1 AND ended_at IS NULL)",
            [project],
            |row| row.get::<_, bool>(0)
        )?,
        "Project {project:?} has an open session"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub archived: bool,
}

/// Read-only discovery, including zero-session projects; legacy projects are all active.
pub fn project_rows(path: &Path) -> Result<Vec<Project>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    ensure!(
        matches!(version, 0 | 1),
        "Unsupported database schema version {version}"
    );
    let sql = if version == 0 {
        "SELECT DISTINCT project, 0 FROM sessions ORDER BY project"
    } else {
        "SELECT name, archived FROM projects ORDER BY name"
    };
    let mut query = conn.prepare(sql)?;
    query
        .query_map([], |row| {
            Ok(Project {
                name: row.get(0)?,
                archived: row.get(1)?,
            })
        })?
        .map(|row| Ok(row?))
        .collect()
}

pub struct ReportRow {
    pub project: String,
    pub seconds: i64,
    pub sessions: i64,
    pub open: i64,
}

pub fn report_rows(path: &Path) -> Result<Vec<ReportRow>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let mut query = conn.prepare(
        "SELECT project, SUM(COALESCE(ended_at, last_seen_at) - started_at),
                COUNT(*), SUM(ended_at IS NULL)
         FROM sessions GROUP BY project ORDER BY project",
    )?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    rows.map(|row| {
        let (project, seconds, sessions, open) = row?;
        Ok(ReportRow {
            project,
            seconds,
            sessions,
            open,
        })
    })
    .collect()
}

pub fn format_duration(seconds: i64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

pub fn report_text(rows: &[ReportRow]) -> String {
    use std::fmt::Write;
    let project_width = rows
        .iter()
        .map(|row| row.project.chars().count())
        .max()
        .unwrap_or(0)
        .max("PROJECT".len());
    let mut text = format!(
        "{:<project_width$}  TIME (HH:MM:SS)  SESSIONS  OPEN\n",
        "PROJECT"
    );
    for ReportRow {
        project,
        seconds,
        sessions,
        open,
    } in rows
    {
        writeln!(
            text,
            "{project:<project_width$}  {:>14}  {sessions:>8}  {open:>4}",
            format_duration(*seconds)
        )
        .unwrap();
    }
    text
}

pub fn report(path: &Path) -> Result<()> {
    print!("{}", report_text(&report_rows(path)?));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }

    fn rows(store: &Store) -> Vec<(i64, i64, Option<i64>, Option<String>)> {
        store
            .0
            .prepare(
                "SELECT started_at, last_seen_at, ended_at, stop_reason FROM sessions ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn insertion_heartbeat_removal_and_reinsertion() {
        let mut store = memory();
        assert!(!store.stop("removed", Some(9)).unwrap());
        assert!(store.record("work", "3B00", "reader", 10).unwrap());
        assert!(!store.record("work", "3B00", "reader", 12).unwrap());
        assert!(store.stop("removed", Some(15)).unwrap());
        assert!(!store.stop("removed", Some(16)).unwrap());
        assert!(store.record("work", "3B00", "reader", 20).unwrap());
        assert_eq!(
            rows(&store),
            vec![
                (10, 12, Some(15), Some("removed".into())),
                (20, 20, None, None),
            ]
        );
    }

    #[test]
    fn resuming_named_project_preserves_time_and_excludes_pause() {
        let mut store = memory();
        assert!(rows(&store).is_empty());
        store.record("work", "3B00", "reader", 10).unwrap();
        store.stop("removed_or_changed", Some(15)).unwrap();
        assert!(!store.stop("removed_or_changed", Some(500)).unwrap());
        assert_eq!(rows(&store).len(), 1);

        store.record("work", "3B00", "reader", 1000).unwrap();
        store.stop("removed_or_changed", Some(1005)).unwrap();
        let totals: (i64, i64) = store
            .0
            .query_row(
                "SELECT COUNT(*), SUM(ended_at - started_at) FROM sessions WHERE project = ?1",
                ["work"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(totals, (2, 10));
        assert_eq!(rows(&store)[0].2, Some(15));
        assert_eq!(rows(&store)[1].0, 1000);
    }

    #[test]
    fn recovery_does_not_count_downtime_or_change_closed_sessions() {
        let mut store = memory();
        store.record("old", "3B00", "reader", 10).unwrap();
        store.stop("shutdown", Some(11)).unwrap();
        store.record("work", "3B00", "reader", 20).unwrap();
        store.record("work", "3B00", "reader", 25).unwrap();
        assert_eq!(store.recover().unwrap(), 1);
        assert_eq!(store.recover().unwrap(), 0);
        store.record("new", "3B00", "reader", 1000).unwrap();
        assert_eq!(
            rows(&store),
            vec![
                (10, 10, Some(11), Some("shutdown".into())),
                (20, 25, Some(25), Some("interrupted".into())),
                (1000, 1000, None, None),
            ]
        );
    }

    #[test]
    fn reader_error_stops_at_last_confirmed_presence() {
        let mut store = memory();
        store.record("work", "3B00", "reader", 10).unwrap();
        store.record("work", "3B00", "reader", 12).unwrap();
        store.stop("reader_error", None).unwrap();
        assert_eq!(rows(&store)[0].2, Some(12));
    }

    #[test]
    fn sqlite_wait_does_not_extend_removal_or_shutdown_past_observation() {
        for reason in ["removed_or_changed", "shutdown"] {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("contended.db");
            let mut store = Store::open(&db).unwrap();
            store.record("work", "3B00", "fake", 10).unwrap();
            let blocker = Connection::open(&db).unwrap();
            blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                tx.send(()).unwrap();
                // Timestamp belongs to the observation, not completion of the blocked write.
                store.record("work", "3B00", "fake", 12).unwrap();
                store.stop(reason, None).unwrap();
                store
            });
            rx.recv_timeout(Duration::from_secs(2)).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            assert!(!worker.is_finished());
            // Simulated removal/Pause while the checkpoint is blocked.
            blocker.execute_batch("COMMIT").unwrap();
            let store = worker.join().unwrap();
            assert_eq!(rows(&store)[0], (10, 12, Some(12), Some(reason.into())));
        }
    }

    #[test]
    fn backwards_clock_does_not_create_negative_durations() {
        let mut store = memory();
        store.record("work", "3B00", "reader", 10).unwrap();
        store.record("work", "3B00", "reader", 5).unwrap();
        store.stop("shutdown", Some(6)).unwrap();
        assert_eq!(rows(&store)[0], (10, 10, Some(10), Some("shutdown".into())));
    }

    #[test]
    fn report_rows_are_read_only_preserve_names_and_do_not_recover() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("report.db");
        assert!(report_rows(&db).is_err());
        assert!(!db.exists());
        let mut store = Store::open(&db).unwrap();
        let name = "client_项目 ' <&>";
        store.record(name, "3B00", "fake", 10).unwrap();
        store.record(name, "3B00", "fake", 15).unwrap();
        let report = report_rows(&db).unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].project, name);
        assert_eq!(
            (report[0].seconds, report[0].sessions, report[0].open),
            (5, 1, 1)
        );
        assert_eq!(
            report_text(&report),
            format!(
                "PROJECT          TIME (HH:MM:SS)  SESSIONS  OPEN\n{name}        00:00:05         1     1\n"
            )
        );
        assert_eq!(rows(&store)[0].2, None);
    }

    #[test]
    fn report_counts_each_insertion_for_its_project_not_heartbeats_or_removals() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("cycles.db");
        let mut store = Store::open(&db).unwrap();
        for (project, start) in [("work", 10), ("another project", 20), ("work", 30)] {
            assert!(store.record(project, "3B00", "reader", start).unwrap());
            assert!(!store.record(project, "3B00", "reader", start + 1).unwrap());
            assert!(store.stop("removed_or_changed", None).unwrap());
            assert!(!store.stop("removed_or_changed", None).unwrap());
        }
        let report = report_rows(&db).unwrap();
        assert_eq!(report.len(), 2);
        assert_eq!(report[0].project, "another project");
        assert_eq!((report[0].sessions, report[0].open), (1, 0));
        assert_eq!(report[1].project, "work");
        assert_eq!((report[1].sessions, report[1].open), (2, 0));
        assert_eq!(
            report_text(&report),
            "PROJECT          TIME (HH:MM:SS)  SESSIONS  OPEN\nanother project        00:00:01         1     0\nwork                   00:00:02         2     0\n"
        );

        assert!(store.record("work", "3B00", "reader", 40).unwrap());
        let report = report_rows(&db).unwrap();
        assert_eq!((report[0].sessions, report[0].open), (1, 0));
        assert_eq!((report[1].sessions, report[1].open), (3, 1));
    }

    const LEGACY_SCHEMA: &str = "
        CREATE TABLE sessions (
            id INTEGER PRIMARY KEY,
            project TEXT NOT NULL CHECK(length(trim(project)) > 0),
            atr TEXT NOT NULL,
            reader TEXT NOT NULL,
            started_at INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL CHECK(last_seen_at >= started_at),
            ended_at INTEGER CHECK(ended_at >= last_seen_at),
            stop_reason TEXT,
            CHECK((ended_at IS NULL) = (stop_reason IS NULL))
        );
        CREATE UNIQUE INDEX one_open_session ON sessions ((1)) WHERE ended_at IS NULL;
        INSERT INTO sessions VALUES (7, ' client_项目 '' <&> ', '3B00', 'reader', 10, 12, 15, 'removed');
        INSERT INTO sessions VALUES (42, 'Work', '3F00', 'other reader', 20, 25, NULL, NULL);";

    fn schema_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn session_dump(conn: &Connection) -> Vec<String> {
        conn.prepare(
            "SELECT quote(id) || ',' || quote(project) || ',' || quote(atr) || ',' ||
                quote(reader) || ',' || quote(started_at) || ',' || quote(last_seen_at) || ',' ||
                quote(ended_at) || ',' || quote(stop_reason) FROM sessions ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    #[test]
    fn migration_preserves_every_session_and_leaves_recovery_separate() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("legacy.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        let before = session_dump(&conn);
        let report_before = report_text(&report_rows(&db).unwrap());
        drop(conn);

        let store = Store::open(&db).unwrap();
        assert_eq!(schema_version(&store.0), 1);
        assert_eq!(session_dump(&store.0), before);
        assert_eq!(report_text(&report_rows(&db).unwrap()), report_before);
        assert_eq!(
            project_rows(&db).unwrap(),
            vec![
                Project {
                    name: " client_项目 ' <&> ".into(),
                    archived: false
                },
                Project {
                    name: "Work".into(),
                    archived: false
                },
            ]
        );
        assert_eq!(rows(&store)[1], (20, 25, None, None));
        assert!(store.0.execute(
            "INSERT INTO sessions (project, atr, reader, started_at, last_seen_at) VALUES ('Work', '', '', 30, 30)", [],
        ).is_err());
        drop(store);
        let store = Store::open(&db).unwrap();
        assert_eq!(session_dump(&store.0), before);
        assert_eq!(store.recover().unwrap(), 1);
        assert_eq!(
            rows(&store)[1],
            (20, 25, Some(25), Some("interrupted".into()))
        );
        assert!(store.0.execute(
            "INSERT INTO sessions (project, atr, reader, started_at, last_seen_at) VALUES ('missing', '', '', 30, 30)", [],
        ).is_err());
    }

    #[test]
    fn failed_migration_rolls_back_schema_data_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("legacy.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        // A damaged legacy database cannot satisfy the single-open-session invariant.
        conn.execute_batch(
            "DROP INDEX one_open_session;
            INSERT INTO sessions VALUES (43, 'Work', '', '', 30, 31, NULL, NULL);",
        )
        .unwrap();
        let before = session_dump(&conn);
        assert!(Store::open(&db).is_err());
        assert_eq!(schema_version(&conn), 0);
        assert_eq!(session_dump(&conn), before);
        let leftovers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('projects', 'sessions_v1')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(leftovers, 0);
        conn.execute("DELETE FROM sessions WHERE id = 43", [])
            .unwrap();
        assert!(Store::open(&db).is_ok());
    }

    #[test]
    fn writer_lock_precedes_migration_and_covers_all_lifecycle_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("legacy.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        let lock = crate::watcher_lock(&db).unwrap();
        assert!(Store::open(&db).is_err());
        assert_eq!(schema_version(&conn), 0);
        assert_eq!(project_rows(&db).unwrap().len(), 2);
        drop(lock);
        let mut store = Store::open(&db).unwrap();
        assert!(Store::open(&dir.path().join(".").join("legacy.db")).is_err());
        let alias = dir.path().join("alias.db");
        std::os::unix::fs::symlink(&db, &alias).unwrap();
        assert!(Store::open(&alias).is_err());
        store.create_project("empty").unwrap();
        store.archive_project("empty").unwrap();
        assert!(Store::open(&db).is_err());
        assert_eq!(report_rows(&db).unwrap().len(), 2);
        drop(store);
        let mut store = Store::open(&db).unwrap();
        store.delete_project("empty").unwrap();
    }

    #[test]
    fn newer_schema_is_rejected_without_initialization_or_journal_changes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("future.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA user_version = 2; CREATE TABLE future (value TEXT);")
            .unwrap();
        assert!(Store::open(&db).is_err());
        assert!(project_rows(&db).is_err());
        assert_eq!(schema_version(&conn), 2);
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "delete");
        assert!(conn.prepare("SELECT * FROM sessions").is_err());
        assert!(conn.prepare("SELECT * FROM projects").is_err());
    }

    #[test]
    fn discovery_is_read_only_on_legacy_missing_and_current_databases() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("legacy.db");
        assert!(project_rows(&db).is_err());
        assert!(!db.exists());
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        let before = session_dump(&conn);
        assert_eq!(project_rows(&db).unwrap().len(), 2);
        assert_eq!(report_rows(&db).unwrap()[1].open, 1);
        assert_eq!(schema_version(&conn), 0);
        assert_eq!(session_dump(&conn), before);
        assert!(!db.with_extension("db.lock").exists());
        drop(conn);
        let mut store = Store::open(&db).unwrap();
        store.create_project("zero sessions").unwrap();
        store.archive_project("zero sessions").unwrap();
        let projects = project_rows(&db).unwrap();
        assert_eq!(projects.len(), 3);
        assert_eq!(
            projects[2],
            Project {
                name: "zero sessions".into(),
                archived: true
            }
        );
        assert_eq!(report_rows(&db).unwrap().len(), 2);
        assert_eq!(rows(&store)[1].2, None);
    }

    #[test]
    fn archive_restore_delete_and_name_reuse_preserve_their_contract() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lifecycle.db");
        let mut store = Store::open(&db).unwrap();
        for missing in ["missing", "", "  "] {
            assert!(store.archive_project(missing).is_err());
            assert!(store.unarchive_project(missing).is_err());
            assert!(store.delete_project(missing).is_err());
        }
        let name = " client_项目 ' <&> ";
        store.create_project(name).unwrap();
        store.create_project(name).unwrap();
        assert!(report_rows(&db).unwrap().is_empty());
        assert!(store.delete_project(name).is_err());
        store.record(name, "3B00", "reader", 10).unwrap();
        store.record(name, "3B00", "reader", 15).unwrap();
        assert!(store.archive_project(name).is_err());
        assert!(store.delete_project(name).is_err());
        assert_eq!(rows(&store)[0].2, None);
        store.stop("removed", None).unwrap();
        let before = session_dump(&store.0);
        store.archive_project(name).unwrap();
        store.archive_project(name).unwrap();
        assert!(project_rows(&db).unwrap()[0].archived);
        assert!(store.record(name, "3B00", "reader", 1000).is_err());
        assert!(store.create_project(name).is_err());
        assert_eq!(session_dump(&store.0), before);
        let report = report_rows(&db).unwrap();
        assert_eq!(
            (report[0].seconds, report[0].sessions, report[0].open),
            (5, 1, 0)
        );
        store.unarchive_project(name).unwrap();
        store.unarchive_project(name).unwrap();
        assert!(!project_rows(&db).unwrap()[0].archived);
        assert_eq!(session_dump(&store.0), before);
        assert!(store.delete_project(name).is_err());
        store.record(name, "3B00", "reader", 2000).unwrap();
        store.stop("removed", None).unwrap();
        store.archive_project(name).unwrap();
        drop(store);
        let mut store = Store::open(&db).unwrap();
        assert!(project_rows(&db).unwrap()[0].archived);
        store.delete_project(name).unwrap();
        assert!(project_rows(&db).unwrap().is_empty());
        assert!(report_rows(&db).unwrap().is_empty());
        assert!(store.unarchive_project(name).is_err());
        assert!(store.delete_project(name).is_err());
        store.create_project(name).unwrap();
        assert!(!project_rows(&db).unwrap()[0].archived);
        store.record(name, "3B00", "reader", 3000).unwrap();
        assert_eq!(rows(&store), vec![(3000, 3000, None, None)]);
    }

    #[test]
    fn exact_names_and_validation_apply_to_registry_and_tracking() {
        let mut store = memory();
        for name in ["", "  ", "\t\n", "\u{2003}"] {
            assert!(store.create_project(name).is_err());
            assert!(store.record(name, "3B00", "reader", 10).is_err());
        }
        let names = ["work", "Work", " work ", "项目'; DROP TABLE projects;--"];
        for name in names {
            store.create_project(name).unwrap();
        }
        store.archive_project("work").unwrap();
        store.record("Work", "3B00", "reader", 10).unwrap();
        // A rejected observation must not checkpoint another project's open session.
        assert!(store.record("work", "3B00", "reader", 20).is_err());
        assert!(store.record("new", "3B00", "reader", 20).is_err());
        assert_eq!(rows(&store)[0].1, 10);
        let count: i64 = store
            .0
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, names.len() as i64);
        store.archive_project(" work ").unwrap();
        store.delete_project(" work ").unwrap();
        store.stop("removed", None).unwrap();
        assert!(store.record("Work", "3B00", "reader", 30).unwrap());
    }

    #[test]
    fn failed_cascade_rolls_back_project_and_history_atomically() {
        let mut store = memory();
        store.record("work", "3B00", "reader", 10).unwrap();
        store.stop("removed", None).unwrap();
        store.archive_project("work").unwrap();
        let before = session_dump(&store.0);
        store
            .0
            .execute_batch(
                "CREATE TRIGGER fail_delete BEFORE DELETE ON sessions
            BEGIN SELECT RAISE(ABORT, 'test delete failure'); END;",
            )
            .unwrap();
        assert!(store.delete_project("work").is_err());
        assert_eq!(session_dump(&store.0), before);
        let archived: bool = store
            .0
            .query_row(
                "SELECT archived FROM projects WHERE name = 'work'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(archived);
        store.0.execute_batch("DROP TRIGGER fail_delete;").unwrap();
        store.delete_project("work").unwrap();
        assert!(rows(&store).is_empty());
    }

    #[test]
    fn duration_formatting_preserves_unbounded_hours() {
        for (seconds, expected) in [
            (0, "00:00:00"),
            (59, "00:00:59"),
            (60, "00:01:00"),
            (3600, "01:00:00"),
            (3723, "01:02:03"),
            (360000, "100:00:00"),
        ] {
            assert_eq!(format_duration(seconds), expected);
        }
    }

    #[test]
    fn metadata_is_stored_without_sql_interpolation() {
        let mut store = memory();
        let project = "client's project; DROP TABLE sessions;";
        store.record(project, "3B00", "reader", 10).unwrap();
        let actual: (String, String, String) = store
            .0
            .query_row("SELECT project, atr, reader FROM sessions", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(actual, (project.into(), "3B00".into(), "reader".into()));
    }
}
