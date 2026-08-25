"""USD pricing per 1M tokens.

Every logged call stores raw token counts as well, so costs can always be
recomputed from a fresher table without re-running a benchmark.

Sources (fetched 2026-08-06):
  DeepSeek:  https://api-docs.deepseek.com/quick_start/pricing
  OpenAI:    https://developers.openai.com/api/docs/pricing
  Anthropic: https://platform.claude.com/docs/en/about-claude/pricing

Shape: {"in": $/M uncached input, "out": $/M output, "cache": $/M cached input}.
`cache` is None where the provider publishes no distinct cache-hit rate; the
uncached input rate is then used for cached tokens too.
"""

PRICING = {
    # -- DeepSeek --
    "deepseek-v4-flash": {"in": 0.14, "out": 0.28, "cache": 0.0028},
    "deepseek-v4-pro": {"in": 0.435, "out": 0.87, "cache": 0.003625},
    # -- OpenAI --
    "gpt-4.1": {"in": 2.00, "out": 8.00, "cache": 0.50},
    "gpt-4.1-mini": {"in": 0.40, "out": 1.60, "cache": 0.10},
    "gpt-4.1-nano": {"in": 0.10, "out": 0.40, "cache": 0.025},
    "gpt-5.4-mini": {"in": 0.75, "out": 4.50, "cache": None},
    # -- Anthropic --
    "claude-haiku-4-5": {"in": 1.00, "out": 5.00, "cache": 0.10},
    "claude-haiku-4-5-20251001": {"in": 1.00, "out": 5.00, "cache": 0.10},
    "claude-sonnet-4-6": {"in": 3.00, "out": 15.00, "cache": 0.30},
    "claude-opus-4-6": {"in": 15.00, "out": 75.00, "cache": 1.50},
}


def cost_usd(model, prompt_tokens=0, completion_tokens=0, cached_tokens=0,
             cache_write_tokens=0):
    """USD cost of one call. Returns 0.0 for models with no pricing row.

    Uncached input = prompt_tokens - cached_tokens. Anthropic cache WRITES bill at
    1.25x the input rate; providers that don't report writes pass 0 and the term
    drops out.
    """
    p = PRICING.get(model)
    if not p:
        return 0.0
    cache_rate = p["cache"] if p["cache"] is not None else p["in"]
    uncached = max(0, prompt_tokens - cached_tokens)
    return (
        uncached * p["in"]
        + cached_tokens * cache_rate
        + cache_write_tokens * p["in"] * 1.25
        + completion_tokens * p["out"]
    ) / 1_000_000.0


def is_priced(model):
    return model in PRICING
