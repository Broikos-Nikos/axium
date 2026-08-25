"""The agent loop.

    classify -> (maybe answer trivially) -> tool loop -> heartbeat -> code review

Cost routing is the point of the design: the primary model handles reasoning-heavy
calls, and the cheap continuation model handles the mechanical "read the tool
result, call the next tool" steps that make up most of a long task. Which calls go
where is decided by `_model_for_iteration`, and every call is metered so a
benchmark can prove the routing is actually paying for itself.
"""
import json
import os
import subprocess
import time

from . import brain as brain_mod
from . import facts as facts_mod
from . import planner as planner_mod
from . import providers, tools as tools_mod, toolspec
from . import verify as verify_mod
from . import skills as skills_mod
from . import trajectory as traj_mod
from .checkpoints import Checkpoints
from .classifier import Classifier, PromptClass
from .compactor import Compactor, estimate_tokens
from .metrics import Meter

MAX_NUDGES = 2
# Consecutive failed edits before the next call is escalated to the primary
# model. Three is the point where "unlucky" stops being the explanation: the
# cheap model is holding a stale picture of the file and more cheap calls just
# buy more of the same wrong answer.
EDIT_STRIKES_BEFORE_ESCALATION = 3
_EDIT_TOOLS = {"patch_file", "write_file", "append_file"}
MAX_VERIFY_ROUNDS = 2
SUBAGENT_MAX_DEPTH = 1

BASE_INSTRUCTIONS = """
[INSTRUCTIONS]
Before acting, outline your plan in 2-3 lines. If a tool fails, read the error and try a
different approach rather than repeating the call. Use independent tools in the same turn.

## Tool Use Protocol
You are an EXECUTION agent, not a narration agent.
- When a task needs a file written or a command run, emit the tool call IMMEDIATELY.
- NEVER say "I'll write the file now" and then end your turn. Decide and call in the same response.
- Do NOT print code in markdown blocks — use write_file or patch_file to put it on disk.
- Your turn is not complete until every necessary tool has been called and the result delivered.

## Persistent Memory
Your memory survives across sessions and appears under [MEMORY] below. Use update_memory for
durable facts only: user details, project conventions, decisions. Never for one-off task state.
"""

# Instructions for subsystems that can be switched off. Describing a block the
# agent will never be shown is an invitation to hallucinate one, so each of these
# is appended only when its subsystem is actually live.
FACTS_INSTRUCTIONS = """
## Standing facts
[FACTS] holds rules, thresholds and decisions that are still binding. They were captured in
earlier turns and are shown to you in full every turn, so a number there is the number: never
say you no longer have it, and never re-derive it from the conversation. Add to it with
remember_fact when the user states a rule you will need later; search it with recall.
"""

CHECKPOINT_INSTRUCTIONS = """
## Undoing work
Every file you change is snapshotted before the write. When asked to put something back, revert,
or undo what you just did, call undo_turn: it restores the exact bytes and removes files the
turn created. Rewriting the files from memory is slower and is not exact.
"""


class Turn:
    """Result of one agent turn."""

    def __init__(self, text="", meter=None, changed=None, error=None, klass="",
                 history=None, compacted=False, asked=None, facts_learned=None,
                 plan="", skills=None):
        self.text = text
        self.meter = meter or Meter()
        self.changed = sorted(changed or [])
        self.error = error
        self.klass = klass
        self.history = history or []
        self.compacted = compacted
        self.asked = asked or []
        # What the turn learned and what steered it — the benchmark grades memory
        # and routing directly rather than inferring them from the prose.
        self.facts_learned = facts_learned or []
        self.plan = plan
        self.skills = skills or []

    def __repr__(self):
        return f"Turn(class={self.klass!r}, changed={len(self.changed)}, err={self.error!r})"


class Agent:
    """One configured agent. Reusable across turns; `run` is a single turn."""

    def __init__(self, cfg, workdir=None, memory=None, db=None, on_event=None,
                 ask_user=None, depth=0, mode=None, facts=None, trajectory=None):
        self.cfg = cfg
        self.workdir = os.path.abspath(workdir or cfg.settings.working_directory or ".")
        self.memory = memory
        self.db = db
        self.on_event = on_event or (lambda kind, payload: None)
        self.ask_user = ask_user
        self.depth = depth
        self.mode = mode or cfg.settings.mode

        s = cfg.settings
        # The fact store is shared across turns of a session, which is the whole
        # point: a fact captured in turn 1 must be in front of the model in turn 20.
        self.facts = facts
        if self.facts is None and s.facts_enabled and depth == 0:
            self.facts = facts_mod.FactStore(cfg.resolve_data_path(s.facts_file))
        # A sub-agent shares no checkpoint with its parent: undoing the parent's
        # turn from inside a delegated sub-task is a bug, not a feature.
        self.checkpoints = Checkpoints(self.workdir) \
            if (s.checkpoints_enabled and depth == 0) else None
        self.trajectory = trajectory
        if self.trajectory is None and depth == 0:
            self.trajectory = traj_mod.Trajectory()
        self.scope = os.path.basename(self.workdir.rstrip("\\/")) or ""

    # -- prompt assembly --
    def system_prompt(self, project_context="", skill_context="", plan=""):
        """Compose the system prompt.

        Order is a caching decision, not a stylistic one. `providers._anthropic_system`
        splits on "\\n\\n[MEMORY]\\n": everything ABOVE that marker is sent with a
        cache breakpoint and must be stable across the turns of a session, and
        everything BELOW it is re-sent each turn. So the Brain (per project, stable)
        goes above; memory, facts, the selected skills and the plan (per turn) go
        below. Putting the facts above the marker would invalidate the prefix cache
        on every turn that learned something, which is most of them.
        """
        s = self.cfg.settings
        parts = [self.cfg.soul or f"You are {self.cfg.agent_name}, an autonomous coding assistant.",
                 f"\n[WORKING DIRECTORY]\n{self.workdir}"]
        if project_context:
            parts.append(f"\n[PROJECT]\n{project_context}")
        if s.brain_enabled:
            preloaded = brain_mod.preload(self.workdir)
            if preloaded:
                parts.append(f"\n[PROJECT BRAIN]\nWhat is already known about this "
                             f"project from earlier sessions. Trust it, but verify "
                             f"anything you are about to change.\n\n{preloaded}")
        parts.append(BASE_INSTRUCTIONS)
        if self.facts is not None:
            parts.append(FACTS_INSTRUCTIONS)
        if self.checkpoints is not None:
            parts.append(CHECKPOINT_INSTRUCTIONS)

        mem = self.memory.content.strip() if self.memory else ""
        parts.append(f"\n\n[MEMORY]\n{mem or '(empty)'}")
        if self.facts is not None:
            rendered = self.facts.render(scope=self.scope)
            if rendered:
                parts.append(f"\n[FACTS]\n{rendered}")
        if skill_context:
            parts.append(f"\n[LOADED SKILLS]\n{skill_context}")
        if plan:
            parts.append("\n" + planner_mod.render(plan))
        return "\n".join(parts)

    # -- cost routing --
    def _model_for_iteration(self, iteration, all_cheap, has_continuation, escalate=False):
        """Which model handles this call.

        Call 1 carries the reasoning, so it gets the primary model unless the
        classifier already judged the whole task cheap. Continuation calls are
        mechanical and go to the cheap model.
        """
        m = self.cfg.models
        # Escalation wins over routing: the point of dropping to the cheap model
        # is that the step is mechanical, and a step that has failed three times
        # has demonstrated it is not.
        if escalate:
            return m.primary, m.primary_provider, "primary"
        if has_continuation and (all_cheap or iteration > 0):
            return m.continuation, m.continuation_provider, "continuation"
        return m.primary, m.primary_provider, "primary"

    # -- the turn --
    def run(self, user_message, history=None, project_context="", meter=None):
        cfg, s = self.cfg, self.cfg.settings
        meter = meter or Meter()
        history = list(history or [])
        classifier = Classifier(cfg, meter=meter)
        compactor = Compactor(cfg, meter=meter)

        klass = PromptClass.MEDIUM
        all_cheap = self.mode == "simple"
        effective_message = user_message
        skill_context, selected_skills, plan = "", [], ""

        if self.mode == "skills":
            # Skills mode never classifies: it loads the relevant workflows and
            # runs the task on the primary model with them in the prompt.
            available = skills_mod.names(workdir=self.workdir)
            if available:
                selected_skills = classifier.select_skills(user_message, available)
                if selected_skills:
                    skill_context = skills_mod.render(selected_skills, workdir=self.workdir)
                    meter.bump("skills_loaded")
                    self.on_event("classified", {"class": "skills",
                                                 "detail": ", ".join(selected_skills)})
        elif self.mode != "simple" and len(user_message.strip()) > 2:
            # The classifier sees the facts too: its TRIVIAL path answers
            # without ever reaching the agent, so a fact it cannot see is a fact
            # the user is told we do not have.
            pc = classifier.classify(
                user_message,
                self.facts.render(scope=self.scope) if self.facts else "")
            klass = pc.kind
            self.on_event("classified", {"class": pc.kind})
            if pc.kind == PromptClass.TRIVIAL:
                meter.bump("trivial_shortcut")
                self.on_event("text", pc.payload)
                return Turn(text=pc.payload, meter=meter, klass=klass,
                            history=history + [{"role": "user", "content": user_message},
                                               {"role": "assistant", "content": pc.payload}])
            if pc.kind == PromptClass.SIMPLE:
                all_cheap = True
            elif pc.kind == PromptClass.COMPLEX and pc.payload:
                effective_message = f"[Original request: {user_message}]\n\n{pc.payload}"
                meter.bump("prompt_enhanced")
        elif self.mode == "simple":
            klass = PromptClass.SIMPLE

        # A grounded plan for complex work. The classifier fixed the WORDING of the
        # task and knows nothing about this codebase; the planner sees the Brain and
        # names real files, so the primary model skips the orientation round-trips it
        # would otherwise pay for at full price.
        if s.planner_enabled and klass == PromptClass.COMPLEX and self.depth == 0:
            plan = classifier.plan(
                effective_message,
                brain_mod.preload(self.workdir) if s.brain_enabled else "",
                self.facts.render(scope=self.scope) if self.facts else "")
            if plan:
                meter.bump("planned")
                self.on_event("plan", plan)

        history.append({"role": "user", "content": effective_message})
        if self.checkpoints is not None:
            self.checkpoints.begin(user_message[:120])
        ctx = tools_mod.new_context(
            self.workdir, timeout=s.terminal_timeout_secs,
            max_output_chars=s.max_output_chars, memory=self.memory, db=self.db,
            ask_user=self.ask_user, spawn_subagent=self._make_spawner(meter, project_context),
            facts=self.facts, checkpoints=self.checkpoints, scope=self.scope)

        text, err, compacted = self._tool_loop(
            history, ctx, meter, compactor, all_cheap, project_context, user_message,
            skill_context, plan)

        # Heartbeat: a turn that stopped early gets one nudge to actually finish.
        if not err and klass in (PromptClass.MEDIUM, PromptClass.COMPLEX) and ctx["changed"]:
            tool_log = ", ".join(t["name"] for t in meter.tool_calls[-20:])
            if not classifier.heartbeat(user_message, tool_log, text):
                meter.bump("heartbeat_incomplete")
                self.on_event("retry", None)
                history.append({"role": "user", "content":
                                "Your response looks incomplete. Finish the task now: make the "
                                "remaining changes and state the concrete result."})
                more, err2, _ = self._tool_loop(history, ctx, meter, compactor, True,
                                                project_context, user_message,
                                                skill_context, plan)
                text = (text + "\n" + more).strip() if more else text
                err = err or err2

        # Runtime verification: the turn changed files, so run them. A failure
        # here is handed back as another round rather than reported as success,
        # because "it parses" is not the claim the user cares about.
        if s.verify_runtime and ctx["changed"] and not err and self.depth == 0:
            result = verify_mod.verify(self.workdir, sorted(ctx["changed"]))
            meter.bump(f"verify_{'skipped' if result.skipped else ('ok' if result.ok else 'failed')}")
            if not result.ok and not result.skipped:
                self.on_event("verify_failed", result.detail)
                history.append({"role": "user", "content": result.as_feedback()})
                more, err2, _ = self._tool_loop(history, ctx, meter, compactor, False,
                                                project_context, user_message,
                                                skill_context, plan)
                text = (text + "\n" + more).strip() if more else text
                err = err or err2
                # Verify once more: a fix that does not fix it must not be
                # reported as though it did.
                again = verify_mod.verify(self.workdir, sorted(ctx["changed"]))
                if not again.ok and not again.skipped:
                    meter.bump("verify_still_failing")
                    self.on_event("verify_failed", again.detail)

        # Silent code review on the diff, for complex work only.
        if klass == PromptClass.COMPLEX and ctx["changed"]:
            diff = self._git_diff()
            if diff:
                notes = classifier.code_review(diff, user_message)
                if notes:
                    meter.bump("review_findings")
                    self.on_event("review", notes)

        if self.checkpoints is not None:
            self.checkpoints.commit()
        learned = self._after_turn(classifier, meter, user_message, text, ctx, err)

        history.append({"role": "assistant", "content": text})
        return Turn(text=text, meter=meter, changed=ctx["changed"], error=err, klass=klass,
                    history=history, compacted=compacted, asked=ctx["asked"],
                    facts_learned=learned, plan=plan, skills=selected_skills)

    # -- post-turn learning --
    def _after_turn(self, classifier, meter, user_message, text, ctx, err):
        """Capture what the turn should still know next week.

        Everything here is best-effort and metered separately. A failure in the
        learning layer must cost a fact, never the turn that already succeeded.
        """
        s = self.cfg.settings
        learned = []

        if self.facts is not None and s.facts_enabled and self.depth == 0:
            try:
                correction = facts_mod.looks_like_correction(user_message)
                if correction:
                    meter.bump("corrections_detected")
                for row in classifier.extract_facts(user_message, text, correction):
                    self.facts.remember(row["value"], type=row["type"], key=row["key"],
                                        importance=row["importance"], scope=self.scope,
                                        source="auto")
                    learned.append(row)
                if learned:
                    meter.bump("facts_learned", len(learned))
                    self.on_event("facts", learned)
            except Exception:                           # noqa: BLE001
                meter.bump("facts_errors")

            # Failure mining: a run that ended badly warns the next one instead of
            # letting it walk into the same wall.
            if err:
                gotcha = traj_mod.mine_failure(
                    user_message, err, ", ".join(t["name"] for t in meter.tool_calls[-8:]))
                if gotcha:
                    self.facts.remember(gotcha["value"], type=gotcha["type"],
                                        key=gotcha["key"], importance=gotcha["importance"],
                                        scope=self.scope, source="failure")
                    meter.bump("failures_mined")

        # The journal is what makes "continue where we left off" survive a restart.
        # Only turns that actually changed something earn an entry.
        if s.brain_enabled and ctx["changed"] and self.depth == 0:
            try:
                summary = classifier.summarise_turn(user_message, text)
                if summary:
                    brain_mod.journal(self.workdir, summary, ctx["changed"], user_message)
            except Exception:                           # noqa: BLE001
                meter.bump("journal_errors")

        if self.trajectory is not None and self.depth == 0:
            self.trajectory.record(user_message,
                                   [t["name"] for t in meter.tool_calls],
                                   ctx["changed"], text, err)
            if s.distill_skills and self.trajectory.should_distill():
                try:
                    skill = classifier.distill_skill(self.trajectory.as_prompt())
                    if skill:
                        root = s.skills_dir or skills_mod.default_roots(self.workdir)[0]
                        if traj_mod.write_skill(skill, root):
                            self.trajectory.distilled = True
                            meter.bump("skills_distilled")
                            self.on_event("skill", skill["name"])
                except Exception:                       # noqa: BLE001
                    meter.bump("distill_errors")

        return learned

    # -- inner loop --
    def _tool_loop(self, history, ctx, meter, compactor, all_cheap, project_context,
                   original_request, skill_context="", plan=""):
        cfg, s = self.cfg, self.cfg.settings
        system = self.system_prompt(project_context, skill_context, plan)
        active_tools = toolspec.tools_for_mode(self.mode)
        has_cont = bool(cfg.models.continuation and
                        cfg.models.continuation != cfg.models.primary)

        final_text, err, nudges, compacted = "", None, 0, False
        iteration = 0
        edit_strikes = 0

        while iteration < s.max_tool_iterations:
            # Compact before the call if the prompt is getting long.
            budget = s.token_limit * max(1, min(100, s.compaction_threshold)) / 100
            if estimate_tokens(history) > budget and len(history) > 6:
                keep = history[-4:]
                summary = compactor.compact(history[:-4])
                if summary:
                    compacted = True
                    meter.bump("compactions")
                    history[:] = [{"role": "user",
                                   "content": f"[Previous conversation summary]\n{summary}"}] + keep
                    self.on_event("compacted", {"messages": len(history)})

            escalate = (s.edit_escalation
                        and edit_strikes >= EDIT_STRIKES_BEFORE_ESCALATION)
            if escalate:
                meter.bump("edit_escalations")
                self.on_event("escalated", {"strikes": edit_strikes})
                edit_strikes = 0
            model, provider, role = self._model_for_iteration(
                iteration, all_cheap, has_cont, escalate)
            effort = s.thinking_effort if role == "primary" else s.cheap_effort
            res = providers.call(
                cfg, model, system, history, provider=provider, tools=active_tools,
                max_tokens=s.max_tokens, on_delta=lambda c: self.on_event("delta", c),
                effort=effort, retries=s.max_retries)
            meter.record_call(res, role=role)

            if res.get("error"):
                # Try the fallback model once before giving up on the turn.
                if cfg.models.fallback and cfg.models.fallback != model:
                    meter.bump("fallback_used")
                    res = providers.call(cfg, cfg.models.fallback, system, history,
                                         provider=cfg.models.fallback_provider,
                                         tools=active_tools, max_tokens=s.max_tokens,
                                         effort=s.thinking_effort, retries=1)
                    meter.record_call(res, role="fallback")
                if res.get("error"):
                    return final_text, res["error"], compacted

            blocks = res["content"]
            text = "".join(b.get("text", "") for b in blocks if b.get("type") == "text")
            calls = [b for b in blocks if b.get("type") == "tool_use"]
            if text.strip():
                final_text = text.strip()

            if not calls:
                # No tools and no text is a dead turn — nudge, then accept.
                if not text.strip() and nudges < MAX_NUDGES:
                    nudges += 1
                    meter.bump("empty_response_nudges")
                    history.append({"role": "user", "content":
                                    "You returned nothing. Either call a tool or give the final answer."})
                    iteration += 1
                    continue
                break

            history.append({"role": "assistant", "content": blocks})
            results = []
            for call in calls:
                name, args = call.get("name", ""), call.get("input") or {}
                self.on_event("tool_call", {"name": name, "input": args})
                t0 = time.time()
                out = tools_mod.execute(name, args, ctx)
                ok = not out.startswith("Error:")
                # A failed EDIT is the signal, not a failed command: `run_command`
                # returning non-zero is often the agent correctly discovering a
                # test fails. A patch that cannot find its target means the model
                # is working from a stale picture of the file.
                if name in _EDIT_TOOLS:
                    edit_strikes = edit_strikes + 1 if not ok else 0
                meter.record_tool(name, ok, time.time() - t0, len(out))
                self.on_event("tool_result", {"name": name, "ok": ok, "output": out})
                results.append({"type": "tool_result", "tool_use_id": call.get("id", ""),
                                "content": out})
            history.append({"role": "user", "content": results})
            iteration += 1

        if iteration >= s.max_tool_iterations:
            meter.bump("iteration_cap_hit")
            if not final_text:
                final_text = (f"Stopped after {s.max_tool_iterations} tool iterations "
                              f"without a final answer.")
        _ = original_request
        return final_text, err, compacted

    # -- sub-agents --
    def _make_spawner(self, parent_meter, project_context):
        if self.depth >= SUBAGENT_MAX_DEPTH:
            return None

        def spawn(task, which="fast"):
            sub_cfg = self.cfg
            # The sub-agent SEES the parent's facts (it has no conversation history,
            # so a standing rule is the only context it gets) but owns no checkpoint
            # and no trajectory: those belong to the turn that spawned it.
            sub = Agent(sub_cfg, workdir=self.workdir, memory=None, db=self.db,
                        depth=self.depth + 1, mode="simple", facts=self.facts,
                        trajectory=self.trajectory)
            if which == "fast" and sub_cfg.models.continuation:
                # A fast sub-agent runs entirely on the cheap model.
                sub.cfg = _with_primary(sub_cfg, sub_cfg.models.continuation,
                                        sub_cfg.models.continuation_provider)
            parent_meter.bump("subagents")
            result = sub.run(task, project_context=project_context, meter=parent_meter)
            return result.text or "(sub-agent returned nothing)"

        return spawn

    def _git_diff(self):
        try:
            r = subprocess.run(tools_mod._shell() + ["git diff --unified=2"],
                               cwd=self.workdir, capture_output=True, text=True,
                               errors="replace", timeout=60)
            return (r.stdout or "")[:20000]
        except Exception:                               # noqa: BLE001
            return ""


def _with_primary(cfg, model, provider):
    """Shallow clone of a config with the primary model swapped."""
    import copy
    c = copy.copy(cfg)
    c.models = copy.copy(cfg.models)
    c.models.primary, c.models.primary_provider = model, provider
    c.models.continuation = ""          # a sub-agent does not route further
    return c


def run_once(cfg, message, workdir=None, memory=None, db=None, on_event=None,
             project_context="", mode=None, ask_user=None, facts=None):
    """Convenience wrapper: one stateless turn. Used by the benchmark harness."""
    agent = Agent(cfg, workdir=workdir, memory=memory, db=db, on_event=on_event,
                  ask_user=ask_user, mode=mode, facts=facts)
    return agent.run(message, project_context=project_context)


def describe_routing(cfg):
    """Human-readable model plan — printed at CLI start and in bench headers."""
    m = cfg.models
    hi, lo = cfg.settings.thinking_effort, cfg.settings.cheap_effort
    s = cfg.settings
    rows = [("primary", f"{m.primary}  (effort={hi})"),
            ("continuation", f"{m.continuation or '(= primary)'}  (effort={lo})"),
            ("classifier", f"{m.classifier}  (effort={lo})"),
            ("compactor", f"{m.compactor}  (effort={lo})"),
            ("review", f"{m.review}  (effort={lo})"),
            ("fallback", m.fallback or "(disabled)"),
            # Printed in every bench header: a log that does not say which of these
            # were on cannot be compared against one that had them off.
            ("durable_context", ", ".join(
                name for name, on in (("facts", s.facts_enabled),
                                      ("brain", s.brain_enabled),
                                      ("planner", s.planner_enabled),
                                      ("checkpoints", s.checkpoints_enabled),
                                      ("distill", s.distill_skills)) if on) or "(all off)")]
    return json.dumps(dict(rows), indent=2)
