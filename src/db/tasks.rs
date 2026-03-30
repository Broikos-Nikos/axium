use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use std::sync::{Mutex, MutexGuard};

/// Lightweight task tracker for autonomous agent work.
/// Tasks are persisted in SQLite alongside chat history.
pub struct TaskDb {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Task {
    pub id: i64,
    pub title: String,
    pub status: String,       // pending | running | done | failed
    pub context: String,      // brief context (kept small to save tokens)
    pub result: String,       // stored result after completion
    pub read: bool,           // whether result has been shown to user
    pub created_at: String,
    pub updated_at: String,
}

impl TaskDb {
    /// Lock the connection, recovering from a poisoned mutex.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA busy_timeout=5000;
            CREATE TABLE IF NOT EXISTS tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                context TEXT NOT NULL DEFAULT '',
                result TEXT NOT NULL DEFAULT '',
                read INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status, updated_at DESC);"
        )?;
        // Migrate existing DBs — silently ignore if columns already exist
        let _ = conn.execute_batch("ALTER TABLE tasks ADD COLUMN result TEXT NOT NULL DEFAULT ''");
        let _ = conn.execute_batch("ALTER TABLE tasks ADD COLUMN read INTEGER NOT NULL DEFAULT 0");
        // Recover any tasks stuck in 'running' state from a previous crash
        let recovered = conn.execute(
            "UPDATE tasks SET status = 'pending', updated_at = ?1 WHERE status = 'running'",
            params![Utc::now().to_rfc3339()],
        ).unwrap_or(0);
        if recovered > 0 {
            eprintln!("[tasks] recovered {} stuck running tasks → pending", recovered);
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn create_task(&self, title: &str, context: &str) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO tasks (title, status, context, created_at, updated_at) VALUES (?1, 'pending', ?2, ?3, ?3)",
            params![title, context, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_task_status(&self, id: i64, status: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status, Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }

    pub fn save_task_result(&self, id: i64, result: &str, status: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE tasks SET result = ?1, status = ?2, read = 0, updated_at = ?3 WHERE id = ?4",
            params![result, status, Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }

    /// Claim a pending task atomically — returns None if none available.
    pub fn claim_pending(&self) -> Result<Option<Task>> {
        let conn = self.conn();
        let now = Utc::now().to_rfc3339();
        // Fetch oldest pending task
        let task = conn.query_row(
            "SELECT id, title, status, context, result, read, created_at, updated_at FROM tasks WHERE status = 'pending' ORDER BY id LIMIT 1",
            [],
            |row| Ok(Task {
                id: row.get(0)?,
                title: row.get(1)?,
                status: row.get(2)?,
                context: row.get(3)?,
                result: row.get(4)?,
                read: row.get::<_, i64>(5)? != 0,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            }),
        ).ok();
        if let Some(ref t) = task {
            let affected = conn.execute(
                "UPDATE tasks SET status = 'running', updated_at = ?1 WHERE id = ?2 AND status = 'pending'",
                params![now, t.id],
            )?;
            // If another worker already claimed it, return None
            if affected == 0 {
                return Ok(None);
            }
        }
        Ok(task)
    }

    /// Return completed tasks that haven't been shown to the user yet.
    pub fn unread_completed(&self) -> Result<Vec<Task>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, title, status, context, result, read, created_at, updated_at FROM tasks WHERE status IN ('done', 'failed') AND read = 0 ORDER BY updated_at",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Task {
                id: row.get(0)?,
                title: row.get(1)?,
                status: row.get(2)?,
                context: row.get(3)?,
                result: row.get(4)?,
                read: row.get::<_, i64>(5)? != 0,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }

    pub fn mark_read(&self, id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("UPDATE tasks SET read = 1 WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn list_active_tasks(&self) -> Result<Vec<Task>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, title, status, context, result, read, created_at, updated_at FROM tasks WHERE status IN ('pending', 'running') ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Task {
                id: row.get(0)?,
                title: row.get(1)?,
                status: row.get(2)?,
                context: row.get(3)?,
                result: row.get(4)?,
                read: row.get::<_, i64>(5)? != 0,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }

    pub fn list_recent_tasks(&self, limit: usize) -> Result<Vec<Task>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, title, status, context, result, read, created_at, updated_at FROM tasks ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(Task {
                id: row.get(0)?,
                title: row.get(1)?,
                status: row.get(2)?,
                context: row.get(3)?,
                result: row.get(4)?,
                read: row.get::<_, i64>(5)? != 0,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }
}
