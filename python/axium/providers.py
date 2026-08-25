"""LLM provider adapters, DeepSeek, OpenAI, Anthropic.

Every adapter returns the SAME normalized shape, modelled on Anthropic's content
blocks (the Rust build does the same, so the agent loop is provider-agnostic):

    {
      "content": [ {"type":"text","text":...} | {"type":"tool_use","id","name","input"} ],
      "stop_reason": "end_turn" | "tool_use" | "max_tokens",
      "usage": {input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                reasoning_tokens},
      "latency_s": float,
      "model": str,
      "error": str | None,
    }

Streaming is always on: text deltas go to the `on_delta` callback as they arrive.
Reasoning/thinking deltas are wrapped in <think>...</think> so a single filter in
the CLI handles DeepSeek's `reasoning_content` and Anthropic's thinking blocks
identically.

A failed call NEVER raises past `call()`, it returns a result with `error` set
and empty content, so a benchmark records the failure instead of dying mid-run.
"""
import json
import time
import logging

import requests

from . import config
from .config import ANTHROPIC, OPENAI, DEEPSEEK, OPENAI_COMPATIBLE, resolve_provider

log = logging.getLogger("axium.providers")

# Error fragments meaning "transient, worth retrying". Read timeouts are excluded
# on purpose: the model is genuinely slow and an identical retry just hangs again.
_TRANSIENT = ("429", "500", "502", "503", "529", "overloaded", "rate limit",
              "temporarily", "try again", "connection reset", "connection aborted")


class ProviderError(RuntimeError):
    pass


def _usage(inp=0, out=0, cache_read=0, cache_write=0, reasoning=0):
    return {"input_tokens": int(inp), "output_tokens": int(out),
            "cache_read_tokens": int(cache_read), "cache_write_tokens": int(cache_write),
            "reasoning_tokens": int(reasoning)}


def _result(content, usage, latency_s, model, stop_reason="end_turn", error=None):
    return {"content": content, "stop_reason": stop_reason, "usage": usage,
            "latency_s": round(latency_s, 3), "model": model, "error": error}


# ── tool schema conversion ───────────────────────────────────────────────────
def tools_anthropic(tools):
    """Neutral tool specs -> Anthropic format, last one tagged for prompt caching."""
    out = [{"name": t["name"], "description": t["description"],
            "input_schema": t["input_schema"]} for t in tools]
    if out:
        out[-1]["cache_control"] = {"type": "ephemeral"}
    return out


def tools_openai(tools):
    """Neutral tool specs -> OpenAI/DeepSeek function-calling format."""
    return [{"type": "function",
             "function": {"name": t["name"], "description": t["description"],
                          "parameters": t["input_schema"]}} for t in tools]


# ── message conversion ───────────────────────────────────────────────────────
def to_openai_messages(system, messages):
    """Anthropic-shaped history -> OpenAI chat messages.

    tool_use blocks become assistant `tool_calls`; tool_result blocks become
    separate role="tool" messages, which is what the OpenAI schema requires.
    """
    out = []
    if system:
        out.append({"role": "system", "content": system})
    for m in messages:
        role, content = m.get("role", "user"), m.get("content")
        if isinstance(content, str):
            out.append({"role": role, "content": content})
            continue
        blocks = content or []
        if role == "assistant":
            text = "".join(b.get("text", "") for b in blocks if b.get("type") == "text")
            calls = [{"id": b["id"], "type": "function",
                      "function": {"name": b["name"],
                                   "arguments": json.dumps(b.get("input") or {})}}
                     for b in blocks if b.get("type") == "tool_use"]
            msg = {"role": "assistant", "content": text or None}
            if calls:
                msg["tool_calls"] = calls
            # An assistant turn with neither text nor calls is not a legal message.
            if msg["content"] or calls:
                out.append(msg)
        else:
            text = ""
            for b in blocks:
                if b.get("type") == "tool_result":
                    out.append({"role": "tool", "tool_call_id": b.get("tool_use_id", ""),
                                "content": str(b.get("content", ""))})
                elif b.get("type") == "text":
                    text += b.get("text", "")
            if text:
                out.append({"role": "user", "content": text})
    return out


# Effort vocabularies differ per provider. Normalise one shared scale
# (off/low/medium/high/max) to what each API actually accepts, so a config can
# say "max" once and mean it everywhere.
#
# DeepSeek V4 takes a nested `thinking` object, not a top-level reasoning_effort.
# Both are accepted by the API, but only the nested form gives a real gradient,
# measured on deepseek-v4-flash, reasoning tokens went low=33 / high=53 / max=58,
# where the flat field barely moved. This is also the shape the orange project
# already sends, so both agents put identical bytes on the wire and their
# benchmark numbers are comparable.
_DEEPSEEK_LEVEL = {"low": "low", "medium": "high", "high": "high", "max": "max"}
_OPENAI_EFFORT = {"low": "low", "medium": "medium", "high": "high", "max": "high"}
# OpenAI only accepts reasoning_effort on its reasoning families; gpt-4.1 rejects it.
_OPENAI_REASONING_PREFIXES = ("o1", "o3", "o4", "gpt-5")
_EFFORT_OFF = ("", "off", "none", "disabled")


def _apply_effort(body, provider, model, effort):
    """Attach the provider's thinking/reasoning control, if it has one."""
    if effort is None:
        return
    if provider == DEEPSEEK:
        if effort in _EFFORT_OFF:
            body["thinking"] = {"type": "disabled"}
        else:
            body["thinking"] = {"type": "enabled",
                                "reasoning_effort": _DEEPSEEK_LEVEL.get(effort, "high")}
    elif provider == OPENAI and model.startswith(_OPENAI_REASONING_PREFIXES):
        if effort in _EFFORT_OFF:
            # Not the same as omitting the field. Measured against gpt-5.4, 5.5
            # and the 5.6 tiers on 21 August 2026: with function tools in the
            # request and no reasoning_effort, the call is refused with
            # "Function tools with reasoning_effort are not supported ... use
            # /v1/responses or set reasoning_effort to 'none'". Sending it
            # explicitly is the only way to use tools on this endpoint.
            body["reasoning_effort"] = "none"
            return
        val = _OPENAI_EFFORT.get(effort)
        if val:
            body["reasoning_effort"] = val


def _anthropic_system(system):
    """Split the system prompt at [MEMORY] so the stable half can be cached
    separately from the half that changes every turn."""
    marker = "\n\n[MEMORY]\n"
    if marker in system:
        soul, dynamic = system.split(marker, 1)
        if soul.strip() and dynamic.strip():
            return [
                {"type": "text", "text": soul, "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": marker.strip() + "\n" + dynamic},
            ]
    return [{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}]


# ── OpenAI-compatible (OpenAI + DeepSeek) ────────────────────────────────────
def call_openai_compatible(provider, key, model, system, messages, tools=None,
                           max_tokens=8192, temperature=0.4, on_delta=None,
                           force_tool=False, timeout=300, effort=""):
    base = config.base_url(provider) or OPENAI_COMPATIBLE[provider]
    # OpenAI's current models reject `max_tokens` outright: "Unsupported
    # parameter: 'max_tokens' is not supported with this model. Use
    # 'max_completion_tokens' instead." DeepSeek takes `max_tokens` and does not
    # know the newer field, so the name is chosen by provider rather than sent
    # hopefully in both spellings.
    cap_field = "max_completion_tokens" if provider == OPENAI else "max_tokens"
    body = {
        "model": model,
        "messages": to_openai_messages(system, messages),
        cap_field: max_tokens,
        "stream": True,
        "stream_options": {"include_usage": True},
    }
    thinking = effort not in ("", "off", "none")
    if temperature is not None and not thinking:
        # Reasoning models reject sampling params; only send temperature when
        # thinking is disabled.
        body["temperature"] = temperature
    _apply_effort(body, provider, model, effort)
    if tools:
        body["tools"] = tools_openai(tools)
        if force_tool:
            body["tool_choice"] = "required"

    t0 = time.time()
    r = requests.post(f"{base}/chat/completions", json=body, timeout=timeout, stream=True,
                      headers={"Authorization": f"Bearer {key}",
                               "Content-Type": "application/json"})
    if r.status_code >= 400:
        raise ProviderError(f"{provider} {r.status_code}: {(r.text or '')[:400]}")

    text_parts = []
    calls = {}            # index -> {id, name, args}
    finish = "stop"
    usage = _usage()

    for raw in r.iter_lines(decode_unicode=True):
        if not raw or not raw.startswith("data: "):
            continue
        data = raw[6:]
        if data == "[DONE]":
            break
        try:
            ev = json.loads(data)
        except json.JSONDecodeError:
            continue

        u = ev.get("usage")
        if u:
            usage = _usage(
                inp=u.get("prompt_tokens", 0),
                out=u.get("completion_tokens", 0),
                # DeepSeek: prompt_cache_hit_tokens. OpenAI: nested cached_tokens.
                cache_read=u.get("prompt_cache_hit_tokens")
                or (u.get("prompt_tokens_details") or {}).get("cached_tokens", 0) or 0,
                reasoning=(u.get("completion_tokens_details") or {}).get("reasoning_tokens", 0) or 0,
            )

        choices = ev.get("choices") or []
        if not choices:
            continue
        ch = choices[0]
        if ch.get("finish_reason"):
            finish = ch["finish_reason"]
        delta = ch.get("delta") or {}

        # DeepSeek streams chain-of-thought separately from the answer.
        reasoning = delta.get("reasoning_content")
        if reasoning and on_delta:
            on_delta(f"<think>{reasoning}</think>")

        chunk = delta.get("content")
        if chunk:
            text_parts.append(chunk)
            if on_delta:
                on_delta(chunk)

        for tc in delta.get("tool_calls") or []:
            slot = calls.setdefault(tc.get("index", 0), {"id": "", "name": "", "args": ""})
            if tc.get("id"):
                slot["id"] = tc["id"]
            fn = tc.get("function") or {}
            if fn.get("name"):
                slot["name"] = fn["name"]
            if fn.get("arguments"):
                slot["args"] += fn["arguments"]

    content = []
    text = "".join(text_parts)
    if text:
        content.append({"type": "text", "text": text})
    for _, c in sorted(calls.items()):
        try:
            args = json.loads(c["args"]) if c["args"].strip() else {}
        except json.JSONDecodeError:
            # Malformed tool JSON is a real failure mode worth measuring, not hiding.
            args = {"__malformed_json__": c["args"][:2000]}
        content.append({"type": "tool_use", "id": c["id"] or f"call_{len(content)}",
                        "name": c["name"], "input": args})

    stop = {"tool_calls": "tool_use", "length": "max_tokens"}.get(finish, "end_turn")
    return _result(content, usage, time.time() - t0, model, stop)



# ── OpenAI Responses API ─────────────────────────────────────────────────────
# Current OpenAI reasoning models refuse function tools on chat completions
# unless reasoning_effort is exactly "none": "Function tools with
# reasoning_effort are not supported ... use /v1/responses or set
# reasoning_effort to 'none'". Measured 21 August 2026 across gpt-5.2, gpt-5.4,
# gpt-5.5 and both gpt-5.6 tiers. So reasoning plus tools lives here.
_RESPONSES_EFFORT = {"low": "low", "medium": "medium", "high": "high",
                     "max": "max", "xhigh": "xhigh"}


def _responses_input(system, messages):
    """Anthropic-shaped history -> Responses input items.

    Tool calls and their results are top-level items here rather than fields on a
    message, which is the one structural difference from chat completions.
    """
    items = []
    if system:
        items.append({"role": "developer", "content": system})
    for m in messages:
        role, content = m.get("role", "user"), m.get("content")
        if isinstance(content, str):
            items.append({"role": role, "content": content})
            continue
        blocks = content or []
        if role == "assistant":
            text = "".join(b.get("text", "") for b in blocks if b.get("type") == "text")
            if text:
                items.append({"role": "assistant", "content": text})
            for b in blocks:
                if b.get("type") == "tool_use":
                    items.append({"type": "function_call", "call_id": b["id"],
                                  "name": b["name"],
                                  "arguments": json.dumps(b.get("input") or {})})
            continue
        plain = []
        for b in blocks:
            if b.get("type") == "tool_result":
                out = b.get("content")
                if isinstance(out, list):
                    out = "".join(x.get("text", "") for x in out
                                  if isinstance(x, dict))
                items.append({"type": "function_call_output",
                              "call_id": b.get("tool_use_id") or b.get("id") or "",
                              "output": str(out or "")})
            elif b.get("type") == "text":
                plain.append(b.get("text", ""))
        if plain:
            items.append({"role": role, "content": "".join(plain)})
    return items


def tools_responses(tools):
    """Neutral tool specs -> Responses function tools, which are flat."""
    return [{"type": "function", "name": t["name"],
             "description": t["description"],
             "parameters": t["input_schema"]} for t in tools]


def call_openai_responses(key, model, system, messages, tools=None, max_tokens=8192,
                          on_delta=None, force_tool=False, timeout=300, effort="high"):
    body = {
        "model": model,
        "input": _responses_input(system, messages),
        "max_output_tokens": max_tokens,
        "store": False,
        "reasoning": {"effort": _RESPONSES_EFFORT.get(effort, "medium")},
    }
    if tools:
        body["tools"] = tools_responses(tools)
        if force_tool:
            body["tool_choice"] = "required"

    t0 = time.time()
    r = requests.post("https://api.openai.com/v1/responses", json=body, timeout=timeout,
                      headers={"Authorization": f"Bearer {key}",
                               "Content-Type": "application/json"})
    if r.status_code >= 400:
        raise ProviderError(f"openai responses {r.status_code}: {r.text[:600]}")
    d = r.json()

    content, text_parts = [], []
    for item in d.get("output") or []:
        kind = item.get("type")
        if kind == "message":
            for c in item.get("content") or []:
                if c.get("type") in ("output_text", "text"):
                    text_parts.append(c.get("text", ""))
        elif kind == "function_call":
            blob = item.get("arguments") or "{}"
            try:
                args = json.loads(blob) if str(blob).strip() else {}
            except json.JSONDecodeError:
                args = {"__malformed_json__": str(blob)[:2000]}
            content.append({"type": "tool_use",
                            "id": item.get("call_id") or item.get("id") or "call_0",
                            "name": item.get("name", ""), "input": args})
        elif kind == "reasoning" and on_delta:
            for c in item.get("summary") or []:
                if c.get("text"):
                    on_delta(f"<think>{c['text']}</think>")

    text = "".join(text_parts)
    if text:
        content.insert(0, {"type": "text", "text": text})
        if on_delta:
            on_delta(text)

    u = d.get("usage") or {}
    usage = _usage(
        inp=u.get("input_tokens", 0),
        out=u.get("output_tokens", 0),
        cache_read=(u.get("input_tokens_details") or {}).get("cached_tokens", 0) or 0,
        reasoning=(u.get("output_tokens_details") or {}).get("reasoning_tokens", 0) or 0,
    )
    stop = "tool_use" if any(c["type"] == "tool_use" for c in content) else "end_turn"
    # The field is present and null on a completed response, so `or {}` rather
    # than a default argument.
    if (d.get("incomplete_details") or {}).get("reason") == "max_output_tokens":
        stop = "max_tokens"
    return _result(content, usage, time.time() - t0, model, stop)


def wants_responses(provider, model, tools, effort):
    """True when this request needs the Responses endpoint to exist at all."""
    if provider != OPENAI or not tools:
        return False
    if effort in _EFFORT_OFF or effort is None:
        return False
    return model.startswith(_OPENAI_REASONING_PREFIXES)


# ── Anthropic ────────────────────────────────────────────────────────────────
def call_anthropic(key, model, system, messages, tools=None, max_tokens=8192,
                   temperature=0.4, on_delta=None, force_tool=False,
                   effort="off", timeout=300):
    thinking = effort in ("low", "medium", "high", "max")
    body = {
        "model": model,
        "max_tokens": max(max_tokens, 16384) if thinking else max_tokens,
        "system": _anthropic_system(system),
        "messages": messages,
        "stream": True,
    }
    if thinking:
        body["thinking"] = {"type": "adaptive"}
        body["output_config"] = {"effort": effort}
    elif temperature is not None:
        body["temperature"] = temperature
    if tools:
        body["tools"] = tools_anthropic(tools)
        if force_tool:
            body["tool_choice"] = {"type": "any"}

    t0 = time.time()
    r = requests.post("https://api.anthropic.com/v1/messages", json=body, timeout=timeout,
                      stream=True,
                      headers={"x-api-key": key, "anthropic-version": "2023-06-01",
                               "content-type": "application/json"})
    if r.status_code >= 400:
        raise ProviderError(f"anthropic {r.status_code}: {(r.text or '')[:400]}")

    content, usage = [], _usage()
    stop_reason = "end_turn"
    cur_text, cur_think, cur_json = [], [], []
    cur_id = cur_name = ""
    kind = None

    for raw in r.iter_lines(decode_unicode=True):
        if not raw or not raw.startswith("data: "):
            continue
        try:
            ev = json.loads(raw[6:])
        except json.JSONDecodeError:
            continue
        etype = ev.get("type")

        if etype == "message_start":
            u = (ev.get("message") or {}).get("usage") or {}
            usage = _usage(inp=u.get("input_tokens", 0),
                           cache_read=u.get("cache_read_input_tokens", 0),
                           cache_write=u.get("cache_creation_input_tokens", 0))
        elif etype == "content_block_start":
            blk = ev.get("content_block") or {}
            kind = blk.get("type")
            if kind == "tool_use":
                cur_id, cur_name, cur_json = blk.get("id", ""), blk.get("name", ""), []
            elif kind == "text":
                cur_text = []
            elif kind == "thinking":
                cur_think = []
        elif etype == "content_block_delta":
            d = ev.get("delta") or {}
            dt = d.get("type")
            if dt == "text_delta":
                cur_text.append(d.get("text", ""))
                if on_delta:
                    on_delta(d.get("text", ""))
            elif dt == "thinking_delta":
                cur_think.append(d.get("thinking", ""))
                if on_delta:
                    on_delta(f"<think>{d.get('thinking', '')}</think>")
            elif dt == "input_json_delta":
                cur_json.append(d.get("partial_json", ""))
        elif etype == "content_block_stop":
            if kind == "text" and cur_text:
                content.append({"type": "text", "text": "".join(cur_text)})
            elif kind == "thinking" and cur_think:
                content.append({"type": "thinking", "thinking": "".join(cur_think)})
            elif kind == "tool_use" and cur_id:
                blob = "".join(cur_json)
                try:
                    args = json.loads(blob) if blob.strip() else {}
                except json.JSONDecodeError:
                    args = {"__malformed_json__": blob[:2000]}
                content.append({"type": "tool_use", "id": cur_id, "name": cur_name,
                                "input": args})
            cur_text, cur_think, cur_json, kind = [], [], [], None
        elif etype == "message_delta":
            stop_reason = (ev.get("delta") or {}).get("stop_reason") or stop_reason
            usage["output_tokens"] = (ev.get("usage") or {}).get("output_tokens", 0)

    return _result(content, usage, time.time() - t0, model, stop_reason)


# ── dispatch ─────────────────────────────────────────────────────────────────
def call(cfg, model, system, messages, provider="", tools=None, max_tokens=8192,
         temperature=0.4, on_delta=None, force_tool=False, effort="off",
         retries=2, timeout=300):
    """Single entry point. Retries transient failures with a linear backoff and
    returns `error` rather than raising, so one bad call never kills a bench run."""
    provider = resolve_provider(model, provider)
    key = cfg.key_for(provider)
    if not key:
        return _result([], _usage(), 0.0, model, "end_turn",
                       error=f"No API key configured for provider '{provider}'")

    attempt, last = 0, None
    while attempt <= retries:
        try:
            if provider == ANTHROPIC:
                res = call_anthropic(key, model, system, messages, tools, max_tokens,
                                     temperature, on_delta, force_tool, effort, timeout)
            elif wants_responses(provider, model, tools, effort):
                res = call_openai_responses(key, model, system, messages, tools,
                                            max_tokens, on_delta, force_tool, timeout,
                                            effort)
            else:
                res = call_openai_compatible(provider, key, model, system, messages, tools,
                                             max_tokens, temperature, on_delta, force_tool,
                                             timeout, effort)
            res["retries"] = attempt
            return res
        except Exception as e:            # noqa: BLE001 - deliberately broad
            last = f"{type(e).__name__}: {e}"
            if attempt >= retries or not any(s in last.lower() for s in _TRANSIENT):
                break
            attempt += 1
            log.warning("transient %s error, retry %d/%d: %s", provider, attempt, retries,
                        last[:120])
            time.sleep(2 * attempt)

    log.error("%s call failed: %s", provider, last)
    return _result([], _usage(), 0.0, model, "end_turn", error=last)


def probe(cfg):
    """Which providers have a usable key this session (no API calls fired)."""
    return cfg.configured_providers()
