r"""Recording proxy: one definition of "a token", and never pay twice.

Three cache-counting bugs were found in one afternoon, each favouring a
different harness, each producing a confident wrong number:

  * Hermes reported no usage on one path -> recorded as 0 -> became "fastest"
  * OpenClaw reports `cacheRead` separately from `input` -> looked 7.7x cheaper
  * Hermes reports `cache_read_tokens` separately too -> 20,662 recorded for a
    run that actually cost 261,631

None of those harnesses is wrong in its own terms. They simply do not agree on
what to count, so no comparison built on self-reported usage can be trusted.

This removes the question. Every harness is pointed at this proxy instead of the
provider; it forwards the request unchanged and records BOTH sides verbatim. The
token counts then come from one place - the provider's own `usage` block, the
thing that is actually billed - under one definition, for every harness.

It also means a benchmark is paid for once. Every request and response is on
disk, so any later question ("how many calls were tool calls?", "what did the
prompts look like?", "recompute with cache excluded") is answered by re-reading
the transcript rather than re-running the suite.

    python proxy.py --port 8899 --upstream https://api.deepseek.com
    python proxy.py --report runs/2026-08-25/            # analyse, costs nothing

Point a harness at it:

    AXIUM_BASE_URL_DEEPSEEK=http://127.0.0.1:8899/v1     (axium, both builds)
    OPENAI_BASE_URL=http://127.0.0.1:8899/v1             (hermes)
    models.providers.deepseek.baseUrl                    (openclaw config)

Streaming is handled: SSE responses are reassembled so the final `usage` chunk
is captured, because a harness that streams would otherwise record nothing.
"""
import argparse
import json
import os
import re
import sys
import threading
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

RUNS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "runs")

# Set by main(); read by the handler.
UPSTREAM = "https://api.deepseek.com"
RUN_DIR = ""
LABEL = ""
_lock = threading.Lock()
_seq = 0


def _next_seq():
    global _seq
    with _lock:
        _seq += 1
        return _seq


def _usage_from(body_text):
    """Pull the provider's usage block out of a response, streamed or not.

    Returns the raw dict, unnormalised. Interpretation belongs in the report,
    not here: the recording must stay a faithful copy of what arrived.
    """
    text = (body_text or "").strip()
    if not text:
        return None
    # Non-streaming: one JSON object.
    if text.startswith("{"):
        try:
            return (json.loads(text) or {}).get("usage")
        except ValueError:
            return None
    # Streaming: usage rides on one of the SSE frames, usually the last with
    # content. Scan backwards - the final frames are where it lives.
    usage = None
    for line in reversed(text.splitlines()):
        line = line.strip()
        if not line.startswith("data:"):
            continue
        payload = line[5:].strip()
        if payload == "[DONE]":
            continue
        try:
            obj = json.loads(payload)
        except ValueError:
            continue
        if isinstance(obj, dict) and obj.get("usage"):
            usage = obj["usage"]
            break
    return usage


_KEY_RE = re.compile(r"(sk-[A-Za-z0-9_\-]{16,})")


def _redact(obj):
    """Strip anything that looks like an API key from a recorded body.

    The transcript is meant to be shareable - that is most of its value - so it
    must not carry the credential that produced it. Conservative on purpose:
    only provider-key-shaped strings are touched, so prose survives intact.
    """
    if isinstance(obj, str):
        return _KEY_RE.sub("sk-REDACTED", obj)
    if isinstance(obj, list):
        return [_redact(v) for v in obj]
    if isinstance(obj, dict):
        return {k: ("REDACTED" if k.lower() in ("authorization", "api_key",
                                                "apikey", "x-api-key")
                    else _redact(v))
                for k, v in obj.items()}
    return obj


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        pass                     # the transcript is the log

    def _record(self, rec):
        path = os.path.join(RUN_DIR, "calls.jsonl")
        with _lock:
            with open(path, "a", encoding="utf-8") as f:
                f.write(json.dumps(rec, ensure_ascii=False) + "\n")

    def do_POST(self):
        self._proxy("POST")

    def do_GET(self):
        self._proxy("GET")

    def _proxy(self, method):
        seq = _next_seq()
        body = b""
        length = int(self.headers.get("Content-Length") or 0)
        if length:
            body = self.rfile.read(length)

        url = UPSTREAM.rstrip("/") + self.path
        headers = {k: v for k, v in self.headers.items()
                   if k.lower() not in ("host", "content-length", "connection")}
        req = urllib.request.Request(url, data=body or None, headers=headers,
                                     method=method)
        started = time.time()
        status, resp_body, err = 0, b"", None
        try:
            with urllib.request.urlopen(req, timeout=600) as r:
                status = r.status
                resp_body = r.read()
                resp_headers = dict(r.headers)
        except urllib.error.HTTPError as e:
            status, resp_body, resp_headers = e.code, e.read(), dict(e.headers)
        except Exception as e:                                  # noqa: BLE001
            status, err, resp_headers = 599, str(e), {}
        elapsed = time.time() - started

        text = resp_body.decode("utf-8", "replace")
        try:
            req_json = json.loads(body.decode("utf-8", "replace")) if body else None
        except ValueError:
            req_json = None

        # The whole point: the provider's own usage block, verbatim.
        usage = _usage_from(text)
        self._record({
            "seq": seq,
            "ts": datetime.now(timezone.utc).isoformat(timespec="seconds"),
            "label": LABEL,
            "path": self.path,
            "status": status,
            "latency_s": round(elapsed, 3),
            "model": (req_json or {}).get("model"),
            "stream": bool((req_json or {}).get("stream")),
            "n_messages": len((req_json or {}).get("messages") or []),
            "n_tools": len((req_json or {}).get("tools") or []),
            "usage": usage,
            "error": err,
            # Full bodies, so nothing has to be re-run to answer a later
            # question. This is the "never pay twice" half of the design.
            # Redacted first: a request body can carry the API key (a config
            # file pasted into a prompt is enough), and these transcripts are
            # the sort of thing that gets attached to a bug report.
            "request": _redact(req_json),
            "response_text": text if len(text) < 400_000 else text[:400_000],
        })

        payload = resp_body if not err else json.dumps({"error": err}).encode()
        self.send_response(status)
        for k, v in (resp_headers or {}).items():
            if k.lower() in ("content-length", "transfer-encoding", "connection"):
                continue
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


# ── reporting (free: reads the transcript, makes no calls) ───────────────────
def report(run_dir):
    path = os.path.join(run_dir, "calls.jsonl")
    if not os.path.exists(path):
        print(f"no transcript at {path}", file=sys.stderr)
        return 1
    rows = [json.loads(l) for l in open(path, encoding="utf-8") if l.strip()]
    by_label = {}
    for r in rows:
        t = by_label.setdefault(r.get("label") or "(unlabelled)", {
            "calls": 0, "prompt": 0, "completion": 0, "cached": 0,
            "billable": 0, "latency": 0.0, "errors": 0})
        t["calls"] += 1
        t["latency"] += r.get("latency_s") or 0.0
        if r.get("status", 0) >= 400 or r.get("error"):
            t["errors"] += 1
        u = r.get("usage") or {}
        # DeepSeek's OpenAI-compatible shape. prompt_tokens ALREADY includes the
        # cache-hit tokens; cache_hit is reported alongside as a subset. So
        # prompt + completion is every token processed, with no double count.
        p = int(u.get("prompt_tokens") or 0)
        c = int(u.get("completion_tokens") or 0)
        hit = int(u.get("prompt_cache_hit_tokens") or 0)
        t["prompt"] += p
        t["completion"] += c
        t["cached"] += hit
        t["billable"] += p + c

    print(f"\n{'label':22}{'calls':>7}{'prompt':>12}{'completion':>12}"
          f"{'cached':>11}{'TOTAL':>12}{'errors':>8}{'api s':>9}")
    for label, t in sorted(by_label.items()):
        print(f"{label:22}{t['calls']:>7}{t['prompt']:>12,}{t['completion']:>12,}"
              f"{t['cached']:>11,}{t['billable']:>12,}{t['errors']:>8}"
              f"{t['latency']:>9.0f}")
    print("\nTOTAL is prompt+completion, straight from the provider's usage block.")
    print("Cached is shown for information; it is already inside prompt.")
    return 0


def main(argv=None):
    global UPSTREAM, RUN_DIR, LABEL
    ap = argparse.ArgumentParser(prog="proxy", description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=8899)
    ap.add_argument("--upstream", default="https://api.deepseek.com")
    ap.add_argument("--run", default="", help="transcript directory name")
    ap.add_argument("--label", default="", help="tag every call (harness name)")
    ap.add_argument("--report", default="", help="analyse a transcript and exit")
    a = ap.parse_args(argv)

    if a.report:
        return report(a.report)

    UPSTREAM = a.upstream
    LABEL = a.label
    RUN_DIR = os.path.join(RUNS, a.run or datetime.now().strftime("%Y%m%d_%H%M%S"))
    os.makedirs(RUN_DIR, exist_ok=True)
    srv = ThreadingHTTPServer(("127.0.0.1", a.port), Handler)
    print(f"recording proxy on http://127.0.0.1:{a.port} -> {UPSTREAM}")
    print(f"transcript: {RUN_DIR}")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
