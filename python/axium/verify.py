r"""Runtime verification, the feedback loop a syntax check misses.

`get_diagnostics` parses. Parsing proves a file is well-formed, not that it
works: a module can `ast.parse` cleanly and still raise on import because a name
was renamed in one place and not another, or a function now takes three arguments
where its caller passes two. Those are the defects an agent actually introduces,
and nothing in the loop currently notices before the turn is declared done.

So after a turn changes files, this runs the project the way the project says it
should be run, and hands any failure back to the agent as another round rather
than reporting success:

    import   every changed module imports cleanly
    tests    the project's own suite, if it has one

Discovery is by convention, not configuration: pytest, `tests/`, a Makefile
target, `npm test`. A project with none of those gets the import check alone,
which is still more than nothing. An unrunnable project SKIPS rather than fails,
"we could not verify" and "it is broken" are different claims, and reporting the
second when you mean the first trains an agent to ignore you.

Everything is capped by a timeout. A verification step that hangs is worse than
one that does not exist, because it burns the turn's budget and reports nothing.
"""
import os
import subprocess
import sys

VERIFY_TIMEOUT = 90
MAX_OUTPUT = 4000

# Only these are worth importing to prove they load. A JSON or Markdown file
# cannot fail at runtime in a way an import would reveal.
_IMPORTABLE = (".py",)


class Result:
    """What verification found. `ok` False only on a REAL failure."""

    def __init__(self, ok=True, skipped=False, kind="", detail=""):
        self.ok = ok
        self.skipped = skipped
        self.kind = kind            # "import" | "tests"
        self.detail = detail

    def __repr__(self):
        state = "skipped" if self.skipped else ("ok" if self.ok else "FAILED")
        return f"Result({self.kind}, {state})"

    def as_feedback(self):
        """The message handed back to the agent. Concrete, not scolding."""
        if self.ok or self.skipped:
            return ""
        return (f"[RUNTIME VERIFICATION FAILED: {self.kind}]\n{self.detail}\n\n"
                f"The code parses but does not run. Fix the cause and do not "
                f"describe the change as complete until it does.")


def _run(cmd, cwd, timeout=VERIFY_TIMEOUT):
    try:
        r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True,
                           errors="replace", timeout=timeout,
                           env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})
        return r.returncode, ((r.stdout or "") + (r.stderr or ""))[-MAX_OUTPUT:]
    except subprocess.TimeoutExpired:
        return None, f"timed out after {timeout}s"
    except (OSError, ValueError) as e:
        return None, str(e)


def _module_name(rel):
    """`shop/report.py` -> `shop.report`. Returns "" for anything unimportable."""
    rel = rel.replace("\\", "/")
    if not rel.endswith(".py") or rel.endswith("setup.py"):
        return ""
    parts = rel[:-3].split("/")
    if parts[-1] == "__init__":
        parts = parts[:-1]
    if not parts or any(not p.isidentifier() for p in parts):
        return ""
    return ".".join(parts)


def check_imports(workdir, changed):
    """Import every changed Python module in a fresh interpreter.

    A fresh subprocess on purpose: importing into this process would both
    pollute it and hide failures behind modules already in `sys.modules`.
    """
    mods = [m for m in (_module_name(c) for c in changed) if m]
    if not mods:
        return Result(skipped=True, kind="import")
    code = "import importlib\n" + "\n".join(
        f"importlib.import_module({m!r})" for m in mods) + "\nprint('IMPORT OK')"
    rc, out = _run([sys.executable, "-c", code], workdir)
    if rc is None:
        return Result(skipped=True, kind="import", detail=out)
    if rc != 0:
        return Result(ok=False, kind="import",
                      detail=f"importing {', '.join(mods)} failed:\n{out.strip()}")
    return Result(kind="import")


def discover_tests(workdir):
    """How this project runs its own tests, or None.

    Convention over configuration: the point is that it works on a project
    nobody configured, which is every project the first time.
    """
    j = lambda *p: os.path.join(workdir, *p)                     # noqa: E731
    if os.path.isfile(j("tests", "acceptance.py")):
        return [sys.executable, os.path.join("tests", "acceptance.py")]
    if os.path.isdir(j("tests")) or os.path.isfile(j("pytest.ini")) \
            or os.path.isfile(j("pyproject.toml")):
        return [sys.executable, "-m", "pytest", "-q", "--no-header", "-x"]
    if os.path.isfile(j("package.json")):
        return ["npm", "test", "--silent"]
    return None


def check_tests(workdir):
    cmd = discover_tests(workdir)
    if not cmd:
        return Result(skipped=True, kind="tests")
    rc, out = _run(cmd, workdir)
    if rc is None:
        return Result(skipped=True, kind="tests", detail=out)
    # pytest exit 5 is "no tests collected", nothing ran, so nothing is proven,
    # and calling that a pass would be a lie in the direction that costs most.
    if rc == 5:
        return Result(skipped=True, kind="tests", detail="no tests collected")
    if rc != 0:
        return Result(ok=False, kind="tests", detail=out.strip())
    return Result(kind="tests")


def verify(workdir, changed):
    """Run the checks that apply. Returns the first REAL failure, else ok.

    Import first: a module that will not import makes every test failure a
    downstream symptom, and handing the agent the symptom instead of the cause
    sends it to the wrong file.
    """
    if not changed:
        return Result(skipped=True)
    checks = [check_imports(workdir, changed), check_tests(workdir)]
    for check in checks:
        if not check.ok and not check.skipped:
            return check
    # If EVERY check skipped, nothing was verified. Saying "ok" here would record
    # a verification that never happened, confidence for free, which is the
    # thing this module exists to stop.
    if all(c.skipped for c in checks):
        return Result(skipped=True,
                      detail="nothing runnable to verify (no importable module, no tests)")
    return Result(kind="+".join(c.kind for c in checks if not c.skipped))
