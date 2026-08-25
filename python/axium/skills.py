r"""Skills, named, reusable workflows stored as Markdown.

A skill is a folder under a skills root holding one or more `.md` files:

    axium-skills/
      rust-development/guidelines.md
      deploy-and-verify/SKILL.md

The folder name is the skill's name; the Markdown is instruction the agent
follows by calling its own tools. Nothing here adds a tool or a code path, a
skill composes what already exists, which is why a non-developer can add one by
dropping in a file.

The Rust build already had this; the Python one only had the mode name. They now
share a format and a selection prompt, so a skill written for one works in the
other and `--mode skills` means the same thing in both benchmarks.

Selection is a cheap-model call that picks from the folder names alone. Only the
selected skills' bodies are read, so a hundred installed skills cost one short
prompt rather than a hundred file reads.
"""
import os

MAX_SKILL_CHARS = 6000          # per skill, after which the body is truncated
MAX_TOTAL_CHARS = 12000         # all selected skills combined

SELECT_SYSTEM = """You are a skill selector for an AI agent. Given a user prompt and a list of \
available skill folders, determine which skills are relevant to help the agent fulfill the request.

Rules:
- Only select skills that are DIRECTLY relevant to the user's request
- When in doubt, select NONE
- Do NOT select skills just because they're tangentially related

Respond with EXACTLY one of these formats:
NONE
SKILLS: skill1, skill2"""


def default_roots(workdir=""):
    """Where skills are looked for, lowest priority first.

    The repo root ships the built-ins; `~/.axium/skills` is the user's own; the
    working directory's `.axium/skills` is the project's. A later root overrides
    an earlier one of the same name, which is how a project pins its own version
    of a shared workflow.
    """
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))   # python/
    repo = os.path.dirname(here)                                          # repo root
    roots = [os.path.join(repo, "axium-skills"),
             os.path.join(os.path.expanduser("~"), ".axium", "skills")]
    if workdir:
        roots.append(os.path.join(os.path.abspath(workdir), ".axium", "skills"))
    return roots


def discover(roots=None, workdir=""):
    """{name: folder} for every skill folder found, later roots winning."""
    out = {}
    for root in (roots if roots is not None else default_roots(workdir)):
        try:
            entries = sorted(os.listdir(root))
        except OSError:
            continue
        for name in entries:
            full = os.path.join(root, name)
            if os.path.isdir(full) and not name.startswith("."):
                out[name] = full
    return out


def names(roots=None, workdir=""):
    return sorted(discover(roots, workdir))


def load(name, roots=None, workdir=""):
    """Concatenated Markdown for one skill, or "" when it has none."""
    folder = discover(roots, workdir).get(name)
    if not folder:
        return ""
    parts = []
    try:
        files = sorted(f for f in os.listdir(folder) if f.lower().endswith(".md"))
    except OSError:
        return ""
    for f in files:
        try:
            with open(os.path.join(folder, f), encoding="utf-8", errors="replace") as fh:
                parts.append(fh.read())
        except OSError:
            continue
    body = "\n\n".join(p.strip() for p in parts if p.strip())
    return body[:MAX_SKILL_CHARS]


def parse_selection(raw, available):
    """Parse the selector's reply. Unknown names are dropped rather than trusted:
    a hallucinated skill name would otherwise read a folder that does not exist."""
    line = (raw or "").strip().splitlines()[0] if (raw or "").strip() else ""
    if not line or line.strip().upper().startswith("NONE"):
        return []
    if ":" in line:
        line = line.split(":", 1)[1]
    picked = [p.strip() for p in line.replace(";", ",").split(",") if p.strip()]
    known = set(available)
    return [p for p in picked if p in known]


def render(selected, roots=None, workdir=""):
    """The `[LOADED SKILLS]` block for the chosen skills."""
    out, used = [], 0
    for name in selected:
        body = load(name, roots, workdir)
        if not body:
            continue
        block = f"### Skill: {name}\n{body}"
        if used + len(block) > MAX_TOTAL_CHARS:
            break
        out.append(block)
        used += len(block) + 2
    return "\n\n".join(out)
