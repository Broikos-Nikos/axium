"""Interactive REPL.

Streams tokens as they arrive, prints tool calls to stderr, and shows the real
cost of every turn. `<think>` blocks are hidden by default (`/think` toggles).
"""
import os
import sys

from . import config as config_mod, providers
from .db import Db
from .memory import Memory
from .router import Agent, describe_routing

HELP = """commands:
  /new       start a fresh session          /think     toggle reasoning display
  /cost      totals for this session        /models    show model routing
  /mode X    simple | supercharge | skills  /quit      exit"""


class ThinkFilter:
    """Hide <think>...</think> across streaming chunk boundaries."""

    OPEN, CLOSE = "<think>", "</think>"

    def __init__(self, show=False):
        self.show = show
        self.buf = ""
        self.inside = False

    def feed(self, chunk):
        self.buf += chunk
        out = []
        while True:
            if self.inside:
                i = self.buf.find(self.CLOSE)
                if i < 0:
                    keep = len(self.CLOSE) - 1
                    if self.show:
                        out.append(self.buf[:-keep] if len(self.buf) > keep else "")
                    self.buf = self.buf[-keep:] if len(self.buf) > keep else self.buf
                    break
                if self.show:
                    out.append(self.buf[:i])
                self.buf = self.buf[i + len(self.CLOSE):]
                self.inside = False
            else:
                i = self.buf.find(self.OPEN)
                if i < 0:
                    keep = len(self.OPEN) - 1
                    if len(self.buf) > keep:
                        out.append(self.buf[:-keep])
                        self.buf = self.buf[-keep:]
                    break
                out.append(self.buf[:i])
                self.buf = self.buf[i + len(self.OPEN):]
                self.inside = True
        return "".join(out)

    def flush(self):
        out = "" if self.inside else self.buf
        self.buf = ""
        return out


def _dim(s):
    return f"\x1b[2m{s}\x1b[0m"


def run(cfg=None, workdir=None):
    cfg = cfg or config_mod.load()
    workdir = os.path.abspath(workdir or cfg.settings.working_directory or ".")

    available = providers.probe(cfg)
    if not available:
        print("No API keys configured. Set DEEPSEEK_API_KEY (or add one to config.json).",
              file=sys.stderr)
        return 1

    memory = Memory(cfg.resolve_data_path(cfg.settings.memory_file))
    db = Db(cfg.resolve_data_path("chat_history.db"))
    session = db.ensure_session("cli", "CLI")

    print(f"{cfg.agent_name}, {cfg.models.primary} (providers: {', '.join(available)})")
    print(f"working dir: {workdir}")
    print(_dim("/help for commands"))

    show_think = False
    session_cost, session_calls = 0.0, 0
    mode = cfg.settings.mode

    while True:
        try:
            line = input("\n> ").strip()
        except (EOFError, KeyboardInterrupt):
            print()
            break
        if not line:
            continue

        if line in ("/quit", "/exit"):
            break
        if line == "/help":
            print(HELP)
            continue
        if line == "/new":
            db.clear_session(session)
            print("Session cleared.")
            continue
        if line == "/think":
            show_think = not show_think
            print(f"Reasoning display {'on' if show_think else 'off'}.")
            continue
        if line == "/models":
            print(describe_routing(cfg))
            continue
        if line == "/cost":
            print(f"{session_calls} calls, ${session_cost:.4f} this session")
            continue
        if line.startswith("/mode"):
            parts = line.split()
            if len(parts) > 1 and parts[1] in ("simple", "supercharge", "skills"):
                mode = parts[1]
                print(f"Mode: {mode}")
            else:
                print(f"Mode: {mode} (use /mode simple|supercharge|skills)")
            continue

        history = db.load_messages(session, cfg.settings.max_history_messages)
        flt = ThinkFilter(show_think)
        streamed = {"any": False}

        def on_event(kind, payload):
            if kind == "delta":
                visible = flt.feed(payload)
                if visible:
                    streamed["any"] = True
                    sys.stdout.write(visible)
                    sys.stdout.flush()
            elif kind == "tool_call":
                print(_dim(f"\n[tool: {payload['name']}]"), file=sys.stderr)
            elif kind == "tool_result" and not payload["ok"]:
                print(_dim(f"[{payload['name']} failed] {payload['output'][:160]}"),
                      file=sys.stderr)
            elif kind == "classified":
                print(_dim(f"[{payload['class']}]"), file=sys.stderr)
            elif kind == "compacted":
                print(_dim(f"[compacted -> {payload['messages']} messages]"), file=sys.stderr)
            elif kind == "retry":
                print(_dim("[incomplete, continuing]"), file=sys.stderr)
            elif kind == "review":
                print(_dim(f"[review] {payload}"), file=sys.stderr)

        agent = Agent(cfg, workdir=workdir, memory=memory, db=db, on_event=on_event,
                      ask_user=lambda q: input(f"\n{q}\n>> "), mode=mode)
        turn = agent.run(line, history=history)

        tail = flt.flush()
        if tail:
            sys.stdout.write(tail)
        if turn.text and not streamed["any"]:
            print(turn.text)
        print()

        if turn.error:
            print(f"\x1b[31mError: {turn.error}\x1b[0m", file=sys.stderr)

        t = turn.meter.totals()
        session_cost += t["cost_usd"]
        session_calls += t["llm_calls"]
        print(_dim(turn.meter.summary_line()), file=sys.stderr)

        db.save_message(session, "user", line)
        if turn.text:
            db.save_message(session, "assistant", turn.text)

    print(f"\n{session_calls} calls, ${session_cost:.4f} this session.")
    db.close()
    return 0
