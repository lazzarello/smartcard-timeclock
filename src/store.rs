use std::{path::Path, time::Duration};

use anyhow::Result;
use rusqlite::{Connection, OpenFlags, params};

pub struct Store(Connection);

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS sessions (
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
             CREATE UNIQUE INDEX IF NOT EXISTS one_open_session
                 ON sessions ((1)) WHERE ended_at IS NULL;",
        )?;
        Ok(Self(conn))
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
