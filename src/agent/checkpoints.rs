//! Turn-level checkpoints — revert a whole turn's file changes in one call.
//!
//! The blast-radius benchmark asks an agent to delete things, then to put them
//! back "exactly". Axium scored the recovery but spent 47 tool calls doing it: it
//! re-read every file it had removed and rebuilt them from memory, which is both
//! expensive and the one path where "exactly" is a coin flip.
//!
//! A checkpoint records the PRE-state of every file a turn touches, before the
//! write lands. `undo` then restores edited files byte-for-byte and deletes files
//! the turn created. One tool call, no reconstruction, no guessing.
//!
//! Snapshots are per-session, never process-global: the benchmark runs turns
//! concurrently and a global checkpoint would let one scenario undo another's
//! work. `Checkpoints` is therefore an owned value handed to the turn context,
//! matching `python/axium/checkpoints.py`.
//!
//! Everything is best-effort. A checkpoint failure must never block the real
//! edit, so `record` swallows its errors and the turn proceeds without an undo
//! point rather than failing.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Per file. A 100MB blob is not undo material, and copying one per turn would
/// dominate the cost of the turn itself.
pub const MAX_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_CHECKPOINTS: usize = 20;

/// What a turn did to one path.
#[derive(Debug, Clone, PartialEq)]
enum PreState {
    /// The file existed; these are its bytes, parked here.
    Snapshot(PathBuf),
    /// The path did not exist before this turn, so undo means deleting it.
    Absent,
}

#[derive(Debug, Clone)]
struct Checkpoint {
    id: String,
    label: String,
    /// BTreeMap, not HashMap: `undo` reports restored/deleted paths and a report
    /// whose order changes between runs is noise in a benchmark diff.
    files: BTreeMap<String, PreState>,
    ts: f64,
}

/// One entry in the `action="list"` output.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointInfo {
    pub id: String,
    pub label: String,
    pub files: usize,
    pub age_s: f64,
}

/// The result of an undo. `failed` is separate from `ok` on purpose: a partial
/// restore must be reported as a partial restore, not rounded to success.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UndoReport {
    pub ok: bool,
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
    pub failed: Vec<String>,
    pub error: String,
}

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub struct Checkpoints {
    workdir: PathBuf,
    store_dir: PathBuf,
    stack: Vec<Checkpoint>,
    active: Option<Checkpoint>,
    /// Monotonic counter appended to the id. Two checkpoints opened inside the
    /// same millisecond would otherwise share an id, and `undo(id)` would pick
    /// the wrong one.
    seq: u64,
}

impl Checkpoints {
    pub fn new(workdir: &str) -> Self {
        let workdir = PathBuf::from(workdir);
        let store_dir = workdir.join(".axium").join("checkpoints");
        Self {
            workdir,
            store_dir,
            stack: Vec::new(),
            active: None,
            seq: 0,
        }
    }

    /// Test-only: an isolated snapshot store, so a test never writes into a
    /// real project's `.axium/checkpoints`.
    #[cfg(test)]
    pub fn with_store(workdir: &str, store_dir: &str) -> Self {
        Self {
            workdir: PathBuf::from(workdir),
            store_dir: PathBuf::from(store_dir),
            stack: Vec::new(),
            active: None,
            seq: 0,
        }
    }

    // ── lifecycle ───────────────────────────────────────────────────────────
    /// Open a checkpoint for the turn.
    ///
    /// A previous uncommitted one is committed first, so a turn that errored
    /// before `commit` is still undoable.
    pub fn begin(&mut self, label: &str) -> String {
        if self
            .active
            .as_ref()
            .is_some_and(|c| !c.files.is_empty())
        {
            self.commit();
        }
        self.seq += 1;
        let id = format!("{:x}-{}", (now_ts() * 1000.0) as u64, self.seq);
        let label: String = label.chars().take(120).collect();
        self.active = Some(Checkpoint {
            id: id.clone(),
            label,
            files: BTreeMap::new(),
            ts: now_ts(),
        });
        id
    }

    /// Close the active checkpoint and push it on the stack, if it touched
    /// anything. Returns how many files it can restore.
    pub fn commit(&mut self) -> usize {
        let Some(cp) = self.active.take() else {
            return 0;
        };
        if cp.files.is_empty() {
            return 0;
        }
        let n = cp.files.len();
        self.stack.push(cp);
        while self.stack.len() > MAX_CHECKPOINTS {
            let old = self.stack.remove(0);
            self.discard(&old);
        }
        n
    }

    // Public API of this module, exercised by its tests. No production
    // caller yet; kept because removing it would mean re-deriving it at the
    // first CLI command or diagnostic that needs it.
    #[allow(dead_code)]
    pub fn touched_count(&self) -> usize {
        self.active.as_ref().map_or(0, |c| c.files.len())
    }

    // ── recording ───────────────────────────────────────────────────────────
    /// Snapshot `path` BEFORE it is written.
    ///
    /// Call once per file per turn: a second call must not overwrite the original
    /// pre-state with the half-edited version, which would make undo restore the
    /// intermediate rather than the original.
    pub fn record(&mut self, path: &str) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let full = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.workdir.join(path)
        };
        let rel = self.rel(&full);
        if active.files.contains_key(&rel) {
            return; // first write of the turn already captured the truth
        }

        let state = match fs::metadata(&full) {
            Err(_) => PreState::Absent, // created by this turn
            Ok(meta) if meta.is_dir() => PreState::Absent,
            Ok(meta) if meta.len() > MAX_SNAPSHOT_BYTES => return, // too big to park
            Ok(_) => {
                let snap = self
                    .store_dir
                    .join(&active.id)
                    .join(rel.replace(['/', '\\'], "__"));
                if let Some(parent) = snap.parent() {
                    if fs::create_dir_all(parent).is_err() {
                        return; // never block the real edit
                    }
                }
                if fs::copy(&full, &snap).is_err() {
                    return;
                }
                PreState::Snapshot(snap)
            }
        };
        if let Some(active) = self.active.as_mut() {
            active.files.insert(rel, state);
        }
    }

    // ── undo ────────────────────────────────────────────────────────────────
    /// Restore the most recent checkpoint, or a named one.
    pub fn undo(&mut self, checkpoint_id: &str) -> UndoReport {
        if self.active.as_ref().is_some_and(|c| !c.files.is_empty()) {
            self.commit();
        }
        let idx = if checkpoint_id.is_empty() {
            self.stack.len().checked_sub(1)
        } else {
            self.stack.iter().rposition(|c| c.id == checkpoint_id)
        };
        let Some(idx) = idx else {
            return UndoReport {
                ok: false,
                error: "no checkpoint to undo".into(),
                ..Default::default()
            };
        };
        let cp = self.stack.remove(idx);

        let mut report = UndoReport::default();
        for (rel, state) in &cp.files {
            let full = self.workdir.join(rel);
            match state {
                PreState::Absent => {
                    let outcome = if full.is_dir() {
                        fs::remove_dir_all(&full)
                    } else if full.is_file() {
                        fs::remove_file(&full)
                    } else {
                        Ok(()) // already gone: nothing to undo, not a failure
                    };
                    match outcome {
                        Ok(()) => {
                            if !full.exists() {
                                report.deleted.push(rel.clone());
                            }
                        }
                        Err(e) => report.failed.push(format!("{rel}: {e}")),
                    }
                }
                PreState::Snapshot(snap) => {
                    if let Some(parent) = full.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    match fs::copy(snap, &full) {
                        Ok(_) => report.restored.push(rel.clone()),
                        Err(e) => report.failed.push(format!("{rel}: {e}")),
                    }
                }
            }
        }
        report.ok = report.failed.is_empty();
        self.discard(&cp);
        report
    }

    /// Workdir-relative paths the most recently committed checkpoint touched.
    ///
    /// This is the turn's "what did it change" set, derived from the snapshots
    /// rather than tracked separately. Effect-based and slightly broad: a
    /// mutating tool that errored still recorded a pre-state, so the path appears
    /// here. For the journal that is the right bias — "the turn touched this
    /// file" is what a reader wants next week.
    pub fn last_files(&self) -> Vec<String> {
        self.stack
            .last()
            .map(|c| c.files.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn list(&self) -> Vec<CheckpointInfo> {
        let now = now_ts();
        self.stack
            .iter()
            .rev()
            .map(|c| CheckpointInfo {
                id: c.id.clone(),
                label: c.label.clone(),
                files: c.files.len(),
                age_s: ((now - c.ts) * 10.0).round() / 10.0,
            })
            .collect()
    }

    // ── internals ───────────────────────────────────────────────────────────
    /// Workdir-relative, forward-slashed, with `.`/`..` collapsed.
    ///
    /// Normalising matters: `record("./app.py")` and `record("app.py")` are the
    /// same file, and if they key differently the second one snapshots the
    /// already-edited version and undo restores the wrong bytes.
    fn rel(&self, full: &Path) -> String {
        let normalised = normalise(full);
        let base = normalise(&self.workdir);
        normalised
            .strip_prefix(&base)
            .unwrap_or(&normalised)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn discard(&self, cp: &Checkpoint) {
        let _ = fs::remove_dir_all(self.store_dir.join(&cp.id));
    }
}

/// Lexical path normalisation. Not `canonicalize`: that hits the filesystem and
/// fails outright on a path that does not exist yet, which is precisely the
/// created-file case this module has to handle.
fn normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Proj {
        dir: PathBuf,
        store: PathBuf,
    }

    impl Proj {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "axium-ckpt-{}-{}-{:x}",
                name,
                std::process::id(),
                (now_ts() * 1000.0) as u64
            ));
            let dir = base.join("proj");
            let store = base.join("store");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("app.py"), "def total(x):\n    return x\n").unwrap();
            Self { dir, store }
        }

        fn cp(&self) -> Checkpoints {
            Checkpoints::with_store(self.dir.to_str().unwrap(), self.store.to_str().unwrap())
        }

        fn read(&self, rel: &str) -> String {
            fs::read_to_string(self.dir.join(rel)).unwrap()
        }

        fn write(&self, rel: &str, body: &str) {
            let p = self.dir.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, body).unwrap();
        }
    }

    impl Drop for Proj {
        fn drop(&mut self) {
            if let Some(base) = self.dir.parent() {
                let _ = fs::remove_dir_all(base);
            }
        }
    }

    #[test]
    fn undo_restores_an_edited_file_byte_for_byte() {
        let p = Proj::new("edit");
        let before = p.read("app.py");
        let mut cp = p.cp();
        cp.begin("edit app");
        cp.record("app.py");
        p.write("app.py", "BROKEN");
        assert_eq!(cp.commit(), 1);

        let r = cp.undo("");
        assert!(r.ok, "{r:?}");
        assert_eq!(r.restored, vec!["app.py".to_string()]);
        assert_eq!(p.read("app.py"), before);
    }

    #[test]
    fn undo_deletes_a_file_the_turn_created() {
        let p = Proj::new("create");
        let mut cp = p.cp();
        cp.begin("create");
        cp.record("new.py");
        p.write("new.py", "x = 1\n");
        cp.commit();

        let r = cp.undo("");
        assert!(r.ok, "{r:?}");
        assert_eq!(r.deleted, vec!["new.py".to_string()]);
        assert!(!p.dir.join("new.py").exists());
    }

    #[test]
    fn a_second_record_does_not_clobber_the_original_pre_state() {
        let p = Proj::new("twice");
        let original = p.read("app.py");
        let mut cp = p.cp();
        cp.begin("two writes");
        cp.record("app.py");
        p.write("app.py", "v1\n");
        cp.record("app.py"); // must be ignored
        p.write("app.py", "v2\n");
        cp.commit();

        cp.undo("");
        assert_eq!(p.read("app.py"), original, "undo restored the intermediate");
    }

    #[test]
    fn path_spelling_does_not_create_a_second_entry() {
        let p = Proj::new("spelling");
        let original = p.read("app.py");
        let mut cp = p.cp();
        cp.begin("mixed spellings");
        cp.record("app.py");
        p.write("app.py", "v1\n");
        // Same file, three ways. Each must be recognised as already captured.
        cp.record("./app.py");
        cp.record(p.dir.join("app.py").to_str().unwrap());
        cp.record("sub/../app.py");
        assert_eq!(cp.touched_count(), 1, "one file, one entry");
        p.write("app.py", "v2\n");
        cp.commit();

        cp.undo("");
        assert_eq!(p.read("app.py"), original);
    }

    #[test]
    fn nested_paths_round_trip() {
        let p = Proj::new("nested");
        p.write("pkg/mod.py", "ORIGINAL\n");
        let mut cp = p.cp();
        cp.begin("nested edit");
        cp.record("pkg/mod.py");
        p.write("pkg/mod.py", "CHANGED\n");
        cp.commit();

        let r = cp.undo("");
        assert!(r.ok, "{r:?}");
        assert_eq!(p.read("pkg/mod.py"), "ORIGINAL\n");
    }

    #[test]
    fn undo_recreates_a_directory_the_turn_removed() {
        let p = Proj::new("rmdir");
        p.write("pkg/mod.py", "ORIGINAL\n");
        let mut cp = p.cp();
        cp.begin("delete a package");
        cp.record("pkg/mod.py");
        fs::remove_dir_all(p.dir.join("pkg")).unwrap();
        cp.commit();

        let r = cp.undo("");
        assert!(r.ok, "{r:?}");
        assert_eq!(p.read("pkg/mod.py"), "ORIGINAL\n", "parent dir must be remade");
    }

    #[test]
    fn commit_without_changes_pushes_nothing() {
        let p = Proj::new("empty");
        let mut cp = p.cp();
        cp.begin("read only turn");
        assert_eq!(cp.commit(), 0);
        assert!(cp.list().is_empty());
        assert!(!cp.undo("").ok, "nothing to undo must not report success");
    }

    #[test]
    fn undo_with_no_checkpoint_reports_an_error_not_a_panic() {
        let p = Proj::new("none");
        let mut cp = p.cp();
        let r = cp.undo("");
        assert!(!r.ok);
        assert_eq!(r.error, "no checkpoint to undo");
    }

    #[test]
    fn begin_commits_an_abandoned_checkpoint_so_it_stays_undoable() {
        let p = Proj::new("abandoned");
        let original = p.read("app.py");
        let mut cp = p.cp();
        cp.begin("turn that errored");
        cp.record("app.py");
        p.write("app.py", "HALF DONE\n");
        // No commit: the turn blew up. The next turn opening a checkpoint must
        // not silently drop the previous one's undo point.
        cp.begin("next turn");
        assert_eq!(cp.list().len(), 1);

        cp.undo("");
        assert_eq!(p.read("app.py"), original);
    }

    #[test]
    fn a_named_checkpoint_can_be_undone_out_of_order() {
        let p = Proj::new("named");
        let original = p.read("app.py");
        let mut cp = p.cp();
        let first = cp.begin("first");
        cp.record("app.py");
        p.write("app.py", "v1\n");
        cp.commit();

        cp.begin("second");
        cp.record("other.py");
        p.write("other.py", "second turn\n");
        cp.commit();

        assert_eq!(cp.list().len(), 2);
        let r = cp.undo(&first);
        assert!(r.ok, "{r:?}");
        assert_eq!(p.read("app.py"), original);
        assert!(p.dir.join("other.py").exists(), "must not touch the other turn");
        assert_eq!(cp.list().len(), 1);
    }

    #[test]
    fn two_checkpoints_in_the_same_millisecond_get_distinct_ids() {
        let p = Proj::new("ids");
        let mut cp = p.cp();
        let a = cp.begin("a");
        cp.record("app.py");
        p.write("app.py", "v1\n");
        cp.commit();
        let b = cp.begin("b");
        assert_ne!(a, b, "ids must be unique even inside one millisecond");
    }

    #[test]
    fn the_stack_is_capped() {
        let p = Proj::new("cap");
        let mut cp = p.cp();
        for i in 0..MAX_CHECKPOINTS + 5 {
            cp.begin(&format!("turn {i}"));
            cp.record("app.py");
            p.write("app.py", &format!("v{i}\n"));
            cp.commit();
        }
        assert_eq!(cp.list().len(), MAX_CHECKPOINTS);
    }

    #[test]
    fn an_oversized_file_is_skipped_rather_than_copied() {
        let p = Proj::new("big");
        let big = vec![b'x'; (MAX_SNAPSHOT_BYTES + 1) as usize];
        fs::write(p.dir.join("big.sql"), &big).unwrap();
        let mut cp = p.cp();
        cp.begin("touch a huge file");
        cp.record("big.sql");
        assert_eq!(cp.touched_count(), 0, "too big to park, so no undo point");
    }

    #[test]
    fn record_outside_an_active_checkpoint_is_a_no_op() {
        let p = Proj::new("inactive");
        let mut cp = p.cp();
        cp.record("app.py"); // no begin()
        assert_eq!(cp.touched_count(), 0);
        assert!(cp.list().is_empty());
    }

    #[test]
    fn undoing_an_already_deleted_created_file_is_not_a_failure() {
        let p = Proj::new("gone");
        let mut cp = p.cp();
        cp.begin("create then remove");
        cp.record("temp.py");
        p.write("temp.py", "x\n");
        cp.commit();
        fs::remove_file(p.dir.join("temp.py")).unwrap(); // something else removed it

        let r = cp.undo("");
        assert!(r.ok, "{r:?}");
        assert!(r.failed.is_empty());
    }

    #[test]
    fn list_reports_label_and_file_count_newest_first() {
        let p = Proj::new("list");
        let mut cp = p.cp();
        cp.begin("older turn");
        cp.record("app.py");
        p.write("app.py", "v1\n");
        cp.commit();
        cp.begin("newer turn");
        cp.record("a.py");
        cp.record("b.py");
        p.write("a.py", "1\n");
        p.write("b.py", "2\n");
        cp.commit();

        let rows = cp.list();
        assert_eq!(rows[0].label, "newer turn");
        assert_eq!(rows[0].files, 2);
        assert_eq!(rows[1].label, "older turn");
    }

    #[test]
    fn snapshots_are_discarded_after_an_undo() {
        let p = Proj::new("cleanup");
        let mut cp = p.cp();
        let id = cp.begin("edit");
        cp.record("app.py");
        p.write("app.py", "v1\n");
        cp.commit();
        assert!(p.store.join(&id).exists());
        cp.undo("");
        assert!(!p.store.join(&id).exists(), "snapshot dir leaked");
    }
}
