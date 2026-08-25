"""History compaction, summarise the old half of a conversation on the cheap
model so a long session keeps its facts without carrying every token.

Triggered by the router at `compaction_threshold` percent of `token_limit`,
before the limit is actually hit: compacting early keeps the prompt prefix small
and cache-friendly.
"""
from . import providers

COMPACTOR_SYSTEM = """You summarise conversations from an autonomous coding agent that uses tools \
(run_command, read_file, write_file, patch_file, search_files, git_command). Your summary REPLACES \
the old messages: the agent will see only your summary plus the most recent turns.

Preserve:
- File paths created, edited or read, and what was done to each
- Commands run and their outcomes, especially errors
- Decisions made and preferences the user stated
- Task status: what is done, what is still pending

Omit:
- Pleasantries, acknowledgements, explanations of work already completed
- Plans that were already executed (keep the result, drop the plan)
- Code bodies : name the file and its purpose instead

One bullet per distinct fact. No narrative. Be terse."""

MAX_SOURCE_CHARS = 100_000


def estimate_tokens(messages, system_overhead=6000):
    """~3.5 chars/token plus per-message framing, matching the Rust estimator."""
    total = system_overhead
    for m in messages:
        c = m.get("content")
        text = c if isinstance(c, str) else str(c)
        total += (len(text) * 2 + 6) // 7 + 4
    return total


class Compactor:
    def __init__(self, cfg, model="", provider="", meter=None):
        self.cfg = cfg
        self.model = model or cfg.models.compactor
        self.provider = provider or cfg.models.compactor_provider
        self.effort = cfg.settings.cheap_effort
        self.meter = meter

    def compact(self, old_messages):
        buf = []
        size = 0
        for m in old_messages:
            c = m.get("content")
            text = c if isinstance(c, str) else str(c)
            line = f"{m.get('role', 'user')}: {text}\n"
            if size + len(line) > MAX_SOURCE_CHARS:
                buf.append("\n[... truncated for compaction ...]\n")
                break
            buf.append(line)
            size += len(line)

        res = providers.call(
            self.cfg, self.model, COMPACTOR_SYSTEM,
            [{"role": "user", "content": "Conversation to summarise:\n" + "".join(buf)}],
            provider=self.provider, max_tokens=2048, temperature=0.2,
            effort=self.effort)
        if self.meter:
            self.meter.record_call(res, role="compactor")
        if res.get("error"):
            return ""
        return "".join(b.get("text", "") for b in res["content"]
                       if b.get("type") == "text").strip()
