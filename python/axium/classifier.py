"""Prompt classifier and post-turn reviewers — all on the cheap model.

Classification decides how much machinery a turn gets:
    TRIVIAL  answer directly from the classifier, never wake the primary model
    SIMPLE   pass through, but run every call on the cheap continuation model
    MEDIUM   primary model, skip the quality review
    COMPLEX  primary model, prompt rewritten into an expert task brief

A local pattern fast-path handles the obvious cases with no API call at all,
which is a large part of why cheap routing pays for itself.
"""
import re

from . import providers

CLASSIFY_SYSTEM = """You route prompts for an autonomous coding agent. Reply with EXACTLY one line, no preamble.

TRIVIAL: <answer>   — greetings, trivia, arithmetic, or anything answerable in one sentence with no tools.
SIMPLE              — a single clear step (read one file, run one command, answer about known context).
MEDIUM              — a well-specified code task needing a few tool calls.
COMPLEX: <brief>    — vague, multi-part, or architectural work. <brief> rewrites the request as an
                      explicit task: concrete goal, the files or areas involved, constraints, and what
                      "done" means. Do NOT solve it, and do NOT invent requirements the user did not state.

Choose the CHEAPEST class that can still succeed."""

ENHANCE_MAX_TOKENS = 700

# Local fast-paths. Matching here skips the classifier call entirely.
_TRIVIAL_EXACT = {"hi", "hello", "hey", "yo", "thanks", "thank you", "ok", "okay",
                  "yes", "no", "sup", "cheers", "bye", "goodbye"}
_TRIVIAL_RX = re.compile(r"^\s*(?:what(?:'s| is)\s+)?\d+\s*[-+*/]\s*\d+\s*\??\s*$")
_SIMPLE_RX = re.compile(
    r"^\s*(?:read|show|cat|open|list|ls|print|display)\b.{0,60}$", re.I)
_COMPLEX_HINTS = ("refactor", "architecture", "migrate", "redesign", "rewrite",
                  "optimi", "audit", "investigate", "figure out", "root cause")

# A request that asks for work done cannot be answered as trivia, however
# confidently the classifier says otherwise. Benchmarking caught the router
# occasionally replying to "find the bug and fix it" with prose and no tool calls,
# which is the single most expensive way to be wrong: it looks like a cheap win.
# `remember|save|store|note` matter as much as the edit verbs: without them the
# router answers "Remember X for next time" with a cheerful "will do" and never
# calls update_memory, so the fact is silently lost between sessions.
_ACTION_RX = re.compile(
    r"\b(fix|add|change|create|write|edit|update|remove|delete|rename|refactor|"
    r"implement|extract|move|replace|make|build|run|install|commit|patch|"
    r"migrate|convert|split|merge|generate|remember|memoris|memoriz|save|store|note)\b",
    re.I)


class PromptClass:
    TRIVIAL, SIMPLE, MEDIUM, COMPLEX = "trivial", "simple", "medium", "complex"

    def __init__(self, kind, payload=""):
        self.kind = kind
        self.payload = payload

    def __repr__(self):
        return f"PromptClass({self.kind})"


def quick_classify(text):
    """Certain-only local classification. Returns None when unsure."""
    t = text.strip().lower()
    if not t:
        return None
    if t.rstrip("!.?") in _TRIVIAL_EXACT:
        return PromptClass(PromptClass.TRIVIAL, "Hello. What would you like me to do?")
    if _TRIVIAL_RX.match(t):
        try:
            expr = re.sub(r"^\s*(?:what(?:'s| is)\s+)?", "", t).rstrip("?").strip()
            return PromptClass(PromptClass.TRIVIAL, str(eval(expr, {"__builtins__": {}})))  # noqa: S307
        except Exception:                               # noqa: BLE001
            return None
    if len(t) < 70 and _SIMPLE_RX.match(t):
        return PromptClass(PromptClass.SIMPLE)
    return None


class Classifier:
    def __init__(self, cfg, model="", provider="", meter=None):
        self.cfg = cfg
        self.model = model or cfg.models.classifier
        self.provider = provider or cfg.models.classifier_provider
        self.effort = cfg.settings.cheap_effort
        self.meter = meter

    def _call(self, system, prompt, max_tokens=400, role="classifier"):
        # Temperature 0: a router that changes its mind between identical runs makes
        # every benchmark number noisier and every cost estimate a guess.
        res = providers.call(self.cfg, self.model, system,
                             [{"role": "user", "content": prompt}],
                             provider=self.provider, max_tokens=max_tokens,
                             temperature=0.0, effort=self.effort)
        if self.meter:
            self.meter.record_call(res, role=role)
        if res.get("error"):
            return ""
        return "".join(b.get("text", "") for b in res["content"] if b.get("type") == "text").strip()

    def classify(self, user_message, facts_block=""):
        """Route one prompt.

        `facts_block` is the same `[FACTS]` text the agent gets. It matters
        because the TRIVIAL path answers from HERE and never reaches the agent,
        so without it a question about something the user told us two turns ago
        ("what is our free-shipping threshold?") gets classified trivial and
        answered "I don't have that information" by a model that was never shown
        the fact. Caught by scenario M1: the fact was stored correctly and the
        routing walked straight past it.
        """
        quick = quick_classify(user_message)
        if quick:
            if self.meter:
                self.meter.bump("classifier_fastpath")
            return quick

        system = CLASSIFY_SYSTEM
        if facts_block:
            system += (
                "\n\n[STANDING FACTS]\n"
                "Rules and decisions from earlier in this session. If the user is "
                "asking about something stated here, answer it as TRIVIAL using the "
                "fact VERBATIM, including any number. Never say you lack information "
                "that appears below.\n" + facts_block)
        raw = self._call(system, user_message, ENHANCE_MAX_TOKENS)
        if not raw:
            return PromptClass(PromptClass.MEDIUM)      # fail open to the primary model

        head = raw.split("\n", 1)[0].strip()
        upper = head.upper()
        if upper.startswith("TRIVIAL"):
            # Veto: a request that asks for work must reach the agent even if the
            # classifier is confident it can answer in prose.
            if _ACTION_RX.search(user_message) or len(user_message) > 200:
                self.meter and self.meter.bump("trivial_vetoed")
                return PromptClass(PromptClass.MEDIUM)
            answer = head.split(":", 1)[1].strip() if ":" in head else raw
            return PromptClass(PromptClass.TRIVIAL, answer)
        if upper.startswith("COMPLEX"):
            brief = raw.split(":", 1)[1].strip() if ":" in raw else ""
            return PromptClass(PromptClass.COMPLEX, brief or user_message)
        if upper.startswith("SIMPLE"):
            # Guard against the classifier under-rating obviously heavy work.
            if any(h in user_message.lower() for h in _COMPLEX_HINTS):
                return PromptClass(PromptClass.MEDIUM)
            return PromptClass(PromptClass.SIMPLE)
        return PromptClass(PromptClass.MEDIUM)

    def heartbeat(self, user_request, tool_log, agent_text):
        """Did the agent actually finish? True = complete."""
        out = self._call(
            "You check whether an agent completed the user's request. "
            "Reply with exactly COMPLETE or INCOMPLETE and nothing else.",
            f"USER REQUEST: {user_request}\n\nTOOL LOG:\n{tool_log or '(no tools called)'}"
            f"\n\nAGENT RESPONSE:\n{agent_text[-2000:]}",
            max_tokens=10, role="heartbeat")
        return "INCOMPLETE" not in out.upper()

    def code_review(self, diff, user_request):
        """Review a diff. Returns "" when it finds nothing worth saying."""
        if len(diff) < 200:
            return ""
        out = self._call(
            "You review code diffs for correctness bugs only. Report at most 3 concrete "
            "defects, each one line as 'file: problem'. If the diff is fine, reply exactly OK.",
            f"REQUEST: {user_request}\n\nDIFF:\n{diff[:12000]}",
            max_tokens=500, role="review")
        return "" if out.strip().upper().startswith("OK") else out

    # -- durable-context passes (all cheap, all optional) --------------------
    def extract_facts(self, user_message, agent_text, correction=False):
        """Pull durable statements out of one turn. Returns fact dicts.

        This is the pass that makes a rule stated in turn 1 survive to turn 20:
        nothing here depends on the model having remembered to call a tool.
        """
        from . import facts as facts_mod
        convo = (f"USER: {user_message[:2000]}\n\n"
                 f"AGENT: {(agent_text or '')[-1500:]}")
        out = self._call(facts_mod.EXTRACT_SYSTEM, convo, max_tokens=400, role="facts")
        rows = facts_mod.parse_extraction(out)
        if correction:
            # The user pushing back is the most expensive thing to forget. Floor
            # the importance rather than trusting the extractor to have judged it.
            for r in rows:
                r["importance"] = max(r["importance"], facts_mod.CORRECTION_FLOOR)
        return rows

    def select_skills(self, user_message, available):
        """Pick which skill folders are relevant. Returns a list of known names."""
        from . import skills as skills_mod
        if not available:
            return []
        out = self._call(
            skills_mod.SELECT_SYSTEM,
            f"Available skills: {', '.join(available)}\n\nUser prompt: {user_message}",
            max_tokens=100, role="skills")
        return skills_mod.parse_selection(out, available)

    def plan(self, task, brain_context="", facts=""):
        """A short, grounded plan for a complex task. "" when it is not useful."""
        from . import planner as planner_mod
        out = self._call(planner_mod.PLAN_SYSTEM,
                         planner_mod.build_prompt(task, brain_context, facts),
                         max_tokens=planner_mod.MAX_PLAN_TOKENS, role="planner")
        return out if planner_mod.is_useful(out) else ""

    def summarise_turn(self, user_message, agent_text):
        """One line for the project journal. Not the agent's own prose: that is
        written to be read by a human mid-session and reads as noise a week later."""
        out = self._call(
            "Summarise what was DONE in one line, under 30 words. State the outcome, "
            "not the intent. No preamble.",
            f"REQUEST: {user_message[:600]}\n\nAGENT: {(agent_text or '')[-1200:]}",
            max_tokens=80, role="journal")
        return out.strip().split("\n")[0][:200]

    def distill_skill(self, trace_text):
        """Turn a session trace into a reusable skill. Returns a dict or None."""
        from . import trajectory as traj_mod
        out = self._call(traj_mod.DISTILL_SYSTEM, trace_text, max_tokens=1200,
                         role="distill")
        return traj_mod.parse_skill(out)

    def session_title(self, messages):
        convo = "\n".join(f"{m['role']}: {str(m['content'])[:200]}" for m in messages[:6])
        out = self._call("Title this conversation in 3-6 words. Reply with the title only.",
                         convo, max_tokens=20, role="title")
        return out.strip().strip('"')[:60] or "Session"
