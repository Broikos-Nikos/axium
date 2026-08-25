"""Config loading and provider resolution.

Mirrors the Rust `src/config/loader.rs` schema so one config.json can drive
either implementation. Keys resolve in this order: config.json -> environment
variable (AXIUM_<PROVIDER>_API_KEY or <PROVIDER>_API_KEY).
"""
import json
import os
from dataclasses import dataclass, field, asdict

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)                      # python/
DEFAULT_CONFIG = os.path.join(ROOT, "config.json")

ANTHROPIC = "anthropic"
OPENAI = "openai"
DEEPSEEK = "deepseek"

# Providers speaking the OpenAI chat-completions wire format, and their base URL.
OPENAI_COMPATIBLE = {
    OPENAI: "https://api.openai.com/v1",
    DEEPSEEK: "https://api.deepseek.com/v1",
}

def base_url(provider):
    """Base URL for a provider, honouring an override.

    `AXIUM_BASE_URL_<PROVIDER>` redirects traffic so a benchmark can record every
    request through a proxy and take token counts from the provider's own
    responses. Self-reported usage is not trustworthy across harnesses: three
    separate cache-counting conventions were found in one afternoon.
    """
    import os as _os
    v = _os.environ.get(f"AXIUM_BASE_URL_{provider.upper()}", "").strip()
    return v.rstrip("/") if v else OPENAI_COMPATIBLE.get(provider)


_ENV_VARS = {
    ANTHROPIC: ("AXIUM_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"),
    OPENAI: ("AXIUM_OPENAI_API_KEY", "OPENAI_API_KEY"),
    DEEPSEEK: ("AXIUM_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"),
}


def detect_provider(model):
    """Sniff the provider from a model id."""
    if model.startswith("claude-"):
        return ANTHROPIC
    if model.startswith("deepseek"):
        return DEEPSEEK
    return OPENAI


def resolve_provider(model, explicit=""):
    """Explicit override wins; an unrecognised label falls back to sniffing the
    model id rather than silently mis-routing (e.g. deepseek-* to OpenAI)."""
    if explicit in (ANTHROPIC, OPENAI, DEEPSEEK):
        return explicit
    return detect_provider(model)


@dataclass
class Models:
    primary: str = "deepseek-v4-pro"
    primary_provider: str = ""
    # Cheaper model for tool-continuation turns. Empty = reuse primary.
    continuation: str = "deepseek-v4-flash"
    continuation_provider: str = ""
    classifier: str = "deepseek-v4-flash"
    classifier_provider: str = ""
    compactor: str = "deepseek-v4-flash"
    compactor_provider: str = ""
    review: str = "deepseek-v4-flash"
    review_provider: str = ""
    # Used when primary fails after max_retries. Empty = disabled.
    fallback: str = ""
    fallback_provider: str = ""


@dataclass
class Settings:
    token_limit: int = 80000
    max_tokens: int = 8192
    max_history_messages: int = 200
    terminal_timeout_secs: int = 120
    max_output_chars: int = 15000
    max_tool_iterations: int = 30
    max_input_chars: int = 12000
    max_retries: int = 2
    max_sessions: int = 50
    working_directory: str = "."
    memory_file: str = "memory.md"
    compaction_threshold: int = 60
    # Reasoning effort for the PRIMARY model: off | low | medium | high | max.
    # Normalised per provider (DeepSeek reasoning_effort, Anthropic thinking).
    thinking_effort: str = "max"
    # Reasoning effort for the cheap roles (continuation, classifier, compactor,
    # review). These calls are mechanical, so paying for deep thinking on them is
    # the fastest way to lose the savings that cost routing exists to create.
    cheap_effort: str = "low"
    # "supercharge" (classify + enhance), "simple" (straight to primary), "skills".
    mode: str = "supercharge"
    conversation_logging: bool = False

    # -- durable-context layer ------------------------------------------------
    # Each of these is independently switchable so a benchmark can attribute a
    # score change to ONE of them rather than to "the new version".
    #
    # facts: typed, importance-scored statements extracted after each turn and
    # rendered into the SYSTEM prompt, where compaction cannot reach them.
    facts_enabled: bool = True
    facts_file: str = "facts.db"
    # brain: per-project .axium/ profile + fingerprinted overview + journal,
    # preloaded so the agent stops re-deriving the same project every session.
    brain_enabled: bool = True
    # planner: a cheap-model, brain-grounded plan before a COMPLEX task starts.
    planner_enabled: bool = True
    # checkpoints: snapshot every file a turn touches so undo_turn can revert it.
    checkpoints_enabled: bool = True
    # distillation: turn a substantive session into a reusable skill folder.
    distill_skills: bool = False
    skills_dir: str = ""
    # -- ported from Orange, MEASURED, and defaulted OFF ----------------------
    # Both of these were implemented, tested and benchmarked against ablations.
    # Neither earned a default. Kept because they are correct and free when off,
    # and because the bench seed cannot produce the failures they guard; on a
    # real project (Orange's original use case was live PHP sites) they may.
    # Turn them on deliberately, and measure on YOUR workload before trusting.
    #
    # verify: after a turn changes files, import them and run the project's
    # tests. Evidence: 12 reps across both models, verification ran every time,
    # PASSED every time, caught zero failures. Cost-neutral (a subprocess, not
    # an API call) but a no-op on this workload.
    verify_runtime: bool = False
    # escalate: three consecutive failed edits send the next call to the primary
    # model. Evidence: 21 runs on the cheap model produced ZERO failed edits, so
    # the trigger never fired once. Untested in anger, not merely unhelpful.
    edit_escalation: bool = False


@dataclass
class Config:
    api_keys: dict = field(default_factory=dict)
    models: Models = field(default_factory=Models)
    settings: Settings = field(default_factory=Settings)
    agent_name: str = "Axium"
    soul: str = ""
    path: str = ""

    def key_for(self, provider):
        """API key for a provider: config.json first, then env vars."""
        v = (self.api_keys or {}).get(provider) or ""
        if v and not v.startswith("sk-ant-...") and not v.endswith("..."):
            return v
        for env in _ENV_VARS.get(provider, ()):
            v = os.environ.get(env)
            if v:
                return v
        return ""

    def configured_providers(self):
        return [p for p in (ANTHROPIC, OPENAI, DEEPSEEK) if self.key_for(p)]

    def data_dir(self):
        return os.path.dirname(os.path.abspath(self.path)) if self.path else ROOT

    def resolve_data_path(self, name):
        return name if os.path.isabs(name) else os.path.join(self.data_dir(), name)

    def to_json(self):
        return {
            "api_keys": self.api_keys,
            "models": asdict(self.models),
            "settings": asdict(self.settings),
            "agent": {"name": self.agent_name, "soul": self.soul},
        }


def _filter_known(cls, d):
    """Drop unknown keys so a config written for the Rust build still loads."""
    known = {f for f in cls.__dataclass_fields__}
    return {k: v for k, v in (d or {}).items() if k in known}


def load(path=None):
    path = path or os.environ.get("AXIUM_CONFIG") or DEFAULT_CONFIG
    raw = {}
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            raw = json.load(f)
    agent = raw.get("agent") or {}
    cfg = Config(
        api_keys=raw.get("api_keys") or {},
        models=Models(**_filter_known(Models, raw.get("models"))),
        settings=Settings(**_filter_known(Settings, raw.get("settings"))),
        agent_name=agent.get("name", "Axium"),
        soul=agent.get("soul", ""),
        path=path,
    )
    # soul.md next to the config overrides the inline soul, so it can be hot-edited.
    soul_md = os.path.join(cfg.data_dir(), "soul.md")
    if os.path.exists(soul_md):
        text = open(soul_md, encoding="utf-8").read().strip()
        if text:
            cfg.soul = text
    return cfg


def save(cfg, path=None):
    path = path or cfg.path or DEFAULT_CONFIG
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(cfg.to_json(), f, indent=2)
    os.replace(tmp, path)
    return path
