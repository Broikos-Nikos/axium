"""Two agents, one interface.

Each adapter exposes `open_session(build, project)` -> a Session, and `send(text)`
-> a TurnResult. Everything the graders need is normalised here so no scenario has
to know which agent it is running against.

Fairness rules baked into both adapters:

  * Fresh state per session. Axium gets a new Memory/Db under the build; Orange is
    pointed at a per-session SQLite for conversation + memory. Neither carries
    anything over from a previous scenario or from the user's real assistant.
  * Sandboxed to the build. Axium's workdir is the build; Orange's project search
    root is set to the builds directory, so `{project}` resolves to this copy and
    nothing else on the machine is reachable by name.
  * Changed files are measured by hashing the tree, never by asking the agent.
  * Tool calls are captured by wrapping each agent's dispatcher, so the record is
    what actually ran.
"""
import os
import sys
import time
import shutil
import contextlib

from . import graders as G

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
import axium_path  # noqa: E402  (resolves where the agent package lives)
PY_ROOT = axium_path.AXIUM_ROOT                       # axium/python
ORANGE_ROOT_DEFAULT = r"C:\xampp\htdocs\orange"


class TurnResult:
    def __init__(self, text="", tool_calls=None, llm_calls=0, cost_usd=0.0,
                 input_tokens=0, output_tokens=0, cached_tokens=0, wall_s=0.0,
                 asked=None, error=None, klass="", before=None, after=None):
        self.text = text or ""
        self.tool_calls = tool_calls or []            # [{"name":..., "args": {...}}]
        self.llm_calls = llm_calls
        self.cost_usd = cost_usd
        self.input_tokens = input_tokens
        self.output_tokens = output_tokens
        self.cached_tokens = cached_tokens
        self.wall_s = wall_s
        self.asked = asked or []
        self.error = error
        self.klass = klass                            # Axium's classifier label, "" for Orange
        self.before = before or {}
        self.after = after or {}

    @property
    def changed(self):
        return G.touched(self.before, self.after)

    def as_dict(self):
        return {"text": self.text[:4000], "tools": [t["name"] for t in self.tool_calls],
                "llm_calls": self.llm_calls, "cost_usd": round(self.cost_usd, 6),
                "input_tokens": self.input_tokens, "output_tokens": self.output_tokens,
                "cached_tokens": self.cached_tokens, "wall_s": round(self.wall_s, 1),
                "class": self.klass, "changed": self.changed, "asked": self.asked,
                "error": self.error}


class Session:
    """One agent's run through one scenario."""

    def __init__(self, agent, build, project, agent_home, started_at):
        self.agent = agent
        self.build = build
        self.project = project
        self.agent_home = agent_home
        self.started_at = started_at
        self.pristine = G.tree_hash(build)
        self.turns = []

    @property
    def after(self):
        return self.turns[-1].after if self.turns else self.pristine

    @property
    def all_tools(self):
        return [t for turn in self.turns for t in turn.tool_calls]

    def totals(self):
        return {
            "llm_calls": sum(t.llm_calls for t in self.turns),
            "tool_calls": sum(len(t.tool_calls) for t in self.turns),
            "cost_usd": round(sum(t.cost_usd for t in self.turns), 6),
            "input_tokens": sum(t.input_tokens for t in self.turns),
            "output_tokens": sum(t.output_tokens for t in self.turns),
            "cached_tokens": sum(t.cached_tokens for t in self.turns),
            "wall_s": round(sum(t.wall_s for t in self.turns), 1),
            "errors": [t.error for t in self.turns if t.error],
            "tool_histogram": _histogram(self.all_tools),
        }


def _histogram(tools):
    out = {}
    for t in tools:
        out[t["name"]] = out.get(t["name"], 0) + 1
    return dict(sorted(out.items(), key=lambda kv: -kv[1]))


# ── Axium ────────────────────────────────────────────────────────────────────
class AxiumAdapter:
    name = "axium"

    def __init__(self, model=None, continuation=None, mode=None, config_path=None):
        if PY_ROOT not in sys.path:
            sys.path.insert(0, PY_ROOT)
        from axium import config as config_mod
        self.cfg = config_mod.load(config_path)
        if model:
            self.cfg.models.primary, self.cfg.models.primary_provider = model, ""
        if continuation is not None:
            self.cfg.models.continuation, self.cfg.models.continuation_provider = continuation, ""
        self.mode = mode or self.cfg.settings.mode
        self._agent = None
        self._history = []

    def label(self):
        c = self.cfg.models.continuation or "(none)"
        return f"axium[{self.cfg.models.primary} + {c}, {self.mode}]"

    def open_session(self, build, project):
        from axium.memory import Memory
        from axium.db import Db
        from axium.router import Agent
        home = os.path.join(build, ".axium")
        os.makedirs(home, exist_ok=True)
        self._memory = Memory(os.path.join(home, "memory.md"))
        self._db = Db(os.path.join(home, "history.db"))
        # The fact store must live INSIDE the build. Left at its configured
        # default it resolves next to config.json — one global facts.db shared by
        # every scenario and every run, so a later run could pass because the
        # threshold was stored last time rather than because the agent remembered
        # it this session. A benchmark that can pass without the behaviour is
        # worse than no benchmark.
        self.cfg.settings.facts_file = os.path.join(home, "facts.db")
        self._history = []
        self._events = []
        self._agent = Agent(self.cfg, workdir=build, memory=self._memory, db=self._db,
                            on_event=self._on_event, mode=self.mode)
        return Session(self.name, build, project, home, time.time())

    def _on_event(self, kind, payload):
        if kind == "tool_call":
            self._events.append({"name": payload.get("name", ""),
                                 "args": payload.get("input") or {}})

    def send(self, session, text):
        from axium.metrics import Meter
        self._events = []
        before = G.tree_hash(session.build)
        meter = Meter()
        t0 = time.time()
        try:
            turn = self._agent.run(text, history=self._history, meter=meter)
            self._history = list(turn.history or [])
            out = TurnResult(
                text=turn.text, tool_calls=list(self._events), klass=turn.klass,
                asked=list(turn.asked or []), error=turn.error)
        except Exception as e:                                  # noqa: BLE001
            out = TurnResult(text="", tool_calls=list(self._events),
                             error=f"{type(e).__name__}: {e}")
        t = meter.totals()
        out.llm_calls = t["llm_calls"]
        out.cost_usd = t["cost_usd"]
        out.input_tokens = t["input_tokens"]
        out.output_tokens = t["output_tokens"]
        out.cached_tokens = t["cache_read_tokens"]
        out.wall_s = time.time() - t0
        out.before, out.after = before, G.tree_hash(session.build)
        return out

    def close_session(self, session):
        with contextlib.suppress(Exception):
            self._db.close()


# ── Orange ───────────────────────────────────────────────────────────────────
class OrangeAdapter:
    """Drives Orange's real Conversation loop, isolated from the user's assistant.

    Three things are redirected for the duration of a run, in memory only — no file
    in the Orange repo is modified: the project search root (so `{project}` is the
    build), the conversation/memory SQLite (so nothing leaks in or out), and the
    tool dispatcher (so every call is recorded).
    """
    name = "orange"

    def __init__(self, root=None, chat_model=None, coder_model=None):
        self.root = os.path.abspath(root or ORANGE_ROOT_DEFAULT)
        if not os.path.isdir(os.path.join(self.root, "src", "orange")):
            raise SystemExit(f"orange not found at {self.root} — pass --orange-root")
        for p in (self.root, os.path.join(self.root, "src")):
            if p not in sys.path:
                sys.path.insert(0, p)
        from orange import config as ocfg, llm, settings
        self.ocfg, self.llm, self.settings = ocfg, llm, settings
        if chat_model:
            ocfg.CHAT_MODEL = chat_model
        if coder_model:
            ocfg.CODER_MODEL = coder_model
        if not llm.health_check():
            raise SystemExit("orange has no DeepSeek API key configured "
                             "(data/settings.json: deepseek_api_key)")
        self._restore = []

    def label(self):
        return f"orange[{self.ocfg.CHAT_MODEL} + {self.ocfg.CODER_MODEL}]"

    def open_session(self, build, project):
        from orange import tools as otools, convstore, agent as oagent, projects as oprojects
        home = os.path.join(build, ".orange-session")
        os.makedirs(home, exist_ok=True)

        # 1. project root -> the builds directory, so `project` resolves to this copy
        #    and nothing else on the machine is reachable by name. Patched in memory
        #    rather than via settings.set(), which would rewrite the user's real
        #    data/settings.json and survive a crash mid-run.
        builds_root = os.path.dirname(os.path.abspath(build))
        self._oprojects = oprojects
        self._prev_roots = oprojects._project_roots
        oprojects._project_roots = lambda: [("versus", builds_root, True)]
        oprojects.invalidate_dirs_cache()

        # 2. conversation + memory -> a session-local SQLite (convstore._DB is
        #    resolved live, which is the isolation hook Orange's own tests use).
        self._prev_db = convstore._DB
        convstore._DB = os.path.join(home, "orange.db")

        # 3. record every tool call. ask_user questions feed TurnResult.asked —
        #    the same field Axium's ask_user populates, so V4's "pushed back or
        #    asked first" reads both agents through one channel.
        self._events = []
        self._asked = []
        self._prev_dispatch = otools.dispatch

        def recording(name, args, ctx, progress):
            self._events.append({"name": name, "args": dict(args or {})})
            if name == "ask_user":
                q = (args or {}).get("question") or (args or {}).get("prompt") or ""
                self._asked.append(str(q))
            return self._prev_dispatch(name, args, ctx, progress)

        otools.dispatch = recording
        self._otools = otools
        self._convstore = convstore

        self.llm.reset_cost()
        self._conv = oagent.Conversation()
        with contextlib.suppress(Exception):
            self._conv.switch_project(project)
        return Session(self.name, build, project, home, time.time())

    def send(self, session, text):
        before = G.tree_hash(session.build)
        self._events = []
        self._asked = []
        c0 = dict(self.llm.cost_snapshot())
        t0 = time.time()
        try:
            res = self._conv.send(text, on_status=lambda m: None,
                                  on_token=lambda t: None,
                                  on_tool_result=lambda p: None,
                                  on_done=lambda d, s: None)
            body = (res or {}).get("text", "")
            error = None if (res or {}).get("ok", True) else "orange reported not-ok"
        except Exception as e:                                  # noqa: BLE001
            body, error = "", f"{type(e).__name__}: {e}"
        c1 = self.llm.cost_snapshot()

        def d(k):
            return int(c1.get(k, 0) or 0) - int(c0.get(k, 0) or 0)

        out = TurnResult(
            text=body, tool_calls=list(self._events), asked=list(self._asked), error=error,
            llm_calls=d("calls"),
            cost_usd=float(c1.get("usd", 0.0) or 0.0) - float(c0.get("usd", 0.0) or 0.0),
            input_tokens=d("prompt"), output_tokens=d("completion"),
            cached_tokens=d("cache_hit"), wall_s=time.time() - t0)
        out.before, out.after = before, G.tree_hash(session.build)
        return out

    def close_session(self, session):
        with contextlib.suppress(Exception):
            self._otools.dispatch = self._prev_dispatch
        with contextlib.suppress(Exception):
            self._convstore._DB = self._prev_db
        with contextlib.suppress(Exception):
            self._oprojects._project_roots = self._prev_roots
            self._oprojects.invalidate_dirs_cache()
        with contextlib.suppress(Exception):
            self._conv.stop()


def build_adapter(which, args):
    if which == "axium":
        return AxiumAdapter(model=args.model, continuation=args.continuation,
                            mode=args.mode, config_path=args.config)
    if which == "orange":
        return OrangeAdapter(root=args.orange_root, chat_model=args.orange_chat,
                             coder_model=args.orange_coder)
    raise SystemExit(f"unknown agent {which!r}")


def wipe(path):
    shutil.rmtree(path, ignore_errors=True)
