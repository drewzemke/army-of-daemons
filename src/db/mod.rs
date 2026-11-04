use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// thread-safe database handle that opens connections on demand.
/// SQLite WAL mode allows concurrent readers + one writer.
#[derive(Debug, Clone)]
pub struct Database {
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub id: i64,
    pub instance_id: String,
    pub created_at: i64,
    pub severity: i32,
    pub title: Option<String>,
    pub dedup_key: Option<String>,
    pub analysis: String,
    pub source_items_json: Option<String>,
    pub session_id: Option<String>,
    pub working_dir: Option<String>,
    pub is_read: bool,
    pub is_error: bool,
}

impl Report {
    pub fn format_text(&self) -> String {
        let mut parts = Vec::new();
        if let Some(ref raw) = self.source_items_json {
            let pretty = serde_json::from_str::<serde_json::Value>(raw)
                .and_then(|v| serde_json::to_string_pretty(&v))
                .unwrap_or_else(|_| raw.clone());
            parts.push(pretty);
            parts.push("---".to_string());
        }
        parts.push(self.analysis.clone());
        if let Some(ref sid) = self.session_id {
            parts.push(format!("\n---\nsession: {sid}"));
        }
        parts.join("\n")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportSummary {
    pub id: i64,
    pub severity: i32,
    pub title: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct DaemonSummary {
    pub instance_id: String,
    #[allow(dead_code)]
    pub daemon_name: String,
    pub unread_count: i64,
    pub max_severity: i32,
    pub last_checked: Option<i64>,
    pub running_since: Option<i64>,
    pub in_error: bool,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Self { path: path.to_path_buf() };
        // run migrations on first open
        let conn = db.conn()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS instances (
                id TEXT PRIMARY KEY,
                daemon_name TEXT NOT NULL,
                params_json TEXT,
                last_checked INTEGER,
                last_run INTEGER,
                run_count INTEGER DEFAULT 0,
                in_error INTEGER DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS reports (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id TEXT NOT NULL REFERENCES instances(id),
                created_at INTEGER NOT NULL,
                severity INTEGER NOT NULL,
                title TEXT,
                dedup_key TEXT,
                analysis TEXT NOT NULL,
                source_items_json TEXT,
                is_read INTEGER DEFAULT 0,
                is_error INTEGER DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS runner_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS seen_keys (
                instance_id TEXT NOT NULL,
                dedup_key TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_reports_dedup
                ON reports(instance_id, dedup_key, created_at);
            CREATE INDEX IF NOT EXISTS idx_reports_unread
                ON reports(is_read, severity, created_at);
            CREATE INDEX IF NOT EXISTS idx_seen_keys
                ON seen_keys(instance_id, dedup_key, created_at);",
        )?;
        // migration: add in_error column to existing instances tables
        let _ = conn.execute_batch(
            "ALTER TABLE instances ADD COLUMN in_error INTEGER DEFAULT 0;"
        );
        // migration: add session_id column to existing reports tables
        let _ = conn.execute_batch(
            "ALTER TABLE reports ADD COLUMN session_id TEXT;"
        );
        // migration: add running_since column to track active pipelines
        let _ = conn.execute_batch(
            "ALTER TABLE instances ADD COLUMN running_since INTEGER;"
        );
        // migration: add working_dir column to reports
        let _ = conn.execute_batch(
            "ALTER TABLE reports ADD COLUMN working_dir TEXT;"
        );
        Ok(db)
    }

    fn conn(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("opening database at {}", self.path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        Ok(conn)
    }

    // -- instance state --

    pub fn upsert_instance(
        &self,
        id: &str,
        daemon_name: &str,
        params_json: Option<&str>,
    ) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO instances (id, daemon_name, params_json, run_count)
             VALUES (?1, ?2, ?3, 0)
             ON CONFLICT(id) DO UPDATE SET daemon_name=?2, params_json=?3",
            params![id, daemon_name, params_json],
        )?;
        Ok(())
    }

    pub fn update_instance_run(&self, id: &str, checked: bool) -> Result<(i64, i64)> {
        let conn = self.conn()?;
        let now = chrono::Utc::now().timestamp();
        if checked {
            conn.execute(
                "UPDATE instances SET last_checked=?1, last_run=?1, run_count=run_count+1 WHERE id=?2",
                params![now, id],
            )?;
        } else {
            conn.execute(
                "UPDATE instances SET last_run=?1, run_count=run_count+1 WHERE id=?2",
                params![now, id],
            )?;
        }
        let run_count: i64 = conn.query_row(
            "SELECT run_count FROM instances WHERE id=?1",
            params![id],
            |row| row.get(0),
        )?;
        Ok((now, run_count))
    }

    pub fn set_running(&self, instance_id: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn()?.execute(
            "UPDATE instances SET running_since=?1 WHERE id=?2",
            params![now, instance_id],
        )?;
        Ok(())
    }

    pub fn clear_running(&self, instance_id: &str) -> Result<()> {
        self.conn()?.execute(
            "UPDATE instances SET running_since=NULL WHERE id=?1",
            params![instance_id],
        )?;
        Ok(())
    }

    pub fn clear_stale_running(&self, max_age_secs: i64) -> Result<usize> {
        let cutoff = chrono::Utc::now().timestamp() - max_age_secs;
        let cleared = self.conn()?.execute(
            "UPDATE instances SET running_since=NULL WHERE running_since IS NOT NULL AND running_since < ?1",
            params![cutoff],
        )?;
        Ok(cleared)
    }

    pub fn get_last_checked(&self, instance_id: &str) -> Result<Option<i64>> {
        let conn = self.conn()?;
        let result = conn.query_row(
            "SELECT last_checked FROM instances WHERE id=?1",
            params![instance_id],
            |row| row.get::<_, Option<i64>>(0),
        );
        match result {
            Ok(ts) => Ok(ts),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // -- reports --

    #[allow(clippy::too_many_arguments)]
    pub fn insert_report(
        &self,
        instance_id: &str,
        severity: i32,
        title: Option<&str>,
        dedup_key: Option<&str>,
        analysis: &str,
        source_items_json: Option<&str>,
        session_id: Option<&str>,
        working_dir: Option<&str>,
        is_error: bool,
    ) -> Result<i64> {
        let conn = self.conn()?;
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO reports (instance_id, created_at, severity, title, dedup_key, analysis, source_items_json, session_id, working_dir, is_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![instance_id, now, severity, title, dedup_key, analysis, source_items_json, session_id, working_dir, is_error as i32],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn check_dedup(&self, instance_id: &str, dedup_key: &str, window_secs: i64) -> Result<bool> {
        let conn = self.conn()?;
        let cutoff = chrono::Utc::now().timestamp() - window_secs;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM seen_keys
             WHERE instance_id=?1 AND dedup_key=?2 AND created_at > ?3",
            params![instance_id, dedup_key, cutoff],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn mark_seen(&self, instance_id: &str, dedup_key: &str) -> Result<()> {
        let conn = self.conn()?;
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO seen_keys (instance_id, dedup_key, created_at) VALUES (?1, ?2, ?3)",
            params![instance_id, dedup_key, now],
        )?;
        Ok(())
    }

    pub fn is_in_error(&self, instance_id: &str) -> Result<bool> {
        let conn = self.conn()?;
        let result = conn.query_row(
            "SELECT in_error FROM instances WHERE id=?1",
            params![instance_id],
            |row| row.get::<_, i32>(0),
        );
        match result {
            Ok(v) => Ok(v != 0),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_error(&self, instance_id: &str, in_error: bool) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE instances SET in_error=?1 WHERE id=?2",
            params![in_error as i32, instance_id],
        )?;
        // when recovering from error, auto-read the error reports
        if !in_error {
            conn.execute(
                "UPDATE reports SET is_read=1 WHERE instance_id=?1 AND is_error=1 AND is_read=0",
                params![instance_id],
            )?;
        }
        Ok(())
    }

    pub fn mark_read(&self, report_id: i64) -> Result<()> {
        self.conn()?.execute("UPDATE reports SET is_read=1 WHERE id=?1", params![report_id])?;
        Ok(())
    }

    pub fn mark_unread(&self, report_id: i64) -> Result<()> {
        self.conn()?.execute("UPDATE reports SET is_read=0 WHERE id=?1", params![report_id])?;
        Ok(())
    }

    pub fn delete_report(&self, report_id: i64) -> Result<()> {
        self.conn()?.execute("DELETE FROM reports WHERE id=?1", params![report_id])?;
        Ok(())
    }

    pub fn mark_all_read(&self, instance_id: &str) -> Result<()> {
        self.conn()?.execute(
            "UPDATE reports SET is_read=1 WHERE instance_id=?1 AND is_read=0",
            params![instance_id],
        )?;
        Ok(())
    }

    pub fn get_report(&self, report_id: i64) -> Result<Option<Report>> {
        let conn = self.conn()?;
        let result = conn.query_row(
            "SELECT id, instance_id, created_at, severity, title, dedup_key, analysis, source_items_json, session_id, working_dir, is_read, is_error
             FROM reports WHERE id=?1",
            params![report_id],
            map_report,
        );
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_reports(&self, instance_id: &str, limit: usize, cutoff_secs: i64) -> Result<Vec<Report>> {
        let conn = self.conn()?;
        let cutoff = display_cutoff_secs(cutoff_secs);
        let mut stmt = conn.prepare(
            "SELECT id, instance_id, created_at, severity, title, dedup_key, analysis, source_items_json, session_id, working_dir, is_read, is_error
             FROM reports WHERE instance_id=?1 AND created_at > ?2
             ORDER BY created_at DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![instance_id, cutoff, limit], map_report)?;
        let mut reports = Vec::new();
        for row in rows {
            reports.push(row?);
        }
        Ok(reports)
    }

    pub fn recent_report_summaries(&self, instance_id: &str, limit: usize) -> Result<Vec<ReportSummary>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, severity, title, created_at
             FROM reports WHERE instance_id=?1 AND is_error=0
             ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![instance_id, limit], |row| {
            Ok(ReportSummary {
                id: row.get(0)?,
                severity: row.get(1)?,
                title: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        let mut summaries = Vec::new();
        for row in rows {
            summaries.push(row?);
        }
        Ok(summaries)
    }

    // -- dashboard queries --

    pub fn daemon_summaries(&self, cutoff_secs: i64) -> Result<Vec<DaemonSummary>> {
        let conn = self.conn()?;
        let cutoff = display_cutoff_secs(cutoff_secs);
        let stale_cutoff = chrono::Utc::now().timestamp() - 600;
        let mut stmt = conn.prepare(
            "SELECT i.id, i.daemon_name, i.last_checked,
                    COALESCE(SUM(CASE WHEN r.is_read=0 AND r.is_error=0 THEN 1 ELSE 0 END), 0) as unread,
                    COALESCE(MAX(CASE WHEN r.is_read=0 AND r.is_error=0 THEN r.severity ELSE 0 END), 0) as max_sev,
                    CASE WHEN i.running_since > ?2 THEN i.running_since ELSE NULL END as running,
                    MAX(r.created_at) as latest_report,
                    i.in_error
             FROM instances i
             LEFT JOIN reports r ON r.instance_id = i.id AND r.created_at > ?1
             GROUP BY i.id
             HAVING latest_report IS NOT NULL
             ORDER BY i.in_error DESC, max_sev DESC, unread DESC, latest_report DESC",
        )?;
        let rows = stmt.query_map(params![cutoff, stale_cutoff], |row| {
            Ok(DaemonSummary {
                instance_id: row.get(0)?,
                daemon_name: row.get(1)?,
                last_checked: row.get(2)?,
                unread_count: row.get(3)?,
                max_severity: row.get(4)?,
                running_since: row.get(5)?,
                in_error: row.get::<_, i32>(7)? != 0,
            })
        })?;
        let mut summaries = Vec::new();
        for row in rows {
            summaries.push(row?);
        }
        Ok(summaries)
    }

    // -- status line --

    /// `icons` maps daemon name -> icon; those daemons get their own segment.
    /// severity-0 (static) reports without an icon fall under DEFAULT_ICON
    pub fn status_line(&self, cutoff_secs: i64, _icons: &std::collections::HashMap<String, String>) -> Result<String> {
        todo!();
        #[allow(unreachable_code)]
        let conn = self.conn()?;
        let cutoff = display_cutoff_secs(cutoff_secs);

        // count errors separately
        let error_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM reports WHERE is_read=0 AND is_error=1 AND created_at > ?1",
            params![cutoff],
            |row| row.get(0),
        )?;

        let mut stmt = conn.prepare(
            "SELECT severity, COUNT(*) FROM reports
             WHERE is_read=0 AND is_error=0 AND created_at > ?1
             GROUP BY severity ORDER BY severity DESC",
        )?;
        let rows: Vec<(i32, i64)> = stmt
            .query_map(params![cutoff], |row| Ok((row.get(0)?, row.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();

        let mut parts: Vec<String> = Vec::new();

        if error_count > 0 {
            parts.push(format!("\u{274c} {error_count}"));
        }

        for (sev, count) in &rows {
            let emoji = severity_emoji(*sev);
            parts.push(format!("{emoji} {count}"));
        }

        if parts.is_empty() {
            Ok("\u{2705}".to_string())
        } else {
            Ok(parts.join(" | "))
        }
    }

    // -- runner heartbeat --

    pub fn write_heartbeat(&self, pid: u32) -> Result<()> {
        let conn = self.conn()?;
        let now = chrono::Utc::now().timestamp().to_string();
        let pid_str = pid.to_string();
        conn.execute(
            "INSERT OR REPLACE INTO runner_state (key, value) VALUES ('heartbeat', ?1)",
            params![now],
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO runner_state (key, value) VALUES ('pid', ?1)",
            params![pid_str],
        )?;
        Ok(())
    }

    pub fn read_heartbeat(&self) -> Result<Option<i64>> {
        let conn = self.conn()?;
        let result = conn.query_row(
            "SELECT value FROM runner_state WHERE key='heartbeat'",
            [],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(s) => Ok(s.parse::<i64>().ok()),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // -- pruning --

    pub fn prune(&self, older_than_days: u32) -> Result<usize> {
        let conn = self.conn()?;
        let cutoff = chrono::Utc::now().timestamp() - (older_than_days as i64 * 86400);
        let deleted = conn.execute(
            "DELETE FROM reports WHERE created_at < ?1",
            params![cutoff],
        )?;
        conn.execute(
            "DELETE FROM seen_keys WHERE created_at < ?1",
            params![cutoff],
        )?;
        Ok(deleted)
    }

    // -- export --

    pub fn export_reports(&self, daemon: Option<&str>, days: u32) -> Result<Vec<Report>> {
        let conn = self.conn()?;
        let cutoff = chrono::Utc::now().timestamp() - (days as i64 * 86400);
        let mut reports = Vec::new();

        if let Some(daemon_name) = daemon {
            let mut stmt = conn.prepare(
                "SELECT r.id, r.instance_id, r.created_at, r.severity, r.title, r.dedup_key,
                        r.analysis, r.source_items_json, r.session_id, r.working_dir, r.is_read, r.is_error
                 FROM reports r
                 JOIN instances i ON r.instance_id = i.id
                 WHERE i.daemon_name = ?1 AND r.created_at > ?2
                 ORDER BY r.created_at DESC",
            )?;
            let rows = stmt.query_map(params![daemon_name, cutoff], map_report)?;
            for row in rows {
                reports.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, instance_id, created_at, severity, title, dedup_key,
                        analysis, source_items_json, session_id, working_dir, is_read, is_error
                 FROM reports WHERE created_at > ?1
                 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map(params![cutoff], map_report)?;
            for row in rows {
                reports.push(row?);
            }
        }

        Ok(reports)
    }
}

fn map_report(row: &rusqlite::Row) -> rusqlite::Result<Report> {
    Ok(Report {
        id: row.get(0)?,
        instance_id: row.get(1)?,
        created_at: row.get(2)?,
        severity: row.get(3)?,
        title: row.get(4)?,
        dedup_key: row.get(5)?,
        analysis: row.get(6)?,
        source_items_json: row.get(7)?,
        session_id: row.get(8)?,
        working_dir: row.get(9)?,
        is_read: row.get::<_, i32>(10)? != 0,
        is_error: row.get::<_, i32>(11)? != 0,
    })
}

/// status-bar glyph for static reports from daemons without an icon
pub const DEFAULT_ICON: &str = "\u{25a0}";

pub fn display_cutoff_secs(secs: i64) -> i64 {
    chrono::Utc::now().timestamp() - secs
}

pub fn severity_emoji(sev: i32) -> &'static str {
    match sev {
        5 => "\u{1f534}",
        4 => "\u{1f7e0}",
        3 => "\u{1f7e1}",
        2 => "\u{1f535}",
        1 => "\u{26aa}",
        _ => "\u{26aa}",
    }
}

pub fn severity_label(sev: i32) -> &'static str {
    match sev {
        5 => "critical",
        4 => "high",
        3 => "medium",
        2 => "low",
        1 => "noise",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn temp_db() -> Database {
        let path = std::env::temp_dir().join(format!(
            "army-test-{}-{}.db",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        Database::open(&path).unwrap()
    }

    fn add(db: &Database, instance: &str, daemon: &str, severity: i32, n: usize) {
        db.upsert_instance(instance, daemon, None).unwrap();
        for _ in 0..n {
            db.insert_report(instance, severity, None, None, "", None, None, None, false).unwrap();
        }
    }

    #[test]
    fn status_line_empty() {
        assert_eq!(temp_db().status_line(3600, &HashMap::new()).unwrap(), "\u{2705}");
    }

    #[test]
    fn status_line_groups_icon_daemons_separately() {
        let db = temp_db();
        add(&db, "gh", "gh", 0, 2);
        add(&db, "plain-static", "plain-static", 0, 1);
        add(&db, "prs::a", "prs", 5, 1);
        add(&db, "prs::b", "prs", 3, 2);
        add(&db, "ci", "ci", 4, 1);
        let icons = HashMap::from([
            ("gh".to_string(), "GG".to_string()),
            ("ci".to_string(), "CI".to_string()),
        ]);
        assert_eq!(
            db.status_line(3600, &icons).unwrap(),
            format!("CI 1 | GG 2 | {DEFAULT_ICON} 1 | \u{1f534} 1 | \u{1f7e1} 2")
        );
    }

    #[test]
    fn status_line_ignores_read_reports() {
        let db = temp_db();
        add(&db, "gh", "gh", 0, 1);
        db.mark_all_read("gh").unwrap();
        let icons = HashMap::from([("gh".to_string(), "GG".to_string())]);
        assert_eq!(db.status_line(3600, &icons).unwrap(), "\u{2705}");
    }
}
