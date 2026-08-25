r"""Grounded planner — a cheap-model plan before the expensive model starts.

The classifier already rewrites a vague COMPLEX request into an explicit brief.
That fixes the *wording* of the task and knows nothing about the codebase, so the
primary model still opens with several orientation calls before it does any work.

The planner closes that loop: it is handed the Project Brain (profile, recent
journal, overview) and the enhanced brief, and returns a short ordered plan naming
the actual files. It runs on the continuation model at low effort, so it costs a
fraction of a cent, and it saves the primary model the orientation round-trips it
would otherwise pay for at full price.

It is advisory. The plan is injected as context, never as a contract: a plan that
turns out wrong must cost the agent one paragraph of prompt, not a locked-in
sequence of edits it cannot leave.
"""

PLAN_SYSTEM = """You plan a coding task for an autonomous agent that will execute it with tools.

You are given what is already known about the project and the task. Produce a
SHORT ordered plan:

1. <step> - name the concrete files or symbols involved
2. ...

Rules:
- At most 5 steps. Fewer is better.
- Name real files from the project context. If the context does not name a file,
  say which file to FIND first, do not invent a path.
- The last step is always the check that proves the task is done.
- Do NOT write code. Do NOT explain. Output only the numbered steps.
- If the task is a question rather than a change, plan how to ANSWER it and say
  explicitly that no files are to be modified."""

MAX_PLAN_TOKENS = 400
MAX_CONTEXT_CHARS = 4000


MAX_FACTS_CHARS = 1500


def build_prompt(task, brain_context="", facts=""):
    """Brain first, then facts, then the task.

    The order is deliberate: the model reads the ground truth about the project
    before it reads what it is being asked to do, which is what stops it from
    inventing file paths. Sections are tested with `.strip()`, not truthiness —
    a whitespace-only block would otherwise announce "here are the standing
    facts" and then show none.
    """
    parts = []
    if (brain_context or "").strip():
        parts.append("[WHAT IS ALREADY KNOWN ABOUT THIS PROJECT]\n"
                     + brain_context[:MAX_CONTEXT_CHARS])
    if (facts or "").strip():
        parts.append("[STANDING FACTS AND RULES]\n" + facts[:MAX_FACTS_CHARS])
    parts.append("[TASK]\n" + (task or "").strip())
    return "\n\n".join(parts)


MIN_USEFUL_CHARS = 30
MIN_NUMBERED_STEPS = 2

# Openings that mean the model declined. A refusal is not a plan, and shipping
# one costs tokens on every call of the loop while steering nothing.
REFUSAL_PREFIXES = ("i cannot", "i can't", "i'm sorry", "sorry,", "unable to")


def _count_numbered_steps(plan):
    """Lines that open with a step number: "1.", "2)", "3 - ", "4".

    The two-digit ceiling is the point. A pasted file listing opens its lines
    with 3-4 digit line numbers, and counting those made a code dump look like a
    five-step plan.
    """
    n = 0
    for line in plan.splitlines():
        digits = ""
        for ch in line.lstrip():
            if not ch.isdigit():
                break
            digits += ch
        if digits and len(digits) <= 2:
            n += 1
    return n


def is_useful(plan):
    """A plan that is empty, apologetic, or a single vague line is worse than none:
    it costs tokens on every call of the loop and steers nothing."""
    p = (plan or "").strip()
    if len(p) < MIN_USEFUL_CHARS:
        return False
    if p.lower().startswith(REFUSAL_PREFIXES):
        return False
    # Two steps, not one: a single step is a restatement of the task.
    return _count_numbered_steps(p) >= MIN_NUMBERED_STEPS


def render(plan):
    return f"[PLAN]\nA cheap pre-pass produced this plan. Follow it where it is right; " \
           f"deviate where it is wrong, and say so.\n\n{plan.strip()}"
