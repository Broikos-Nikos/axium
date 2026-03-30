use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::sync::{Mutex, MutexGuard};

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub created_at: String,
    pub message_count: i64,
    pub preview: String,
    pub title: String,
}

pub struct ChatDb {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    pub timestamp: String,
}

impl ChatDb {
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
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                timestamp TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                summary TEXT DEFAULT ''
            );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn create_session(&self) -> Result<String> {
        let id = format!("s_{}", Utc::now().format("%Y%m%d_%H%M%S_%3f"));
        let conn = self.conn();
        conn.execute(
            "INSERT INTO sessions (id, created_at) VALUES (?1, ?2)",
            params![id, Utc::now().to_rfc3339()],
        )?;
        Ok(id)
    }

    pub fn save_message(&self, session_id: &str, role: &str, content: &str) -> Result<i64> {
        let conn = self.conn();
        // Dedup guard: skip if the last message in this session has the same role+content.
        // Prevents double-saves when multiple event handlers fire for the same turn.
        let dup: bool = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE id = (SELECT MAX(id) FROM messages WHERE session_id = ?1) AND role = ?2 AND content = ?3",
            params![session_id, role, content],
            |row| row.get::<_, i64>(0),
        ).unwrap_or(0) > 0;
        if dup {
            tracing::debug!(session_id, role, "Dedup: skipped duplicate save_message");
            return Ok(0);
        }
        conn.prepare_cached(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES (?1, ?2, ?3, ?4)",
        )?.execute(params![session_id, role, content, Utc::now().to_rfc3339()])?;
        Ok(conn.last_insert_rowid())
    }

    pub fn load_session_messages(&self, session_id: &str) -> Result<Vec<ChatMessage>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT role, content, timestamp FROM messages WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(ChatMessage {
                role: row.get(0)?,
                content: row.get(1)?,
                timestamp: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Find an existing session by exact ID prefix, or create a new one with that prefix.
    pub fn find_or_create_session(&self, prefix: &str) -> Result<String> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id FROM sessions WHERE id LIKE ?1 ESCAPE '\\' ORDER BY created_at DESC LIMIT 1",
        )?;
        let escaped = prefix.replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("{}%", escaped);
        let mut rows = stmt.query(params![pattern])?;
        if let Some(row) = rows.next()? {
            return Ok(row.get(0)?);
        }
        drop(rows);
        drop(stmt);
        // Create new session with this prefix
        let id = format!("{}_{}", prefix, Utc::now().format("%Y%m%d_%H%M%S"));
        conn.execute(
            "INSERT INTO sessions (id, created_at) VALUES (?1, ?2)",
            params![id, Utc::now().to_rfc3339()],
        )?;
        Ok(id)
    }

    /// Clear all messages from a session (keeps the session itself).
    pub fn clear_session_messages(&self, session_id: &str) -> Result<usize> {
        let conn = self.conn();
        let deleted = conn.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(deleted)
    }

    /// Get the most recent session ID, if any.
    pub fn latest_session(&self) -> Result<Option<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id FROM sessions ORDER BY created_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    /// Count total sessions.
    pub fn session_count(&self) -> Result<usize> {
        let conn = self.conn();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sessions", [], |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Prune old sessions, keeping only the most recent `keep` sessions.
    pub fn prune_old_sessions(&self, keep: usize) -> Result<usize> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM messages WHERE session_id NOT IN (SELECT id FROM sessions ORDER BY created_at DESC LIMIT ?1)",
            params![keep as i64],
        )?;
        let deleted = conn.execute(
            "DELETE FROM sessions WHERE id NOT IN (SELECT id FROM sessions ORDER BY created_at DESC LIMIT ?1)",
            params![keep as i64],
        )?;
        Ok(deleted)
    }

    /// List all sessions with metadata.
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.created_at,
                    COALESCE((SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id), 0),
                    COALESCE((SELECT m.content FROM messages m WHERE m.session_id = s.id AND m.role = 'user' ORDER BY m.id LIMIT 1), ''),
                    COALESCE(s.summary, '')
             FROM sessions s
             ORDER BY CASE WHEN s.id LIKE 'telegram_%' THEN 0 ELSE 1 END, s.created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let preview: String = row.get(3)?;
            let preview = if preview.len() > 100 {
                let mut b = 100; while b > 0 && !preview.is_char_boundary(b) { b -= 1; }
                format!("{}...", &preview[..b])
            } else {
                preview
            };
            Ok(SessionInfo {
                id: row.get(0)?,
                created_at: row.get(1)?,
                message_count: row.get(2)?,
                preview,
                title: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Update the generated title for a session (stored in the summary column).
    pub fn update_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE sessions SET summary = ?1 WHERE id = ?2",
            params![title, session_id],
        )?;
        Ok(())
    }

    /// Get the stored title for a session (empty string if none set).
    pub fn get_session_title(&self, session_id: &str) -> String {
        let conn = self.conn();
        conn.query_row(
            "SELECT COALESCE(summary, '') FROM sessions WHERE id = ?1",
            params![session_id],
            |row| row.get::<_, String>(0),
        ).unwrap_or_default()
    }

    /// Count messages in a session.
    pub fn message_count(&self, session_id: &str) -> usize {
        let conn = self.conn();
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
            params![session_id],
            |row| row.get::<_, i64>(0),
        ).unwrap_or(0) as usize
    }

    /// Delete a session and all its messages atomically.
    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let r1 = conn.execute("DELETE FROM messages WHERE session_id = ?1", params![session_id]);
        let r2 = conn.execute("DELETE FROM sessions WHERE id = ?1", params![session_id]);
        if r1.is_err() || r2.is_err() {
            let _ = conn.execute_batch("ROLLBACK");
            r1?;
            r2?;
        }
        conn.execute_batch("COMMIT")?;
        Ok(())
    }
}
