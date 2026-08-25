"""The grader bridge: fixtures and graders as a subprocess, JSON in and out.

`bench-rust/` drives the Rust binary through the same scenarios as `bench.runner`
drives the Python one. The graders are Python that imports the agent's output
code and executes it, so reimplementing them in Rust would mean two graders that
could disagree — and a benchmark whose two halves disagree about what "correct"
means is not a comparison. So the graders stay here, single-source, and this is
the process boundary `bench-rust` talks to.

    python bridge.py generate  <dest>                  fresh seed project at <dest>
    python bridge.py sanity                            validate the graders (free)
    python bridge.py grade <id> --build <dir> --turn <turn.json> [--memory <file>]
    python bridge.py regression --build <dir>

Every subcommand prints exactly one JSON object on stdout. Errors go to stderr
and a non-zero exit, never into the JSON, so a caller cannot mistake a crashed
grader for a scenario that scored zero.

`grade` takes the turn as the JSON `--once` prints, and adapts it into the shape
the behaviour graders expect: they were written against the Python agent's
`Turn` object (`turn.meter.tool_calls`, `turn.klass`, `turn.asked`, ...), and
the adapter is where those two worlds meet rather than in every grader.
"""
import argparse
import json
import os
import shutil
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from bench import fixtures, grade, scenarios      # noqa: E402


# ── turn adapter ─────────────────────────────────────────────────────────────
class _Meter:
    """The subset of `axium.metrics.Meter` the behaviour graders read."""

    def __init__(self, metrics):
        hist = metrics.get("tool_histogram") or {}
        # Graders iterate tool_calls by name; the histogram carries the same
        # information, expanded back to one entry per call.
        self.tool_calls = [{"name": name} for name, n in hist.items()
                           for _ in range(int(n))]
        self.calls = [{"role": role} for role in (metrics.get("by_role") or {})]
        self.cost = float(metrics.get("cost_usd") or 0.0)


class _Turn:
    """What `--once` printed, wearing the Python `Turn` interface."""

    def __init__(self, once, memory_text=""):
        self.text = once.get("text") or ""
        self.changed = list(once.get("changed") or [])
        self.klass = once.get("class") or ""
        self.asked = list(once.get("asked") or [])
        self.error = once.get("error")
        self.memory_text = memory_text
        self.meter = _Meter(once.get("metrics") or {})


def _rows(rows):
    return [[name, bool(ok)] for name, ok in rows]


# ── subcommands ──────────────────────────────────────────────────────────────
def cmd_generate(a):
    fixtures.generate(a.dest)
    return {"ok": True, "dest": os.path.abspath(a.dest)}


def cmd_sanity(a):
    """Same check `bench.runner --sanity` runs, so the two suites trust the same
    graders for the same reason."""
    import tempfile
    from datetime import datetime
    build = os.path.join(tempfile.gettempdir(), "axium-bench-builds",
                         f"sanity_{datetime.now():%H%M%S%f}")
    fixtures.generate(build)
    problems = []
    try:
        reg = grade.regression(build)
        broken = [n for n, ok in reg if not ok]
        if broken:
            problems.append(f"regression suite is NOT green on the pristine seed: {broken}")
        for sc in scenarios.SCENARIOS:
            if sc["kind"] not in ("fix", "refactor", "feature"):
                continue
            if all(ok for _, ok in sc["grade"](build)):
                problems.append(f"{sc['id']} grader already passes before any change")
    finally:
        shutil.rmtree(build, ignore_errors=True)
    return {"ok": not problems, "scenarios": len(scenarios.ALL),
            "regression_checks": len(reg), "problems": problems}


def cmd_grade(a):
    sc = scenarios.BY_ID.get(a.id.upper())
    if not sc:
        raise SystemExit(f"unknown scenario id: {a.id}")
    with open(a.turn, encoding="utf-8") as f:
        once = json.load(f)
    memory_text = ""
    if a.memory and os.path.exists(a.memory):
        with open(a.memory, encoding="utf-8", errors="replace") as f:
            memory_text = f.read()
    turn = _Turn(once, memory_text)

    # Same three-way split as bench.runner.run_scenario, so a Rust row and a
    # Python row for the same scenario were graded by the same code path.
    if sc["kind"] == "aware":
        change = sc["grade_answer"](a.build, turn.text)
        change.append(("made no edits", not turn.changed))
        regress = []
    elif sc["kind"] == "behaviour":
        change = sc["grade_turn"](turn, a.build)
        regress = grade.regression(a.build)
    else:
        change = sc["grade"](a.build)
        regress = grade.regression(a.build)

    return {
        "id": sc["id"], "name": sc["name"], "kind": sc["kind"],
        "difficulty": sc.get("difficulty", 1),
        "change": grade.pct(change),
        "regress": grade.pct(regress) if regress else None,
        "change_detail": _rows(change),
        "regress_misses": [n for n, ok in regress if not ok],
    }


def cmd_regression(a):
    rows = grade.regression(a.build)
    return {"regress": grade.pct(rows), "detail": _rows(rows)}


def cmd_request(a):
    """The exact prompt text the Python runner would send, prefix included."""
    sc = scenarios.BY_ID.get(a.id.upper())
    if not sc:
        raise SystemExit(f"unknown scenario id: {a.id}")
    request = sc["request"]
    if sc["kind"] == "aware":
        request = scenarios.AWARE_PREFIX + request
    return {"id": sc["id"], "request": request}


def main(argv=None):
    ap = argparse.ArgumentParser(prog="bridge", description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    g = sub.add_parser("generate"); g.add_argument("dest"); g.set_defaults(fn=cmd_generate)
    s = sub.add_parser("sanity"); s.set_defaults(fn=cmd_sanity)
    r = sub.add_parser("grade")
    r.add_argument("id"); r.add_argument("--build", required=True)
    r.add_argument("--turn", required=True, help="JSON file: what --once printed")
    r.add_argument("--memory", default="", help="the memory file the turn wrote to")
    r.set_defaults(fn=cmd_grade)
    q = sub.add_parser("regression"); q.add_argument("--build", required=True)
    q.set_defaults(fn=cmd_regression)
    t = sub.add_parser("request"); t.add_argument("id"); t.set_defaults(fn=cmd_request)

    a = ap.parse_args(argv)
    try:
        out = a.fn(a)
    except SystemExit:
        raise
    except Exception as e:                                  # noqa: BLE001
        # A crashed grader must look like a crash, not like a zero score.
        print(f"bridge {a.cmd} failed: {type(e).__name__}: {e}", file=sys.stderr)
        return 2
    print(json.dumps(out, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
