// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The engine's host-local database, `/var/lib/bmac/bmac.db` (SQLite in WAL
//! mode). Each schema change is one entry in [`MIGRATIONS`], applied in order
//! and recorded in `PRAGMA user_version`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

pub const DATABASE: &str = "/var/lib/bmac/bmac.db";

/// Never edit an applied entry; append a new one.
const MIGRATIONS: &[&str] = &[
    // 1: sessions (token hashes only) and audit events.
    "CREATE TABLE sessions (
        token_hash TEXT PRIMARY KEY NOT NULL,
        user TEXT NOT NULL,
        role TEXT NOT NULL,
        client TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        last_used_ms INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX sessions_by_user ON sessions (user);
    CREATE TABLE audit (
        id INTEGER PRIMARY KEY,
        time_ms INTEGER NOT NULL,
        host TEXT NOT NULL,
        kind TEXT NOT NULL,
        user TEXT,
        client TEXT,
        message TEXT NOT NULL
    ) STRICT;
    CREATE INDEX audit_by_time ON audit (time_ms);",
    // 2: the keyed record log (job events, audit, bmacd's log) and the
    // compressed frames that finished jobs' events move into. Audit events
    // become records; no host ever stored any in the old table.
    "DROP TABLE audit;
    CREATE TABLE streams (
        origin TEXT NOT NULL,
        stream TEXT NOT NULL,
        started_ms INTEGER NOT NULL,
        PRIMARY KEY (origin, stream)
    ) STRICT;
    CREATE TABLE segments (
        id INTEGER PRIMARY KEY,
        file TEXT NOT NULL,
        offset INTEGER NOT NULL,
        length INTEGER NOT NULL,
        raw_bytes INTEGER NOT NULL
    ) STRICT;
    CREATE TABLE records (
        origin TEXT NOT NULL,
        stream TEXT NOT NULL,
        seq INTEGER NOT NULL,
        time_ms INTEGER NOT NULL,
        kind TEXT NOT NULL,
        job TEXT,
        job_seq INTEGER,
        body TEXT,
        segment INTEGER REFERENCES segments (id),
        hash BLOB NOT NULL,
        PRIMARY KEY (origin, stream, seq),
        CHECK ((body IS NULL) != (segment IS NULL))
    ) STRICT;
    CREATE INDEX records_by_job ON records (job, job_seq) WHERE job IS NOT NULL;
    CREATE INDEX records_by_kind_time ON records (kind, time_ms);",
    // 3: the metric history, as a sum and count per bucket.
    "CREATE TABLE metrics (
        host TEXT NOT NULL,
        metric TEXT NOT NULL,
        resolution_ms INTEGER NOT NULL,
        time_ms INTEGER NOT NULL,
        sum REAL NOT NULL,
        count INTEGER NOT NULL,
        PRIMARY KEY (host, metric, resolution_ms, time_ms)
    ) STRICT;
    CREATE INDEX metrics_by_age ON metrics (resolution_ms, time_ms);",
    // 4: log copying, search, and retention: every record keeps the hash
    // before it (so a copy can be checked without its predecessor), its
    // size, event type, severity, and eviction class; evicted records keep
    // only their key and hash; a stream's floor is the last sequence deleted
    // for age.
    "ALTER TABLE records ADD COLUMN prev BLOB;
    ALTER TABLE records ADD COLUMN evicted INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE records ADD COLUMN size INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE records ADD COLUMN etype TEXT;
    ALTER TABLE records ADD COLUMN severity INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE records ADD COLUMN class INTEGER NOT NULL DEFAULT 2;
    ALTER TABLE streams ADD COLUMN floor INTEGER NOT NULL DEFAULT 0;
    UPDATE records SET prev = (SELECT p.hash FROM records p
        WHERE p.origin = records.origin AND p.stream = records.stream AND p.seq = records.seq - 1);
    UPDATE records SET prev = zeroblob(32) WHERE seq = 1;
    UPDATE records SET size = COALESCE(length(CAST(body AS BLOB)), 0);
    UPDATE records SET etype = json_extract(body, '$.type')
        WHERE kind = 'job_event' AND body IS NOT NULL AND json_valid(body);
    UPDATE records SET class = CASE
        WHEN kind = 'audit' OR etype IN ('failed', 'interrupted') THEN 4
        WHEN kind = 'log' THEN 3
        WHEN etype = 'output' THEN 1
        ELSE 2 END;
    UPDATE records SET severity = CASE
        WHEN etype IN ('failed', 'interrupted') THEN 2
        WHEN etype IN ('warning', 'validation_error', 'cancelled') THEN 1
        WHEN etype = 'output' AND json_extract(body, '$.level') = 'error' THEN 2
        WHEN etype = 'output' AND json_extract(body, '$.level') = 'warning' THEN 1
        WHEN kind = 'log' AND json_extract(body, '$.level') = 'error' THEN 2
        WHEN kind = 'log' AND json_extract(body, '$.level') = 'warning' THEN 1
        ELSE 0 END;
    CREATE INDEX records_by_time ON records (time_ms);
    CREATE INDEX records_by_class_time ON records (class, time_ms) WHERE evicted = 0;
    CREATE TABLE retention_events (name TEXT PRIMARY KEY, time_ms INTEGER NOT NULL);",
];

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("the database's schema version {found} is newer than this bmacd knows ({known})")]
    TooNew { found: i64, known: i64 },
    #[error("the database task stopped: {0}")]
    Task(String),
}

/// One connection behind a lock. Calls run on the blocking thread pool.
#[derive(Clone)]
pub struct Database {
    connection: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Database")
    }
}

impl Database {
    pub fn open(path: &Path) -> Result<Database, DbError> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&connection)?;
        Ok(Database {
            connection: Arc::new(Mutex::new(connection)),
            path: path.to_path_buf(),
        })
    }

    /// Where compressed segments go: `segments/` next to the database.
    pub fn segments_dir(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap_or(Path::new("."))
            .join("segments")
    }

    /// Runs `f` with the connection on the blocking pool.
    pub async fn run<T, F>(&self, f: F) -> Result<T, DbError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, rusqlite::Error> + Send + 'static,
    {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let guard = connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            f(&guard).map_err(DbError::from)
        })
        .await
        .map_err(|e| DbError::Task(e.to_string()))?
    }
}

fn migrate(connection: &Connection) -> Result<(), DbError> {
    let known = MIGRATIONS.len() as i64;
    let found: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > known {
        return Err(DbError::TooNew { found, known });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(sql)?;
        transaction.pragma_update(None, "user_version", index as i64 + 1)?;
        transaction.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_once_and_a_newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bmac.db");
        drop(Database::open(&path).unwrap());
        drop(Database::open(&path).unwrap());
        let connection = Connection::open(&path).unwrap();
        let mode: String = connection
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        connection.pragma_update(None, "user_version", 99).unwrap();
        drop(connection);
        assert!(matches!(
            Database::open(&path),
            Err(DbError::TooNew { found: 99, .. })
        ));
    }
}
