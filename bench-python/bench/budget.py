r"""Budget-capped scoring: solving is not enough, solving *within budget* is.

Correctness saturates. Both DeepSeek models solve every scenario in the tiered
suite, including the one labelled "extremely hard", so a correctness score
cannot rank three harnesses driving the same model. What does not saturate is
what each harness SPENDS to get there, and unlike correctness, that is a
property of the harness rather than of the model.

So every scenario carries a budget, and a run that exceeds it is scored as a
failure even if it eventually produced the right answer. An agent that needs
120k tokens where another needs 10k has not solved the same problem: it has
solved a more expensive one, and on a real task with a real context window it
would have run out.

One honest limit on that: the ceiling is checked BETWEEN turns, because a turn is
a single subprocess call that cannot be interrupted part-way. On a multi-turn
scenario the run really is stopped early. On a single-turn one the turn completes
first and the ceiling is applied to the result, which is a label rather than an
interruption. Both are reported; do not describe the second as "stopped".

## The one rule that keeps this honest

**Budgets are absolute and declared with the scenario, never derived from any
harness's own usage.**

The tempting version, "cap it at 10x what Axium spent", is rigged by
construction: Axium cannot exceed a multiple of itself, so it scores 100% by
definition and every number built on it is worthless the moment anyone checks.
A budget has to come from the task: what a competent solve costs, plus headroom.
`justify()` records the reasoning for each one so the choice is auditable rather
than asserted.

That headroom matters in both directions. Too tight and the benchmark measures
luck; too loose and it measures nothing. The budgets here are set at roughly 3-4x
a measured competent solve, which is generous enough that exceeding one is a
real signal and not variance.
"""


class Budget:
    """What one scenario is allowed to spend.

    Any limit left as `None` is not enforced: a budget should constrain what
    the scenario is actually about, and inventing a wall-clock cap for a task
    that is not about latency just adds noise.
    """

    def __init__(self, tokens=None, tool_calls=None, llm_calls=None,
                 wall_s=None, why=""):
        self.tokens = tokens
        self.tool_calls = tool_calls
        self.llm_calls = llm_calls
        self.wall_s = wall_s
        self.why = why

    def __repr__(self):
        parts = [f"{k}={v}" for k, v in (("tokens", self.tokens),
                                         ("tools", self.tool_calls),
                                         ("calls", self.llm_calls),
                                         ("wall", self.wall_s)) if v]
        return f"Budget({', '.join(parts)})"

    def justify(self):
        return self.why or "(no justification recorded: set one)"

    def check(self, metrics):
        """Which limits this run broke. Empty list means within budget.

        `metrics` is the shared row shape: total tokens are input+output, since
        a harness that reads 100k tokens of context to answer has spent them
        whether or not it wrote much back.
        """
        used = usage_of(metrics)
        broken = []
        if self.tokens and used["tokens"] > self.tokens:
            broken.append(f"tokens {used['tokens']:,} > {self.tokens:,}")
        if self.tool_calls and used["tool_calls"] > self.tool_calls:
            broken.append(f"tool calls {used['tool_calls']} > {self.tool_calls}")
        if self.llm_calls and used["llm_calls"] > self.llm_calls:
            broken.append(f"llm calls {used['llm_calls']} > {self.llm_calls}")
        if self.wall_s and used["wall_s"] > self.wall_s:
            broken.append(f"wall {used['wall_s']:.0f}s > {self.wall_s}s")
        return broken


def usage_of(metrics):
    """Normalise a metrics dict from any harness into the four numbers we cap.

    Cached input still counts. A harness that keeps 90k tokens of context warm
    is buying something real with it, and pretending cache hits are free would
    reward exactly the behaviour the budget exists to expose.
    """
    m = metrics or {}
    return {
        "tokens": int(m.get("input_tokens", 0)) + int(m.get("output_tokens", 0)),
        "tool_calls": int(m.get("tool_calls", 0)),
        "llm_calls": int(m.get("llm_calls", 0)),
        "wall_s": float(m.get("wall_s", 0.0)),
    }


def score(change_rows, metrics, budget):
    """Fold the budget into the score.

    Returns (solved, within_budget, rows). `rows` is the grader's own list with
    a budget row appended, so a report that knows nothing about budgets still
    renders something truthful, and a run that solved the task but blew the
    budget shows exactly which limit it broke rather than a bare zero.
    """
    solved = bool(change_rows) and all(ok for _, ok in change_rows)
    broken = budget.check(metrics) if budget else []
    within = not broken
    rows = list(change_rows)
    if budget:
        label = "within budget" if within else f"OVER BUDGET: {'; '.join(broken)}"
        rows.append((label, within))
    return solved, within, rows


def verdict(solved, within):
    """The four outcomes, named so a report cannot blur them together."""
    if solved and within:
        return "solved"
    if solved and not within:
        return "over_budget"          # right answer, unaffordable
    if not solved and within:
        return "failed"               # wrong answer, cheaply
    return "failed_expensive"         # wrong answer, expensively


def headline(rows):
    """The marketing line, stated only in the form the data supports.

    Deliberately refuses to compare a solve against a non-solve on token count:
    "solved in 10k where the other used 120k" is only true if the other one
    actually solved it. If it did not, the honest sentence is that it failed
    within the budget, and the token number is not the story.
    """
    ours = next((r for r in rows if r["harness"] == "axium"), None)
    if not ours or verdict(ours["solved"], ours["within"]) != "solved":
        return "No claim: Axium did not solve this within budget."
    out = []
    for r in rows:
        if r["harness"] == "axium":
            continue
        v = verdict(r["solved"], r["within"])
        if v == "solved":
            ratio = r["tokens"] / max(1, ours["tokens"])
            out.append(f"{r['harness']} solved it too, using {ratio:.1f}x the tokens"
                       if ratio >= 1.1 else
                       f"{r['harness']} solved it for comparable cost")
        elif v == "over_budget":
            out.append(f"{r['harness']} reached the answer only after exceeding "
                       f"the {r['budget_tokens']:,}-token budget "
                       f"({r['tokens']:,} used)")
        else:
            out.append(f"{r['harness']} did not solve it within "
                       f"{r['budget_tokens']:,} tokens")
    return (f"Axium solved it in {ours['tokens']:,} tokens; " + "; ".join(out)) \
        if out else f"Axium solved it in {ours['tokens']:,} tokens."


# ── the budgets ──────────────────────────────────────────────────────────────
# ALL FOUR ARE PROVISIONAL until a real solve is measured and `justify()` is
# rewritten from that measurement. They were written from an estimate of what
# these tasks should cost, and an estimate asserted as a measurement is exactly
# the unfounded number this module exists to prevent.
#
# Once measured they become absolute ceilings applied identically to every
# harness, NOT a multiple of Axium's usage. Axium can and should fail one if it
# regresses; a bar its subject cannot fail is not a bar.
BUDGETS = {
    # MEASURED 2026-08-25, Axium on deepseek-v4-pro against the 2,834-line seed.
    # The estimates originally written here were wrong by roughly 4x in both
    # directions, which is the argument for measuring rather than asserting.
    #
    # These ceilings exist to stop runaway spend, not to decide the contest.
    # They are ~3x a measured solve, absolute, and identical for every harness.
    # The published rule is the separate DNF multiple in classify.py, which is
    # set against the FIELD's best rather than against any one harness.
    #
    # Axium can fail these. That is the point of setting them from the task.

    # Measured: 350,872 tokens / 26 tools / 97s. Expensive because finding one
    # function among ~200 in 2,800 lines genuinely means reading a lot of them.
    "X-LOCATE": Budget(tokens=1_050_000, tool_calls=120,
                       why="measured 350,872 tok / 26 tools (Axium, v4-pro, 2026-08-25); 3x"),
    # Measured: 119,682 tokens / 19 tools / 34s.
    "X-SPREAD": Budget(tokens=360_000, tool_calls=90,
                       why="measured 119,682 tok / 19 tools (Axium, v4-pro, 2026-08-25); 3x"),
    # Six turns. Not yet measured; sized from X-LOCATE's single-turn cost times
    # the turn count, which is generous for a harness that retains context and
    # tight for one that re-reads the project every turn. Recalibrate after the
    # first full run.
    "X-RECALL": Budget(tokens=900_000, tool_calls=120,
                       why="ESTIMATE from X-LOCATE x turn count; recalibrate after first run"),
    # Two turns, the second a full restore. Not yet measured.
    "X-RESTORE": Budget(tokens=700_000, tool_calls=120,
                        why="ESTIMATE; recalibrate after first run"),
}



# ── the 3 x 3 matrix ─────────────────────────────────────────────────────────
# Anchored on two real measurements against the 2,834-line seed: X-LOCATE at
# 350,872 tokens and X-SPREAD at 119,682, both Axium on v4-pro, 2026-08-25.
#
# The very-hard and crazy-hard tiers run against a 7,630-line seed, 2.7x the
# surface, so their ceilings scale with it. The M tiers scale with turn count,
# not surface: twelve turns instead of six.
#
# These are RUNAWAY STOPS, not the contest. They are deliberately loose - about
# 3x a projected solve - because a ceiling that decides the result is a ceiling
# that was set to decide the result. What decides the contest is the DNF
# multiple in classify.py, measured against the FIELD's fastest solve.
#
# PROVISIONAL until the first real run; recalibrate from measurement, the way
# X-LOCATE and X-SPREAD were.
MATRIX_BUDGETS = {
    "N1": Budget(tokens=1_050_000, tool_calls=120,
                 why="measured 350,872 tok (Axium, v4-pro, 2,834-line seed); 3x"),
    "N2": Budget(tokens=1_600_000, tool_calls=160,
                 why="N1 measurement scaled 2.7x for the 7,630-line seed; ~1.7x that"),
    "N3": Budget(tokens=1_600_000, tool_calls=160,
                 why="as N2; the misleading note costs attempts, not surface"),
    "M1": Budget(tokens=900_000, tool_calls=120,
                 why="six turns on the 2,834-line seed; recalibrate after first run"),
    "M2": Budget(tokens=1_800_000, tool_calls=200,
                 why="twelve turns on the 7,630-line seed"),
    "M3": Budget(tokens=1_800_000, tool_calls=200, why="as M2"),
    "R1": Budget(tokens=700_000, tool_calls=120,
                 why="two turns, the second a full restore; 2,834-line seed"),
    "R2": Budget(tokens=1_000_000, tool_calls=140,
                 why="as R1 on the 7,630-line seed, plus three edits"),
    "R3": Budget(tokens=1_200_000, tool_calls=160,
                 why="as R2 plus a third turn"),
}
BUDGETS.update(MATRIX_BUDGETS)


def for_scenario(scenario_id):
    return BUDGETS.get(scenario_id)
