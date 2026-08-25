//! Project Brain — durable per-project knowledge in `<project>/.axium/`.
//!
//! Axium re-derives a project's shape on every session. `scan_project` is cheap,
//! but the reasoning built on top of it is not: in the head-to-head benchmark the
//! "delete what we don't need, then put it back" scenario cost 47 tool calls,
//! most of them re-reading files the agent had already read in an earlier turn.
//!
//! The Brain makes that knowledge persist:
//!
//! ```text
//! <project>/.axium/
//!   PROFILE.md    stack, entry points, key files, conventions. Human-editable;
//!                 generated only when missing, and a human-written one is never
//!                 clobbered (the marker tells them apart).
//!   overview.md   annotated structure, rebuilt when the CODE changes rather than
//!                 on a wall-clock TTL — a fingerprint, not a timer.
//!   fingerprint   the hash overview.md was built from.
//!   journal.md    newest-first log of what changed and why, so "continue where
//!                 we left off" survives a restart.
//! ```
//!
//! File names, the marker and the budgets match `python/axium/brain.py` exactly:
//! one `.axium/` directory serves both builds, and a Brain written by the Rust
//! agent must be readable by the Python one for the two benchmarks to compare.
//!
//! Everything here is best-effort. A failed brain build must never break a turn,
//! so every public function degrades to an empty string rather than propagating.

use chrono::Local;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

pub const PROFILE_MARKER: &str = "<!-- axium:auto-profile -->";
pub const JOURNAL_MAX_ENTRIES: usize = 120;
pub const PRELOAD_CHAR_BUDGET: usize = 4000;
const MAX_FINGERPRINT_FILES: usize = 4000;
const OVERVIEW_MAX_CHARS: usize = 6000;

/// Extensions that count toward the change fingerprint (code plus templates).
const CODE_EXTS: [&str; 27] = [
    "py", "rs", "js", "jsx", "ts", "tsx", "mjs", "cjs", "go", "java", "rb", "php", "c", "h",
    "cpp", "hpp", "cs", "swift", "kt", "sql", "sh", "toml", "json", "yaml", "yml", "html", "css",
];

const SKIP_DIRS: [&str; 14] = [
    ".git",
    ".axium",
    "node_modules",
    "__pycache__",
    "target",
    "venv",
    ".venv",
    "dist",
    "build",
    ".idea",
    ".vscode",
    ".pytest_cache",
    ".ruff_cache",
    "vendor",
];

// ── paths ───────────────────────────────────────────────────────────────────
pub fn brain_dir(root: &str) -> PathBuf {
    Path::new(root).join(".axium")
}

pub fn ensure_brain_dir(root: &str) -> PathBuf {
    let d = brain_dir(root);
    let _ = fs::create_dir_all(&d);
    d
}

pub fn profile_path(root: &str) -> PathBuf {
    brain_dir(root).join("PROFILE.md")
}

pub fn overview_path(root: &str) -> PathBuf {
    brain_dir(root).join("overview.md")
}

pub fn fingerprint_path(root: &str) -> PathBuf {
    brain_dir(root).join("fingerprint")
}

pub fn journal_path(root: &str) -> PathBuf {
    brain_dir(root).join("journal.md")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Atomic write via a temp file, so a crash mid-write cannot leave a half-file
/// that the next session reads as truth.
fn write(path: &Path, text: &str) -> bool {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, text).is_err() {
        return false;
    }
    fs::rename(&tmp, path).is_ok()
}

/// Truncate on a char boundary — byte-slicing a Greek or emoji value panics.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

// ── fingerprint ─────────────────────────────────────────────────────────────
/// Hash of (relative path, size, mtime) over the project's code files.
///
/// A wall-clock TTL rebuilds an overview that is still correct and keeps a stale
/// one that is not. A content fingerprint rebuilds exactly when the code moved.
///
/// Returns "" when the project has no code files at all, which the callers treat
/// as "nothing to fingerprint" rather than as a change.
pub fn fingerprint(root: &str) -> String {
    let mut entries: Vec<String> = Vec::new();
    collect_fingerprint_entries(Path::new(root), Path::new(root), &mut entries);
    if entries.is_empty() {
        return String::new();
    }
    // Sorted so the hash depends on the tree, not on directory iteration order,
    // which is not stable across platforms or filesystems.
    entries.sort();
    let mut hasher = DefaultHasher::new();
    for e in &entries {
        e.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

fn collect_fingerprint_entries(root: &Path, dir: &Path, out: &mut Vec<String>) {
    if out.len() >= MAX_FINGERPRINT_FILES {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                continue;
            }
            subdirs.push(path);
            continue;
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !CODE_EXTS.contains(&ext.as_str()) {
            continue;
        }
        // Milliseconds, not seconds. A one-character edit keeps the file the same
        // size, and at second precision an edit made within the same second as the
        // last scan is invisible — the agent then reasons from a stale overview
        // for the rest of the session. Caught by a test that did exactly that.
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        out.push(format!("{}:{}:{}", rel, meta.len(), mtime));
        if out.len() >= MAX_FINGERPRINT_FILES {
            return;
        }
    }
    for sub in subdirs {
        collect_fingerprint_entries(root, &sub, out);
        if out.len() >= MAX_FINGERPRINT_FILES {
            return;
        }
    }
}

// Public API of this module, exercised by its tests. No production
// caller yet; kept because removing it would mean re-deriving it at the
// first CLI command or diagnostic that needs it.
#[allow(dead_code)]
pub fn is_stale(root: &str) -> bool {
    let fp = fingerprint(root);
    if fp.is_empty() {
        return false;
    }
    read(&fingerprint_path(root)).trim() != fp
}

// ── overview ────────────────────────────────────────────────────────────────
/// Rebuild `overview.md` from `scan` when the fingerprint moved.
///
/// `scan` is injected rather than calling `tools::project::scan_project`
/// directly, so this module stays free of the tool layer and is testable with a
/// stub that costs nothing.
pub fn build_overview<F>(root: &str, scan: F) -> String
where
    F: FnOnce(&str) -> String,
{
    let fp = fingerprint(root);
    if fp.is_empty() {
        return read(&overview_path(root));
    }
    if read(&fingerprint_path(root)).trim() == fp {
        let cached = read(&overview_path(root));
        if !cached.is_empty() {
            return cached;
        }
    }
    let body = truncate_chars(&scan(root), OVERVIEW_MAX_CHARS);
    if body.trim().is_empty() {
        return String::new();
    }
    let stamp = Local::now().format("%Y-%m-%d %H:%M");
    let short: String = fp.chars().take(8).collect();
    let text = format!("# Project overview\n\n_Rebuilt {stamp} (fingerprint {short})._\n\n{body}\n");
    if !write(&overview_path(root), &text) {
        return String::new();
    }
    // The fingerprint is written only after the overview lands, so a failure
    // between the two re-scans next time instead of trusting a missing file.
    write(&fingerprint_path(root), &fp);
    text
}

// ── profile ─────────────────────────────────────────────────────────────────
pub fn read_profile(root: &str) -> String {
    read(&profile_path(root))
}

/// Write `PROFILE.md`. An existing profile WITHOUT the auto marker was written by
/// a human and is never overwritten. Returns whether it wrote.
pub fn write_profile(root: &str, body: &str) -> bool {
    let existing = read(&profile_path(root));
    if !existing.trim().is_empty() && !existing.contains(PROFILE_MARKER) {
        return false;
    }
    write(
        &profile_path(root),
        &format!("{PROFILE_MARKER}\n# Project profile\n\n{}\n", body.trim()),
    )
}

// ── journal ─────────────────────────────────────────────────────────────────
/// Prepend one entry. Newest first, because that is the half anyone reads.
pub fn journal(root: &str, summary: &str, files: &[String], request: &str) -> bool {
    let summary = summary.trim();
    if summary.is_empty() {
        return false;
    }
    let stamp = Local::now().format("%Y-%m-%d %H:%M");
    let mut sorted: Vec<&String> = files.iter().collect();
    sorted.sort();
    let touched = if sorted.is_empty() {
        "(no files)".to_string()
    } else {
        sorted
            .iter()
            .take(12)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let entry = format!(
        "## {stamp}\n- request: {}\n- files: {touched}\n- result: {}\n",
        if request.trim().is_empty() {
            "(none)".to_string()
        } else {
            truncate_chars(request.trim(), 200)
        },
        truncate_chars(summary, 600)
    );

    let old = read(&journal_path(root));
    let body = old.strip_prefix("# Change journal\n\n").unwrap_or(&old);
    let kept: Vec<&str> = body
        .split("\n## ")
        .map(|e| e.trim_start_matches("## "))
        .filter(|e| !e.trim().is_empty())
        .take(JOURNAL_MAX_ENTRIES.saturating_sub(1))
        .collect();
    let rest = if kept.is_empty() {
        String::new()
    } else {
        format!("## {}", kept.join("\n## "))
    };
    write(
        &journal_path(root),
        &format!("# Change journal\n\n{entry}{rest}"),
    )
}

pub fn recent_journal(root: &str, n: usize) -> String {
    let body = read(&journal_path(root));
    if body.is_empty() {
        return String::new();
    }
    let stripped = body.strip_prefix("# Change journal\n\n").unwrap_or(&body);
    let entries: Vec<&str> = stripped
        .split("\n## ")
        .map(|e| e.trim_start_matches("## "))
        .filter(|e| !e.trim().is_empty())
        .take(n)
        .collect();
    if entries.is_empty() {
        return String::new();
    }
    format!("## {}", entries.join("\n## ")).trim_end().to_string()
}

// ── preload ─────────────────────────────────────────────────────────────────
/// The `[PROJECT BRAIN]` block: profile, then recent journal, then overview, in
/// that order of value per token.
///
/// Empty when the project has no brain yet, so a first-touch project pays nothing.
pub fn preload(root: &str) -> String {
    preload_with(root, PRELOAD_CHAR_BUDGET)
}

pub fn preload_with(root: &str, budget: usize) -> String {
    let mut parts: Vec<String> = Vec::new();

    let profile = read_profile(root).replace(PROFILE_MARKER, "").trim().to_string();
    if !profile.is_empty() {
        parts.push(profile);
    }
    let jour = recent_journal(root, 3);
    if !jour.is_empty() {
        parts.push(format!("## Recent changes\n{jour}"));
    }
    let overview = read(&overview_path(root)).trim().to_string();
    if !overview.is_empty() {
        parts.push(overview);
    }

    let mut out: Vec<String> = Vec::new();
    let mut used = 0usize;
    for p in parts {
        if used + p.len() > budget {
            let room = budget.saturating_sub(used);
            if room > 0 {
                out.push(truncate_chars(&p, room));
            }
            break;
        }
        used += p.len() + 2;
        out.push(p);
    }
    out.join("\n\n").trim().to_string()
}

// Public API of this module, exercised by its tests. No production
// caller yet; kept because removing it would mean re-deriving it at the
// first CLI command or diagnostic that needs it.
#[allow(dead_code)]
pub fn has_brain(root: &str) -> bool {
    profile_path(root).exists() || overview_path(root).exists() || journal_path(root).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch project with one code file. Dropped with the TempDir.
    struct Proj {
        dir: PathBuf,
    }

    impl Proj {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "axium-brain-{}-{}",
                name,
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("app.py"), "def total(x):\n    return x\n").unwrap();
            Self { dir }
        }

        fn root(&self) -> &str {
            self.dir.to_str().unwrap()
        }
    }

    impl Drop for Proj {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn fingerprint_changes_only_when_code_changes() {
        let p = Proj::new("fp");
        let a = fingerprint(p.root());
        assert!(!a.is_empty());
        assert_eq!(a, fingerprint(p.root()), "must be stable across calls");

        // A non-code file must not move it: otherwise every log write rebuilds.
        fs::write(p.dir.join("notes.txt"), "hello").unwrap();
        assert_eq!(a, fingerprint(p.root()));

        fs::write(p.dir.join("app.py"), "def total(x):\n    return x * 2\n").unwrap();
        assert_ne!(a, fingerprint(p.root()), "a code edit must move the fingerprint");
    }

    #[test]
    fn fingerprint_skips_vendor_and_build_dirs() {
        let p = Proj::new("skip");
        let a = fingerprint(p.root());
        let nm = p.dir.join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("huge.js"), "module.exports = 1;").unwrap();
        assert_eq!(a, fingerprint(p.root()), "node_modules must not count");
    }

    #[test]
    fn empty_project_has_no_fingerprint_and_is_not_stale() {
        let dir = std::env::temp_dir().join(format!("axium-brain-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let root = dir.to_str().unwrap();
        assert_eq!(fingerprint(root), "");
        assert!(!is_stale(root), "no code means nothing to rebuild");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overview_is_cached_until_the_code_moves() {
        let p = Proj::new("ov");
        let mut calls = 0;
        let scan = |_: &str| {
            calls += 1;
            "app.py: total".to_string()
        };
        let first = build_overview(p.root(), scan);
        assert!(first.contains("app.py: total"));
        assert_eq!(calls, 1);

        // Second call with an unchanged tree must NOT re-scan.
        let mut calls2 = 0;
        let cached = build_overview(p.root(), |_: &str| {
            calls2 += 1;
            "SHOULD NOT RUN".to_string()
        });
        assert_eq!(calls2, 0, "unchanged tree must reuse the cache");
        assert!(cached.contains("app.py: total"));

        // After a real edit it rebuilds.
        fs::write(p.dir.join("app.py"), "def total(x):\n    return x + 1\n").unwrap();
        let mut calls3 = 0;
        let rebuilt = build_overview(p.root(), |_: &str| {
            calls3 += 1;
            "app.py: total, helper".to_string()
        });
        assert_eq!(calls3, 1, "a code edit must force a re-scan");
        assert!(rebuilt.contains("helper"));
    }

    #[test]
    fn empty_scan_writes_nothing() {
        let p = Proj::new("emptyscan");
        assert_eq!(build_overview(p.root(), |_: &str| "   ".to_string()), "");
        assert!(!overview_path(p.root()).exists());
    }

    #[test]
    fn a_human_written_profile_is_never_clobbered() {
        let p = Proj::new("prof");
        ensure_brain_dir(p.root());

        assert!(write_profile(p.root(), "Stack: Python."));
        assert!(read_profile(p.root()).contains(PROFILE_MARKER));
        // An auto profile may be regenerated.
        assert!(write_profile(p.root(), "Stack: Python 3.12."));
        assert!(read_profile(p.root()).contains("3.12"));

        // A human edit removes the marker; from then on it is theirs.
        fs::write(profile_path(p.root()), "# My notes\nHand written.\n").unwrap();
        assert!(!write_profile(p.root(), "auto would say this"));
        assert!(read_profile(p.root()).contains("Hand written."));
    }

    #[test]
    fn journal_is_newest_first_and_capped() {
        let p = Proj::new("jour");
        assert!(journal(p.root(), "first change", &["a.py".into()], "do a"));
        assert!(journal(p.root(), "second change", &["b.py".into()], "do b"));

        let recent = recent_journal(p.root(), 3);
        let first_pos = recent.find("first change").unwrap();
        let second_pos = recent.find("second change").unwrap();
        assert!(second_pos < first_pos, "newest must come first:\n{recent}");

        for i in 0..JOURNAL_MAX_ENTRIES + 20 {
            journal(p.root(), &format!("change {i}"), &[], "");
        }
        let body = read(&journal_path(p.root()));
        let count = body.matches("\n- result:").count();
        assert!(count <= JOURNAL_MAX_ENTRIES, "journal grew unbounded: {count}");
    }

    #[test]
    fn empty_summary_writes_no_journal_entry() {
        let p = Proj::new("jempty");
        assert!(!journal(p.root(), "   ", &["a.py".into()], "req"));
        assert_eq!(recent_journal(p.root(), 3), "");
    }

    #[test]
    fn preload_is_empty_for_a_fresh_project() {
        let p = Proj::new("fresh");
        assert_eq!(preload(p.root()), "", "a first-touch project must cost nothing");
        assert!(!has_brain(p.root()));
    }

    #[test]
    fn preload_orders_profile_then_journal_then_overview() {
        let p = Proj::new("order");
        write_profile(p.root(), "Stack: Python.");
        journal(p.root(), "did a thing", &["app.py".into()], "req");
        build_overview(p.root(), |_: &str| "app.py: total".to_string());

        let out = preload(p.root());
        let prof = out.find("Stack: Python.").unwrap();
        let jour = out.find("Recent changes").unwrap();
        let ovr = out.find("Project overview").unwrap();
        assert!(prof < jour && jour < ovr, "wrong order:\n{out}");
        assert!(!out.contains(PROFILE_MARKER), "the marker is plumbing, not context");
        assert!(has_brain(p.root()));
    }

    #[test]
    fn preload_respects_its_budget() {
        let p = Proj::new("budget");
        write_profile(p.root(), &"x".repeat(9000));
        let out = preload(p.root());
        assert!(out.len() <= PRELOAD_CHAR_BUDGET, "budget blown: {}", out.len());
        assert!(!out.is_empty(), "a truncated profile is still worth sending");
    }

    #[test]
    fn is_stale_flips_after_an_edit_and_clears_after_a_rebuild() {
        let p = Proj::new("stale");
        assert!(is_stale(p.root()), "no fingerprint on file yet");
        build_overview(p.root(), |_: &str| "app.py: total".to_string());
        assert!(!is_stale(p.root()));
        fs::write(p.dir.join("app.py"), "def total(x):\n    return 0\n").unwrap();
        assert!(is_stale(p.root()));
    }

    #[test]
    fn multibyte_summary_does_not_panic() {
        let p = Proj::new("greek");
        let long_greek = "καταστημα ".repeat(200);
        assert!(journal(p.root(), &long_greek, &[], &long_greek));
        assert!(recent_journal(p.root(), 1).contains("καταστημα"));
    }
}
