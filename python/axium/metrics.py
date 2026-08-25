"""Per-turn instrumentation.

The Meter is threaded through every LLM call and every tool call, so a benchmark
gets cost, latency and behaviour without the agent loop knowing it is being
measured. It is also what makes the CLI able to print a real per-turn cost.
"""
import time
from collections import Counter

from .pricing import cost_usd, is_priced


class Meter:
    def __init__(self):
        self.calls = []              # one row per LLM call
        self.tool_calls = []         # one row per tool invocation
        self.events = Counter()      # named counters: retries, compactions, ...
        self.t0 = time.time()

    # -- recording --
    def record_call(self, result, role="primary"):
        """Record one LLM call result (the dict returned by providers.call)."""
        u = result.get("usage") or {}
        model = result.get("model", "")
        row = {
            "role": role,
            "model": model,
            "input_tokens": u.get("input_tokens", 0),
            "output_tokens": u.get("output_tokens", 0),
            "cache_read_tokens": u.get("cache_read_tokens", 0),
            "cache_write_tokens": u.get("cache_write_tokens", 0),
            "reasoning_tokens": u.get("reasoning_tokens", 0),
            "latency_s": result.get("latency_s", 0.0),
            "stop_reason": result.get("stop_reason", ""),
            "error": result.get("error"),
            "priced": is_priced(model),
            "cost_usd": cost_usd(model, u.get("input_tokens", 0), u.get("output_tokens", 0),
                                 u.get("cache_read_tokens", 0), u.get("cache_write_tokens", 0)),
        }
        self.calls.append(row)
        if result.get("error"):
            self.events["api_errors"] += 1
        self.events["retries"] += result.get("retries", 0) or 0
        return row

    def record_tool(self, name, ok=True, duration_s=0.0, output_len=0):
        self.tool_calls.append({"name": name, "ok": ok,
                                "duration_s": round(duration_s, 3), "output_len": output_len})
        if not ok:
            self.events["tool_errors"] += 1

    def bump(self, event, n=1):
        self.events[event] += n

    # -- aggregates --
    @property
    def cost(self):
        return sum(c["cost_usd"] for c in self.calls)

    @property
    def wall_s(self):
        return time.time() - self.t0

    @property
    def api_latency_s(self):
        return sum(c["latency_s"] for c in self.calls)

    def totals(self):
        def s(k):
            return sum(c[k] for c in self.calls)
        cached = s("cache_read_tokens")
        inp = s("input_tokens")
        return {
            "llm_calls": len(self.calls),
            "input_tokens": inp,
            "output_tokens": s("output_tokens"),
            "cache_read_tokens": cached,
            "cache_write_tokens": s("cache_write_tokens"),
            "reasoning_tokens": s("reasoning_tokens"),
            "cache_hit_rate": round(cached / inp, 3) if inp else 0.0,
            "cost_usd": round(self.cost, 6),
            "unpriced_models": sorted({c["model"] for c in self.calls if not c["priced"]}),
            "api_latency_s": round(self.api_latency_s, 2),
            "wall_s": round(self.wall_s, 2),
            "tool_calls": len(self.tool_calls),
            "tool_errors": self.events.get("tool_errors", 0),
            "api_errors": self.events.get("api_errors", 0),
            "retries": self.events.get("retries", 0),
            "tool_histogram": dict(Counter(t["name"] for t in self.tool_calls)),
            "by_role": self._by_role(),
            "events": dict(self.events),
        }

    def _by_role(self):
        out = {}
        for c in self.calls:
            r = out.setdefault(c["role"], {"calls": 0, "input_tokens": 0,
                                           "output_tokens": 0, "cost_usd": 0.0})
            r["calls"] += 1
            r["input_tokens"] += c["input_tokens"]
            r["output_tokens"] += c["output_tokens"]
            r["cost_usd"] = round(r["cost_usd"] + c["cost_usd"], 6)
        return out

    def summary_line(self):
        t = self.totals()
        return (f"{t['llm_calls']} calls · {t['tool_calls']} tools · "
                f"{t['input_tokens']}in/{t['output_tokens']}out "
                f"({t['cache_hit_rate']:.0%} cached) · "
                f"${t['cost_usd']:.4f} · {t['wall_s']:.1f}s")
