// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The todo list, kept in one SQLite file.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;

/// The app is reachable from the internet, so the list is capped.
pub const MAX_TODOS: i64 = 100;
pub const MAX_TITLE_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Todo {
    pub id: i64,
    pub title: String,
    pub done: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    /// The request is malformed; the message is safe to show the client.
    Invalid(String),
    NotFound,
    Full,
    Database(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        StoreError::Database(error.to_string())
    }
}

pub struct Store {
    connection: Connection,
}

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')";

fn validate_title(title: &str) -> Result<String, StoreError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(StoreError::Invalid("title must not be empty".into()));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(StoreError::Invalid(format!(
            "title must be at most {MAX_TITLE_CHARS} characters"
        )));
    }
    if title.chars().any(char::is_control) {
        return Err(StoreError::Invalid(
            "title must not contain control characters".into(),
        ));
    }
    Ok(title.to_string())
}

fn row_to_todo(row: &rusqlite::Row<'_>) -> rusqlite::Result<Todo> {
    Ok(Todo {
        id: row.get(0)?,
        title: row.get(1)?,
        done: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

impl Store {
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        Self::init(connection)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(connection: Connection) -> Result<Self, StoreError> {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS todos (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 title TEXT NOT NULL,
                 done INTEGER NOT NULL DEFAULT 0 CHECK (done IN (0, 1)),
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );",
        )?;
        Ok(Store { connection })
    }

    pub fn list(&self) -> Result<Vec<Todo>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT id, title, done, created_at, updated_at FROM todos ORDER BY id")?;
        let rows = statement.query_map([], row_to_todo)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn count(&self) -> Result<i64, StoreError> {
        Ok(self
            .connection
            .query_row("SELECT COUNT(*) FROM todos", [], |row| row.get(0))?)
    }

    pub fn get(&self, id: i64) -> Result<Todo, StoreError> {
        self.connection
            .query_row(
                "SELECT id, title, done, created_at, updated_at FROM todos WHERE id = ?1",
                params![id],
                row_to_todo,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    pub fn create(&mut self, title: &str) -> Result<Todo, StoreError> {
        let title = validate_title(title)?;
        // The count and the insert happen under one write lock, so concurrent
        // requests can't push the list past the cap.
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 =
            transaction.query_row("SELECT COUNT(*) FROM todos", [], |row| row.get(0))?;
        if count >= MAX_TODOS {
            return Err(StoreError::Full);
        }
        transaction.execute(
            &format!("INSERT INTO todos (title, done, created_at, updated_at) VALUES (?1, 0, {NOW}, {NOW})"),
            params![title],
        )?;
        let id = transaction.last_insert_rowid();
        transaction.commit()?;
        self.get(id)
    }

    pub fn update(
        &mut self,
        id: i64,
        title: Option<&str>,
        done: Option<bool>,
    ) -> Result<Todo, StoreError> {
        if title.is_none() && done.is_none() {
            return Err(StoreError::Invalid(
                "nothing to update: send title and/or done".into(),
            ));
        }
        let title = title.map(validate_title).transpose()?;
        let changed = self.connection.execute(
            &format!(
                "UPDATE todos SET title = COALESCE(?2, title), done = COALESCE(?3, done), \
                 updated_at = {NOW} WHERE id = ?1"
            ),
            params![id, title, done],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        self.get(id)
    }

    pub fn delete(&mut self, id: i64) -> Result<(), StoreError> {
        match self
            .connection
            .execute("DELETE FROM todos WHERE id = ?1", params![id])?
        {
            0 => Err(StoreError::NotFound),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_list_update_delete() {
        let mut store = Store::open_in_memory().unwrap();
        let first = store.create("  Buy milk  ").unwrap();
        assert_eq!(first.title, "Buy milk");
        assert!(!first.done);
        let second = store.create("Walk the dog").unwrap();
        assert_eq!(store.list().unwrap(), vec![first.clone(), second.clone()]);

        let done = store.update(first.id, None, Some(true)).unwrap();
        assert!(done.done);
        assert_eq!(done.title, "Buy milk");
        let renamed = store.update(first.id, Some("Buy oat milk"), None).unwrap();
        assert_eq!(
            (renamed.title.as_str(), renamed.done),
            ("Buy oat milk", true)
        );

        store.delete(first.id).unwrap();
        assert_eq!(store.delete(first.id), Err(StoreError::NotFound));
        assert_eq!(store.get(first.id), Err(StoreError::NotFound));
        assert_eq!(store.list().unwrap(), vec![second]);
    }

    #[test]
    fn the_list_is_capped() {
        let mut store = Store::open_in_memory().unwrap();
        for index in 0..MAX_TODOS {
            store.create(&format!("todo {index}")).unwrap();
        }
        assert_eq!(store.create("one too many"), Err(StoreError::Full));
        assert_eq!(store.count().unwrap(), MAX_TODOS);
        let first = store.list().unwrap()[0].id;
        store.delete(first).unwrap();
        store.create("room again").unwrap();
    }

    #[test]
    fn titles_are_validated() {
        let mut store = Store::open_in_memory().unwrap();
        for bad in ["", "   ", "tab\there", &"x".repeat(MAX_TITLE_CHARS + 1)] {
            assert!(
                matches!(store.create(bad), Err(StoreError::Invalid(_))),
                "{bad:?}"
            );
        }
        assert!(store.create(&"é".repeat(MAX_TITLE_CHARS)).is_ok());
        let todo = store.create("ok").unwrap();
        assert!(matches!(
            store.update(todo.id, None, None),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            store.update(todo.id, Some(" "), None),
            Err(StoreError::Invalid(_))
        ));
        assert_eq!(
            store.update(9999, None, Some(true)),
            Err(StoreError::NotFound)
        );
    }

    #[test]
    fn data_survives_reopening_the_file() {
        let directory =
            std::env::temp_dir().join(format!("hello-todo-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("todos.sqlite3");
        {
            let mut store = Store::open(path.to_str().unwrap()).unwrap();
            store.create("persisted").unwrap();
        }
        let store = Store::open(path.to_str().unwrap()).unwrap();
        assert_eq!(store.list().unwrap()[0].title, "persisted");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
