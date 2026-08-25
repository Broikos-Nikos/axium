"""Tool implementations.

Every tool returns a plain string — that string is what the model sees as the
tool result, so errors are returned as text (never raised) and the agent gets a
chance to recover.

All relative paths resolve against the turn's working directory, and every
resolved path is confined to it. A tool that tries to escape returns an error
instead of touching the file: benchmark scenarios run against generated fixtures
and a stray `rm -rf ..` would take the harness with it.
"""
import ast
import fnmatch
import json
import os
import re
import shutil
import subprocess
import sys

MAX_LIST_ENTRIES = 400
MAX_SEARCH_HITS = 200
SKIP_DIRS = {".git", "node_modules", "__pycache__", "target", "venv", ".venv",
             "dist", "build", ".idea", ".vscode", ".pytest_cache", ".ruff_cache"}
BINARY_EXT = {".png", ".jpg", ".jpeg", ".gif", ".ico", ".pdf", ".zip", ".gz",
              ".exe", ".dll", ".so", ".pyc", ".woff", ".woff2", ".ttf", ".mp4"}


class ToolError(Exception):
    """Raised internally; the dispatcher converts it to an error string."""


# ── shell selection ──────────────────────────────────────────────────────────
def _shell():
    """Prefer bash (the tool descriptions promise POSIX syntax). On Windows fall
    back to Git Bash if present, then cmd — a scenario that needs bash and gets
    cmd will fail its grader loudly rather than pass on a technicality."""
    exe = shutil.which("bash")
    if exe:
        return [exe, "-c"]
    for c in (r"C:\Program Files\Git\bin\bash.exe", r"C:\Program Files\Git\usr\bin\bash.exe"):
        if os.path.exists(c):
            return [c, "-c"]
    if os.name == "nt":
        return [os.environ.get("COMSPEC", "cmd.exe"), "/c"]
    return ["/bin/sh", "-c"]


# ── path safety ──────────────────────────────────────────────────────────────
def resolve_path(path, workdir):
    """Resolve `path` against `workdir` and refuse anything outside it."""
    if not path:
        raise ToolError("path is required")
    p = os.path.expanduser(path)
    full = os.path.abspath(p if os.path.isabs(p) else os.path.join(workdir, p))
    root = os.path.abspath(workdir)
    if os.path.commonpath([full, root]) != root:
        raise ToolError(f"path escapes the working directory: {path}")
    return full


def _snapshot(ctx, *paths):
    """Record the pre-state of every path a mutating tool is about to touch.

    Called on entry rather than immediately before the write, so a tool that
    fails halfway still leaves an undo point. Recording a file that ends up
    unchanged is free: undo then restores identical bytes.
    """
    cp = ctx.get("checkpoints")
    if cp is None:
        return
    for p in paths:
        if p:
            cp.record(p)


def _rel(p, workdir):
    try:
        return os.path.relpath(p, workdir).replace("\\", "/")
    except ValueError:
        return p


def _read_text(path):
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        return f.read()


def _write_text(path, content):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="") as f:
        f.write(content)


def _walk(root, max_depth=None):
    """Directory walk that skips vendor/build noise and respects a depth cap."""
    root = os.path.abspath(root)
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS and not d.startswith(".git"))
        if max_depth is not None:
            depth = dirpath[len(root):].count(os.sep)
            if depth >= max_depth:
                dirnames[:] = []
        yield dirpath, dirnames, sorted(filenames)


# ── syntax checking ──────────────────────────────────────────────────────────
def syntax_check(path):
    """Cheap per-language syntax check. Returns an error string, or "" if clean
    or unsupported."""
    ext = os.path.splitext(path)[1].lower()
    try:
        if ext == ".py":
            ast.parse(_read_text(path), filename=path)
        elif ext == ".json":
            json.loads(_read_text(path))
        elif ext == ".php" and shutil.which("php"):
            r = subprocess.run(["php", "-l", path], capture_output=True, text=True, timeout=30)
            if r.returncode != 0:
                return (r.stdout + r.stderr).strip()[:800]
    except SyntaxError as e:
        return f"SyntaxError line {e.lineno}: {e.msg}"
    except json.JSONDecodeError as e:
        return f"Invalid JSON: {e}"
    except Exception:                                   # noqa: BLE001
        return ""
    return ""


# ── individual tools ─────────────────────────────────────────────────────────
def t_run_command(args, ctx):
    cmd = (args.get("command") or "").strip()
    if not cmd:
        return "Error: command is required"
    try:
        r = subprocess.run(_shell() + [cmd], cwd=ctx["workdir"], capture_output=True,
                           text=True, errors="replace", timeout=ctx["timeout"])
    except subprocess.TimeoutExpired:
        return f"Error: command timed out after {ctx['timeout']}s"
    except Exception as e:                              # noqa: BLE001
        return f"Error running command: {e}"
    out = (r.stdout or "").strip()
    err = (r.stderr or "").strip()
    parts = []
    if out:
        parts.append(out)
    if err:
        parts.append(f"[stderr]\n{err}")
    parts.append(f"[exit {r.returncode}]")
    return "\n".join(parts)


def t_read_file(args, ctx):
    path = resolve_path(args.get("path"), ctx["workdir"])
    if not os.path.exists(path):
        return f"Error: file not found: {args.get('path')}"
    if os.path.isdir(path):
        return f"Error: {args.get('path')} is a directory — use list_directory"
    if os.path.splitext(path)[1].lower() in BINARY_EXT:
        return f"Error: {args.get('path')} looks binary ({os.path.getsize(path)} bytes)"
    lines = _read_text(path).splitlines()
    start = max(1, int(args.get("start_line") or 1))
    end = int(args.get("end_line") or len(lines))
    end = min(end, len(lines))
    if start > len(lines):
        return f"Error: start_line {start} is past end of file ({len(lines)} lines)"
    numbered = args.get("numbered", True) is not False
    chunk = lines[start - 1:end]
    body = "\n".join(f"{start + i}\t{ln}" for i, ln in enumerate(chunk)) if numbered \
        else "\n".join(chunk)
    header = f"[{_rel(path, ctx['workdir'])} lines {start}-{end} of {len(lines)}]\n"
    return header + body



def _definition_names(text, path):
    """Top-level definition names, from the parser where possible."""
    if path.lower().endswith(".py"):
        try:
            tree = ast.parse(text)
            return {n.name for n in tree.body
                    if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef,
                                      ast.ClassDef))}
        except SyntaxError:
            pass
    return set(re.findall(r"^\s*(?:export\s+)?(?:async\s+)?(?:def|class|function)\s+"
                          r"([A-Za-z_][A-Za-z0-9_]*)", text or "", re.M))


def definitions_lost(path, new_content):
    """Names the file has now and would not have after this write."""
    if not os.path.exists(path):
        return []
    old = _read_text(path)
    if not old.strip():
        return []
    return sorted(_definition_names(old, path) - _definition_names(new_content or "", path))

def t_write_file(args, ctx):
    path = resolve_path(args.get("path"), ctx["workdir"])
    _snapshot(ctx, path)
    content = args.get("content")
    if content is None:
        return "Error: content is required"
    existed = os.path.exists(path)
    # A whole-file write that drops definitions the file already has is almost
    # never what was asked. Measured: asked to ADD one function to a module with
    # three, the model wrote a file containing only the new one, the syntax check
    # passed because the result was valid Python, and three functions were gone.
    if existed and not args.get("replace"):
        lost = definitions_lost(path, content)
        if lost:
            return ("Error: refusing to overwrite %s. This write removes %d "
                    "definition(s) the file already has: %s. Change part of a file "
                    "with patch_file or append_file. Pass replace=true only if the "
                    "whole file should go."
                    % (_rel(path, ctx["workdir"]), len(lost), ", ".join(lost[:8])))
    _write_text(path, content)
    ctx["changed"].add(_rel(path, ctx["workdir"]))
    msg = f"{'Overwrote' if existed else 'Created'} {_rel(path, ctx['workdir'])} " \
          f"({len(content)} bytes, {content.count(chr(10)) + 1} lines)"
    err = syntax_check(path)
    return f"{msg}\nWARNING syntax check failed: {err}" if err else msg


def t_append_file(args, ctx):
    path = resolve_path(args.get("path"), ctx["workdir"])
    _snapshot(ctx, path)
    content = args.get("content") or ""
    after = args.get("after")
    if after:
        if not os.path.exists(path):
            return f"Error: cannot insert after marker — file not found: {args.get('path')}"
        lines = _read_text(path).splitlines()
        for i, ln in enumerate(lines):
            if after in ln:
                lines.insert(i + 1, content.rstrip("\n"))
                _write_text(path, "\n".join(lines) + "\n")
                ctx["changed"].add(_rel(path, ctx["workdir"]))
                return f"Inserted after line {i + 1} of {_rel(path, ctx['workdir'])}"
        return f"Error: marker not found in {args.get('path')}: {after[:80]}"
    prev = _read_text(path) if os.path.exists(path) else ""
    if prev and not prev.endswith("\n"):
        prev += "\n"
    _write_text(path, prev + content)
    ctx["changed"].add(_rel(path, ctx["workdir"]))
    return f"Appended {len(content)} bytes to {_rel(path, ctx['workdir'])}"


def _normalise(s):
    return "\n".join(ln.strip() for ln in s.strip().splitlines())


def t_patch_file(args, ctx):
    path = resolve_path(args.get("path"), ctx["workdir"])
    _snapshot(ctx, path)
    if not os.path.exists(path):
        return f"Error: file not found: {args.get('path')}"
    new_text = args.get("new_text")
    if new_text is None:
        return "Error: new_text is required"
    text = _read_text(path)

    # Line-range replacement.
    if args.get("start_line"):
        lines = text.splitlines()
        s = max(1, int(args["start_line"]))
        e = min(int(args.get("end_line") or s), len(lines))
        if s > len(lines):
            return f"Error: start_line {s} is past end of file ({len(lines)} lines)"
        lines[s - 1:e] = new_text.splitlines()
        _write_text(path, "\n".join(lines) + ("\n" if text.endswith("\n") else ""))
        ctx["changed"].add(_rel(path, ctx["workdir"]))
        return f"Replaced lines {s}-{e} of {_rel(path, ctx['workdir'])}"

    old = args.get("old_text")
    if not old:
        return "Error: old_text is required unless start_line/end_line are given"
    occ = args.get("occurrence", 1)
    count = text.count(old)

    if count:
        if str(occ).lower() == "all":
            out, n = text.replace(old, new_text), count
        else:
            n_idx = max(1, int(occ))
            if count < n_idx:
                return f"Error: only {count} occurrence(s) found, occurrence {n_idx} requested"
            pos = -1
            for _ in range(n_idx):
                pos = text.index(old, pos + 1)
            out, n = text[:pos] + new_text + text[pos + len(old):], 1
        _write_text(path, out)
        ctx["changed"].add(_rel(path, ctx["workdir"]))
        return f"Patched {_rel(path, ctx['workdir'])} ({n} replacement(s))" + \
               (lambda e: f"\nWARNING syntax check failed: {e}" if e else "")(syntax_check(path))

    # Fallback: whitespace-normalised block match, for indentation drift.
    lines, want = text.splitlines(), _normalise(old).splitlines()
    if want:
        for i in range(len(lines) - len(want) + 1):
            if [ln.strip() for ln in lines[i:i + len(want)]] == want:
                indent = lines[i][:len(lines[i]) - len(lines[i].lstrip())]
                repl = [indent + ln if ln.strip() else ln for ln in new_text.splitlines()]
                lines[i:i + len(want)] = repl
                _write_text(path, "\n".join(lines) + ("\n" if text.endswith("\n") else ""))
                ctx["changed"].add(_rel(path, ctx["workdir"]))
                return (f"Patched {_rel(path, ctx['workdir'])} "
                        f"(whitespace-normalised match at line {i + 1})")
    return (f"Error: old_text not found in {args.get('path')}. "
            f"Read the file first — it may have changed.")


def t_search_files(args, ctx):
    pattern = args.get("pattern")
    if not pattern:
        return "Error: pattern is required"
    try:
        rx = re.compile(pattern)
    except re.error as e:
        return f"Error: invalid regex: {e}"
    root = resolve_path(args.get("path") or ".", ctx["workdir"])
    inc = args.get("include")
    hits = []
    targets = [(os.path.dirname(root), [os.path.basename(root)])] if os.path.isfile(root) \
        else [(dp, fn) for dp, _, fn in _walk(root)]
    for dirpath, filenames in targets:
        for fname in filenames:
            if inc and not fnmatch.fnmatch(fname, inc):
                continue
            if os.path.splitext(fname)[1].lower() in BINARY_EXT:
                continue
            fpath = os.path.join(dirpath, fname)
            try:
                for n, line in enumerate(_read_text(fpath).splitlines(), 1):
                    if rx.search(line):
                        hits.append(f"{_rel(fpath, ctx['workdir'])}:{n}: {line.strip()[:200]}")
                        if len(hits) >= MAX_SEARCH_HITS:
                            hits.append(f"[truncated at {MAX_SEARCH_HITS} matches]")
                            return "\n".join(hits)
            except (OSError, UnicodeDecodeError):
                continue
    return "\n".join(hits) if hits else f"No matches for /{pattern}/"


def t_list_directory(args, ctx):
    path = resolve_path(args.get("path") or ".", ctx["workdir"])
    if not os.path.isdir(path):
        return f"Error: not a directory: {args.get('path') or '.'}"
    rows = []
    for name in sorted(os.listdir(path)):
        full = os.path.join(path, name)
        rows.append(f"{name}/" if os.path.isdir(full) else f"{name}  ({os.path.getsize(full)}b)")
        if len(rows) >= MAX_LIST_ENTRIES:
            rows.append("[truncated]")
            break
    return "\n".join(rows) if rows else "(empty directory)"


_SYMBOL_RX = {
    ".py": re.compile(r"^\s*(?:async\s+)?(?:def|class)\s+(\w+)"),
    ".php": re.compile(r"^\s*(?:abstract\s+|final\s+)?(?:class|interface|trait|function)\s+(\w+)"),
    ".js": re.compile(r"^\s*(?:export\s+)?(?:async\s+)?(?:function|class|const|let)\s+(\w+)"),
    ".ts": re.compile(r"^\s*(?:export\s+)?(?:async\s+)?(?:function|class|const|interface|type)\s+(\w+)"),
    ".rs": re.compile(r"^\s*(?:pub\s+)?(?:async\s+)?(?:fn|struct|enum|trait|impl)\s+(\w+)"),
    ".go": re.compile(r"^\s*(?:func|type)\s+(\w+)"),
}
_SYMBOL_RX[".jsx"] = _SYMBOL_RX[".js"]
_SYMBOL_RX[".tsx"] = _SYMBOL_RX[".ts"]


def t_scan_project(args, ctx):
    root = resolve_path(args.get("path") or ".", ctx["workdir"])
    depth = int(args.get("max_depth") or 4)
    out = [f"{_rel(root, ctx['workdir'])}/"]
    for dirpath, _, filenames in _walk(root, depth):
        level = dirpath[len(root):].count(os.sep)
        pad = "  " * (level + 1)
        if dirpath != root:
            out.append(f"{pad}{os.path.basename(dirpath)}/")
        for fname in filenames:
            ext = os.path.splitext(fname)[1].lower()
            if ext in BINARY_EXT:
                continue
            fpath = os.path.join(dirpath, fname)
            line = f"{pad}  {fname}"
            rx = _SYMBOL_RX.get(ext)
            if rx:
                try:
                    syms = [m.group(1) for m in
                            (rx.match(ln) for ln in _read_text(fpath).splitlines()) if m]
                except OSError:
                    syms = []
                if syms:
                    shown = ", ".join(syms[:12]) + (" ..." if len(syms) > 12 else "")
                    line += f"    [{shown}]"
            out.append(line)
            if len(out) > MAX_LIST_ENTRIES:
                out.append("[truncated]")
                return "\n".join(out)
    return "\n".join(out)


def t_git_command(args, ctx):
    raw = (args.get("args") or "").strip()
    if not raw:
        return "Error: args is required"
    try:
        r = subprocess.run(_shell() + [f"git {raw}"], cwd=ctx["workdir"],
                           capture_output=True, text=True, errors="replace", timeout=120)
    except Exception as e:                              # noqa: BLE001
        return f"Error running git: {e}"
    return ((r.stdout or "") + (r.stderr or "")).strip() or f"[git exit {r.returncode}]"


def t_delete_file(args, ctx):
    path = resolve_path(args.get("path"), ctx["workdir"])
    _snapshot(ctx, path)
    if not os.path.exists(path):
        return f"Error: not found: {args.get('path')}"
    try:
        if os.path.isdir(path):
            os.rmdir(path)
        else:
            os.remove(path)
    except OSError as e:
        return f"Error deleting {args.get('path')}: {e}"
    ctx["changed"].add(_rel(path, ctx["workdir"]))
    return f"Deleted {_rel(path, ctx['workdir'])}"


def t_move_file(args, ctx):
    src = resolve_path(args.get("source"), ctx["workdir"])
    dst = resolve_path(args.get("destination"), ctx["workdir"])
    _snapshot(ctx, src, dst)
    if not os.path.exists(src):
        return f"Error: source not found: {args.get('source')}"
    os.makedirs(os.path.dirname(dst) or ".", exist_ok=True)
    shutil.move(src, dst)
    ctx["changed"].update({_rel(src, ctx["workdir"]), _rel(dst, ctx["workdir"])})
    return f"Moved {_rel(src, ctx['workdir'])} -> {_rel(dst, ctx['workdir'])}"


def t_find_references(args, ctx):
    symbol = args.get("symbol")
    if not symbol:
        return "Error: symbol is required"
    return t_search_files({"pattern": r"\b" + re.escape(symbol) + r"\b",
                           "path": args.get("path") or "."}, ctx)


def t_get_diagnostics(args, ctx):
    path = resolve_path(args.get("path") or ".", ctx["workdir"])
    files = [path] if os.path.isfile(path) else [
        os.path.join(dp, f) for dp, _, fns in _walk(path) for f in fns
        if os.path.splitext(f)[1].lower() in (".py", ".json", ".php")
    ]
    problems = [f"{_rel(f, ctx['workdir'])}: {err}"
                for f in files if (err := syntax_check(f))]
    if problems:
        return f"{len(problems)} problem(s):\n" + "\n".join(problems[:50])
    return f"No syntax problems in {len(files)} file(s)."


_IMPORT_RX = [
    re.compile(r"^\s*(?:from|import)\s+([\w.]+)", re.M),
    re.compile(r"""(?:require|require_once|include|include_once)\s*\(?\s*['"]([^'"]+)['"]""", re.M),
    re.compile(r"""(?:import|from)\s+['"]([^'"]+)['"]""", re.M),
]


def t_get_dependency_graph(args, ctx):
    target = resolve_path(args.get("path"), ctx["workdir"])
    direction = args.get("direction") or "both"
    stem = os.path.splitext(os.path.basename(target))[0]
    out = []
    if direction in ("dependencies", "both") and os.path.isfile(target):
        text = _read_text(target)
        deps = sorted({m for rx in _IMPORT_RX for m in rx.findall(text)})
        out.append("dependencies (this file imports):\n  " +
                   ("\n  ".join(deps) if deps else "(none found)"))
    if direction in ("dependents", "both"):
        hits = []
        for dp, _, fns in _walk(ctx["workdir"]):
            for f in fns:
                fp = os.path.join(dp, f)
                if fp == target or os.path.splitext(f)[1].lower() in BINARY_EXT:
                    continue
                try:
                    if re.search(r"\b" + re.escape(stem) + r"\b", _read_text(fp)):
                        hits.append(_rel(fp, ctx["workdir"]))
                except (OSError, UnicodeDecodeError):
                    continue
        out.append("dependents (files referencing it):\n  " +
                   ("\n  ".join(sorted(hits)[:60]) if hits else "(none found)"))
    return "\n\n".join(out)


def t_update_memory(args, ctx):
    mem = ctx.get("memory")
    if mem is None:
        return "Error: memory is not available in this context"
    section, content = args.get("section") or "Notes", args.get("content") or ""
    if args.get("action") == "replace":
        mem.replace_section(section, content)
    else:
        mem.append_to_section(section, content)
    ctx["memory_ops"].append({"action": args.get("action", "append"),
                              "section": section, "content": content})
    return f"Memory updated: [{section}]"


def t_update_project_knowledge(args, ctx):
    path = os.path.join(ctx["workdir"], ".axium", "knowledge.md")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    section = args.get("section") or "Notes"
    prev = _read_text(path) if os.path.exists(path) else "# Project knowledge\n"
    with open(path, "w", encoding="utf-8") as f:
        f.write(f"{prev.rstrip()}\n\n## {section}\n{args.get('content', '')}\n")
    return f"Project knowledge updated: [{section}]"


def t_search_history(args, ctx):
    db = ctx.get("db")
    if db is None:
        return "Error: history database is not available in this context"
    limit = min(int(args.get("limit") or 10), 30)
    rows = db.search(args.get("query") or "", limit)
    if not rows:
        return "No matching messages in history."
    return "\n".join(f"[{r['session']} {r['ts']}] {r['role']}: {r['content'][:300]}"
                     for r in rows)


def t_task_manage(args, ctx):
    db = ctx.get("db")
    if db is None:
        return "Error: task database is not available in this context"
    action = args.get("action")
    if action == "create":
        tid = db.create_task(args.get("title") or "(untitled)", args.get("context") or "")
        return f"Task #{tid} created."
    if action == "update_status":
        db.update_task(int(args.get("task_id") or 0), args.get("status") or "pending")
        return f"Task #{args.get('task_id')} -> {args.get('status')}"
    rows = db.list_tasks()
    return "\n".join(f"#{r['id']} [{r['status']}] {r['title']}" for r in rows) or "No tasks."


def t_ask_user(args, ctx):
    ask = ctx.get("ask_user")
    q = args.get("question") or ""
    if ask is None:
        # Non-interactive (worker, benchmark): auto-approve so the run continues,
        # and record it — an agent that asks instead of acting is a measurable trait.
        ctx["asked"].append(q)
        return "yes (auto-approved: non-interactive session)"
    return ask(q)


def t_set_autonomous(args, ctx):
    ctx["autonomous"] = bool(args.get("enabled"))
    return f"Autonomous mode {'enabled' if ctx['autonomous'] else 'disabled'}."


def t_run_subagent(args, ctx):
    spawn = ctx.get("spawn_subagent")
    if spawn is None:
        return "Error: sub-agents are not available at this depth"
    return spawn(args.get("task") or "", args.get("model") or "fast")


def t_undo_turn(args, ctx):
    """Revert a turn's file changes from the snapshots taken before each write."""
    cp = ctx.get("checkpoints")
    if cp is None:
        return "Error: checkpoints are not enabled in this context"
    if args.get("action") == "list":
        rows = cp.list()
        if not rows:
            return "No checkpoints recorded."
        return "\n".join(f"{r['id']}  {r['files']} file(s)  {r['age_s']}s ago  "
                         f"{r['label']}" for r in rows)
    res = cp.undo(args.get("checkpoint_id") or "")
    if res["error"]:
        return f"Error: {res['error']}"
    for rel in res["restored"] + res["deleted"]:
        ctx["changed"].add(rel)
    parts = []
    if res["restored"]:
        parts.append(f"Restored {len(res['restored'])}: {', '.join(res['restored'][:20])}")
    if res["deleted"]:
        parts.append(f"Removed {len(res['deleted'])} file(s) this turn created: "
                     f"{', '.join(res['deleted'][:20])}")
    if res["failed"]:
        parts.append(f"FAILED on {len(res['failed'])}: {'; '.join(res['failed'][:5])}")
    return "\n".join(parts) or "Checkpoint was empty — nothing to undo."


def t_remember_fact(args, ctx):
    """Write one durable fact explicitly, with a type and an importance."""
    store = ctx.get("facts")
    if store is None:
        return "Error: the fact store is not available in this context"
    value = (args.get("value") or "").strip()
    if not value:
        return "Error: value is required"
    try:
        importance = float(args.get("importance", 0.7))
    except (TypeError, ValueError):
        importance = 0.7
    fid = store.remember(value, type=args.get("type") or "note",
                         key=args.get("key") or "", importance=importance,
                         scope=ctx.get("scope", ""), source="tool")
    return f"Remembered (#{fid}, importance {importance:.2f}): {value[:160]}"


def t_recall(args, ctx):
    """Search durable facts. Cheaper and more precise than searching history."""
    store = ctx.get("facts")
    if store is None:
        return "Error: the fact store is not available in this context"
    q = (args.get("query") or "").strip()
    limit = min(int(args.get("limit") or 10), 30)
    rows = store.search(q, scope=ctx.get("scope", ""), limit=limit) if q \
        else store.all(scope=ctx.get("scope", ""), limit=limit)
    if not rows:
        return "No matching facts."
    return "\n".join(f"[{r['type']} {r['importance']:.2f}] {r['key']}: {r['value']}"
                     for r in rows)


def t_learn_project(args, ctx):
    """Force a Project Brain rebuild: re-scan, refresh the overview, write a profile."""
    from . import brain
    root = ctx["workdir"]
    brain.brain_dir(root, create=True)
    overview = brain.build_overview(root, lambda r: t_scan_project({"path": r}, ctx))
    body = (args.get("profile") or "").strip()
    wrote_profile = brain.write_profile(root, body) if body else False
    bits = [f"Overview {'rebuilt' if overview else 'unavailable'} "
            f"({len(overview)} chars)"]
    if body:
        bits.append("profile written" if wrote_profile
                    else "profile left alone (a human-written PROFILE.md exists)")
    return ". ".join(bits) + "."


DISPATCH = {
    "run_command": t_run_command,
    "read_file": t_read_file,
    "write_file": t_write_file,
    "append_file": t_append_file,
    "patch_file": t_patch_file,
    "search_files": t_search_files,
    "list_directory": t_list_directory,
    "scan_project": t_scan_project,
    "git_command": t_git_command,
    "delete_file": t_delete_file,
    "move_file": t_move_file,
    "find_references": t_find_references,
    "get_diagnostics": t_get_diagnostics,
    "get_dependency_graph": t_get_dependency_graph,
    "update_memory": t_update_memory,
    "update_project_knowledge": t_update_project_knowledge,
    "search_history": t_search_history,
    "task_manage": t_task_manage,
    "ask_user": t_ask_user,
    "set_autonomous": t_set_autonomous,
    "run_subagent": t_run_subagent,
    "undo_turn": t_undo_turn,
    "remember_fact": t_remember_fact,
    "recall": t_recall,
    "learn_project": t_learn_project,
}


def execute(name, args, ctx):
    """Run a tool by name. Always returns a string; never raises."""
    fn = DISPATCH.get(name)
    if fn is None:
        return f"Error: unknown tool '{name}'"
    if "__malformed_json__" in (args or {}):
        return ("Error: your tool arguments were not valid JSON. "
                "Re-issue the call with well-formed JSON.")
    try:
        out = fn(args or {}, ctx)
    except ToolError as e:
        return f"Error: {e}"
    except Exception as e:                              # noqa: BLE001
        return f"Error: {type(e).__name__}: {e}"
    out = out if isinstance(out, str) else str(out)
    limit = ctx.get("max_output_chars", 15000)
    if len(out) > limit:
        head = out[: limit // 2]
        tail = out[-limit // 2:]
        return f"{head}\n\n[... {len(out) - limit} chars truncated ...]\n\n{tail}"
    return out


def new_context(workdir, *, timeout=120, max_output_chars=15000, memory=None, db=None,
                ask_user=None, spawn_subagent=None, facts=None, checkpoints=None,
                scope=""):
    """Build the mutable per-turn tool context.

    `facts` and `checkpoints` are optional: a context without them simply has no
    fact tools and no undo, which is what the benchmark's ablation runs want.
    """
    return {
        "workdir": os.path.abspath(workdir),
        "timeout": timeout,
        "max_output_chars": max_output_chars,
        "memory": memory,
        "db": db,
        "ask_user": ask_user,
        "spawn_subagent": spawn_subagent,
        "facts": facts,
        "checkpoints": checkpoints,
        "scope": scope,
        "changed": set(),
        "memory_ops": [],
        "asked": [],
        "autonomous": False,
    }
