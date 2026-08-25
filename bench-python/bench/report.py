"""Aggregate bench/logs/*.jsonl into comparison tables.

    python -m bench.report                  # every log
    python -m bench.report --by difficulty  # where it starts failing
    python -m bench.report --scenarios      # per-scenario detail
"""
import argparse
import glob
import json
import os
import sys
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
LOGS = os.path.join(HERE, "logs")


def load(pattern="*.jsonl", logs_dir=None):
    rows = []
    for path in sorted(glob.glob(os.path.join(logs_dir or LOGS, pattern))):
        with open(path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if line:
                    try:
                        rows.append(json.loads(line))
                    except json.JSONDecodeError:
                        continue
    return rows


def _avg(vals):
    vals = [v for v in vals if v is not None]
    return sum(vals) / len(vals) if vals else 0.0


def _regress(rows, width=7):
    """Read-only scenarios have no regression axis. Print a dash, not 0%, which
    would read as 'this broke everything'."""
    vals = [r["regress"] for r in rows if r.get("regress") is not None]
    return f"{sum(vals) / len(vals):{width}.0%}" if vals else "-".rjust(width)


def _key(r):
    """Group by every knob that changes the result, so two different setups never
    average into one misleading row."""
    c = r.get("config") or {}
    bits = [c.get("primary") or r["model"], c.get("mode") or r.get("mode", "")]
    cont = c.get("continuation", r.get("continuation"))
    bits.append("noroute" if not cont else f"+{cont.replace('deepseek-v4-', '')}")
    if c.get("effort"):
        bits.append(f"{c['effort']}/{c.get('cheap_effort', '?')}")
    if c.get("max_iterations") and c["max_iterations"] != 20:
        bits.append(f"it{c['max_iterations']}")
    return " ".join(bits)


def by_config(rows):
    print(f"{'configuration':44s} {'change':>7s} {'regress':>8s} {'cost':>9s} "
          f"{'$/pass':>8s} {'calls':>6s} {'cache':>6s} {'time':>7s} {'n':>4s}")
    print("-" * 106)
    groups = defaultdict(list)
    for r in rows:
        groups[_key(r)].append(r)
    for k, rs in sorted(groups.items(), key=lambda kv: -_avg([r["change"] for r in kv[1]])):
        ch = _avg([r["change"] for r in rs])
        cost = sum(r["metrics"]["cost_usd"] for r in rs)
        passes = sum(r["change"] for r in rs)
        print(f"{k:44s} {ch:6.0%} {_regress(rs)} "
              f"${cost:8.4f} ${(cost / passes if passes else 0):7.4f} "
              f"{_avg([r['metrics']['llm_calls'] for r in rs]):6.1f} "
              f"{_avg([r['metrics']['cache_hit_rate'] for r in rs]):5.0%} "
              f"{_avg([r['wall_s'] for r in rs]):6.0f}s {len(rs):4d}")


def by_kind(rows):
    print(f"\n{'kind':12s} {'change':>7s} {'regress':>8s} {'cost':>9s} {'n':>4s}")
    print("-" * 46)
    groups = defaultdict(list)
    for r in rows:
        groups[r["kind"]].append(r)
    for k, rs in sorted(groups.items()):
        print(f"{k:12s} {_avg([r['change'] for r in rs]):6.0%} "
              f"{_regress(rs)} "
              f"${sum(r['metrics']['cost_usd'] for r in rs):8.4f} {len(rs):4d}")


def by_difficulty(rows):
    print(f"\n{'difficulty':12s} {'change':>7s} {'regress':>8s} {'n':>4s}")
    print("-" * 36)
    groups = defaultdict(list)
    for r in rows:
        groups[r.get("difficulty", 1)].append(r)
    for d, rs in sorted(groups.items()):
        print(f"d{d:<11d} {_avg([r['change'] for r in rs]):6.0%} "
              f"{_regress(rs)} {len(rs):4d}")


def by_scenario(rows):
    print(f"\n{'id':5s} {'kind':10s} {'name':34s} {'change':>7s} {'regress':>8s} "
          f"{'cost':>8s} {'n':>3s}")
    print("-" * 84)
    groups = defaultdict(list)
    for r in rows:
        groups[r["id"]].append(r)
    for sid, rs in sorted(groups.items()):
        r0 = rs[0]
        print(f"{sid:5s} {r0['kind']:10s} {r0['name'][:34]:34s} "
              f"{_avg([r['change'] for r in rs]):6.0%} "
              f"{_regress(rs)} "
              f"${_avg([r['metrics']['cost_usd'] for r in rs]):7.4f} {len(rs):3d}")


def failures(rows):
    misses = defaultdict(int)
    for r in rows:
        for name, ok in r.get("change_detail", []):
            if not ok:
                misses[f"{r['id']}: {name}"] += 1
    if not misses:
        print("\nNo failed checks.")
        return
    print(f"\n{'most-failed checks':60s} {'n':>4s}")
    print("-" * 66)
    for k, n in sorted(misses.items(), key=lambda kv: -kv[1])[:20]:
        print(f"{k[:60]:60s} {n:4d}")


def tool_usage(rows):
    hist = defaultdict(int)
    for r in rows:
        for name, n in (r["metrics"].get("tool_histogram") or {}).items():
            hist[name] += n
    if not hist:
        return
    print(f"\n{'tool':26s} {'calls':>7s}")
    print("-" * 35)
    for name, n in sorted(hist.items(), key=lambda kv: -kv[1]):
        print(f"{name:26s} {n:7d}")


def main(argv=None):
    ap = argparse.ArgumentParser(prog="bench.report")
    ap.add_argument("--pattern", default="*.jsonl")
    ap.add_argument("--dir", default=None,
                    help="logs directory (default: bench/logs). Point it at "
                         "../bench-rust/logs to report on the Rust build: the "
                         "rows share a schema, so the same report reads both.")
    ap.add_argument("--scenarios", action="store_true")
    ap.add_argument("--tools", action="store_true")
    ap.add_argument("--all", action="store_true")
    a = ap.parse_args(argv)

    rows = load(a.pattern, a.dir)
    logs_dir = a.dir or LOGS
    if not rows:
        print(f"No logs in {logs_dir} matching {a.pattern}. Run bench.runner first.",
              file=sys.stderr)
        return 1

    print(f"{len(rows)} run(s) from {LOGS}\n")
    by_config(rows)
    by_kind(rows)
    by_difficulty(rows)
    if a.scenarios or a.all:
        by_scenario(rows)
    failures(rows)
    if a.tools or a.all:
        tool_usage(rows)
    return 0


if __name__ == "__main__":
    sys.exit(main())
