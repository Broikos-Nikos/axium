r"""Typed, importance-scored facts — the half of memory the model does not have
to remember to write.

`memory.Memory` is a markdown file the agent edits deliberately via
`update_memory`. That works for facts the agent *notices* are durable, and fails
for the ones a user drops mid-sentence: "shipping is free over 50 euro" stated in
turn 1 is gone by turn 6, because nothing ever called a tool about it and
compaction summarised the turn away.

This module closes that gap. After every turn a cheap-model pass extracts durable
statements into typed rows:

    type        rule | convention | decision | preference | gotcha | reference | note
    key         stable id for dedup ("shipping.free_threshold"); derived from the
                value when omitted, so restating the same fact updates in place
    value       the statement itself
    importance  0..1 — drives ordering and what survives the render budget

The rows render into a `[FACTS]` block that sits in the SYSTEM prompt, above the
conversation. Compaction rewrites history; it cannot touch the system prompt, so
a fact captured in turn 1 is still verbatim in front of the model in turn 20.

A user correction ("no, not like that", "oxi, lathos") is extracted as a directive
at high importance — the thing an agent is most expensive to forget.

Storage is SQLite next to the memory file. Readers never create tables: the
per-turn prompt build must be side-effect-free.
"""
import os
import re
import sqlite3
import threading
import time

_LOCK = threading.Lock()

TYPES = ("rule", "convention", "decision", "preference", "gotcha", "reference", "note")

# The [FACTS] block is paid for on every single call of the loop. 1800 chars is
# roughly 500 tokens: enough for two dozen real facts, small enough that it never
# competes with the conversation for the window.
RENDER_CHAR_BUDGET = 1800
RENDER_MAX_FACTS = 24

# Credential-shaped material must never reach a store that renders into every
# prompt. Same reasoning as the plain memory file, but this one is automatic:
# nothing here was reviewed by a human before it was written.
_SK_KEY = re.compile(r"\bsk-[A-Za-z0-9_\-.]{10,}\b")
_CRED_ASSIGN = re.compile(
    r"(?i)\b(password|passwd|pwd|passphrase|api[ _-]?key|secret[ _-]?key|secret|token|"
    r"credentials?)\b\s*(?:=|:=|:|->|\bis\b|\bare\b)\s*[\"'`]?([^,;\n\"'`]{4,200})")
REDACTED = "<redacted>"


def sanitize(value):
    """Redact anything credential-shaped before it can be persisted."""
    v = _SK_KEY.sub(REDACTED, value or "")
    return _CRED_ASSIGN.sub(lambda m: f"{m.group(1)}: {REDACTED}", v)


def _derive_key(value):
    """A stable key from the value, so the same fact restated collapses instead of
    accumulating near-duplicates."""
    words = re.findall(r"[a-z0-9]+", (value or "").lower())
    return ".".join(words[:6])[:80] or "fact"


class FactStore:
    def __init__(self, path):
        self.path = os.path.abspath(path)
        os.makedirs(os.path.dirname(self.path) or ".", exist_ok=True)

    def _conn(self, create=False):
        c = sqlite3.connect(self.path, timeout=5.0, check_same_thread=False)
        c.row_factory = sqlite3.Row
        c.execute("PRAGMA busy_timeout=5000")
        try:
            c.execute("PRAGMA journal_mode=WAL")
        except sqlite3.DatabaseError:
            pass
        if create:
            c.execute(
                "CREATE TABLE IF NOT EXISTS facts("
                " id INTEGER PRIMARY KEY AUTOINCREMENT,"
                " scope TEXT NOT NULL DEFAULT '',"
                " type TEXT NOT NULL DEFAULT 'note',"
                " key TEXT NOT NULL,"
                " value TEXT NOT NULL,"
                " importance REAL NOT NULL DEFAULT 0.5,"
                " source TEXT DEFAULT '',"
                " hits INTEGER NOT NULL DEFAULT 0,"
                " created_ts REAL NOT NULL,"
                " updated_ts REAL NOT NULL)")
            c.execute("CREATE UNIQUE INDEX IF NOT EXISTS idx_facts_key ON facts(scope, key)")
            c.commit()
        return c

    @staticmethod
    def _table_exists(c):
        return bool(c.execute("SELECT 1 FROM sqlite_master WHERE type='table' "
                              "AND name='facts'").fetchone())

    # -- writes --
    def remember(self, value, type="note", key="", importance=0.5, scope="", source=""):
        """Insert or update one fact. Re-stating a known key keeps the HIGHER
        importance: a rule restated casually must not demote the emphatic version."""
        value = sanitize((value or "").strip())
        if not value:
            return None
        type = type if type in TYPES else "note"
        key = (key or _derive_key(value)).strip()[:80]
        importance = max(0.0, min(1.0, float(importance)))
        now = time.time()
        with _LOCK:
            c = self._conn(create=True)
            try:
                row = c.execute("SELECT id, importance FROM facts WHERE scope=? AND key=?",
                                (scope, key)).fetchone()
                if row:
                    c.execute("UPDATE facts SET value=?, type=?, importance=?, "
                              "source=?, updated_ts=? WHERE id=?",
                              (value, type, max(importance, row["importance"] or 0.0),
                               source, now, row["id"]))
                    fid = row["id"]
                else:
                    cur = c.execute(
                        "INSERT INTO facts(scope,type,key,value,importance,source,"
                        "created_ts,updated_ts) VALUES(?,?,?,?,?,?,?,?)",
                        (scope, type, key, value, importance, source, now, now))
                    fid = cur.lastrowid
                c.commit()
                return fid
            finally:
                c.close()

    def forget(self, key, scope=""):
        with _LOCK:
            c = self._conn()
            try:
                if not self._table_exists(c):
                    return 0
                n = c.execute("DELETE FROM facts WHERE scope=? AND key=?", (scope, key)).rowcount
                c.commit()
                return n
            finally:
                c.close()

    # -- reads (never create tables) --
    def all(self, scope=None, limit=200):
        c = self._conn()
        try:
            if not self._table_exists(c):
                return []
            if scope is None:
                rows = c.execute(
                    "SELECT * FROM facts ORDER BY importance DESC, updated_ts DESC LIMIT ?",
                    (limit,)).fetchall()
            else:
                rows = c.execute(
                    "SELECT * FROM facts WHERE scope IN ('', ?) "
                    "ORDER BY importance DESC, updated_ts DESC LIMIT ?",
                    (scope, limit)).fetchall()
            return [dict(r) for r in rows]
        finally:
            c.close()

    def search(self, query, scope=None, limit=10):
        """Substring match, case-folded so it works for Greek as well as ASCII
        (SQLite's LOWER() only folds ASCII)."""
        q = (query or "").strip().casefold()
        if not q:
            return []
        hits = [f for f in self.all(scope, limit=400) if q in f["value"].casefold()]
        return hits[:limit]

    def render(self, scope=None, budget=RENDER_CHAR_BUDGET, max_facts=RENDER_MAX_FACTS):
        """The `[FACTS]` block. Highest importance first, truncated by budget so a
        runaway store can never crowd out the conversation."""
        out, used = [], 0
        for f in self.all(scope, limit=max_facts * 3):
            line = f"- ({f['type']}) {f['value']}"
            if used + len(line) + 1 > budget or len(out) >= max_facts:
                break
            out.append(line)
            used += len(line) + 1
        return "\n".join(out)

    def count(self):
        c = self._conn()
        try:
            if not self._table_exists(c):
                return 0
            return c.execute("SELECT COUNT(*) FROM facts").fetchone()[0]
        finally:
            c.close()


# -- extraction --------------------------------------------------------------
EXTRACT_SYSTEM = """You extract DURABLE facts from one turn of a coding session.

A durable fact is something that must still govern behaviour many turns later:
a rule or threshold the user stated, a convention, a decision, a stated
preference, a gotcha discovered the hard way.

NOT durable: what the agent just did, file contents, task status, pleasantries,
anything true only inside this turn.

Output one fact per line, or the single word NONE. Format, exactly:

TYPE|KEY|IMPORTANCE|VALUE

TYPE       rule, convention, decision, preference, gotcha, reference
KEY        short dotted id, e.g. shipping.free_threshold
IMPORTANCE 0.1-1.0. A number, threshold or hard rule the user gave: 0.9.
           A correction of something the agent got wrong: 0.9.
           Ordinary conventions: 0.6. Background: 0.3.
VALUE      the fact as one self-contained sentence, including any number
           VERBATIM. "Free shipping over 50 euro" - never "the threshold
           discussed earlier".

At most 4 lines. Prefer NONE over a vague fact."""

# A user turn that corrects the agent is the single most expensive thing to
# forget, and the classifier never sees it as an action. Detect it locally and
# raise the floor on whatever gets extracted from that turn.
_CORRECTION_RX = re.compile(
    r"(?i)(?:^|\b)(no,|not like that|that'?s wrong|wrong,|don'?t do that|"
    r"i said|i told you|stop doing|never do|you broke|revert that|"
    r"οχι|λάθος|μην )")

_MAX_LINES = 4
CORRECTION_FLOOR = 0.9


def looks_like_correction(text):
    return bool(_CORRECTION_RX.search(text or ""))


def parse_extraction(raw):
    """Parse the extractor's output into fact dicts. Tolerant: a malformed line is
    skipped, never fatal — a bad extraction must not cost the turn."""
    out = []
    for line in (raw or "").splitlines():
        line = line.strip().lstrip("-*• ").strip()
        if not line or line.upper() == "NONE":
            continue
        parts = line.split("|")
        if len(parts) < 4:
            continue
        typ = parts[0].strip().lower()
        key = parts[1].strip()
        try:
            imp = max(0.0, min(1.0, float(parts[2].strip())))
        except ValueError:
            imp = 0.5
        value = "|".join(parts[3:]).strip()
        if not value:
            continue
        out.append({"type": typ if typ in TYPES else "note", "key": key,
                    "importance": imp, "value": value})
        if len(out) >= _MAX_LINES:
            break
    return out
