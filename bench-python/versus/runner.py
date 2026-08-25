"""Run the head-to-head.

    python -m versus.runner --sanity                  # prove the graders work
    python -m versus.runner                           # both agents, all 5 scenarios
    python -m versus.runner --only V1,V4 --agents axium
    python -m versus.runner --reps 3 --keep

Every (agent, scenario, rep) gets its own freshly generated copy of the seed
project, so nothing an agent does can reach the next run. One JSONL row per
session lands in versus/logs/.
"""
import os
import sys
import json
import time
import shutil
import argparse
import tempfile
from datetime import datetime, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
import axium_path  # noqa: E402  (resolves where the agent package lives)
PY_ROOT = axium_path.AXIUM_ROOT
if PY_ROOT not in sys.path:
    sys.path.insert(0, PY_ROOT)

from bench import fixtures                                   # noqa: E402
from versus import adapters, graders as G, scenarios         # noqa: E402

LOGS = os.path.join(HERE, "logs")
BUILDS = os.path.join(tempfile.gettempdir(), "axium-versus-builds")


def _fresh(agent, sid, rep):
    """A fresh seed copy. The folder name IS the project name Orange resolves."""
    name = f"shop-{sid.lower()}-{agent}-{rep}-{datetime.now():%H%M%S%f}"
    dest = os.path.join(BUILDS, name)
    fixtures.generate(dest)
    return dest, name


# ── sanity ───────────────────────────────────────────────────────────────────
def sanity():
    """No paid run starts until the graders are shown to measure something.

    Three properties matter: the acceptance suite is green on an untouched seed
    (so any later red is agent damage), the scenario graders are NOT already
    satisfied before an agent touches anything, and the executable probes read the
    seed correctly.
    """
    build, _ = _fresh("sanity", "S0", 0)
    problems = []
    try:
        reg = G.grade.regression(build)
        broken = [n for n, ok in reg if not ok]
        if broken:
            problems.append(f"acceptance suite is not green on the pristine seed: {broken}")

        thr, free_above = G.shipping_boundary(build)
        if thr is None or abs(thr - 50.0) > 0.01:
            problems.append(f"shipping probe read the seed threshold as {thr}, expected 50.0")
        if free_above is not False:
            problems.append("shipping probe should see the seed as free BELOW the threshold "
                            "(that is planted bug B4)")

        pristine = G.tree_hash(build)
        if G.touched(pristine, G.tree_hash(build)):
            problems.append("tree_hash is not stable across two reads")

        # A grader that already passes on an untouched project measures nothing.
        stub = _StubSession(build, pristine)
        for sc in scenarios.ALL:
            if sc["id"] == "V4":
                continue          # V4 scores restraint; passing on an untouched tree is correct
            rows = sc["grade"](stub)
            if rows and all(ok for _, ok in rows):
                problems.append(f"{sc['id']} grader already passes before any change")

        print(f"sanity: {len(scenarios.ALL)} scenarios, {len(reg)} acceptance checks, "
              f"{len(problems)} problem(s)")
        for p in problems:
            print("  !!", p)
        if not problems:
            print("  graders detect the planted state and the baseline is green.")
        return not problems
    finally:
        shutil.rmtree(build, ignore_errors=True)


class _StubSession:
    """An untouched session, used only by --sanity."""

    def __init__(self, build, pristine):
        self.build, self.pristine, self.agent_home = build, pristine, build
        self.after = pristine
        self.all_tools = []
        self.turns = [adapters.TurnResult(text="", before=pristine, after=pristine)
                      for _ in range(8)]


# ── one session ──────────────────────────────────────────────────────────────
def run_session(adapter, sc, rep, keep=False, verbose=False, max_turns=0):
    build, project = _fresh(adapter.name, sc["id"], rep)
    session = adapter.open_session(build, project)
    turns = sc["turns"][:max_turns] if max_turns else sc["turns"]
    t0 = time.time()
    try:
        for i, turn in enumerate(turns, 1):
            text = turn["text"].replace("{project}", project)
            r = adapter.send(session, text)
            session.turns.append(r)
            if verbose:
                tools = ",".join(t["name"] for t in r.tool_calls)[:70]
                print(f"      t{i}  {r.wall_s:5.0f}s  {r.llm_calls:2d} calls  "
                      f"${r.cost_usd:.4f}  [{tools}]"
                      + (f"  ERROR {str(r.error)[:50]}" if r.error else ""))

        # Graders index turns positionally, so a truncated (plumbing-only) run is
        # padded rather than allowed to IndexError into a false score.
        while len(session.turns) < len(sc["turns"]):
            session.turns.append(adapters.TurnResult(
                before=session.after, after=session.after))
        change = sc["grade"](session)
        regress = G.grade.regression(build)
    finally:
        adapter.close_session(session)

    tot = session.totals()
    rec = {
        "agent": adapter.name, "label": adapter.label(),
        "id": sc["id"], "name": sc["name"], "axis": sc["axis"], "rep": rep,
        "change": G.pct(change), "regress": G.pct(regress),
        "change_detail": [[n, bool(ok)] for n, ok in change],
        "regress_misses": [n for n, ok in regress if not ok],
        "changed_files": G.touched(session.pristine, session.after),
        "turns": [t.as_dict() for t in session.turns],
        "metrics": tot,
        "wall_s": round(time.time() - t0, 1),
        "stamp": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    }

    passed = sum(1 for _, ok in change if ok)
    misses = [n for n, ok in regress if not ok]
    print(f"  [{sc['id']}] {adapter.name:7s} {sc['name'][:34]:34s} "
          f"change {passed:2d}/{len(change):<2d}  "
          f"regress {len(regress) - len(misses)}/{len(regress)}"
          + (f"  BROKE: {misses[0][:40]}" if misses else ""))
    print(f"       {tot['wall_s']:6.0f}s  {tot['llm_calls']:2d} calls  "
          f"{tot['tool_calls']:2d} tools  {tot['input_tokens']:>8,}in/"
          f"{tot['output_tokens']:>6,}out  ${tot['cost_usd']:.4f}"
          + (f"  ERRORS {len(tot['errors'])}" if tot["errors"] else ""))
    for n, ok in change:
        if not ok:
            print(f"         MISS  {n}")

    if keep:
        rec["build"] = build
    else:
        shutil.rmtree(build, ignore_errors=True)
    return rec


# ── main ─────────────────────────────────────────────────────────────────────
def main(argv=None):
    p = argparse.ArgumentParser(description="Axium vs Orange, five sessions each.")
    p.add_argument("--agents", default="axium,orange",
                   help="comma list: axium, orange (default both)")
    p.add_argument("--only", default="", help="scenario ids, e.g. V1,V4")
    p.add_argument("--reps", type=int, default=1)
    p.add_argument("--sanity", action="store_true", help="check the graders and exit")
    p.add_argument("--keep", action="store_true", help="keep the build directories")
    p.add_argument("--verbose", action="store_true", help="per-turn line while running")
    p.add_argument("--no-sanity", action="store_true", help="skip the pre-run grader check")
    p.add_argument("--max-turns", type=int, default=0,
                   help="stop each session after N turns — smoke-tests the plumbing "
                        "cheaply; scores from such a run are not comparable")
    # axium knobs
    p.add_argument("--config", default=None)
    p.add_argument("--model", default=None, help="axium primary model")
    p.add_argument("--continuation", default=None, help="axium cheap model ('' disables)")
    p.add_argument("--mode", default=None, help="axium tool mode: full|simple")
    # orange knobs
    p.add_argument("--orange-root", default=None)
    p.add_argument("--orange-chat", default=None)
    p.add_argument("--orange-coder", default=None)
    args = p.parse_args(argv)

    if args.sanity:
        return 0 if sanity() else 1
    if not args.no_sanity and not sanity():
        print("\nrefusing to run: fix the graders first (or pass --no-sanity).")
        return 1

    scs = scenarios.select([s for s in args.only.split(",")] if args.only else None)
    if not scs:
        print("no scenarios selected")
        return 1
    names = [a.strip() for a in args.agents.split(",") if a.strip()]

    os.makedirs(LOGS, exist_ok=True)
    os.makedirs(BUILDS, exist_ok=True)
    all_recs = []
    for which in names:
        adapter = adapters.build_adapter(which, args)
        print(f"\n=== {adapter.label()} ===")
        log = os.path.join(LOGS, f"{which}.jsonl")
        for rep in range(args.reps):
            for sc in scs:
                rec = run_session(adapter, sc, rep, keep=args.keep, verbose=args.verbose,
                                  max_turns=args.max_turns)
                all_recs.append(rec)
                if args.max_turns:
                    rec["partial_turns"] = args.max_turns
                    continue          # a truncated run must never pollute the logs
                with open(log, "a", encoding="utf-8") as f:
                    f.write(json.dumps(rec) + "\n")

    print()
    from versus import report
    report.summarise(all_recs)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
