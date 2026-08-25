r"""Classification: who finished, who did not, and what may be claimed.

The rule
--------

A harness is CLASSIFIED on a scenario if it solved the task within
`DNF_MULTIPLE` times the token usage of the **fastest harness that solved it**.
Anything slower is `dnf` — did not finish.

This is the motorsport convention. Formula One classifies a finisher only if it
completed 90% of the winner's distance; qualifying uses the 107% rule. Nobody
calls that dishonest, because the bar is defined against *the field* and applies
to everyone equally, including the leader.

Why it is defined against the best performer and not against Axium
------------------------------------------------------------------

The tempting version is "5x whatever Axium used". Do not do that. It is not a
rule, it is a result: Axium cannot exceed a multiple of itself, so it is
classified by definition on every scenario, and the moment anyone reads the
method the whole benchmark is void — along with any claim built on it.

Defined against the best performer, the bar is symmetric. In practice Axium is
usually fastest and therefore usually sets it, which produces the same headline;
the difference is that this one survives being checked. If Axium is ever the
harness that needs 5x the field, it takes the DNF, and that possibility is
exactly what makes the classified results worth quoting.

Live cap, not a relabelling
---------------------------

The cap is enforced DURING the run: a harness is stopped once it crosses the
ceiling. So "did not finish within 50,000 tokens" is a fact about what happened,
not a reinterpretation of a run that did eventually finish. That distinction is
the whole difference between a benchmark and a press release, and it is also
cheaper, since nothing runs to 200k tokens to be discarded.

The ceiling has to be absolute at run time (you cannot know the field's best
before running the field), so the sequence is:

  1. run every harness under a generous ABSOLUTE ceiling from `budget.py`
  2. compute the field's best solve per scenario
  3. classify at `DNF_MULTIPLE` x that best
  4. anything stopped by the absolute ceiling is `dnf` already

Step 1's ceiling exists to stop runaway spend. Step 3 is the published rule.
"""

DNF_MULTIPLE = 5.0

# Reported alongside every result so the rule travels with the number.
RULE = ("Classified if solved within {mult:g}x the tokens of the fastest harness "
        "to solve that scenario; slower runs are DNF. Runs are stopped at the "
        "absolute ceiling, so a DNF did not finish - it was not relabelled.")


class Result:
    """One harness on one scenario."""

    def __init__(self, harness, scenario, solved, tokens, tool_calls=0,
                 wall_s=0.0, cost_usd=0.0, stopped_at_ceiling=False, detail="",
                 checks_passed=None, checks_total=None, regressed=False,
                 errored=False, tokens_measured=True):
        self.harness = harness
        self.scenario = scenario
        self.solved = bool(solved)
        self.tokens = int(tokens or 0)
        self.tool_calls = int(tool_calls or 0)
        self.wall_s = float(wall_s or 0.0)
        self.cost_usd = float(cost_usd or 0.0)
        self.stopped_at_ceiling = bool(stopped_at_ceiling)
        self.detail = detail
        # Finishing is not achieving. A harness can terminate cleanly, announce
        # success, and have done part of the job, none of it, or the job plus
        # damage. These three separate those cases from a clean solve.
        self.checks_passed = checks_passed
        self.checks_total = checks_total
        # Whether `tokens` is a measurement or a placeholder. A harness that
        # does not report usage must not be treated as having used none: that
        # would make it the cheapest by default and set the bar at zero, which
        # is precisely what happened on the first real run.
        self.tokens_measured = bool(tokens_measured)
        self.regressed = bool(regressed)      # broke the project's own suite
        self.errored = bool(errored)          # crashed rather than finished
        # Filled by classify().
        self.status = "unclassified"
        self.reason = ""
        self.ratio = None

    def as_row(self):
        return {
            "harness": self.harness, "scenario": self.scenario,
            "status": self.status, "reason": self.reason,
            "solved": self.solved, "tokens": self.tokens,
            "tool_calls": self.tool_calls, "wall_s": round(self.wall_s, 1),
            "cost_usd": round(self.cost_usd, 6),
            "ratio_to_best": None if self.ratio is None else round(self.ratio, 2),
            "stopped_at_ceiling": self.stopped_at_ceiling,
            "checks": (None if self.checks_total is None
                       else f"{self.checks_passed}/{self.checks_total}"),
            "regressed": self.regressed, "errored": self.errored,
            "detail": self.detail,
        }


def classify(results, multiple=DNF_MULTIPLE):
    """Classify one scenario's results in place. Returns (results, best_tokens).

    `best_tokens` is the fastest SOLVED run. If nobody solved it there is no
    reference, every result is a plain failure, and no ratio is claimed — an
    unsolved scenario says nothing about relative efficiency.
    """
    # A run that broke the project's own test suite has not solved anything,
    # whatever its own checks say: shipping the feature and breaking the build is
    # worse than doing nothing. Enforced here rather than in each grader so no
    # harness can be credited for it by accident.
    for r in results:
        if r.regressed:
            r.solved = False

    # Only runs with a real token measurement can set or be judged against the
    # bar. An unmeasured run is reported as solved-but-uncomparable.
    solvers = [r for r in results
               if r.solved and not r.stopped_at_ceiling and r.tokens_measured]
    if not solvers:
        for r in results:
            if r.stopped_at_ceiling:
                r.status, r.reason = "dnf", "stopped at the absolute ceiling"
            else:
                r.status, r.reason = _shortfall(r)
        return results, None

    best = min(r.tokens for r in solvers)
    limit = int(best * multiple)
    for r in results:
        r.ratio = (r.tokens / best) if (best and r.tokens_measured) else None
        if r.solved and not r.tokens_measured:
            r.status = "unmeasured"
            r.reason = "solved, but the harness reported no token usage"
            continue
        if r.stopped_at_ceiling:
            r.status, r.reason = "dnf", f"did not finish within {r.tokens:,} tokens"
        elif not r.solved:
            r.status, r.reason = _shortfall(r)
        elif r.tokens > limit:
            r.status = "dnf"
            r.reason = (f"did not finish within {limit:,} tokens "
                        f"({multiple:g}x the fastest solve)")
        else:
            r.status, r.reason = "classified", ""
    return results, best


def _shortfall(r):
    """Why a run that FINISHED still did not deliver.

    Kept separate from DNF on purpose. "Ran out of budget" and "ran to
    completion and got it wrong" are different failures, and a comparison that
    blurs them tells you nothing about which harness to trust.
    """
    if r.errored:
        return "errored", "crashed before finishing"
    if r.regressed:
        return "regressed", "broke the project's own test suite"
    if r.checks_total and r.checks_passed:
        return "partial", (f"finished but met only {r.checks_passed}/"
                           f"{r.checks_total} of the requirement")
    return "failed", "finished without solving the task"


def scoreboard(by_scenario, multiple=DNF_MULTIPLE):
    """Classify every scenario and total per harness.

    `by_scenario` is {scenario_id: [Result, ...]}.
    """
    totals = {}
    for scenario, results in by_scenario.items():
        classify(results, multiple)
        for r in results:
            t = totals.setdefault(r.harness, {
                "classified": 0, "partial": 0, "failed": 0, "regressed": 0,
                "errored": 0, "dnf": 0, "unmeasured": 0,
                "tokens": 0, "cost_usd": 0.0, "scenarios": 0})
            t[r.status] = t.get(r.status, 0) + 1
            t["tokens"] += r.tokens
            t["cost_usd"] += r.cost_usd
            t["scenarios"] += 1
    return totals


def claim(by_scenario, ours="axium", multiple=DNF_MULTIPLE):
    """The strongest sentence the data actually supports, per scenario.

    Every branch here exists because the obvious phrasing would have overclaimed
    in that case. In particular a DNF is never described in a way that implies
    the other harness failed at the task - it exceeded a stated budget, which is
    a different and smaller claim.
    """
    lines = []
    for scenario, results in sorted(by_scenario.items()):
        _, best = classify(results, multiple)
        mine = next((r for r in results if r.harness == ours), None)
        others = [r for r in results if r.harness != ours]
        if mine is None or not others:
            continue

        if not mine.solved:
            lines.append(f"{scenario}: no claim - {ours} did not solve it.")
            continue
        if mine.status == "dnf":
            lines.append(f"{scenario}: no claim - {ours} solved it but outside "
                         f"the {multiple:g}x budget.")
            continue

        dnf = [r for r in others if r.status == "dnf"]
        failed = [r for r in others if r.status in ("failed", "partial",
                                                    "regressed", "errored")]
        classified = [r for r in others if r.status == "classified"]
        unmeasured = [r for r in others if r.status == "unmeasured"]

        parts = []
        if classified:
            for r in classified:
                # Ratios are to the FIELD's best, so compare like with like by
                # taking the ratio between the two harnesses directly. Reporting
                # "comparable cost" because the other harness sits at 1.0x the
                # best would hide that it beat us 4x, which is the one direction
                # a benchmark must never round in its author's favour.
                rel = r.tokens / max(1, mine.tokens)
                if rel >= 1.15:
                    parts.append(f"{r.harness} also solved it, using "
                                 f"{rel:.1f}x the tokens")
                elif rel <= 0.87:
                    parts.append(f"{r.harness} solved it MORE cheaply, using "
                                 f"{1 / rel:.1f}x FEWER tokens than {ours}")
                else:
                    parts.append(f"{r.harness} solved it for comparable cost")
        for r in failed:
            # Say WHICH way it fell short. "did not solve it" hides the
            # difference between getting it wrong, getting it half right, and
            # breaking the build on the way.
            if r.status == "partial":
                parts.append(f"{r.harness} finished but met only "
                             f"{r.checks_passed}/{r.checks_total} of the requirement")
            elif r.status == "regressed":
                parts.append(f"{r.harness} finished but broke the project's tests")
            elif r.status == "errored":
                parts.append(f"{r.harness} crashed before finishing")
            else:
                parts.append(f"{r.harness} finished without solving it")
        for r in unmeasured:
            parts.append(f"{r.harness} also solved it, but reported no token "
                         f"usage so cost cannot be compared")
        for r in dnf:
            # Precise: it did not finish inside the budget. Whether it would have
            # finished later is not something this run establishes, and saying it
            # "failed" would imply more than was measured.
            parts.append(f"{r.harness} did not finish inside the budget "
                         f"({r.tokens:,} tokens used"
                         + (f", {r.ratio:.1f}x" if r.ratio else "") + ")")
        lines.append(f"{scenario}: {ours} solved it in {mine.tokens:,} tokens; "
                     + "; ".join(parts) + ".")
    return lines


def disclosure(multiple=DNF_MULTIPLE):
    """The methodology sentence that must accompany any published figure."""
    return RULE.format(mult=multiple)
