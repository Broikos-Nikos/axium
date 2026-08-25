r"""Session trajectory + opportunistic skill distillation.

Every turn's (request, tools, outcome) is appended to a per-session JSONL trace.
That is useful on its own, "what did this agent actually do today", and it is
the raw material for the part that compounds: after a substantive multi-step
session, a gated background pass distills the trace into a named skill under
`axium-skills/`, so a workflow performed once can be selected by name the next
time a similar request arrives.

The gates matter more than the distillation. An agent that writes a skill after
every trivial turn fills its own selector prompt with noise, and noise in the
selector is worse than having no skills at all:

  - at least MIN_TURNS turns in the session
  - at least MIN_TOOLS distinct tools used
  - at least one file actually changed
  - one distillation per process, ever

Also here: failure mining. A turn that ends in an error writes a `gotcha` fact,
so the next run on that project starts already warned instead of rediscovering
the same wall.
"""
import json
import logging
import os
import re
import time

MIN_TURNS = 3
MIN_TOOLS = 4
MAX_TRACE_TURNS = 40

DISTILL_SYSTEM = """You turn a session of agent actions into ONE reusable skill: a named, \
general workflow the agent can follow again on a similar task.

Return STRICT JSON and nothing else:
{"name": "kebab-case-name", "description": "one line", "body": "numbered steps"}

The body is instructions the agent follows by calling its own tools - not code.
Generalise away one-off specifics: exact filenames, this project's name, this
session's numbers. Keep the ORDER and the CHECKS that made the session work.

If the session is too trivial or too one-off to generalise, return {"name": ""}."""

_NAME_RX = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")


class Trajectory:
    """One session's trace. Cheap to keep, never raises into the caller."""

    def __init__(self, trace_dir="", session_id=""):
        self.session_id = session_id or time.strftime("%Y%m%d_%H%M%S")
        self.trace_dir = trace_dir or os.path.join(
            os.path.expanduser("~"), ".axium", "trajectories")
        self.turns = []
        self.distilled = False

    @property
    def path(self):
        return os.path.join(self.trace_dir, f"{self.session_id}.jsonl")

    def record(self, request, tools, changed, summary, error=None):
        """Append one turn. Best-effort: a failed write costs a trace line, not a turn."""
        row = {"ts": time.time(), "request": (request or "")[:800],
               "tools": list(tools or []), "changed": sorted(changed or []),
               "summary": (summary or "")[:800], "error": str(error) if error else None}
        self.turns.append(row)
        if len(self.turns) > MAX_TRACE_TURNS:
            self.turns.pop(0)
        try:
            os.makedirs(self.trace_dir, exist_ok=True)
            with open(self.path, "a", encoding="utf-8") as f:
                f.write(json.dumps(row, ensure_ascii=False) + "\n")
        except OSError:
            logging.debug("trajectory write failed", exc_info=True)
        return row

    # -- gates --
    def should_distill(self):
        if self.distilled or len(self.turns) < MIN_TURNS:
            return False
        tools = {t for row in self.turns for t in row["tools"]}
        changed = any(row["changed"] for row in self.turns)
        return len(tools) >= MIN_TOOLS and changed

    def as_prompt(self):
        lines = []
        for i, row in enumerate(self.turns, 1):
            lines.append(f"Turn {i} request: {row['request'][:300]}")
            if row["tools"]:
                lines.append(f"  tools: {', '.join(row['tools'][:20])}")
            if row["changed"]:
                lines.append(f"  changed: {', '.join(row['changed'][:10])}")
            if row["error"]:
                lines.append(f"  error: {row['error'][:200]}")
            lines.append(f"  result: {row['summary'][:300]}")
        return "\n".join(lines)


def parse_skill(raw):
    """Parse the distiller's JSON. Returns None when it declined or produced junk:
    a malformed distillation must be dropped, never written half-formed."""
    text = (raw or "").strip()
    if not text:
        return None
    if text.startswith("```"):
        text = text.strip("`")
        text = text.split("\n", 1)[1] if "\n" in text else text
        text = text.rsplit("```", 1)[0]
    start, end = text.find("{"), text.rfind("}")
    if start < 0 or end <= start:
        return None
    try:
        data = json.loads(text[start:end + 1])
    except (ValueError, TypeError):
        return None
    name = str(data.get("name") or "").strip().lower()
    body = str(data.get("body") or "").strip()
    if not name or not body or not _NAME_RX.match(name):
        return None
    return {"name": name, "description": str(data.get("description") or "").strip()[:200],
            "body": body[:8000]}


def write_skill(skill, skills_root):
    """Write a distilled skill. Refuses to overwrite an existing folder: a skill a
    human edited must not be silently replaced by a fresh distillation."""
    try:
        # Re-validate rather than trusting the caller. `parse_skill` already
        # enforces this, but the name becomes a DIRECTORY here, and a caller that
        # built the dict by hand would otherwise write outside the skills root.
        # Rejected outright rather than sanitised: a name like "../../etc" was not
        # a skill, and quietly repairing it hides that.
        if not _NAME_RX.match(str(skill.get("name") or "")):
            return ""
        folder = os.path.join(skills_root, skill["name"])
        if os.path.isdir(folder):
            return ""
        os.makedirs(folder, exist_ok=True)
        path = os.path.join(folder, "SKILL.md")
        with open(path, "w", encoding="utf-8") as f:
            f.write(f"# {skill['name'].replace('-', ' ').title()}\n\n"
                    f"{skill['description']}\n\n{skill['body']}\n\n"
                    f"<!-- axium:distilled {time.strftime('%Y-%m-%d')} -->\n")
        return path
    except OSError:
        logging.debug("skill write failed", exc_info=True)
        return ""


# -- failure mining ----------------------------------------------------------
def mine_failure(request, error, tool_log=""):
    """Turn a failed turn into a `gotcha` fact, or None when there is nothing to
    learn. Deliberately narrow: only failures with a concrete cause are worth the
    prompt space they will occupy on every future turn."""
    err = (str(error) or "").strip()
    if not err or len(err) < 12:
        return None
    head = err.splitlines()[0][:200]
    if not any(ch.isalpha() for ch in head):
        return None
    ctx = f" while: {request.strip()[:120]}" if request else ""
    tools = f" (tools: {tool_log[:80]})" if tool_log else ""
    return {"type": "gotcha",
            "key": "fail." + re.sub(r"[^a-z0-9]+", ".", head.lower())[:60].strip("."),
            "importance": 0.7,
            "value": f"Previously failed{ctx}: {head}{tools}"}
