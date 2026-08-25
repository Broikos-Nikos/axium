r"""Turn-level checkpoints, revert a whole turn's file changes in one call.

The blast-radius benchmark asks an agent to delete things, then to put them back
"exactly". Axium scored the recovery but spent 47 tool calls doing it: it re-read
every file it had removed and rebuilt them from memory, which is both expensive
and the one path where "exactly" is a coin flip.

A checkpoint records the PRE-state of every file a turn touches, before the write
lands. `undo` then restores edited files byte-for-byte and deletes files the turn
created. One tool call, no reconstruction, no guessing.

State is per-context, not global: the benchmark runs turns concurrently and a
process-global checkpoint would let one scenario undo another's work.

Everything is best-effort. A checkpoint failure must never block the real edit,
so `record` swallows its errors and the turn proceeds without an undo point.
"""
import os
import shutil
import time

MAX_SNAPSHOT_BYTES = 8 * 1024 * 1024        # per file; a 100MB blob is not undo material
MAX_CHECKPOINTS = 20


class Checkpoints:
    """Recorded pre-states for one agent context."""

    def __init__(self, workdir, store_dir=None):
        self.workdir = os.path.abspath(workdir)
        self.store_dir = os.path.abspath(
            store_dir or os.path.join(self.workdir, ".axium", "checkpoints"))
        self.stack = []                      # [{id, label, files: {rel: snap|None}}]
        self.active = None
        # Monotonic counter appended to the id. Two checkpoints opened inside the
        # same millisecond would otherwise share an id, and undo(checkpoint_id)
        # would restore the wrong turn.
        self._seq = 0

    # -- lifecycle --
    def begin(self, label=""):
        """Open a checkpoint for the turn. A previous uncommitted one is committed
        first, so a turn that errored before commit is still undoable."""
        if self.active and self.active["files"]:
            self.commit()
        self._seq += 1
        self.active = {"id": f"{int(time.time() * 1000):x}-{self._seq}",
                       "label": str(label)[:120], "files": {}, "ts": time.time()}
        return self.active["id"]

    def commit(self):
        """Close the active checkpoint and push it on the stack (if it touched
        anything). Returns the number of files it can restore."""
        cp, self.active = self.active, None
        if not cp or not cp["files"]:
            return 0
        self.stack.append(cp)
        while len(self.stack) > MAX_CHECKPOINTS:
            self._discard(self.stack.pop(0))
        return len(cp["files"])

    def touched_count(self):
        return len(self.active["files"]) if self.active else 0

    # -- recording --
    def record(self, path):
        """Snapshot `path` BEFORE it is written. Call once per file per turn: a
        second call must not overwrite the original pre-state with the half-edited
        version."""
        if not self.active:
            return
        try:
            rel = self._rel(path)
            if rel in self.active["files"]:
                return
            if not os.path.exists(path):
                self.active["files"][rel] = None        # created by this turn
                return
            if os.path.isdir(path):
                self.active["files"][rel] = None
                return
            if os.path.getsize(path) > MAX_SNAPSHOT_BYTES:
                return
            snap = os.path.join(self.store_dir, self.active["id"],
                                rel.replace("/", "__").replace("\\", "__"))
            os.makedirs(os.path.dirname(snap), exist_ok=True)
            shutil.copy2(path, snap)
            self.active["files"][rel] = snap
        except OSError:
            pass                                        # never block the real edit

    # -- undo --
    def undo(self, checkpoint_id=""):
        """Restore the most recent checkpoint (or a named one). Returns a report."""
        cp = None
        if self.active and self.active["files"]:
            self.commit()
        if checkpoint_id:
            for c in reversed(self.stack):
                if c["id"] == checkpoint_id:
                    cp = c
                    break
        elif self.stack:
            cp = self.stack[-1]
        if cp is None:
            return {"ok": False, "restored": [], "deleted": [], "failed": [],
                    "error": "no checkpoint to undo"}

        restored, deleted, failed = [], [], []
        for rel, snap in cp["files"].items():
            full = os.path.join(self.workdir, rel)
            try:
                if snap is None:
                    if os.path.isfile(full):
                        os.remove(full)
                        deleted.append(rel)
                    elif os.path.isdir(full):
                        shutil.rmtree(full, ignore_errors=True)
                        deleted.append(rel)
                else:
                    os.makedirs(os.path.dirname(full) or ".", exist_ok=True)
                    shutil.copy2(snap, full)
                    restored.append(rel)
            except OSError as e:
                failed.append(f"{rel}: {e}")

        self.stack.remove(cp)
        self._discard(cp)
        return {"ok": not failed, "restored": sorted(restored),
                "deleted": sorted(deleted), "failed": failed, "error": ""}

    def list(self):
        return [{"id": c["id"], "label": c["label"], "files": len(c["files"]),
                 "age_s": round(time.time() - c["ts"], 1)}
                for c in reversed(self.stack)]

    # -- internals --
    def _rel(self, path):
        try:
            return os.path.relpath(os.path.abspath(path), self.workdir).replace("\\", "/")
        except ValueError:
            return os.path.basename(path)

    def _discard(self, cp):
        try:
            shutil.rmtree(os.path.join(self.store_dir, cp["id"]), ignore_errors=True)
        except OSError:
            pass
