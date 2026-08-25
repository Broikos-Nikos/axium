r"""Project Brain, durable per-project knowledge in `<project>/.axium/`.

Axium re-derives a project's shape on every session. `scan_project` is cheap, but
the reasoning built on top of it is not: in the head-to-head benchmark the "delete
what we don't need, then put it back" scenario cost 47 tool calls, most of them
re-reading files the agent had already read in an earlier turn.

The Brain makes that knowledge persist:

    <project>/.axium/
      PROFILE.md    stack, entry points, key files, conventions. Human-editable;
                    generated only when missing, and a human-written one is never
                    clobbered (the marker tells them apart).
      overview.md   annotated structure, rebuilt when the CODE changes rather than
                    on a wall-clock TTL, a fingerprint, not a timer.
      fingerprint   the hash overview.md was built from.
      journal.md    newest-first log of what changed and why, so "continue where we
                    left off" survives a restart.

Everything here is best-effort. A failed brain build must never break a turn, so
every public function catches and degrades to an empty string.
"""
import hashlib
import os
import time

_CODE_EXTS = {".py", ".rs", ".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".go",
              ".java", ".rb", ".php", ".c", ".h", ".cpp", ".hpp", ".cs", ".swift",
              ".kt", ".sql", ".sh", ".toml", ".json", ".yaml", ".yml", ".html", ".css"}
_SKIP_DIRS = {".git", ".axium", "node_modules", "__pycache__", "target", "venv",
              ".venv", "dist", "build", ".idea", ".vscode", ".pytest_cache",
              ".ruff_cache", "vendor"}

PROFILE_MARKER = "<!-- axium:auto-profile -->"
JOURNAL_MAX_ENTRIES = 120
PRELOAD_CHAR_BUDGET = 4000
_MAX_FINGERPRINT_FILES = 4000


def brain_dir(root, create=False):
    d = os.path.join(root or ".", ".axium")
    if create:
        try:
            os.makedirs(d, exist_ok=True)
        except OSError:
            pass
    return d


def profile_path(root):
    return os.path.join(brain_dir(root), "PROFILE.md")


def overview_path(root):
    return os.path.join(brain_dir(root), "overview.md")


def fingerprint_path(root):
    return os.path.join(brain_dir(root), "fingerprint")


def journal_path(root):
    return os.path.join(brain_dir(root), "journal.md")


def _read(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return ""


def _write(path, text):
    try:
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        tmp = path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(text)
        os.replace(tmp, path)
        return True
    except OSError:
        return False


# -- fingerprint --------------------------------------------------------------
def fingerprint(root):
    """Hash of (relative path, size, mtime) over the project's code files.

    A wall-clock TTL rebuilds an overview that is still correct and keeps a stale
    one that is not. A content fingerprint rebuilds exactly when the code moved.
    """
    h = hashlib.sha256()
    n = 0
    try:
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = sorted(d for d in dirnames if d not in _SKIP_DIRS
                                 and not d.startswith("."))
            for name in sorted(filenames):
                if os.path.splitext(name)[1].lower() not in _CODE_EXTS:
                    continue
                full = os.path.join(dirpath, name)
                try:
                    st = os.stat(full)
                except OSError:
                    continue
                rel = os.path.relpath(full, root).replace("\\", "/")
                # Milliseconds, not seconds. A one-character edit keeps the file
                # the same size, and at second precision an edit made within the
                # same second as the last scan is invisible, the agent then
                # reasons from a stale overview for the rest of the session.
                h.update(f"{rel}:{st.st_size}:{int(st.st_mtime * 1000)}\n".encode())
                n += 1
                if n >= _MAX_FINGERPRINT_FILES:
                    return h.hexdigest()[:32]
    except OSError:
        return ""
    return h.hexdigest()[:32] if n else ""


def is_stale(root):
    fp = fingerprint(root)
    if not fp:
        return False
    return _read(fingerprint_path(root)).strip() != fp


# -- overview -----------------------------------------------------------------
def build_overview(root, scan_fn, max_chars=6000):
    """Rebuild overview.md from `scan_fn(root)` when the fingerprint moved.

    `scan_fn` is injected rather than imported so this module stays free of the
    tool layer and is trivially testable with a stub.
    """
    try:
        fp = fingerprint(root)
        if not fp:
            return _read(overview_path(root))
        if _read(fingerprint_path(root)).strip() == fp:
            cached = _read(overview_path(root))
            if cached:
                return cached
        body = (scan_fn(root) or "")[:max_chars]
        if not body.strip():
            return ""
        stamp = time.strftime("%Y-%m-%d %H:%M")
        _write(overview_path(root),
               f"# Project overview\n\n_Rebuilt {stamp} (fingerprint {fp[:8]})._\n\n{body}\n")
        _write(fingerprint_path(root), fp)
        return _read(overview_path(root))
    except Exception:                                   # noqa: BLE001
        return ""


# -- profile ------------------------------------------------------------------
def read_profile(root):
    return _read(profile_path(root))


def write_profile(root, body, auto=True):
    """Write PROFILE.md. An existing profile WITHOUT the auto marker was written by
    a human and is never overwritten."""
    existing = _read(profile_path(root))
    if existing.strip() and PROFILE_MARKER not in existing:
        return False
    head = f"{PROFILE_MARKER}\n" if auto else ""
    return _write(profile_path(root), f"{head}# Project profile\n\n{body.strip()}\n")


# -- journal ------------------------------------------------------------------
def journal(root, summary, files=(), request=""):
    """Prepend one entry. Newest first, because that is the half anyone reads."""
    try:
        summary = (summary or "").strip()
        if not summary:
            return False
        stamp = time.strftime("%Y-%m-%d %H:%M")
        touched = ", ".join(sorted(files)[:12]) or "(no files)"
        entry = (f"## {stamp}\n"
                 f"- request: {(request or '').strip()[:200] or '(none)'}\n"
                 f"- files: {touched}\n"
                 f"- result: {summary[:600]}\n")
        old = _read(journal_path(root))
        body = old.split("\n", 1)[1] if old.startswith("# ") else old
        entries = [e for e in body.split("\n## ") if e.strip()]
        kept = entries[: JOURNAL_MAX_ENTRIES - 1]
        rest = ("\n## " + "\n## ".join(kept)) if kept else ""
        return _write(journal_path(root), f"# Change journal\n\n{entry}{rest}\n")
    except Exception:                                   # noqa: BLE001
        return False


def recent_journal(root, n=3):
    body = _read(journal_path(root))
    if not body:
        return ""
    entries = [e for e in body.split("\n## ") if e.strip()][:n]
    return "\n## ".join(entries).strip()


# -- preload ------------------------------------------------------------------
def preload(root, budget=PRELOAD_CHAR_BUDGET):
    """The `[PROJECT BRAIN]` block: profile, then recent journal, then overview,
    in that order of value per token. Empty when the project has no brain yet, so
    a first-touch project pays nothing."""
    parts = []
    prof = read_profile(root).replace(PROFILE_MARKER, "").strip()
    if prof:
        parts.append(prof)
    jour = recent_journal(root, 3)
    if jour:
        parts.append("## Recent changes\n## " + jour)
    ov = _read(overview_path(root)).strip()
    if ov:
        parts.append(ov)
    out, used = [], 0
    for p in parts:
        if used + len(p) > budget:
            out.append(p[: max(0, budget - used)])
            break
        out.append(p)
        used += len(p) + 2
    return "\n\n".join(x for x in out if x).strip()


def has_brain(root):
    return any(os.path.exists(p) for p in
               (profile_path(root), overview_path(root), journal_path(root)))
