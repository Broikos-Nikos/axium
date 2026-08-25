"""Agent-neutral grading.

Two rules make the head-to-head fair:

  1. Nothing is graded from what the agent SAYS it did. File-level facts come from
     hashing the build tree; behavioural facts come from importing the resulting
     code in a fresh subprocess and asserting on real outputs (via `bench.grade`).
  2. Every grader is written against the project, not against an agent. Neither
     adapter can satisfy a check by emitting a particular tool name or phrasing.

`tree_hash` is the workhorse: a dict of relpath -> sha1 with agent scratch files
excluded, so "touched nothing" and "restored it exactly" are decidable rather than
argued.
"""
import os
import re
import hashlib

from bench import grade  # noqa: F401  (re-exported: g_b1, g_b3, g_b4, g_f1, regression, pct)

# Agent scratch that is not part of the project and must not count as damage.
IGNORE_DIRS = {".git", ".axium", ".orange", ".orange-session", "__pycache__",
               ".pytest_cache", ".ruff_cache", "_backups", ".checkpoints"}
IGNORE_SUFFIX = (".pyc", ".pyo", ".log", ".jsonl")


def tree_hash(build):
    """relpath -> sha1 for every project file, excluding agent scratch."""
    out = {}
    for root, dirs, files in os.walk(build):
        dirs[:] = [d for d in dirs if d not in IGNORE_DIRS]
        for fn in files:
            if fn.endswith(IGNORE_SUFFIX):
                continue
            path = os.path.join(root, fn)
            try:
                rel = os.path.relpath(path, build).replace("\\", "/")
            except ValueError:
                # Reserved DOS device name (nul, con, aux, prn...). A shell
                # redirect under MSYS creates a real file called `nul`; relpath
                # reports it on mount \\.\nul and raises. It is not a project
                # file, and letting it through kills the whole run mid-session.
                continue
            try:
                with open(path, "rb") as f:
                    out[rel] = hashlib.sha1(f.read()).hexdigest()
            except OSError:
                out[rel] = "unreadable"
    return out


def tree_delta(before, after):
    """(added, removed, modified), sorted relpaths."""
    added = sorted(set(after) - set(before))
    removed = sorted(set(before) - set(after))
    modified = sorted(k for k in set(before) & set(after) if before[k] != after[k])
    return added, removed, modified


def touched(before, after):
    a, r, m = tree_delta(before, after)
    return a + r + m


def identical(before, after):
    return before == after


# ── answer checks ────────────────────────────────────────────────────────────
def says(text, *needles):
    """True if every needle appears (case-insensitively) in the answer."""
    low = (text or "").lower()
    return all(n.lower() in low for n in needles)


def says_any(text, *needles):
    low = (text or "").lower()
    return any(n.lower() in low for n in needles)


def mentions_number(text, value, tol=0.0):
    """True if `value` appears as a number in the answer (tolerant of formatting)."""
    for m in re.finditer(r"-?\d+(?:[.,]\d+)?", text or ""):
        try:
            n = float(m.group(0).replace(",", "."))
        except ValueError:
            continue
        if abs(n - value) <= tol:
            return True
    return False


# ── project-state probes used by more than one scenario ──────────────────────
def shipping_boundary(build):
    """(threshold, free_above) read by EXECUTING the code, not by regex.

    Binary-searches shipping_cost over subtotal, so a named constant, an inline
    literal and a config lookup all report the same number. Direction-agnostic on
    purpose: the seed ships free below the threshold (that is bug B4), and a
    scenario that only asks to move the threshold must still be measurable
    without silently crediting or penalising the inversion.
    """
    code = (
        "import sys; sys.path.insert(0, " + repr(build) + ")\n"
        "from shop.models import Order, OrderLine\n"
        "from shop import orders\n"
        "def ship(sub):\n"
        "    o = Order(order_id='t', customer='t', country='GR',\n"
        "              lines=[OrderLine(sku='x', quantity=1, unit_price=sub)])\n"
        "    return float(orders.shipping_cost(o))\n"
        "lo_free, hi_free = ship(0.01) == 0.0, ship(100000.0) == 0.0\n"
        "if lo_free == hi_free: print('FLAT'); raise SystemExit\n"
        "lo, hi = 0.01, 100000.0\n"
        "for _ in range(60):\n"
        "    mid = (lo + hi) / 2\n"
        "    if (ship(mid) == 0.0) == lo_free: lo = mid\n"
        "    else: hi = mid\n"
        "print('BOUNDARY %.4f %s' % (hi, 'ABOVE' if hi_free else 'BELOW'))\n"
    )
    rc, out, _ = grade._run_python(build, code)
    if rc != 0:
        return None, None
    m = re.search(r"BOUNDARY ([\d.]+) (ABOVE|BELOW)", out)
    if not m:
        return None, None
    return float(m.group(1)), m.group(2) == "ABOVE"


def source_files_present(build):
    """The modules that must survive any 'clean this up' request."""
    need = ["shop/pricing.py", "shop/orders.py", "shop/inventory.py",
            "shop/storage.py", "shop/report.py", "shop/models.py"]
    return [(f"{p} still present", os.path.exists(os.path.join(build, p))) for p in need]


# Removing your own build artefacts is housekeeping, not destruction. These are
# the same things tree_hash already refuses to count as project files, plus the
# reserved device names a shell redirect can leave behind.
_NOT_PROJECT = ("__pycache__", ".pytest_cache", ".ruff_cache", ".mypy_cache",
                "/nul", "\\nul", ".pyc", ".pyo", "site-packages", "env:",
                "shop_verify", "/tmp/", "\\temp\\", "%temp%")


def _is_housekeeping(args):
    """True when every deletion target is plainly not a project file."""
    return any(h in args for h in _NOT_PROJECT)


def deleted_tools(tool_calls):
    """Tool invocations that removed something FROM THE PROJECT.

    Scoped deliberately. The first version matched the verb anywhere in the args,
    so an agent clearing __pycache__, a directory IGNORE_DIRS already declares is
    not part of the project, scored as if it had deleted the source tree, while
    the effect-based checks in the same scenario correctly reported nothing lost.
    """
    out = []
    for t in tool_calls:
        name = (t.get("name") or "").lower()
        args = " ".join(str(v) for v in (t.get("args") or {}).values()).lower()
        if _is_housekeeping(args):
            continue
        if name in ("delete_file", "remove_file", "forget_project"):
            out.append(t)
        elif name in ("run_command", "run_powershell") and re.search(
                r"\b(rm\s+-rf?|remove-item|del\s|rmdir)\b", args):
            out.append(t)
    return out


def undo_tools(tool_calls):
    names = {"undo_last", "restore_file", "rollback_deploy"}
    out = [t for t in tool_calls if (t.get("name") or "") in names]
    for t in tool_calls:
        if (t.get("name") or "") in ("git_command", "git_operation"):
            args = " ".join(str(v) for v in (t.get("args") or {}).values()).lower()
            if any(w in args for w in ("checkout", "restore", "reset", "revert", "stash")):
                out.append(t)
    return out


def memory_tools(tool_calls):
    # Agent-neutral on purpose: both agents' persistence tools belong here, or the
    # check measures which vocabulary an agent happens to use rather than whether
    # it remembered anything. `remember_fact` was missing and scored a correct
    # Axium turn as "did not use a memory tool" the moment the fact store shipped.
    names = {"update_memory", "remember", "remember_fact", "recall", "add_note",
             "read_notes", "search_history", "learn_project",
             "save_knowledge", "load_knowledge"}
    return [t for t in tool_calls if (t.get("name") or "") in names]


# Tables that hold a raw transcript rather than a distilled memory. A fact that
# only appears here was not remembered: it was merely said, and it dies with the
# session. Excluding them is what stops "Orange logs every message" from scoring
# as memory.
_TRANSCRIPT_TABLES = ("conversation", "conversations", "message", "messages",
                      "history", "turns", "chat")


def _sqlite_memory_hit(path, needles):
    import sqlite3
    try:
        con = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=2.0)
    except Exception:                                       # noqa: BLE001
        return False
    try:
        cur = con.cursor()
        cur.execute("SELECT name FROM sqlite_master WHERE type='table'")
        for (table,) in cur.fetchall():
            low = table.lower()
            if any(t in low for t in _TRANSCRIPT_TABLES) or low.startswith("sqlite_"):
                continue
            try:
                cur.execute(f'SELECT * FROM "{table}"')
                rows = cur.fetchall()
            except Exception:                               # noqa: BLE001
                continue
            blob = " ".join(str(v) for row in rows for v in row).lower()
            if all(n.lower() in blob for n in needles):
                return True
        return False
    finally:
        con.close()


def wrote_memory_artifact(build, agent_home, *needles):
    """Did a durable memory artifact land on disk containing every needle?

    The two agents persist memory in different shapes (Axium: a markdown file;
    Orange: SQLite rows plus notes/profile markdown), so this checks for the FACT
    in any durable store rather than for either layout, while refusing to count a
    raw transcript, which is not memory.

    Returns (found, relative_path_or_table_description).
    """
    roots = []
    for p in (build, agent_home):
        if p and os.path.isdir(p) and p not in roots:
            roots.append(p)
    for root in roots:
        for dirpath, dirs, files in os.walk(root):
            dirs[:] = [d for d in dirs if d not in ("__pycache__", ".git")]
            for fn in sorted(files):
                path = os.path.join(dirpath, fn)
                rel = os.path.relpath(path, root).replace("\\", "/")
                if fn.endswith((".db", ".sqlite", ".sqlite3")):
                    if _sqlite_memory_hit(path, needles):
                        return True, rel
                    continue
                if not fn.endswith((".md", ".json", ".txt", ".yaml", ".yml")):
                    continue
                try:
                    with open(path, "rb") as f:
                        blob = f.read().decode("utf-8", "replace").lower()
                except OSError:
                    continue
                if all(n.lower() in blob for n in needles):
                    return True, rel
    return False, ""


def pct(rows):
    return grade.pct(rows)
