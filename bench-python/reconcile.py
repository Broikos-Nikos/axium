r"""Check what each harness SAID it spent against what actually crossed the wire.

The self-reported numbers were each verified exact once, against a single run.
That is a snapshot, not a guarantee: a harness that reports correctly on a
one-turn task can still lose a sub-agent's usage on a twelve-turn one, and the
whole point of the recording proxy is that the question is answerable without
paying for another suite.

So every suite gets reconciled. Per harness:

    self-reported   summed from the runner's own log
    wire            summed from the provider's usage blocks in the transcript
    delta           the two should agree; anything else is a finding

Costs nothing - it reads two files.

    python reconcile.py                                  # newest suite log
    python reconcile.py --log logs_xharness/xharness_....jsonl
"""
import argparse
import glob
import json
import os
import sys
from datetime import datetime, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
LOGS = os.path.join(HERE, "logs_xharness")
RUNS = os.path.join(HERE, "runs")


def wire_totals(run_root, since="", until=""):
    """Provider-reported usage, per harness, straight from the transcripts.

    prompt_tokens ALREADY includes the cache-hit tokens on DeepSeek's
    OpenAI-compatible shape, so prompt + completion is every token processed
    with nothing double counted. That single definition is why the proxy
    exists: the three harnesses do not agree on it among themselves.

    `since`/`until` scope the transcript to one suite. The transcripts are
    append-only and span every run ever recorded, so comparing a whole
    transcript against one suite's log reports a difference that is really just
    the other runs sitting in the same file. That looked exactly like a 26-30%
    undercount by all three harnesses at once - which is the shape of ONE
    measurement bug, not three independent reporting bugs. An upper bound
    matters just as much: a suite running concurrently appends to the same
    transcript and inflates the comparison while it runs.
    """
    out = {}
    for path in glob.glob(os.path.join(run_root, "*", "calls.jsonl")):
        harness = os.path.basename(os.path.dirname(path))
        t = out.setdefault(harness, {"calls": 0, "prompt": 0, "completion": 0,
                                     "cached": 0, "errors": 0})
        with open(path, encoding="utf-8") as f:
            for line in f:
                if not line.strip():
                    continue
                try:
                    r = json.loads(line)
                except ValueError:
                    continue
                ts = r.get("ts") or ""
                if (since and ts < since) or (until and ts > until):
                    continue
                t["calls"] += 1
                if r.get("status", 0) >= 400 or r.get("error"):
                    t["errors"] += 1
                u = r.get("usage") or {}
                t["prompt"] += int(u.get("prompt_tokens") or 0)
                t["completion"] += int(u.get("completion_tokens") or 0)
                t["cached"] += int(u.get("prompt_cache_hit_tokens") or 0)
    for t in out.values():
        t["total"] = t["prompt"] + t["completion"]
    return out


def self_totals(log_path):
    out = {}
    with open(log_path, encoding="utf-8") as f:
        for line in f:
            if not line.strip():
                continue
            r = json.loads(line)
            h = r.get("harness")
            t = out.setdefault(h, {"runs": 0, "tokens": 0, "unmeasured": 0,
                                   "tool_calls": 0})
            t["runs"] += 1
            t["tokens"] += int(r.get("tokens") or 0)
            t["tool_calls"] += int(r.get("tool_calls") or 0)
            if r.get("unknown_usage"):
                t["unmeasured"] += 1
    return out


def main(argv=None):
    ap = argparse.ArgumentParser(prog="reconcile", description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--log", default="", help="suite log; default is the newest")
    ap.add_argument("--runs", default=os.path.join(RUNS, "matrix"),
                    help="transcript root, one directory per harness")
    ap.add_argument("--since", default="",
                    help="ISO timestamp; default is the suite log's own stamp")
    ap.add_argument("--until", default="",
                    help="ISO timestamp; default is the suite log's mtime")
    ap.add_argument("--tolerance", type=float, default=0.02,
                    help="fraction the two may differ by before it is a finding")
    a = ap.parse_args(argv)

    log = a.log
    if not log:
        candidates = sorted(glob.glob(os.path.join(LOGS, "xharness_*.jsonl")))
        if not candidates:
            print("no suite log found", file=sys.stderr)
            return 1
        log = candidates[-1]

    mine = self_totals(log)
    since = a.since
    if not since:
        with open(log, encoding="utf-8") as f:
            for line in f:
                if line.strip():
                    since = json.loads(line).get("stamp") or ""
                    break
    until = a.until
    if not until:
        # When the suite log was last written. Anything after it belongs to a
        # different run, concurrent or later.
        until = datetime.fromtimestamp(os.path.getmtime(log),
                                       timezone.utc).isoformat(timespec="seconds")
    wire = wire_totals(a.runs, since, until)

    print(f"\nsuite log : {os.path.relpath(log, HERE)}")
    print(f"transcripts: {os.path.relpath(a.runs, HERE)}")
    print(f"window     : {since or chr(40)+chr(41)} .. {until}\n")
    print(f"{'harness':10}{'runs':>6}{'self-reported':>15}{'wire':>13}"
          f"{'delta':>12}{'wire calls':>12}{'errors':>8}")

    findings = []
    for h in sorted(set(mine) | set(wire)):
        m = mine.get(h, {"runs": 0, "tokens": 0, "unmeasured": 0})
        w = wire.get(h)
        if not w:
            print(f"{h:10}{m['runs']:>6}{m['tokens']:>15,}{'NO TRANSCRIPT':>13}")
            findings.append(f"{h}: ran but produced no transcript - it bypassed "
                            f"the proxy, so its numbers are unverified")
            continue
        delta = m["tokens"] - w["total"]
        frac = abs(delta) / w["total"] if w["total"] else (1.0 if delta else 0.0)
        print(f"{h:10}{m['runs']:>6}{m['tokens']:>15,}{w['total']:>13,}"
              f"{delta:>+12,}{w['calls']:>12}{w['errors']:>8}")
        if frac > a.tolerance:
            findings.append(
                f"{h}: self-reported {m['tokens']:,} but the wire saw "
                f"{w['total']:,} ({frac:.0%} out). Use the wire figure.")
        if m.get("unmeasured"):
            findings.append(f"{h}: {m['unmeasured']} run(s) reported no usage "
                            f"at all - unmeasured, not zero")

    print("\nwire = prompt + completion from the provider's own usage block.")
    print("prompt already includes cache hits on this provider, so nothing is "
          "double counted.")

    print("\n" + "=" * 74)
    if findings:
        print(f"FINDINGS ({len(findings)}):")
        for f in findings:
            print("  -", f)
        return 1
    print(f"RECONCILED: every harness agrees with the wire within "
          f"{a.tolerance:.0%}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
