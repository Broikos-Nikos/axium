"""SQLite conversation history (with FTS5 full-text search) and task queue.

Matches the Rust build's tables closely enough that `search_history` behaves the
same in both. FTS5 is optional, if the local SQLite lacks it, search degrades to
LIKE instead of failing.
"""
import os
import sqlite3
import threading
import time
from datetime import datetime, timezone

_LOCK = threading.Lock()


class Db:
    def __init__(self, path):
        self.path = os.path.abspath(path)
        os.makedirs(os.path.dirname(self.path) or ".", exist_ok=True)
        self.conn = sqlite3.connect(self.path, check_same_thread=False)
        self.conn.row_factory = sqlite3.Row
        self.has_fts = False
        self._init()

    def _init(self):
        c = self.conn
        c.executescript("""
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY, title TEXT DEFAULT '', created_at TEXT, updated_at TEXT);
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
                role TEXT NOT NULL, content TEXT NOT NULL, ts TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
            CREATE TABLE IF NOT EXISTS tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL,
                context TEXT DEFAULT '', status TEXT DEFAULT 'pending',
                result TEXT DEFAULT '', created_at TEXT, updated_at TEXT);
        """)
        try:
            c.executescript("""
                CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts
                    USING fts5(content, content='messages', content_rowid='id');
                CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
                    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
                END;
                CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
                    INSERT INTO messages_fts(messages_fts, rowid, content)
                        VALUES('delete', old.id, old.content);
                END;
            """)
            self.has_fts = True
        except sqlite3.OperationalError:
            self.has_fts = False        # SQLite built without FTS5
        c.commit()

    # -- sessions / messages --
    def ensure_session(self, session_id, title=""):
        now = datetime.now(timezone.utc).isoformat(timespec="seconds")
        with _LOCK:
            self.conn.execute(
                "INSERT OR IGNORE INTO sessions(id,title,created_at,updated_at) VALUES(?,?,?,?)",
                (session_id, title, now, now))
            self.conn.commit()
        return session_id

    def save_message(self, session_id, role, content):
        now = datetime.now(timezone.utc).isoformat(timespec="seconds")
        with _LOCK:
            self.conn.execute(
                "INSERT INTO messages(session_id,role,content,ts) VALUES(?,?,?,?)",
                (session_id, role, content, now))
            self.conn.execute("UPDATE sessions SET updated_at=? WHERE id=?", (now, session_id))
            self.conn.commit()

    def load_messages(self, session_id, limit=200):
        rows = self.conn.execute(
            "SELECT role,content FROM messages WHERE session_id=? ORDER BY id DESC LIMIT ?",
            (session_id, limit)).fetchall()
        return [{"role": r["role"], "content": r["content"]} for r in reversed(rows)]

    def clear_session(self, session_id):
        with _LOCK:
            self.conn.execute("DELETE FROM messages WHERE session_id=?", (session_id,))
            self.conn.commit()

    def search(self, query, limit=10):
        query = (query or "").strip()
        if not query:
            return []
        try:
            if self.has_fts:
                rows = self.conn.execute(
                    "SELECT m.session_id AS session, m.role, m.content, m.ts "
                    "FROM messages_fts f JOIN messages m ON m.id = f.rowid "
                    "WHERE messages_fts MATCH ? ORDER BY rank LIMIT ?",
                    (query, limit)).fetchall()
            else:
                raise sqlite3.OperationalError("no fts")
        except sqlite3.OperationalError:
            rows = self.conn.execute(
                "SELECT session_id AS session, role, content, ts FROM messages "
                "WHERE content LIKE ? ORDER BY id DESC LIMIT ?",
                (f"%{query}%", limit)).fetchall()
        return [dict(r) for r in rows]

    # -- tasks --
    def create_task(self, title, context=""):
        now = datetime.now(timezone.utc).isoformat(timespec="seconds")
        with _LOCK:
            cur = self.conn.execute(
                "INSERT INTO tasks(title,context,status,created_at,updated_at) "
                "VALUES(?,?,'pending',?,?)", (title, context, now, now))
            self.conn.commit()
            return cur.lastrowid

    def update_task(self, task_id, status, result=""):
        now = datetime.now(timezone.utc).isoformat(timespec="seconds")
        with _LOCK:
            self.conn.execute("UPDATE tasks SET status=?,result=?,updated_at=? WHERE id=?",
                              (status, result, now, task_id))
            self.conn.commit()

    def list_tasks(self, limit=50):
        rows = self.conn.execute(
            "SELECT id,title,status FROM tasks ORDER BY id DESC LIMIT ?", (limit,)).fetchall()
        return [dict(r) for r in rows]

    def prune_sessions(self, keep):
        with _LOCK:
            self.conn.execute(
                "DELETE FROM messages WHERE session_id IN ("
                "  SELECT id FROM sessions ORDER BY updated_at DESC LIMIT -1 OFFSET ?)", (keep,))
            self.conn.execute(
                "DELETE FROM sessions WHERE id IN ("
                "  SELECT id FROM sessions ORDER BY updated_at DESC LIMIT -1 OFFSET ?)", (keep,))
            self.conn.commit()

    def close(self):
        try:
            self.conn.close()
        except sqlite3.Error:
            pass


def memory_db():
    """In-memory database for benchmarks that must not touch real history."""
    return Db(os.path.join(os.environ.get("TEMP", "."), f"axium-bench-{int(time.time()*1000)}.db"))
