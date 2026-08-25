//! Typed, importance-scored facts — the half of memory the model does not have
//! to remember to write.
//!
//! `memory::store::Memory` is a markdown file the agent edits deliberately via
//! `update_memory`. That works for facts the agent *notices* are durable, and
//! fails for the ones a user drops mid-sentence: "shipping is free over 50 euro"
//! stated in turn 1 is gone by turn 6, because nothing ever called a tool about
//! it and compaction summarised the turn away.
//!
//! This module closes that gap. After every turn a cheap-model pass extracts
//! durable statements into typed rows, which render into a `[FACTS]` block in the
//! SYSTEM prompt. Compaction rewrites history; it cannot touch the system prompt,
//! so a fact captured in turn 1 is still verbatim in front of the model in turn 20.
//!
//! The schema is byte-identical to `python/axium/facts.py`, deliberately: one
//! `facts.db` serves both builds, and the two benchmarks are only comparable if
//! the two agents remember the same things the same way.

use anyhow::Result;
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const TYPES: [&str; 7] = [
    "rule",
    "convention",
    "decision",
    "preference",
    "gotcha",
    "reference",
    "note",
];

/// The `[FACTS]` block is paid for on every single call of the loop. 1800 chars
/// is roughly 500 tokens: enough for two dozen real facts, small enough that it
/// never competes with the conversation for the window.
pub const RENDER_CHAR_BUDGET: usize = 1800;
pub const RENDER_MAX_FACTS: usize = 24;

/// A user correction is the most expensive thing to forget, and the classifier
/// never sees it as an action. Floor whatever is extracted from such a turn.
pub const CORRECTION_FLOOR: f64 = 0.9;

const REDACTED: &str = "<redacted>";
const MAX_EXTRACTED_LINES: usize = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub id: i64,
    pub scope: String,
    pub kind: String,
    pub key: String,
    pub value: String,
    pub importance: f64,
    pub source: String,
    pub updated_ts: f64,
}

/// One extracted-but-not-yet-stored fact.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedFact {
    pub kind: String,
    pub key: String,
    pub importance: f64,
    pub value: String,
}

// ── regex singletons ────────────────────────────────────────────────────────
// Compiled once. A `Regex::new` inside the per-turn render path would rebuild
// these on every call of the loop.
fn sk_key_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r"(?i)\bsk-[A-Za-z0-9_\-.]{10,}\b").unwrap())
}

fn cred_assign_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| {
        Regex::new(
            r"(?i)\b(password|passwd|pwd|passphrase|api[ _-]?key|secret[ _-]?key|secret|token|credentials?)\b\s*(?:=|:=|:|->|\bis\b|\bare\b)\s*[\x22'`]?([^,;\n\x22'`]{4,200})",
        )
        .unwrap()
    })
}

fn correction_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| {
        Regex::new(
            r"(?i)(no,|not like that|that'?s wrong|wrong,|don'?t do that|i said|i told you|stop doing|never do|you broke|revert that|\u{03bf}\u{03c7}\u{03b9}|\u{03bb}\u{03ac}\u{03b8}\u{03bf}\u{03c2}|\u{03bc}\u{03b7}\u{03bd} )",
        )
        .unwrap()
    })
}

fn key_word_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r"[a-z0-9]+").unwrap())
}

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Redact anything credential-shaped before it can be persisted.
///
/// Facts render into the system prompt on every request, and nothing here was
/// reviewed by a human first — this store is written automatically.
pub fn sanitize(value: &str) -> String {
    let stage1 = sk_key_rx().replace_all(value, REDACTED).into_owned();
    cred_assign_rx()
        .replace_all(&stage1, |c: &regex::Captures| {
            format!("{}: {}", &c[1], REDACTED)
        })
        .into_owned()
}

/// A stable key from the value, so the same fact restated collapses instead of
/// accumulating near-duplicates.
pub fn derive_key(value: &str) -> String {
    let lower = value.to_lowercase();
    let words: Vec<&str> = key_word_rx()
        .find_iter(&lower)
        .take(6)
        .map(|m| m.as_str())
        .collect();
    if words.is_empty() {
        return "fact".to_string();
    }
    let joined = words.join(".");
    truncate_chars(&joined, 80)
}

pub fn looks_like_correction(text: &str) -> bool {
    correction_rx().is_match(text)
}

/// Truncate on a char boundary. Rust byte-slicing a multibyte string panics, and
/// Greek is a first-class case here.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

fn normalise_kind(kind: &str) -> String {
    let k = kind.trim().to_lowercase();
    if TYPES.contains(&k.as_str()) {
        k
    } else {
        "note".to_string()
    }
}

// ── the store ───────────────────────────────────────────────────────────────
pub struct FactStore {
    conn: Mutex<Connection>,
}

impl FactStore {
    /// Open (creating the file and schema if needed).
    ///
    /// Unlike the Python side, the table is created on open rather than lazily:
    /// the Rust build shares one long-lived store across a session, so there is
    /// no per-turn read path that a `CREATE TABLE` could make side-effecting.
    pub fn open(path: &str) -> Result<Self> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS facts (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 scope TEXT NOT NULL DEFAULT '',
                 type TEXT NOT NULL DEFAULT 'note',
                 key TEXT NOT NULL,
                 value TEXT NOT NULL,
                 importance REAL NOT NULL DEFAULT 0.5,
                 source TEXT DEFAULT '',
                 hits INTEGER NOT NULL DEFAULT 0,
                 created_ts REAL NOT NULL,
                 updated_ts REAL NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_facts_key ON facts(scope, key);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Test-only: an in-memory store, so a test never touches a real facts.db.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS facts (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 scope TEXT NOT NULL DEFAULT '',
                 type TEXT NOT NULL DEFAULT 'note',
                 key TEXT NOT NULL,
                 value TEXT NOT NULL,
                 importance REAL NOT NULL DEFAULT 0.5,
                 source TEXT DEFAULT '',
                 hits INTEGER NOT NULL DEFAULT 0,
                 created_ts REAL NOT NULL,
                 updated_ts REAL NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_facts_key ON facts(scope, key);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Insert or update one fact.
    ///
    /// Re-stating a known key keeps the HIGHER importance: a rule restated
    /// casually must not demote the emphatic version that was stored first.
    pub fn remember(
        &self,
        value: &str,
        kind: &str,
        key: &str,
        importance: f64,
        scope: &str,
        source: &str,
    ) -> Result<Option<i64>> {
        let value = sanitize(value.trim());
        if value.is_empty() {
            return Ok(None);
        }
        let kind = normalise_kind(kind);
        let key = if key.trim().is_empty() {
            derive_key(&value)
        } else {
            truncate_chars(key.trim(), 80)
        };
        let importance = importance.clamp(0.0, 1.0);
        let now = now_ts();

        let conn = self.conn();
        let existing: Option<(i64, f64)> = conn
            .query_row(
                "SELECT id, importance FROM facts WHERE scope=?1 AND key=?2",
                params![scope, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        match existing {
            Some((id, old_importance)) => {
                conn.execute(
                    "UPDATE facts SET value=?1, type=?2, importance=?3, source=?4, updated_ts=?5 \
                     WHERE id=?6",
                    params![
                        value,
                        kind,
                        importance.max(old_importance),
                        source,
                        now,
                        id
                    ],
                )?;
                Ok(Some(id))
            }
            None => {
                conn.execute(
                    "INSERT INTO facts(scope,type,key,value,importance,source,created_ts,updated_ts) \
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![scope, kind, key, value, importance, source, now, now],
                )?;
                Ok(Some(conn.last_insert_rowid()))
            }
        }
    }

    // Public API of this module, exercised by its tests. No production
    // caller yet; kept because removing it would mean re-deriving it at the
    // first CLI command or diagnostic that needs it.
    #[allow(dead_code)]
    pub fn forget(&self, key: &str, scope: &str) -> Result<usize> {
        let conn = self.conn();
        Ok(conn.execute(
            "DELETE FROM facts WHERE scope=?1 AND key=?2",
            params![scope, key],
        )?)
    }

    /// Facts visible from `scope`: that scope's own, plus the unscoped ones.
    /// Pass `None` to list every scope (used by the CLI, never by the prompt).
    pub fn all(&self, scope: Option<&str>, limit: usize) -> Result<Vec<Fact>> {
        let conn = self.conn();
        // Two statements rather than one with a nullable scope: `scope IN ('', NULL)`
        // is not "match everything", it silently matches only the unscoped rows.
        let sql = match scope {
            Some(_) => {
                "SELECT id,scope,type,key,value,importance,source,updated_ts FROM facts \
                 WHERE scope IN ('', ?1) ORDER BY importance DESC, updated_ts DESC LIMIT ?2"
            }
            None => {
                "SELECT id,scope,type,key,value,importance,source,updated_ts FROM facts \
                 ORDER BY importance DESC, updated_ts DESC LIMIT ?1"
            }
        };
        let mut stmt = conn.prepare(sql)?;
        let map = |r: &rusqlite::Row<'_>| {
            Ok(Fact {
                id: r.get(0)?,
                scope: r.get(1)?,
                kind: r.get(2)?,
                key: r.get(3)?,
                value: r.get(4)?,
                importance: r.get(5)?,
                source: r.get(6).unwrap_or_default(),
                updated_ts: r.get(7)?,
            })
        };
        let rows = match scope {
            Some(s) => stmt
                .query_map(params![s, limit as i64], map)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            None => stmt
                .query_map(params![limit as i64], map)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    /// Substring match, case-folded in Rust rather than in SQL.
    ///
    /// SQLite's `LOWER()` only folds ASCII, so a Greek query would silently miss
    /// a Greek fact. The store is small and capped, so scanning is fine.
    pub fn search(&self, query: &str, scope: Option<&str>, limit: usize) -> Result<Vec<Fact>> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return self.all(scope, limit);
        }
        let candidates = self.all(scope, 400)?;
        Ok(candidates
            .into_iter()
            .filter(|f| f.value.to_lowercase().contains(&q))
            .take(limit)
            .collect())
    }

    /// The `[FACTS]` block. Highest importance first, truncated by budget so a
    /// runaway store can never crowd out the conversation.
    pub fn render(&self, scope: Option<&str>) -> Result<String> {
        self.render_with(scope, RENDER_CHAR_BUDGET, RENDER_MAX_FACTS)
    }

    pub fn render_with(
        &self,
        scope: Option<&str>,
        budget: usize,
        max_facts: usize,
    ) -> Result<String> {
        let rows = self.all(scope, max_facts.saturating_mul(3))?;
        let mut out: Vec<String> = Vec::new();
        let mut used = 0usize;
        for f in rows {
            let line = format!("- ({}) {}", f.kind, f.value);
            if used + line.len() + 1 > budget || out.len() >= max_facts {
                break;
            }
            used += line.len() + 1;
            out.push(line);
        }
        Ok(out.join("\n"))
    }

    // Public API of this module, exercised by its tests. No production
    // caller yet; kept because removing it would mean re-deriving it at the
    // first CLI command or diagnostic that needs it.
    #[allow(dead_code)]
    pub fn count(&self) -> Result<i64> {
        let conn = self.conn();
        Ok(conn.query_row("SELECT COUNT(*) FROM facts", [], |r| r.get(0))?)
    }
}

// ── extraction ──────────────────────────────────────────────────────────────
pub const EXTRACT_SYSTEM: &str = r#"You extract DURABLE facts from one turn of a coding session.

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

At most 4 lines. Prefer NONE over a vague fact."#;

/// Parse the extractor's output.
///
/// Tolerant by design: a malformed line is skipped, never fatal. A bad
/// extraction must cost a fact, not the turn that already succeeded.
pub fn parse_extraction(raw: &str) -> Vec<ExtractedFact> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim().trim_start_matches(['-', '*', '\u{2022}']).trim();
        if line.is_empty() || line.eq_ignore_ascii_case("NONE") {
            continue;
        }
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 4 {
            continue;
        }
        let importance = parts[2].trim().parse::<f64>().unwrap_or(0.5).clamp(0.0, 1.0);
        let value = parts[3..].join("|").trim().to_string();
        if value.is_empty() {
            continue;
        }
        out.push(ExtractedFact {
            kind: normalise_kind(parts[0]),
            key: parts[1].trim().to_string(),
            importance,
            value,
        });
        if out.len() >= MAX_EXTRACTED_LINES {
            break;
        }
    }
    out
}

/// Turn a failed turn into a `gotcha`, or `None` when there is nothing to learn.
///
/// Deliberately narrow: only failures with a concrete cause are worth the prompt
/// space they will occupy on every future turn.
pub fn mine_failure(request: &str, error: &str, tool_log: &str) -> Option<ExtractedFact> {
    let err = error.trim();
    if err.chars().count() < 12 {
        return None;
    }
    let head: String = truncate_chars(err.lines().next().unwrap_or(""), 200);
    if !head.chars().any(|c| c.is_alphabetic()) {
        return None;
    }
    let ctx = if request.trim().is_empty() {
        String::new()
    } else {
        format!(" while: {}", truncate_chars(request.trim(), 120))
    };
    let tools = if tool_log.is_empty() {
        String::new()
    } else {
        format!(" (tools: {})", truncate_chars(tool_log, 80))
    };
    let slug: String = head
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '.' })
        .collect();
    let slug = truncate_chars(&slug, 60);
    let slug = slug.trim_matches('.').to_string();
    Some(ExtractedFact {
        kind: "gotcha".to_string(),
        key: format!("fail.{}", slug),
        importance: 0.7,
        value: format!("Previously failed{}: {}{}", ctx, head, tools),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> FactStore {
        FactStore::open_in_memory().unwrap()
    }

    #[test]
    fn restating_a_key_keeps_the_higher_importance() {
        let s = store();
        s.remember("Free shipping over 50.", "rule", "ship.free", 0.9, "", "t")
            .unwrap();
        s.remember("Free shipping over 50.", "rule", "ship.free", 0.2, "", "t")
            .unwrap();
        let all = s.all(None, 10).unwrap();
        assert_eq!(all.len(), 1, "the second write must UPDATE, not insert");
        assert!((all[0].importance - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn credentials_are_redacted_before_persistence() {
        let s = store();
        s.remember("The FTP password is hunter2xyz", "note", "", 0.5, "", "t")
            .unwrap();
        s.remember("key sk-ant-abcdefghijklmno is live", "note", "k2", 0.5, "", "t")
            .unwrap();
        let values: Vec<String> = s.all(None, 10).unwrap().into_iter().map(|f| f.value).collect();
        let joined = values.join(" ");
        assert!(!joined.contains("hunter2xyz"), "{joined}");
        assert!(!joined.contains("sk-ant-abcdefghijklmno"), "{joined}");
        assert!(joined.contains(REDACTED), "{joined}");
    }

    #[test]
    fn render_is_ordered_by_importance_and_respects_the_budget() {
        let s = store();
        s.remember("Low value note.", "note", "a", 0.1, "", "t").unwrap();
        s.remember("Critical rule here.", "rule", "b", 0.95, "", "t").unwrap();
        let rendered = s.render(None).unwrap();
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines[0].contains("Critical rule"), "{rendered}");

        for i in 0..40 {
            s.remember(&format!("Filler fact number {i} padded out to some length."),
                       "note", &format!("f{i}"), 0.5, "", "t").unwrap();
        }
        let big = s.render(None).unwrap();
        assert!(big.len() <= RENDER_CHAR_BUDGET, "budget blown: {}", big.len());
        assert!(big.lines().count() <= RENDER_MAX_FACTS);
    }

    #[test]
    fn scope_sees_its_own_facts_and_the_unscoped_ones_only() {
        let s = store();
        s.remember("Global convention.", "convention", "g", 0.5, "", "t").unwrap();
        s.remember("Shop rule.", "rule", "s", 0.5, "shop", "t").unwrap();
        s.remember("Blog rule.", "rule", "b", 0.5, "blog", "t").unwrap();
        let shop = s.all(Some("shop"), 20).unwrap();
        let values: Vec<&str> = shop.iter().map(|f| f.value.as_str()).collect();
        assert!(values.contains(&"Global convention."));
        assert!(values.contains(&"Shop rule."));
        assert!(!values.contains(&"Blog rule."), "scopes must not leak: {values:?}");
    }

    #[test]
    fn search_folds_case_including_greek() {
        let s = store();
        s.remember("Το κατάστημα κλείνει στις 9.", "note", "gr", 0.5, "", "t")
            .unwrap();
        assert_eq!(s.search("ΚΑΤΆΣΤΗΜΑ", None, 5).unwrap().len(), 1);
        assert_eq!(s.search("nothing here", None, 5).unwrap().len(), 0);
    }

    #[test]
    fn empty_value_is_not_stored() {
        let s = store();
        assert!(s.remember("   ", "note", "k", 0.5, "", "t").unwrap().is_none());
        assert_eq!(s.count().unwrap(), 0);
    }

    #[test]
    fn derived_key_collapses_the_same_fact_restated() {
        let s = store();
        s.remember("Free shipping over 50 euro.", "rule", "", 0.5, "", "t").unwrap();
        s.remember("Free shipping over 50 euro.", "rule", "", 0.5, "", "t").unwrap();
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn parse_extraction_survives_junk() {
        let rows = parse_extraction(
            "rule|a.b|0.9|Real fact.\ngarbage line\n|||\nnote|c|notanumber|Second.",
        );
        assert_eq!(rows.len(), 2);
        assert!((rows[1].importance - 0.5).abs() < f64::EPSILON);
        assert_eq!(rows[0].kind, "rule");
    }

    #[test]
    fn parse_extraction_caps_at_four_and_honours_none() {
        assert!(parse_extraction("NONE").is_empty());
        let many = (0..10)
            .map(|i| format!("note|k{i}|0.5|Fact {i}."))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(parse_extraction(&many).len(), MAX_EXTRACTED_LINES);
    }

    #[test]
    fn unknown_type_falls_back_to_note() {
        let rows = parse_extraction("nonsense|k|0.5|Value.");
        assert_eq!(rows[0].kind, "note");
    }

    #[test]
    fn correction_detection_en_and_el() {
        assert!(looks_like_correction("No, that's wrong"));
        assert!(looks_like_correction("οχι, μην το κανεις ετσι"));
        assert!(!looks_like_correction("add a test for the parser"));
    }

    #[test]
    fn mine_failure_ignores_noise_and_catches_real_errors() {
        assert!(mine_failure("x", "short", "").is_none());
        assert!(mine_failure("x", "!!!!!!!!!!!!!!!", "").is_none());
        let g = mine_failure("run the migration", "ConnectionError: refused by host", "run_command")
            .unwrap();
        assert_eq!(g.kind, "gotcha");
        assert!(g.key.starts_with("fail."));
        assert!(g.value.contains("refused by host"));
    }

    #[test]
    fn derive_key_handles_a_value_with_no_ascii_words() {
        assert_eq!(derive_key("!!! ???"), "fact");
        assert_eq!(derive_key("Free shipping over 50 euro"), "free.shipping.over.50.euro");
    }

    #[test]
    fn long_multibyte_value_does_not_panic_on_truncation() {
        let s = store();
        let greek = "καταστημα ".repeat(60);
        s.remember(&greek, "note", "", 0.5, "", "t").unwrap();
        assert_eq!(s.count().unwrap(), 1);
    }
}
