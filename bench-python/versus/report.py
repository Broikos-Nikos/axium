"""Aggregate versus/logs/*.jsonl into a head-to-head table.

    python -m versus.report            # latest run per (agent, scenario)
    python -m versus.report --all      # average every rep on file
    python -m versus.report --detail   # per-check breakdown where they differ

The headline number is deliberately NOT change%. An agent that scores 90% and
breaks the acceptance suite is worse than one that scores 70% and breaks nothing,
so `net` folds regressions in, and `$/pt` prices each point of real progress.
"""
import os
import sys
import json
import glob
import argparse
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
LOGS = os.path.join(HERE, "logs")


def load(all_reps=False):
    recs = []
    for path in sorted(glob.glob(os.path.join(LOGS, "*.jsonl"))):
        with open(path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if line:
                    recs.append(json.loads(line))
    if all_reps:
        return recs
    latest = {}
    for r in recs:
        latest[(r["agent"], r["id"], r.get("rep", 0))] = r
    return list(latest.values())


def _net(r):
    """change, penalised by anything the agent broke that used to work.

    `change` and `regress` are stored as fractions (grade.pct); everything printed
    here is a percentage, so the conversion happens once, at the boundary.
    """
    return round(100.0 * r["change"] * r["regress"], 1)


def summarise(recs, label=""):
    if not recs:
        print("no records")
        return
    by = defaultdict(list)
    for r in recs:
        by[(r["agent"], r["id"])].append(r)

    ids = sorted({r["id"] for r in recs})
    agents = sorted({r["agent"] for r in recs})
    if label:
        print(label)

    head = f"{'scenario':<34s}" + "".join(f"{a:>26s}" for a in agents)
    print(head)
    print("-" * len(head))
    for sid in ids:
        name = next(r["name"] for r in recs if r["id"] == sid)
        row = f"{sid} {name[:30]:<31s}"
        for a in agents:
            rs = by.get((a, sid), [])
            if not rs:
                row += f"{'-':>26s}"
                continue
            ch = 100.0 * sum(r["change"] for r in rs) / len(rs)
            rg = 100.0 * sum(r["regress"] for r in rs) / len(rs)
            cost = sum(r["metrics"]["cost_usd"] for r in rs) / len(rs)
            row += f"{ch:>7.0f}% chg {rg:>4.0f}% reg ${cost:>6.4f}"
        print(row)

    print("-" * len(head))
    tot = f"{'TOTAL':<34s}"
    for a in agents:
        rs = [r for r in recs if r["agent"] == a]
        net = sum(_net(r) for r in rs) / len(rs)
        cost = sum(r["metrics"]["cost_usd"] for r in rs)
        pts = sum(_net(r) for r in rs) / 100.0
        per = (cost / pts) if pts else float("nan")
        tot += f"{net:>9.0f} net ${cost:>7.4f} ${per:>5.3f}/pt"
    print(tot)

    print()
    for a in agents:
        rs = [r for r in recs if r["agent"] == a]
        m = [r["metrics"] for r in rs]
        calls = sum(x["llm_calls"] for x in m)
        tools = sum(x["tool_calls"] for x in m)
        inp = sum(x["input_tokens"] for x in m)
        outp = sum(x["output_tokens"] for x in m)
        cached = sum(x["cached_tokens"] for x in m)
        wall = sum(x["wall_s"] for x in m)
        errs = sum(len(x["errors"]) for x in m)
        hist = defaultdict(int)
        for x in m:
            for k, v in (x.get("tool_histogram") or {}).items():
                hist[k] += v
        top = ", ".join(f"{k} x{v}" for k, v in
                        sorted(hist.items(), key=lambda kv: -kv[1])[:6])
        print(f"{a:8s} {calls:4d} llm calls  {tools:4d} tool calls  "
              f"{inp:>9,}in/{outp:>7,}out  {cached / inp if inp else 0:>4.0%} cached  "
              f"{wall:6.0f}s  {errs} error(s)")
        print(f"         tools: {top or '(none)'}")
        print(f"         label: {rs[0]['label']}")


def detail(recs):
    """Per-check breakdown, showing only the checks the agents disagreed on."""
    by = defaultdict(dict)
    for r in recs:
        for name, ok in r["change_detail"]:
            by[(r["id"], name)][r["agent"]] = ok
    agents = sorted({r["agent"] for r in recs})
    current = None
    for (sid, name), votes in by.items():
        vals = [votes.get(a) for a in agents]
        if len(set(vals)) <= 1:
            continue
        if sid != current:
            print(f"\n{sid}")
            current = sid
        marks = "  ".join(f"{a}:{'PASS' if votes.get(a) else 'fail'}" for a in agents)
        print(f"   {name[:56]:<56s} {marks}")


def main(argv=None):
    p = argparse.ArgumentParser()
    p.add_argument("--all", action="store_true", help="average every rep on file")
    p.add_argument("--detail", action="store_true", help="per-check disagreements")
    args = p.parse_args(argv)
    recs = load(all_reps=args.all)
    summarise(recs)
    if args.detail:
        detail(recs)
    return 0


if __name__ == "__main__":
    sys.exit(main())
