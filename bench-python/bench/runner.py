"""Benchmark runner — drives the REAL agent loop across the scenarios.

Every scenario gets a freshly generated copy of the seed project, so runs never
contaminate each other and a destructive scenario cannot damage anything real.

    python -m bench.runner --sanity                    # validate the graders first
    python -m bench.runner --model deepseek-v4-pro
    python -m bench.runner --only B1,B4,R1 --reps 2
    python -m bench.runner --kind fix --mode simple
    python -m bench.runner --compare deepseek-v4-pro,deepseek-v4-flash

--sanity is not optional ceremony: it proves each fix grader FAILS on the
untouched project and the regression suite PASSES. Without that, a green score
could just mean the grader is broken.
"""
import argparse
import json
import os
import shutil
import sys
import tempfile
import time
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import axium_path  # noqa: E402,F401  (puts the agent package on sys.path)

from axium import config as config_mod                      # noqa: E402
from axium.memory import Memory                             # noqa: E402
from axium.db import Db                                     # noqa: E402
from axium.metrics import Meter                             # noqa: E402
from axium.router import Agent                              # noqa: E402
from bench import fixtures, grade, scenarios                # noqa: E402
from bench import hard_fixtures, hard_scenarios             # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
LOGS = os.path.join(HERE, "logs")
BUILDS = os.path.join(tempfile.gettempdir(), "axium-bench-builds")


def _fresh(tag, seed="small"):
    """A throwaway copy of a seed. `hard` is the larger billing project whose
    defects survive its own test suite."""
    dest = os.path.join(BUILDS, f"{tag}_{datetime.now():%H%M%S%f}")
    (hard_fixtures if seed == "hard" else fixtures).generate(dest)
    return dest


def _all_scenarios():
    return scenarios.ALL + hard_scenarios.SCENARIOS


def _select(ids, kind, tier):
    out = _all_scenarios()
    if tier:
        out = [s for s in out if s.get("tier") == tier]
    if kind:
        out = [s for s in out if s["kind"] == kind]
    want = {i.strip().upper() for i in (ids or []) if i.strip()}
    if want:
        out = [s for s in out if s["id"] in want]
    return out


# ── sanity ───────────────────────────────────────────────────────────────────
def sanity():
    """Graders must detect the planted state on a pristine copy."""
    build = _fresh("sanity")
    problems = []
    try:
        reg = grade.regression(build)
        broken = [n for n, ok in reg if not ok]
        if broken:
            problems.append(f"regression suite is NOT green on the pristine seed: {broken}")

        for sc in scenarios.SCENARIOS:
            if sc["kind"] not in ("fix", "refactor", "feature"):
                continue
            rows = sc["grade"](build)
            if all(ok for _, ok in rows):
                problems.append(f"{sc['id']} grader already passes before any change")

        # The hard seed gets the same two invariants: its smoke suite passes on
        # a pristine copy, and every grader FAILS there.
        hard_build = _fresh("sanity-hard", "hard")
        try:
            hsm = hard_scenarios.smoke(hard_build)
            if not all(ok for _, ok in hsm):
                problems.append(f"hard seed smoke suite is NOT green: "
                                f"{[n for n, ok in hsm if not ok]}")
            for sc in hard_scenarios.SCENARIOS:
                if "grade" not in sc:
                    continue
                if all(ok for _, ok in sc["grade"](hard_build)):
                    problems.append(f"{sc['id']} grader already passes before any change")
        finally:
            shutil.rmtree(hard_build, ignore_errors=True)

        print(f"sanity: {len(_all_scenarios())} scenarios, "
              f"{len(reg)} regression checks, {len(problems)} problem(s)")
        for p in problems:
            print("  !!", p)
        if not problems:
            print("  graders detect the planted state and the baseline is green.")
        return not problems
    finally:
        shutil.rmtree(build, ignore_errors=True)


# ── one scenario ─────────────────────────────────────────────────────────────
def run_scenario(sc, cfg, mode, keep=False, verbose=False):
    hard = sc.get("seed") == "hard"
    build = _fresh(sc["id"], sc.get("seed", "small"))
    workdir = os.path.abspath(build)
    meter = Meter()
    memory = Memory(os.path.join(build, ".axium", "memory.md"))
    db = Db(os.path.join(build, ".axium", "bench.db"))
    # Per-build fact store, for the same reason: the configured default resolves
    # next to config.json, giving every scenario one shared facts.db and letting
    # an earlier run's fact make a later one pass.
    cfg.settings.facts_file = os.path.join(build, ".axium", "facts.db")

    request = sc["request"]
    if sc["kind"] == "aware":
        request = (hard_scenarios.AWARE_PREFIX if hard else scenarios.AWARE_PREFIX) + request

    # M2 compares the restored tree against the untouched seed byte for byte, so
    # the comparison copy has to be taken BEFORE the agent runs. It lives under
    # .axium/, which every grader and the fingerprint already skip.
    if sc.get("pristine_copy"):
        pristine = os.path.join(build, ".axium", "_pristine")
        os.makedirs(os.path.dirname(pristine), exist_ok=True)
        shutil.copytree(build, pristine,
                        ignore=shutil.ignore_patterns(".axium", "__pycache__", ".git"))

    def on_event(kind, payload):
        if verbose and kind == "tool_call":
            print(f"      [{payload['name']}]", file=sys.stderr)

    t0 = time.time()
    error = None
    try:
        agent = Agent(cfg, workdir=workdir, memory=memory, db=db, on_event=on_event, mode=mode)
        # A warmup turn sets the scenario up (M3 builds the Brain) and is NOT
        # graded; a followup is the turn that gets graded, after the setup turn
        # has had a chance to be compacted away (M1). Both share the agent, so
        # history, memory and facts carry across exactly as in a real session.
        history = []
        if sc.get("warmup"):
            warm = agent.run(sc["warmup"], history=history, meter=Meter())
            history = warm.history
        turn = agent.run(request, history=history, meter=meter)
        if sc.get("followup"):
            history = turn.history
            for filler in scenarios.MECHANISM_FILLER:
                history = agent.run(filler, history=history, meter=Meter()).history
            turn = agent.run(sc["followup"], history=history, meter=meter)
        error = turn.error
    except Exception as e:                                  # noqa: BLE001
        turn = type("T", (), {"text": "", "changed": [], "klass": "", "asked": [],
                              "meter": meter, "error": f"{type(e).__name__}: {e}"})()
        error = turn.error
    wall = time.time() - t0
    turn.memory_text = memory.content

    # -- grade --
    # The hard seed carries its own smoke suite; the small one has acceptance.py.
    regression = (lambda b: hard_scenarios.smoke(b)) if hard else grade.regression
    if sc["kind"] == "aware":
        change = sc["grade_answer"](build, turn.text)
        change.append(("made no edits", not turn.changed))
        regress = []
    elif sc["kind"] == "behaviour":
        change = sc["grade_turn"](turn, build)
        regress = regression(build)
    else:
        change = sc["grade"](build)
        regress = regression(build)

    t = meter.totals()
    rec = {
        # Which implementation produced the row. bench-rust writes "rust" into
        # the same schema, so a report over both directories can tell them apart.
        "impl": "python",
        "id": sc["id"], "name": sc["name"], "kind": sc["kind"],
        "difficulty": sc.get("difficulty", 1),
        "tier": sc.get("tier", "baseline"), "seed": sc.get("seed", "small"),
        "model": cfg.models.primary, "continuation": cfg.models.continuation,
        "mode": mode, "class": getattr(turn, "klass", ""),
        "config": {"primary": cfg.models.primary, "continuation": cfg.models.continuation,
                   "effort": cfg.settings.thinking_effort,
                   "cheap_effort": cfg.settings.cheap_effort,
                   "max_iterations": cfg.settings.max_tool_iterations, "mode": mode,
                   "facts": cfg.settings.facts_enabled,
                   "brain": cfg.settings.brain_enabled,
                   "planner": cfg.settings.planner_enabled,
                   "checkpoints": cfg.settings.checkpoints_enabled,
                   "verify": cfg.settings.verify_runtime,
                   "escalation": cfg.settings.edit_escalation},
        "change": grade.pct(change), "regress": grade.pct(regress) if regress else None,
        "change_detail": [[n, bool(ok)] for n, ok in change],
        "regress_misses": [n for n, ok in regress if not ok],
        "changed_files": sorted(turn.changed or []),
        "answer": (turn.text or "")[:3000],
        "asked": list(getattr(turn, "asked", []) or []),
        "error": error,
        "wall_s": round(wall, 1),
        "metrics": t,
        "stamp": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    }

    ok = f"{sum(1 for _, o in change if o)}/{len(change)}"
    reg_txt = ""
    if regress:
        misses = [n for n, o in regress if not o]
        reg_txt = f"  regress {len(regress) - len(misses)}/{len(regress)}"
        if misses:
            reg_txt += f"  BROKE: {misses[0][:50]}"
    print(f"  [{sc['id']}] {sc['name'][:38]:38s} change {ok:>6s}{reg_txt}")
    print(f"       {wall:5.0f}s  {t['llm_calls']:2d} calls  {t['tool_calls']:2d} tools  "
          f"{t['input_tokens']:>7,}in/{t['output_tokens']:>6,}out  "
          f"{t['cache_hit_rate']:>4.0%} cached  ${t['cost_usd']:.4f}"
          + (f"  ERROR {str(error)[:60]}" if error else ""))
    for n, o in change:
        if not o:
            print(f"         MISS  {n}")

    db.close()
    if keep:
        rec["build"] = build
    else:
        shutil.rmtree(build, ignore_errors=True)
    return rec


# ── main ─────────────────────────────────────────────────────────────────────
DEFAULT_ITERATIONS = 20
DEFAULT_CONTINUATION = "deepseek-v4-flash"


def config_tag(cfg, mode, args):
    """A log tag that captures every knob that changes the result.

    Without this, `--continuation ""` and `--max-iterations 2` land in the same
    file as the default config and the report averages three different setups
    into one meaningless row.
    """
    parts = [cfg.models.primary, mode]
    if not cfg.models.continuation:
        parts.append("noroute")
    elif cfg.models.continuation != DEFAULT_CONTINUATION:
        parts.append(f"cont-{cfg.models.continuation}")
    if cfg.settings.max_tool_iterations != DEFAULT_ITERATIONS:
        parts.append(f"it{cfg.settings.max_tool_iterations}")
    parts.append(f"eff-{cfg.settings.thinking_effort}-{cfg.settings.cheap_effort}")
    ablations = [name for name, off in (
        ("nofacts", not cfg.settings.facts_enabled),
        ("nobrain", not cfg.settings.brain_enabled),
        ("noplanner", not cfg.settings.planner_enabled),
        ("nockpt", not cfg.settings.checkpoints_enabled),
        ("noverify", not cfg.settings.verify_runtime),
        ("noescal", not cfg.settings.edit_escalation),
    ) if off]
    if ablations:
        parts.append("-".join(ablations))
    _ = args
    return "__".join(parts)


def build_config(args):
    cfg = config_mod.load(args.config)
    if args.model:
        cfg.models.primary = args.model
        cfg.models.primary_provider = ""
    if args.continuation is not None:
        cfg.models.continuation = args.continuation
        cfg.models.continuation_provider = ""
    if args.classifier:
        cfg.models.classifier = args.classifier
        cfg.models.classifier_provider = ""
    cfg.settings.max_tool_iterations = args.max_iterations
    cfg.settings.thinking_effort = args.effort
    cfg.settings.cheap_effort = args.cheap_effort
    # Ablations. Each flag removes exactly ONE mechanism and nothing else, which
    # is what lets a score difference be attributed to that mechanism rather than
    # to "the new version". Same flags, same names, as bench-rust.
    if getattr(args, "no_facts", False):
        cfg.settings.facts_enabled = False
    if getattr(args, "no_brain", False):
        cfg.settings.brain_enabled = False
    if getattr(args, "no_planner", False):
        cfg.settings.planner_enabled = False
    if getattr(args, "no_checkpoints", False):
        cfg.settings.checkpoints_enabled = False
    if getattr(args, "no_verify", False):
        cfg.settings.verify_runtime = False
    if getattr(args, "no_escalation", False):
        cfg.settings.edit_escalation = False
    return cfg


def run_suite(cfg, scs, mode, reps, keep, verbose, tag):
    print(f"\n=== {cfg.models.primary} · continuation={cfg.models.continuation or '(none)'} "
          f"· mode={mode} · {len(scs)} scenario(s) x {reps} rep(s) ===\n")
    # Append after EVERY scenario, not once at the end. A suite that writes only
    # on completion loses the whole run to any interruption — a Ctrl-C, a
    # timeout, a killed shell — and every one of those scenarios was paid for
    # with real API calls. Learned by losing 21 scenarios' worth, twice.
    os.makedirs(LOGS, exist_ok=True)
    path = os.path.join(LOGS, f"{tag}.jsonl")
    recs = []
    for rep in range(reps):
        if reps > 1:
            print(f"-- rep {rep + 1}/{reps} --")
        for sc in scs:
            rec = run_scenario(sc, cfg, mode, keep, verbose)
            recs.append(rec)
            with open(path, "a", encoding="utf-8") as f:
                f.write(json.dumps(rec, ensure_ascii=False, default=str) + "\n")
    print(f"\nlogged {len(recs)} run(s) -> {os.path.relpath(path, HERE)}")
    return recs


def summarise(recs, label=""):
    print("\n" + "=" * 78)
    if label:
        print(label)
    total_cost = sum(r["metrics"]["cost_usd"] for r in recs)
    for kind in scenarios.KINDS:
        rows = [r for r in recs if r["kind"] == kind]
        if not rows:
            continue
        ch = sum(r["change"] for r in rows) / len(rows)
        rg = [r["regress"] for r in rows if r["regress"] is not None]
        cost = sum(r["metrics"]["cost_usd"] for r in rows)
        wall = sum(r["wall_s"] for r in rows)
        line = f"{kind:10s} change {ch:6.0%}"
        if rg:
            line += f"   regress {sum(rg) / len(rg):6.0%}"
        line += f"   ${cost:7.4f}  {wall / 60:5.1f}min  ({len(rows)} runs)"
        print(line)
    overall = sum(r["change"] for r in recs) / len(recs) if recs else 0
    allreg = [r["regress"] for r in recs if r["regress"] is not None]
    print("-" * 78)
    print(f"{'OVERALL':10s} change {overall:6.0%}"
          + (f"   regress {sum(allreg) / len(allreg):6.0%}" if allreg else "")
          + f"   ${total_cost:7.4f}"
          + f"  {sum(r['wall_s'] for r in recs) / 60:5.1f}min")
    errs = [r for r in recs if r["error"]]
    if errs:
        print(f"{len(errs)} run(s) errored: " + ", ".join(r["id"] for r in errs))
    return overall, total_cost


def main(argv=None):
    ap = argparse.ArgumentParser(prog="bench.runner")
    ap.add_argument("--config")
    ap.add_argument("--model", help="primary model (default: from config)")
    ap.add_argument("--continuation", help="cheap continuation model ('' to disable routing)")
    ap.add_argument("--classifier")
    ap.add_argument("--mode", default="supercharge",
                    choices=["simple", "supercharge", "skills"])
    ap.add_argument("--kind", choices=list(scenarios.KINDS))
    ap.add_argument("--tier", choices=list(hard_scenarios.TIERS),
                    help="medium | hard | very hard | extremely hard")
    ap.add_argument("--only", default="", help="comma-separated scenario ids")
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--max-iterations", type=int, default=20)
    ap.add_argument("--effort", default="max",
                    help="reasoning effort for the PRIMARY model: off|low|medium|high|max")
    ap.add_argument("--cheap-effort", default="low",
                    help="reasoning effort for classifier/continuation/compactor/review")
    ap.add_argument("--compare", default="",
                    help="comma-separated models: run the whole suite once per model")
    ap.add_argument("--no-facts", action="store_true",
                    help="ablation: disable the typed fact store")
    ap.add_argument("--no-brain", action="store_true",
                    help="ablation: disable the Project Brain preload")
    ap.add_argument("--no-planner", action="store_true",
                    help="ablation: disable the grounded planner")
    ap.add_argument("--no-checkpoints", action="store_true",
                    help="ablation: disable turn snapshots and undo_turn")
    ap.add_argument("--no-verify", action="store_true",
                    help="ablation: disable post-edit runtime verification")
    ap.add_argument("--no-escalation", action="store_true",
                    help="ablation: disable 3-strike edit escalation")
    ap.add_argument("--keep", action="store_true", help="keep build dirs for inspection")
    ap.add_argument("--sanity", action="store_true", help="validate graders and exit")
    ap.add_argument("--list", action="store_true", help="list scenarios and exit")
    ap.add_argument("-v", "--verbose", action="store_true")
    a = ap.parse_args(argv)

    if a.list:
        for s in _all_scenarios():
            print(f"{s['id']:4s} {s['kind']:10s} d{s.get('difficulty', 1)}  "
                  f"{s.get('tier', 'baseline'):15s} {s['name']}")
        return 0

    if a.sanity:
        return 0 if sanity() else 1

    scs = _select(a.only.split(","), a.kind, a.tier)
    if not scs:
        print("No scenarios matched.", file=sys.stderr)
        return 1

    if not sanity():
        print("\nAborting: graders are not trustworthy.", file=sys.stderr)
        return 1

    os.makedirs(BUILDS, exist_ok=True)
    results = {}
    models = [m.strip() for m in a.compare.split(",") if m.strip()] or [None]
    for m in models:
        args_copy = argparse.Namespace(**vars(a))
        if m:
            args_copy.model = m
        cfg = build_config(args_copy)
        tag = config_tag(cfg, a.mode, a)
        recs = run_suite(cfg, scs, a.mode, a.reps, a.keep, a.verbose, tag)
        results[cfg.models.primary] = recs
        summarise(recs, f"{cfg.models.primary} · mode={a.mode}")

    if len(results) > 1:
        print("\n" + "=" * 78)
        print("MODEL COMPARISON")
        print(f"{'model':26s} {'change':>8s} {'regress':>9s} {'cost':>9s} {'time':>8s}")
        for model, recs in results.items():
            ch = sum(r["change"] for r in recs) / len(recs)
            rg = [r["regress"] for r in recs if r["regress"] is not None]
            print(f"{model:26s} {ch:7.0%} "
                  f"{(sum(rg) / len(rg) if rg else 0):8.0%} "
                  f"${sum(r['metrics']['cost_usd'] for r in recs):8.4f} "
                  f"{sum(r['wall_s'] for r in recs) / 60:7.1f}m")
    return 0


if __name__ == "__main__":
    sys.exit(main())
