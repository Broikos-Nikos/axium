//! Session trajectory + opportunistic skill distillation.
//!
//! Every turn's (request, tools, outcome) is appended to a per-session JSONL
//! trace. That is useful on its own, "what did this agent actually do today",
//! and it is the raw material for the part that compounds: after a substantive
//! multi-step session, a gated pass distills the trace into a named skill under
//! `axium-skills/`, so a workflow performed once can be selected by name the next
//! time a similar request arrives.
//!
//! The gates matter more than the distillation. An agent that writes a skill
//! after every trivial turn fills its own selector prompt with noise, and noise
//! in the selector is worse than having no skills at all:
//!
//!   - at least `MIN_TURNS` turns in the session
//!   - at least `MIN_TOOLS` distinct tools used
//!   - at least one file actually changed
//!   - one distillation per session, ever
//!
//! Thresholds, the skill file layout and the distillation prompt match
//! `python/axium/trajectory.py`, so a skill distilled by one build is selectable
//! by the other.
//!
//! Everything here is best-effort and never propagates an error into the turn.

use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MIN_TURNS: usize = 3;
pub const MIN_TOOLS: usize = 4;
pub const MAX_TRACE_TURNS: usize = 40;

pub const DISTILL_SYSTEM: &str = r#"You turn a session of agent actions into ONE reusable skill: a named, general workflow the agent can follow again on a similar task.

Return STRICT JSON and nothing else:
{"name": "kebab-case-name", "description": "one line", "body": "numbered steps"}

The body is instructions the agent follows by calling its own tools - not code.
Generalise away one-off specifics: exact filenames, this project's name, this
session's numbers. Keep the ORDER and the CHECKS that made the session work.

If the session is too trivial or too one-off to generalise, return {"name": ""}."#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnRecord {
    pub ts: f64,
    pub request: String,
    pub tools: Vec<String>,
    pub changed: Vec<String>,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DistilledSkill {
    pub name: String,
    pub description: String,
    pub body: String,
}

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Truncate on a char boundary, byte-slicing a Greek or emoji value panics.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// `kebab-case-name`, and nothing else.
///
/// The name becomes a directory name and is then interpolated into the selector
/// prompt. Anything with a slash, a dot-dot or a space is rejected outright
/// rather than sanitised: a model that returned `../../etc/passwd` was not
/// producing a skill, and quietly repairing it hides that.
fn is_valid_skill_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let mut prev_dash = true; // leading dash is invalid
    for c in name.chars() {
        match c {
            'a'..='z' | '0'..='9' => prev_dash = false,
            '-' if !prev_dash => prev_dash = true,
            _ => return false,
        }
    }
    !prev_dash // trailing dash is invalid
}

pub struct Trajectory {
    pub session_id: String,
    trace_dir: PathBuf,
    turns: Vec<TurnRecord>,
    pub distilled: bool,
}

impl Trajectory {
    pub fn new(trace_dir: &str) -> Self {
        let dir = if trace_dir.is_empty() {
            dirs_trajectories()
        } else {
            PathBuf::from(trace_dir)
        };
        Self {
            session_id: Local::now().format("%Y%m%d_%H%M%S").to_string(),
            trace_dir: dir,
            turns: Vec::new(),
            distilled: false,
        }
    }

    pub fn path(&self) -> PathBuf {
        self.trace_dir.join(format!("{}.jsonl", self.session_id))
    }

    /// Append one turn. Best-effort: a failed write costs a trace line, not a turn.
    pub fn record(
        &mut self,
        request: &str,
        tools: &[String],
        changed: &[String],
        summary: &str,
        error: Option<&str>,
    ) {
        let mut changed_sorted: Vec<String> = changed.to_vec();
        changed_sorted.sort();
        let row = TurnRecord {
            ts: now_ts(),
            request: truncate_chars(request, 800),
            tools: tools.to_vec(),
            changed: changed_sorted,
            summary: truncate_chars(summary, 800),
            error: error.map(|e| e.to_string()),
        };

        // Write before trimming: the on-disk trace is the full session even when
        // the in-memory window has rolled. Losing the log is not the same as
        // losing the window.
        if fs::create_dir_all(&self.trace_dir).is_ok() {
            if let Ok(line) = serde_json::to_string(&row) {
                if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(self.path()) {
                    let _ = writeln!(f, "{line}");
                }
            }
        }

        self.turns.push(row);
        if self.turns.len() > MAX_TRACE_TURNS {
            self.turns.remove(0);
        }
    }

    // Public API of this module, exercised by its tests. No production
    // caller yet; kept because removing it would mean re-deriving it at the
    // first CLI command or diagnostic that needs it.
    #[allow(dead_code)]
    pub fn turns(&self) -> &[TurnRecord] {
        &self.turns
    }

    // ── gates ───────────────────────────────────────────────────────────────
    pub fn should_distill(&self) -> bool {
        if self.distilled || self.turns.len() < MIN_TURNS {
            return false;
        }
        let mut distinct: Vec<&str> = self
            .turns
            .iter()
            .flat_map(|t| t.tools.iter().map(|s| s.as_str()))
            .collect();
        distinct.sort_unstable();
        distinct.dedup();
        let changed = self.turns.iter().any(|t| !t.changed.is_empty());
        distinct.len() >= MIN_TOOLS && changed
    }

    /// The trace, as the distiller's user message.
    pub fn as_prompt(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        for (i, row) in self.turns.iter().enumerate() {
            lines.push(format!(
                "Turn {} request: {}",
                i + 1,
                truncate_chars(&row.request, 300)
            ));
            if !row.tools.is_empty() {
                lines.push(format!(
                    "  tools: {}",
                    row.tools
                        .iter()
                        .take(20)
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !row.changed.is_empty() {
                lines.push(format!(
                    "  changed: {}",
                    row.changed
                        .iter()
                        .take(10)
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if let Some(e) = &row.error {
                lines.push(format!("  error: {}", truncate_chars(e, 200)));
            }
            lines.push(format!("  result: {}", truncate_chars(&row.summary, 300)));
        }
        lines.join("\n")
    }
}

fn dirs_trajectories() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::new().join(home).join(".axium").join("trajectories")
}

/// Parse the distiller's JSON.
///
/// Returns `None` when it declined or produced junk. A malformed distillation is
/// dropped, never written half-formed: a broken skill is selected by name for
/// the rest of the install's life.
pub fn parse_skill(raw: &str) -> Option<DistilledSkill> {
    let mut text = raw.trim();
    if text.is_empty() {
        return None;
    }
    // Models fence their JSON despite being told not to.
    if text.starts_with("```") {
        text = text.trim_start_matches('`');
        if let Some(nl) = text.find('\n') {
            text = &text[nl + 1..];
        }
        if let Some(end) = text.rfind("```") {
            text = &text[..end];
        }
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let data: Value = serde_json::from_str(&text[start..=end]).ok()?;

    let name = data.get("name")?.as_str()?.trim().to_lowercase();
    let body = data.get("body").and_then(|v| v.as_str()).unwrap_or("").trim();
    if name.is_empty() || body.is_empty() || !is_valid_skill_name(&name) {
        return None;
    }
    Some(DistilledSkill {
        name,
        description: truncate_chars(
            data.get("description").and_then(|v| v.as_str()).unwrap_or("").trim(),
            200,
        ),
        body: truncate_chars(body, 8000),
    })
}

/// Write a distilled skill. Returns the path written, or "" when it declined.
///
/// Refuses to overwrite an existing folder: a skill a human edited must not be
/// silently replaced by a fresh distillation of a worse session.
pub fn write_skill(skill: &DistilledSkill, skills_root: &str) -> String {
    if !is_valid_skill_name(&skill.name) {
        return String::new(); // belt and braces: this becomes a directory name
    }
    let folder = Path::new(skills_root).join(&skill.name);
    if folder.is_dir() {
        return String::new();
    }
    if fs::create_dir_all(&folder).is_err() {
        return String::new();
    }
    let title = skill
        .name
        .split('-')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let path = folder.join("SKILL.md");
    let stamp = Local::now().format("%Y-%m-%d");
    let text = format!(
        "# {title}\n\n{}\n\n{}\n\n<!-- axium:distilled {stamp} -->\n",
        skill.description, skill.body
    );
    if fs::write(&path, text).is_err() {
        return String::new();
    }
    path.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "axium-traj-{}-{}-{:x}",
            name,
            std::process::id(),
            (now_ts() * 1000.0) as u64
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn substantive(t: &mut Trajectory) {
        t.record("read the pricing code", &["read_file".into(), "scan_project".into()], &[], "read it", None);
        t.record("fix the discount", &["patch_file".into()], &["shop/pricing.py".into()], "patched", None);
        t.record("run the tests", &["run_command".into()], &[], "green", None);
    }

    #[test]
    fn gates_reject_a_thin_session() {
        let d = tmp("thin");
        let mut t = Trajectory::new(d.to_str().unwrap());
        assert!(!t.should_distill(), "no turns");
        t.record("hi", &["read_file".into()], &[], "hello", None);
        assert!(!t.should_distill(), "one turn is not a workflow");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn gates_reject_a_session_that_changed_nothing() {
        let d = tmp("readonly");
        let mut t = Trajectory::new(d.to_str().unwrap());
        for i in 0..5 {
            t.record(
                &format!("question {i}"),
                &["read_file".into(), "search_files".into(), "scan_project".into(), "recall".into()],
                &[],
                "answered",
                None,
            );
        }
        assert!(!t.should_distill(), "a read-only session has no workflow to reuse");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn gates_reject_too_few_distinct_tools() {
        let d = tmp("fewtools");
        let mut t = Trajectory::new(d.to_str().unwrap());
        for i in 0..6 {
            t.record(&format!("edit {i}"), &["patch_file".into()], &["a.py".into()], "done", None);
        }
        assert!(!t.should_distill(), "one tool repeated is not a workflow");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_substantive_session_passes_the_gates_once() {
        let d = tmp("good");
        let mut t = Trajectory::new(d.to_str().unwrap());
        substantive(&mut t);
        assert!(t.should_distill());
        t.distilled = true;
        assert!(!t.should_distill(), "one distillation per session, ever");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn every_turn_is_written_to_the_jsonl_trace() {
        let d = tmp("jsonl");
        let mut t = Trajectory::new(d.to_str().unwrap());
        substantive(&mut t);
        let body = fs::read_to_string(t.path()).unwrap();
        assert_eq!(body.lines().count(), 3);
        let first: TurnRecord = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(first.request, "read the pricing code");
        assert!(first.error.is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_in_memory_window_rolls_but_the_log_keeps_everything() {
        let d = tmp("roll");
        let mut t = Trajectory::new(d.to_str().unwrap());
        for i in 0..MAX_TRACE_TURNS + 10 {
            t.record(&format!("turn {i}"), &["read_file".into()], &[], "ok", None);
        }
        assert_eq!(t.turns().len(), MAX_TRACE_TURNS, "window must be capped");
        let lines = fs::read_to_string(t.path()).unwrap().lines().count();
        assert_eq!(lines, MAX_TRACE_TURNS + 10, "the log must keep the whole session");
        assert!(t.turns()[0].request.contains("turn 10"), "oldest must be dropped first");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn as_prompt_includes_tools_changes_and_errors() {
        let d = tmp("prompt");
        let mut t = Trajectory::new(d.to_str().unwrap());
        t.record("do a thing", &["patch_file".into()], &["a.py".into()], "done", Some("boom"));
        let p = t.as_prompt();
        assert!(p.contains("Turn 1 request: do a thing"));
        assert!(p.contains("tools: patch_file"));
        assert!(p.contains("changed: a.py"));
        assert!(p.contains("error: boom"));
        assert!(p.contains("result: done"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn parse_skill_accepts_fenced_json() {
        let s = parse_skill("```json\n{\"name\": \"ship-it\", \"description\": \"d\", \"body\": \"1. go\"}\n```")
            .unwrap();
        assert_eq!(s.name, "ship-it");
        assert_eq!(s.body, "1. go");
    }

    #[test]
    fn parse_skill_accepts_prose_wrapped_json() {
        let s = parse_skill("Here you go:\n{\"name\": \"deploy-check\", \"body\": \"1. test\"}\nHope that helps.")
            .unwrap();
        assert_eq!(s.name, "deploy-check");
        assert_eq!(s.description, "");
    }

    #[test]
    fn parse_skill_rejects_junk_and_declines() {
        assert!(parse_skill("").is_none());
        assert!(parse_skill("not json at all").is_none());
        assert!(parse_skill("{\"name\": \"\"}").is_none(), "an explicit decline");
        assert!(parse_skill("{\"name\": \"ok-name\"}").is_none(), "no body");
        assert!(parse_skill("{\"name\": \"ok-name\", \"body\": \"  \"}").is_none());
        assert!(parse_skill("{broken json").is_none());
    }

    #[test]
    fn parse_skill_rejects_a_name_that_is_not_kebab_case() {
        for bad in [
            "{\"name\": \"Bad Name\", \"body\": \"x\"}",
            "{\"name\": \"under_score\", \"body\": \"x\"}",
            "{\"name\": \"-leading\", \"body\": \"x\"}",
            "{\"name\": \"trailing-\", \"body\": \"x\"}",
            "{\"name\": \"double--dash\", \"body\": \"x\"}",
        ] {
            assert!(parse_skill(bad).is_none(), "accepted: {bad}");
        }
    }

    #[test]
    fn parse_skill_rejects_a_path_traversal_name() {
        // A name becomes a directory. Repairing this quietly would hide it.
        assert!(parse_skill("{\"name\": \"../../etc/passwd\", \"body\": \"x\"}").is_none());
        assert!(parse_skill("{\"name\": \"a/b\", \"body\": \"x\"}").is_none());
        assert!(parse_skill("{\"name\": \"..\", \"body\": \"x\"}").is_none());
    }

    #[test]
    fn write_skill_creates_a_readable_folder() {
        let d = tmp("write");
        let skill = DistilledSkill {
            name: "deploy-and-verify".into(),
            description: "Test, then deploy".into(),
            body: "1. Run the tests\n2. Only then deploy".into(),
        };
        let path = write_skill(&skill, d.to_str().unwrap());
        assert!(!path.is_empty());
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.starts_with("# Deploy And Verify"));
        assert!(body.contains("Only then deploy"));
        assert!(body.contains("axium:distilled"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn write_skill_refuses_to_overwrite_an_existing_one() {
        let d = tmp("nooverwrite");
        let skill = DistilledSkill {
            name: "ship".into(),
            description: "d".into(),
            body: "1. go".into(),
        };
        assert!(!write_skill(&skill, d.to_str().unwrap()).is_empty());
        let human = d.join("ship").join("SKILL.md");
        fs::write(&human, "# Hand written\n").unwrap();

        assert!(write_skill(&skill, d.to_str().unwrap()).is_empty(), "must decline");
        assert_eq!(fs::read_to_string(&human).unwrap(), "# Hand written\n");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn write_skill_refuses_an_invalid_name_even_if_handed_one_directly() {
        let d = tmp("badname");
        let skill = DistilledSkill {
            name: "../escape".into(),
            description: "d".into(),
            body: "1. go".into(),
        };
        assert!(write_skill(&skill, d.to_str().unwrap()).is_empty());
        assert!(!d.parent().unwrap().join("escape").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn multibyte_request_does_not_panic() {
        let d = tmp("greek");
        let mut t = Trajectory::new(d.to_str().unwrap());
        let long = "καταστημα ".repeat(200);
        t.record(&long, &["read_file".into()], &[], &long, Some(&long));
        assert!(t.as_prompt().contains("καταστημα"));
        let _ = fs::remove_dir_all(&d);
    }
}
