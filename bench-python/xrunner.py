r"""Cross-harness runner: Axium against Hermes and OpenClaw, one model.

Every harness gets byte-identical copies of the same seed, the same prompt text,
the same model, and the same absolute token ceiling. What differs is only the
harness, which is the point: correctness saturates for any model worth using, so
the discriminator is what each harness SPENDS and whether it actually delivers.

    python xrunner.py --calibrate            measure a solve, print budget advice
    python xrunner.py --only X-LOCATE
    python xrunner.py --harness axium,hermes
    python xrunner.py --dnf-multiple 10

Outcomes are kept distinct, because a comparison that blurs them is useless:

    classified  solved, inside the DNF budget
    partial     finished, met only part of the requirement
    regressed   finished, broke the project's own tests
    errored     crashed before finishing
    failed      finished, solved nothing
    dnf         did not finish inside the budget

Prompt text is agent-neutral. No tool names, no framework vocabulary, nothing
that reads as written for one harness.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import axium_path  # noqa: E402,F401

from axium import pricing                       # noqa: E402
from bench import budget as budget_mod          # noqa: E402
from bench import classify as classify_mod      # noqa: E402
from bench import large_fixtures, matrix, xharness   # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
LOGS = os.path.join(HERE, "logs_xharness")
BUILDS = os.path.join(tempfile.gettempdir(), "axium-xharness-builds")

AXIUM_BIN = os.path.join(REPO, "target", "release", "axium.exe")
HERMES = r"C:\tools\harnesses\hermes-agent"
OPENCLAW = r"C:\tools\harnesses\openclaw"

MODEL = "deepseek-v4-pro"
TURN_TIMEOUT = 900


def _key():
    with open(os.path.join(REPO, "python", "config.json"), encoding="utf-8") as f:
        return json.load(f)["api_keys"]["deepseek"]


# Every harness is pointed at the recording proxy rather than the provider, so
# one transcript holds every request and response for the whole suite. Two
# reasons, both learned the hard way:
#
#   1. The three harnesses disagree about what a token is. Three cache-counting
#      bugs turned up in one afternoon, each favouring a different harness. The
#      provider's own usage block is the only definition all three share.
#   2. A benchmark should be paid for once. With the bodies on disk, any later
#      question is answered by re-reading rather than re-running.
#
# One port per harness, each recording to its own transcript. A single shared
# port would work but every call would land in one file with no reliable way to
# attribute it afterwards - the request bodies do not say who sent them.
#
# Empty disables recording and every harness talks to the provider directly.
PROXY_PORTS = {"axium": 8901, "hermes": 8902, "openclaw": 8903}
PROXY_HOST = ""            # e.g. "http://127.0.0.1"; empty disables
CURRENT = [""]             # harness currently running; set by run_one


def proxy_url(harness):
    port = PROXY_PORTS.get(harness)
    return f"{PROXY_HOST}:{port}/v1" if (PROXY_HOST and port) else ""


def _proxy_env():
    """Base-URL overrides for the harness currently running.

    OpenClaw is absent on purpose: it takes its base URL from its profile
    config, not the environment, so its port is set there instead."""
    url = proxy_url(CURRENT[0])
    if not url:
        return {}
    return {
        "AXIUM_BASE_URL_DEEPSEEK": url,       # axium, both builds
        "OPENAI_BASE_URL": url,               # hermes (verified: routes)
        "DEEPSEEK_BASE_URL": url,             # hermes, belt and braces
    }


def _run(cmd, cwd, env=None, timeout=TURN_TIMEOUT):
    e = {**os.environ, **_proxy_env(), **(env or {})}
    try:
        r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True,
                           errors="replace", timeout=timeout, env=e, stdin=subprocess.DEVNULL)
        return r.returncode, r.stdout, r.stderr, False
    except subprocess.TimeoutExpired:
        return None, "", f"timed out after {timeout}s", True


# ── adapters ─────────────────────────────────────────────────────────────────
# Each returns (text, usage) where usage is {tokens, tool_calls, llm_calls,
# cost_usd}. Every harness reports these differently; normalising here is what
# makes the comparison possible at all.
class Axium:
    name = "axium"

    def __init__(self, model):
        self.model = model

    def prepare(self, build):
        """Per-build config so nothing reaches real memory, facts or history."""
        cfg_dir = os.path.join(build, ".axium")
        os.makedirs(cfg_dir, exist_ok=True)
        with open(os.path.join(REPO, "python", "config.json"), encoding="utf-8") as f:
            cfg = json.load(f)
        cfg["models"]["primary"] = self.model
        # The Rust loader requires fields a Python-flavoured config omits, and
        # refuses to start without them. Same gap bench-rust hit.
        for k, v in (("max_history_messages", 200), ("token_limit", 80000),
                     ("max_tokens", 8192), ("terminal_timeout_secs", 120),
                     ("max_output_chars", 15000)):
            cfg["settings"].setdefault(k, v)
        cfg["settings"].update({
            "working_directory": build.replace("\\", "/"),
            "memory_file": "memory.md", "facts_file": "facts.db",
            "conversation_logging": False, "mode": "supercharge",
        })
        if not cfg["models"].get("compactor"):
            cfg["models"]["compactor"] = cfg["models"].get("classifier") or self.model
        cfg.setdefault("agent", {"name": "Axium", "soul": ""})
        path = os.path.join(cfg_dir, "config.json")
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            json.dump(cfg, f, indent=2)
        return path

    def turn(self, build, prompt, session, state):
        cfg = state.setdefault("cfg", self.prepare(build))
        code, out, err, to = _run(
            [AXIUM_BIN, "--once", prompt, "--workdir", build,
             "--config", cfg, "--session", session], build)
        if to:
            return "", {"timeout": True}
        try:
            d = json.loads(out.strip())
        except (ValueError, TypeError):
            return "", {"error": f"no JSON (exit {code}): {err.strip()[-300:]}"}
        m = d.get("metrics") or {}
        return d.get("text", ""), {
            "tokens": int(m.get("input_tokens", 0)) + int(m.get("output_tokens", 0)),
            "tool_calls": int(m.get("tool_calls", 0)),
            "llm_calls": int(m.get("llm_calls", 0)),
            "cost_usd": float(m.get("cost_usd", 0.0)),
            "error": d.get("error"),
        }


class Hermes:
    name = "hermes"

    def __init__(self, model):
        self.model = model

    def turn(self, build, prompt, session, state):
        exe = os.path.join(HERMES, ".venv", "Scripts", "hermes.exe")
        # --usage-file is the one-shot usage report: token counts, api_calls and
        # an estimated cost. Without it Hermes reports nothing on this path, and
        # an unmeasured harness cannot be compared on cost at all.
        usage_path = os.path.join(build, ".axium", f"hermes_usage_{session}.json")
        os.makedirs(os.path.dirname(usage_path), exist_ok=True)
        code, out, err, to = _run(
            # cwd is what actually scopes Hermes; --in did not bind on a
            # verification run (it answered about the HOME directory instead).
            # Both are passed, and cwd=build below is the one that matters.
            [exe, "-z", prompt, "--in", build, "-m", self.model,
             "--provider", "deepseek", "--yolo", "--usage-file", usage_path],
            build, env={"DEEPSEEK_API_KEY": _key()})
        if to:
            return "", {"timeout": True}
        u = {"tokens": None, "tool_calls": None, "llm_calls": None,
             "cost_usd": None,
             "error": None if code == 0 else err.strip()[-300:]}
        try:
            with open(usage_path, encoding="utf-8") as f:
                rep = json.load(f)
            # THIRD cache-counting bug of this exercise. Hermes reports
            # cache_read_tokens SEPARATELY from input_tokens, exactly as OpenClaw
            # does, and summing input+output alone missed 243,840 of 287,081
            # tokens on a verification run - an 85% undercount that would have
            # made Hermes look several times cheaper than it is.
            #
            # Every harness counts cache differently. Prefer the harness's own
            # total when it publishes one, and reconstruct it otherwise.
            total = rep.get("total_tokens")
            if total:
                u["tokens"] = int(total)
            else:
                u["tokens"] = (int(rep.get("input_tokens", 0) or 0)
                               + int(rep.get("output_tokens", 0) or 0)
                               + int(rep.get("cache_read_tokens", 0) or 0))
            u["llm_calls"] = rep.get("api_calls")
            u["cost_usd"] = rep.get("estimated_cost_usd",
                                    rep.get("estimated_cost", rep.get("cost")))
            u["breakdown"] = {k: rep.get(k) for k in
                              ("input_tokens", "output_tokens", "cache_read_tokens",
                               "reasoning_tokens", "total_tokens", "api_calls")}
        except (OSError, ValueError, TypeError):
            pass          # stays None: unmeasured, never zero
        return out.strip(), u


class OpenClaw:
    name = "openclaw"

    def __init__(self, model):
        self.model = model

    def turn(self, build, prompt, session, state):
        exe = os.path.join(OPENCLAW, "node_modules", ".bin", "openclaw.cmd")
        code, out, err, to = _run(
            [exe, "--profile", "bench", "agent", "--local", "--json",
             "--session-id", session, "--model", f"deepseek/{self.model}",
             "-m", prompt],
            build, env={"DEEPSEEK_API_KEY": _key(),
                        "OPENCLAW_WORKSPACE_DIR": build})
        if to:
            return "", {"timeout": True}
        try:
            d = json.loads(out.strip())
        except (ValueError, TypeError):
            return "", {"error": f"no JSON (exit {code}): {err.strip()[-300:]}"}
        payloads = d.get("payloads") or []
        text = " ".join(p.get("text", "") for p in payloads if isinstance(p, dict))
        meta = d.get("meta") or {}
        usage = (meta.get("agentMeta") or {}).get("usage") or {}
        tools = (meta.get("toolSummary") or {}).get("tools") or []
        # cacheRead is reported SEPARATELY from input here, whereas Axium's
        # input_tokens already contains its cached reads (verified: cache_read
        # is a subset, hit rate 0.62). Summing all three puts both harnesses on
        # "every token the provider processed".
        #
        # Getting this wrong made OpenClaw look 7.7x cheaper than Axium on the
        # first real run, because one side was counting cache and the other was
        # not. It is the single easiest way to publish a false comparison.
        tokens = (int(usage.get("input", 0)) + int(usage.get("output", 0))
                  + int(usage.get("cacheRead", 0)))
        return text, {
            "tokens": tokens,
            "tool_calls": len(tools),
            "llm_calls": None,
            "cost_usd": None,
            "error": None,
            "breakdown": {"input": usage.get("input"), "output": usage.get("output"),
                          "cacheRead": usage.get("cacheRead")},
        }


ADAPTERS = {a.name: a for a in (Axium, Hermes, OpenClaw)}


# ── one scenario against one harness ─────────────────────────────────────────
def run_one(adapter, sc, ceiling, keep=False, verbose=False):
    sid = sc["id"]
    CURRENT[0] = adapter.name
    build = os.path.join(BUILDS, f"{sid}_{adapter.name}_{datetime.now():%H%M%S%f}")
    # Each scenario carries its own seed builder: the matrix tiers use trees of
    # different sizes, and two of them plant deliberately misleading evidence.
    seed = sc.get("seed") or large_fixtures.generate
    seed(build)
    pristine = None
    if sc["kind"] == "restore":
        pristine = build + "_pristine"
        seed(pristine)

    session = f"x-{sid}-{adapter.name}-{int(time.time())}"
    state, spent = {}, {"tokens": 0, "tool_calls": 0, "llm_calls": 0, "cost_usd": 0.0}
    err_text = ""
    unknown_usage = False
    stopped, errored, last_text = False, False, ""
    t0 = time.time()

    turns = [sc["request"]] + list(sc.get("filler") or [])
    if sc.get("followup"):
        turns.append(sc["followup"])

    for i, prompt in enumerate(turns):
        if verbose:
            print(f"      [{adapter.name}] turn {i + 1}/{len(turns)}", file=sys.stderr)
        text, u = adapter.turn(build, prompt, session, state)
        if u.get("timeout"):
            errored, last_text = True, ""
            err_text = f"turn {i + 1}/{len(turns)}: timed out after {TURN_TIMEOUT}s"
            break
        if u.get("error"):
            # Keep the message. An errored run with no error text is a dead end:
            # the build is gone, the turn is gone, and the only way left to find
            # out what happened is to pay for the scenario again. M3 cost
            # exactly that lesson.
            errored = True
            err_text = f"turn {i + 1}/{len(turns)}: {u['error']}"
            last_text = text
            break
        # Only a missing TOKEN count makes a run uncomparable on cost. A harness
        # that reports tokens but not its own cost estimate is still measurable;
        # flagging that as "unknown" would discard good data.
        if u.get("tokens") is None:
            unknown_usage = True
        for k in ("tokens", "tool_calls", "llm_calls", "cost_usd"):
            v = u.get(k)
            if v is not None:
                spent[k] += v
        if u.get("breakdown"):
            state.setdefault("breakdowns", []).append(u["breakdown"])
        last_text = text or last_text
        # The live cap: stop the moment the ceiling is crossed, so a DNF is a
        # fact about what happened rather than a label applied afterwards.
        if ceiling and ceiling.tokens and spent["tokens"] > ceiling.tokens:
            stopped = True
            break

    wall = time.time() - t0

    # ── grade ──
    if sc["kind"] == "restore":
        rows = sc["grade_restore"](build, pristine)
    elif sc["kind"] == "recall":
        rows = sc["grade_recall"](last_text, build)
    else:
        rows = sc["grade"](build)
    passed = sum(1 for _, ok in rows if ok)
    smoke_rows = xharness.smoke(build)
    regressed = not all(ok for _, ok in smoke_rows)

    res = classify_mod.Result(
        adapter.name, sid, solved=(passed == len(rows)) and not regressed,
        tokens=spent["tokens"], tool_calls=spent["tool_calls"],
        wall_s=wall, cost_usd=spent["cost_usd"],
        stopped_at_ceiling=stopped, checks_passed=passed, checks_total=len(rows),
        regressed=regressed, errored=errored, tokens_measured=not unknown_usage,
        detail="; ".join(n for n, ok in rows if not ok)[:300])
    # The final answer, logged. A text-graded check that fails is otherwise
    # undiagnosable without re-running the scenario, and re-running is the
    # expensive thing this whole setup exists to avoid. Truncated: enough to
    # see what was said, not enough to bloat the log.
    res.answer = (last_text or "")[:1500]
    res.error_text = err_text
    res.unknown_usage = unknown_usage
    res.tokens_measured = not unknown_usage
    res.rows = rows

    if not keep:
        shutil.rmtree(build, ignore_errors=True)
        if pristine:
            shutil.rmtree(pristine, ignore_errors=True)
    return res


def main(argv=None):
    ap = argparse.ArgumentParser(prog="xrunner", description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--harness", default="axium,hermes,openclaw")
    ap.add_argument("--only", default="")
    ap.add_argument("--suite", default="matrix", choices=("matrix", "x"),
                    help="matrix = the 3x3 N/M/R grid; x = the original four")
    ap.add_argument("--cat", default="", help="navigation,memory,restore")
    ap.add_argument("--tier", default="", help="hard,very hard,crazy hard")
    ap.add_argument("--model", default=MODEL)
    ap.add_argument("--dnf-multiple", type=float, default=classify_mod.DNF_MULTIPLE)
    ap.add_argument("--calibrate", action="store_true",
                    help="run Axium only and print measured budget advice")
    ap.add_argument("--proxy", default="http://127.0.0.1",
                    help="host of the recording proxies (one port per harness, "
                         "see PROXY_PORTS); '' to disable recording")
    ap.add_argument("--budget-usd", type=float, default=5.0,
                    help="hard stop: abandon remaining runs once the estimated "
                         "spend crosses this. Upper bound, cache ignored.")
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("-v", "--verbose", action="store_true")
    a = ap.parse_args(argv)

    names = ["axium"] if a.calibrate else [n.strip() for n in a.harness.split(",") if n.strip()]
    adapters = [ADAPTERS[n](a.model) for n in names if n in ADAPTERS]
    if a.suite == "matrix":
        scs = matrix.select(a.only.split(","), a.cat.split(","), a.tier.split(","))
    else:
        scs = xharness.select(a.only.split(","))
    if not scs:
        print("No scenarios matched.", file=sys.stderr)
        return 1
    if not os.path.exists(AXIUM_BIN) and "axium" in names:
        print(f"axium binary missing: {AXIUM_BIN} (cargo build --release)", file=sys.stderr)
        return 1

    global PROXY_HOST
    PROXY_HOST = a.proxy.strip().rstrip("/")
    if PROXY_HOST:
        # Fail loudly rather than silently benchmarking against the provider: a
        # suite that quietly bypasses its own recorder produces numbers nobody
        # can check afterwards, which is the whole thing this avoids. Checked
        # for every harness in the run, not just the first.
        import urllib.error
        import urllib.request
        for h in names:
            port = PROXY_PORTS.get(h)
            if not port:
                continue
            try:
                urllib.request.urlopen(f"{PROXY_HOST}:{port}/", timeout=5)
            except urllib.error.HTTPError:
                pass                  # answered, which is all that is needed
            except Exception as exc:  # noqa: BLE001
                print(f"no proxy for {h} at {PROXY_HOST}:{port} ({exc})",
                      file=sys.stderr)
                print("start it, or pass --proxy '' to run unrecorded.",
                      file=sys.stderr)
                return 1
        print("recording: " + ", ".join(f"{h}->{PROXY_PORTS[h]}"
                                        for h in names if h in PROXY_PORTS))

    os.makedirs(BUILDS, exist_ok=True)
    os.makedirs(LOGS, exist_ok=True)
    print(f"\nmodel={a.model}  harnesses={', '.join(names)}  "
          f"scenarios={len(scs)}  DNF at {a.dnf_multiple:g}x\n")

    # Write each result the moment it exists. A suite that logs only at the end
    # donates every completed run to whatever interrupts it, and these are paid
    # API calls. Learned twice already this session.
    stamp = datetime.now(timezone.utc).isoformat(timespec="seconds")
    path = os.path.join(LOGS, f"xharness_{datetime.now():%Y%m%d_%H%M%S}.jsonl")

    def _append(r):
        row = r.as_row()
        row.update({"model": a.model, "stamp": stamp,
                    "dnf_multiple": a.dnf_multiple,
                    "unknown_usage": getattr(r, "unknown_usage", False),
                    "answer": getattr(r, "answer", ""),
                    "error_text": getattr(r, "error_text", "")})
        with open(path, "a", encoding="utf-8") as f:
            f.write(json.dumps(row, ensure_ascii=False) + chr(10))

    # Upper-bound $/token for the run: every token priced as uncached input,
    # bar a 5% output share. Real spend is lower because all three harnesses
    # cache, and v4-pro's cache rate is two orders of magnitude cheaper. An
    # over-estimate is the right error for a spend cap to make.
    _p = pricing.PRICING.get(a.model) or {"in": 0.435, "out": 0.87}
    usd_per_token = (0.95 * _p["in"] + 0.05 * _p["out"]) / 1_000_000.0
    spent_tokens = [0]
    halted = []

    def _est_usd():
        return spent_tokens[0] * usd_per_token

    by_scenario = {}
    for sc in scs:
        if halted:
            break
        ceiling = budget_mod.for_scenario(sc["id"])
        print(f"  {sc['id']}  {sc['name']}")
        print(f"      ceiling {ceiling.tokens:,} tokens" if ceiling else "      no ceiling")
        results = []
        for ad in adapters:
            # Checked BEFORE the run, not after: the cap has to stop the next
            # call, and a check afterwards has already spent the money.
            if a.budget_usd and _est_usd() >= a.budget_usd:
                halted.append(f"{sc['id']}/{ad.name}")
                print(f"      HALTED at ~${_est_usd():.2f} of ${a.budget_usd:.2f}")
                break
            r = run_one(ad, sc, ceiling, a.keep, a.verbose)
            spent_tokens[0] += r.tokens or 0
            results.append(r)
            _append(r)
            usage = "usage not reported" if getattr(r, "unknown_usage", False) \
                else f"{r.tokens:,} tok, {r.tool_calls} tools"
            print(f"      {ad.name:9} {r.checks_passed}/{r.checks_total} checks  "
                  f"{usage}  {r.wall_s:.0f}s"
                  + ("  REGRESSED" if r.regressed else "")
                  + ("  STOPPED AT CEILING" if r.stopped_at_ceiling else "")
                  + ("  ERRORED" if r.errored else ""))
            if r.detail:
                print(f"                missed: {r.detail[:110]}")
        if results:
            by_scenario[sc["id"]] = results
        print(f"      running total ~${_est_usd():.2f} "
              f"({spent_tokens[0]:,} tokens, all harnesses)")

    print("\n" + "=" * 78)
    if a.calibrate:
        print("CALIBRATION - measured Axium usage, for setting absolute budgets")
        print("Budgets must be set from the TASK with headroom, and must never be")
        print("expressed as a multiple of any one harness's usage.\n")
        for sid, rs in by_scenario.items():
            r = rs[0]
            print(f"  {sid:11} solved={r.solved}  {r.tokens:,} tokens  "
                  f"{r.tool_calls} tools -> suggest ceiling "
                  f"{max(40_000, int(r.tokens * 4)):,}")
        return 0

    for line in classify_mod.claim(by_scenario, "axium", a.dnf_multiple):
        print(line)
    print("\n" + classify_mod.disclosure(a.dnf_multiple))

    totals = classify_mod.scoreboard(by_scenario, a.dnf_multiple)
    print(f"\n{'harness':10}{'classified':>11}{'partial':>9}{'failed':>8}"
          f"{'regressed':>11}{'dnf':>6}{'tokens':>12}")
    for h, t in sorted(totals.items()):
        print(f"{h:10}{t['classified']:>11}{t['partial']:>9}{t['failed']:>8}"
              f"{t['regressed']:>11}{t['dnf']:>6}{t['tokens']:>12,}")

    print(f"\nestimated spend ~${_est_usd():.2f} over {spent_tokens[0]:,} tokens "
          f"(upper bound: cache discounts ignored)")
    if halted:
        print(f"HALTED before: {', '.join(halted)} - raise --budget-usd to finish.")
    print(f"\nlogged -> {os.path.relpath(path, HERE)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
