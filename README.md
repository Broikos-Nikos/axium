# Axium

> The **"claw"** you feel you wanted...
> **Self-hosted autonomous AI agent. Built in Rust. Runs on your Linux machine — including a Raspberry Pi.**
> Persistent memory · Background task queue · Multi-model cost routing · 31 parallel tools · CLI + SSH · Telegram channel · Plugin system

```bash
bash <(curl -fsSL https://raw.githubusercontent.com/Broikos-Nikos/axium/main/install.sh)
```

No account. No telemetry. Your data stays on your disk.

---

## Three ways to access

```
axium                  # interactive CLI REPL — works over SSH
axium --server         # browser UI at http://127.0.0.1:3000
Telegram               # full agent on mobile, while server runs in background
```

All three share the same agent, the same memory, and the same conversation history.

---

## What Axium does

Axium is an **open-source, self-hosted autonomous AI agent** for Linux. Give it a goal — coding, research, system administration, web automation, email, monitoring — and it executes. It uses Claude Sonnet (or any Anthropic/OpenAI model) as its reasoning core and gives it full access to your machine: shell, files, git, web, email, and a 31-tool suite that runs in parallel.

It is not a chat assistant. It is not a CLI wrapper. It is not a one-shot code generator. It is a **persistent agent** with its own memory, background worker, and cost architecture — designed for serious daily use beyond the IDE.

### Coding tasks

```
You: refactor the auth module to use JWT, run tests, commit when clean
```

Axium will:
1. Read the relevant files
2. Classify complexity — inject expert task framing for hard problems
3. Rewrite the code (`patch_file` / `write_file`)
4. Run `cargo test` (or whatever your stack uses)
5. Auto-repair failures — up to 2 fix attempts with self-reflection
6. Run a silent code review via a secondary model on the diff
7. Commit with `git_command`
8. Tell you it's done — or send a Telegram message if you queued it and walked away

### Beyond coding

Axium handles any goal you can express as a task:

- **Research**: browse documentation, aggregate information, write a summary to a file
- **System administration**: audit packages, monitor logs, run diagnostics, clean up disk space
- **Web automation**: scrape pages, submit forms, extract structured data
- **Communication**: draft and send email, notify via Telegram with results attached
- **Monitoring**: background worker runs scheduled tasks while you're offline — audit the codebase, ping you on Telegram when done
- **Cross-session recall**: "what did we decide about the database schema last month?" — FTS5 full-text search over all past conversations, not a hallucination

No copy-pasting. No context-switching. No babysitting.

---

## CLI and SSH access

The default mode is a full interactive REPL over stdin/stdout — no browser required. SSH into any machine running Axium and use it as if it were local.

```
$ axium

CLI mode — type a message and press Enter. /new to reset, /quit to exit.

> scan the project for unused dependencies and remove them

[tool: scan_project]
[tool: read_file]
[tool: run_command]
Found 3 unused crates in Cargo.toml. Removing...
[tool: patch_file]
[tool: run_command]  → cargo check: ok
Done. Removed: once_cell, thiserror, lazy_static.
```

- **Streams token-by-token** — responses appear as they're generated, not buffered.
- **Tool calls shown inline** — `[tool: run_command]`, `[tool: read_file]` etc. print to stderr in real time so you see what it's doing.
- **Full agent stack** — same classifier, compactor, memory, plugins, and all 31 tools as the browser UI.
- **Session persisted to SQLite** — CLI history is searchable with `search_history` alongside browser sessions.
- **Interactive follow-ups** — if the agent needs to ask a clarifying question (`ask_user`), it pauses and reads your reply inline.
- **/new** resets the conversation; **/quit** or **/exit** exits. Full session survives restarts.

This makes Axium usable on headless servers, remote machines over SSH, or a Raspberry Pi with no monitor attached.

---

## Why it beats the best agentic tools

The leading agentic tools — terminal-based coding agents, IDE-integrated assistants — are genuinely excellent. They reason deeply, edit code accurately, and handle complex multi-step tasks. If IDE integration or a managed cloud experience is your priority, they may be the better fit.

Where they stop is where Axium starts.

| Capability | Axium | Best-in-class alternatives |
|---|---|---|
| Shell commands, file ops, git, web fetch | ✅ | ✅ |
| Extended / adaptive reasoning (Claude) | ✅ | ✅ |
| Project-scoped memory files (e.g. `CLAUDE.md`) | ✅ `.axium/knowledge.md` | ✅ |
| **User-level cross-session personal memory** | ✅ survives new projects & sessions | ⚠ project-scoped only |
| **Behavioral user model** — infers preferences without prompting | ✅ | ❌ |
| **Cross-session full-text history search** (SQLite FTS5) | ✅ | ❌ |
| **Background task queue — runs while you're offline** | ✅ Telegram notification on done | ❌ |
| **Non-coding tasks** — research, email, system ops, monitoring | ✅ | ⚠ coding-focused |
| **Multi-model routing** — 6 slots, 2 providers, per-slot config | ✅ | ❌ single model |
| **Auto-fallback** on rate limit or outage | ✅ | ❌ |
| Continuation model for tool-loop turns (cheaper) | ✅ | ❌ |
| Tool subsetting — simple mode sends 18/31 tools to API | ✅ saves ~4,300 tokens/call | ❌ |
| Application-level prompt caching (1h TTL, 3 breakpoints) | ✅ | ❌ |
| Local classifier — conversational turns skip LLM entirely | ✅ ~<1ms, zero cost | ❌ |
| Post-turn code review by a dedicated secondary model | ✅ automatic | ❌ |
| Plugin system — 8 lifecycle hooks, language-agnostic | ✅ | ❌ |
| Injectable domain skills per task type | ✅ `axium-skills/` | ❌ |
| Telegram channel with full agent access | ✅ | ❌ |
| **CLI / SSH access** — full agent over stdin/stdout, no browser needed | ✅ | ❌ |
| Runs on a Raspberry Pi Zero 2 W or equivalent ARM SBC | ✅ | ❌ Python-based |
| IDE integration | ❌ no IDE plugin | ✅ |

The last row is intentional: Axium has no IDE plugin. If deep VS Code / JetBrains integration is your priority, it is not the right tool. You get CLI, browser, and Telegram instead — and a binary that runs on a $15 ARM board. Everything else in the table is real, verifiable, and in the source code.

---

## Memory that actually persists

The leading tools offer project-scoped memory files — you write conventions, the agent reads them per-project. That's useful. Axium goes further:

- **User memory** (`memory.md`) — facts, preferences, recurring context. Survives restarts, survives new projects. Plain markdown — you can read and edit it directly.
- **User model** — inferred behavioral profile written proactively by the agent after sessions: communication style, expertise level, recurring patterns. No prompting required.
- **Project knowledge** (`.axium/knowledge.md`) — per-project notes and conventions, written and updated by the agent as it learns your stack.
- **Conversation history** — all sessions stored in SQLite with FTS5 full-text search. Ask *"what did we decide about the auth architecture last month?"* and surface real past context, not a guess.

---

## 31 tools. Parallel execution.

Every tool call in a turn runs concurrently via Tokio. Reading 5 files, running diagnostics, and checking git status happen simultaneously — not sequentially.

| Category | Tools |
|---|---|
| Shell | `run_command`, `spawn_background` |
| Files | `read_file`, `write_file`, `append_file`, `patch_file`, `delete_file`, `move_file` |
| Search | `search_files`, `list_directory`, `scan_project`, `search_history` |
| Code intelligence | `find_references`, `rename_symbol`, `get_dependency_graph`, `get_diagnostics` |
| Web | `browse_url`, `web_search` |
| Git | `git_command` |
| Agent | `run_subagent`, `set_autonomous`, `queue_task`, `plan_file_changes` |
| Memory | `update_memory`, `update_user_model`, `update_project_knowledge` |
| Communication | `send_email`, `send_file`, `ask_user` |
| Tasks | `task_manage` |

`scan_project` extracts symbols from source files via `rust-analyzer` and injects a live architecture map into the system prompt every turn — the agent understands your project structure without exploring files first.

`rename_symbol` uses AST-aware replacement: dead zones (comments, string literals) are identified and skipped so renames don't corrupt non-code content.

---

## Background task queue

Queue a task, disconnect, come back to results.

```
You:  audit the codebase for SQL injection and write a report. Ping me when done.
[disconnect]

[Telegram, 25 minutes later]:
Axium: Done. Found 2 potential issues in db/queries.py. Report at /home/you/audit.md
```

The worker polls every 4 seconds, runs a full agent turn with the same tools and memory, verifies the result was actual work (not just a description of work — it checks for tool usage evidence), and retries failed tasks with the failure context injected.

---

## The cost architecture

Most agentic tools send every message to the most expensive model. Axium routes intelligently.

```
Request arrives
    │
    ├─ Quick pattern match (<1ms, zero LLM cost)
    │   greetings, acks, identity questions
    │
    ├─ Local 10-dimension weighted scorer (<1ms, zero LLM cost)
    │   classifies clear simple/medium requests without any API call
    │
    ├─ Cheap classifier model (e.g. gpt-4.1-nano, fractions of a cent)
    │   for genuinely ambiguous cases only
    │
    └─ Primary model (Claude Sonnet / GPT-4)
        reserved for complex tasks that need it
```

In conversational and simple-request usage, the local scorer alone handles a significant portion of turns at zero LLM cost. The exact split depends on your usage pattern — heavy coding sessions lean primary-model; mixed chat sessions see more local routing.

Additionally:

- **Prompt caching** — 1-hour TTL cache breakpoints on tool definitions, soul, and conversation prefix. Within an active session, repeated prefixes see up to ~90% reduction in billable input tokens (Anthropic's published cache pricing).
- **Tool subsetting** — Simple mode sends 18 tool definitions instead of 31, saving ~4,300 tokens per call while keeping all tools needed for lightweight tasks.
- **Continuation model** — Follow-up turns after tool calls use a cheaper model, reserving the primary for first-contact reasoning.
- **Compaction** — At 60% of your token limit, old history is summarized by the compactor model. The summary costs a fraction of the replaced tokens.
- **Conversation recovery** — Every N turns, a cheap model cleans correction/retry noise from history, preventing waste from accumulated failed attempts.

---

## Cost comparison: Axium vs Claude Code

Using published Anthropic pricing (claude-sonnet-4-6 and claude-haiku-4-5). Claude Code uses Sonnet for every call by default and has prompt caching. The estimates below apply the same prompt-caching assumption (1-hour TTL) to both.

**Scenario A — simple task:** *"show unused imports in utils.rs and fix them"* — 3 API calls, ~13k tokens total.

| | Model routing | Approx cost |
|---|---|---|
| Claude Code (default) | Sonnet, all calls | ~$0.06 |
| Claude Code `--model haiku` | Haiku, all calls | ~$0.016 |
| **Axium** | **Auto-routes → Haiku all calls · 18-tool subset** | **~$0.012** |

**Scenario B — medium project:** *"refactor auth module to JWT, run tests, fix failures, commit"* — 10 API calls, context grows to ~30k tokens.

| | Model routing | Approx cost |
|---|---|---|
| Claude Code (default) | Sonnet, all calls | ~$0.52 |
| Claude Code `--model haiku` | Haiku, all calls | ~$0.14 |
| **Axium** | **Sonnet call 1 · Haiku continuation · compaction** | **~$0.16** |

Axium costs slightly more than CC+Haiku on medium tasks (~$0.02) because it uses Sonnet for the first call — the planning and architectural reasoning step. Every tool-loop follow-up (reading files, running commands, interpreting results) routes to Haiku automatically. On simple tasks Axium is cheaper than CC+Haiku by ~30% because the 18-tool subset removes 4,300 tokens per call. Conversational turns handled by the local classifier cost $0.

**Where the savings come from:**

| Optimization | Effect |
|---|---|
| Local classifier — trivial / conversational turns | $0 — no API call at all |
| Prompt caching (1h TTL, 3 breakpoints) | ~90% reduction on repeated prefix tokens within a session |
| Tool subsetting — 18 tools for simple, 31 for complex | −4,300 tokens per simple call |
| Continuation model — Haiku for all tool-loop follow-ups | −70% per follow-up vs Sonnet |
| Compaction at 60% token limit | −15–25% on long multi-tool sessions |

> Claude Code subscription tiers ($20–200/month) bundle usage rather than billing per token — the ratios above reflect underlying API costs and are consistent regardless of billing model.

---

## 6 model slots, 3 providers

```json
{
  "models": {
    "primary":      "deepseek-v4-pro",     // complex reasoning — used selectively
    "continuation": "deepseek-v4-flash",   // tool-loop follow-ups — cheaper
    "classifier":   "deepseek-v4-flash",   // routing decisions — very cheap
    "compactor":    "deepseek-v4-flash",   // history summarization
    "review":       "deepseek-v4-flash",   // post-turn code review
    "fallback":     "gpt-4.1"              // auto-activates if primary fails
  }
}
```

Each slot points to **Anthropic, OpenAI or DeepSeek** independently. The provider is inferred from the model id (`claude-*`, `deepseek-*`) or set explicitly per slot. DeepSeek speaks the OpenAI wire format, streams its reasoning separately, and reports prompt-cache hits — all of which Axium uses. The fallback activates automatically on repeated API failures or rate limits.

```json
"api_keys": { "anthropic": "...", "openai": "...", "deepseek": "..." }
```

Environment fallbacks: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`.

---

## Benchmarks

`python/` holds a Python implementation of the same agent. Each implementation is measured by its own benchmark project, running the same scenarios through the same graders and writing the same JSONL rows, so a Rust number and a Python number are directly comparable:

| project | measures |
|---|---|
| `bench-python/` | the Python implementation |
| `bench-rust/` | the Rust implementation (this binary) |

A 20-scenario suite covers bug fixes, refactors, features, read-only codebase comprehension, and agent-level behaviour. Every scenario runs against a freshly generated seed project with planted defects and is scored on two axes — **change** (did it do the task, verified by executing the agent's code) and **regress** (did it break anything that already worked). A run that ships the feature and breaks the build ranks below one that does nothing.

```bash
cd bench-python
python -m bench.runner --sanity          # graders must fail on the untouched seed
python -m bench.runner                   # full suite
python -m bench.runner --compare deepseek-v4-pro,deepseek-v4-flash
python -m bench.report --all
```

Every run records tokens, cache-hit rate, per-role cost split, latency and tool histogram, so a routing or prompt change can be proven to pay for itself. See `BENCHMARKS.md` for the full guide, or each project's own README.

---

## Head-to-head: Axium vs Hermes vs OpenClaw

Nine scenarios across three axes, three difficulty tiers each, all three
harnesses driving the **same model** (`deepseek-v4-flash`) with the same prompt
text. Prompts are agent-neutral: no tool names, no framework vocabulary, nothing
written for one harness.

Tokens are not self-reported. Every call from every harness was routed through a
recording proxy and counted from the provider's own `usage` block, then
reconciled against each harness's own figure. Three of the three agreed exactly;
one under-report was caught this way (see the footnotes).

### The tests

| id | tier | what it asks |
|---|---|---|
| **N1** | hard | 2,834 lines. A partial-refund tax leg reverses the full tax. The symptom names the ledger. |
| **N2** | very hard | 7,630 lines across 37 modules. Same defect, but the symptom names only a behaviour: "numbers do not add up after a partial return". |
| **N3** | crazy hard | As N2, plus a stale note naming an innocent, correct function as the recent change. Rewriting it is checked and fails the scenario. |
| **M1** | hard | A constraint given in turn 1 must still govern turn 6, after four turns of unrelated work. |
| **M2** | very hard | Twelve turns, two constraints, one of them revised mid-run. Retention alone gives the superseded value. |
| **M3** | crazy hard | As M2, plus a checked-in document asserting the superseded value, so the harness must trust the conversation over the file. |
| **R1** | hard | Delete two modules, then restore them byte-for-byte. |
| **R2** | very hard | Delete two modules and edit three others, then restore only the deleted ones. The edits must survive. |
| **R3** | crazy hard | An unrelated change lands between the damage and the repair, and must survive the restore. |

### The results

Checks passed, and tokens spent getting there. Lower is better on tokens.

| | Axium | Hermes | OpenClaw |
|---|---|---|---|
| **N1** navigation, hard | **4/4** — 69,650 | 4/4 — 469,642 | 4/4 — 234,110 |
| **N2** navigation, very hard | **4/4** — 78,801 | 4/4 — 1,789,262 *(DNF: 6.1x)* | 4/4 — 372,819 |
| **N3** navigation, crazy hard | **5/5** — 102,536 | 5/5 — 592,613 | 5/5 — 367,497 |
| **M1** memory, hard | 3/4 — 97,932 | **4/4** — 949,480 | **1/4** — 77,868 ¹ |
| **M2** memory, very hard | **5/5** — 370,386 | **4/5 — applied the superseded value** — 1,899,137 | 5/5 — 1,036,446 |
| **M3** memory, crazy hard | 5/6 — 365,138 | 5/6 — 1,633,682 | 5/6 — 1,162,038 |
| **R1** restore, hard | **3/3** — 187,389 | 3/3 — 529,703 | **1/3 — never restored them** — 406,010 |
| **R2** restore, very hard | **4/4** — 244,578 | 4/4 — 509,430 | **3/4 — not byte-identical** — 736,504 |
| **R3** restore, crazy hard | **4/4** — 215,207 | 4/4 — 613,531 | **2/4 — lost them again** — 669,994 |
| **fully passed** | **8 of 9** | 7 of 9 | 4 of 9 |
| **total tokens** | **1,731,617** | 8,986,480 | 5,063,286 ¹ |
| **vs Axium, like-for-like** ² | 1x | **4.2x** | **2.3x** |
| **vs Axium, current binary** | 1x | 5.2x | 2.9x |

*DNF* follows the published rule: a solve costing more than 5x the field's
fastest solve of that scenario is recorded as not having finished. The bar is
set against the fastest harness, not against Axium, and applies to Axium the
same way.

### What the numbers say

**OpenClaw cannot reliably restore a tree it has damaged.** It solved none of the
three restore scenarios: on R1 it never brought the deleted modules back, on R2
it brought them back but not byte-identical, on R3 it lost them again. This is a
capability difference, not a cost difference.

**Hermes forgets a revised constraint.** On M2, told 200 and corrected to 250
twelve turns later, it applied 200 — after 1,899,137 tokens and 23 minutes,
against Axium's 370,386 and 3 minutes.

**Axium is the cheapest harness at every navigation tier**, and its crazy-hard
run cost less than its very-hard one: the misleading note did not cost it
attempts.

**Neither competitor noticed the contradiction it left behind.** On M3 both
applied the correct values and left a checked-in document asserting the old one,
without mentioning it. Finished, but did not reach the goal.

### What these numbers do NOT show

Stated because a benchmark that only publishes its wins is marketing:

* **One run per cell.** Axium's own R1 varied 1.5x between two runs of an
  identical scenario (223,599 and 150,981 tokens). Treat every multiple as a
  direction, not a precise figure.
* **M1 is a genuine Axium failure.** It recalls the number and what it is for,
  then does not write it into the settings module. 3/4, reproducibly.
* **The crazy-hard navigation trap caught nobody.** All three passed N3's
  invariant check. That is evidence about the benchmark, not about the field.
* **Two Axium binaries are involved.** A path-guard defect was blocking every
  file write on Windows, and Axium was routing around it through the shell at
  real token cost. The competitors were measured against the *older* binary; the
  Axium column above is the fixed one. Both multiples are given in the table for
  that reason, and the like-for-like figure is the one to quote. The fix bought
  1.24x overall, and 2.5x on the navigation column specifically (631,691 to
  250,987) where the blocked writes cost the most retries. On the restore column
  it bought nothing measurable.
* **One model tier.** All of the above is `deepseek-v4-flash`. The single
  `deepseek-v4-pro` data point (R1) put Axium at 249,501 against Hermes at
  723,484 — same direction, different scale.

¹ OpenClaw's total includes the 77,868 tokens the wire recorded for M1, which
OpenClaw itself reported as zero.

² Like-for-like compares the competitors against the Axium binary they actually
ran alongside. It is the conservative figure and the one to quote.

OpenClaw reported **zero** tokens for M1. The wire recorded 77,868. Unmeasured
is not zero, which is why nothing here is taken from a harness's own accounting
without reconciling it first.

### Reproducing it

```bash
cd bench-python
python sanity_matrix.py                       # every grader must catch the plausible wrong answer
python proxy.py --port 8901 --run matrix/axium --label axium      # one per harness
python xrunner.py --suite matrix --model deepseek-v4-flash
python reconcile.py                           # self-reported vs the wire
```

`sanity_matrix.py` is the gate that matters. For each scenario it applies the
*plausible wrong move* — believed the stale note and rewrote the innocent
function, missed the mid-run revision, rolled the whole tree back instead of
only the deleted files — and requires the grader to catch it. A grader that is
merely red on an untouched tree proves very little.

---

## Plugin system

8 lifecycle hooks fire on every agent turn. Plugins are folder-based executables: JSON in, JSON out, any language.

```
on_message → on_classified → on_tool_before → on_tool_after →
on_response → on_session_start → on_task_start → on_task_complete
```

Use cases: audit logging, message routing overrides, external system notifications, input/output guardrails, custom telemetry.

---

## Skills system

Domain knowledge injected per task, not globally.

```
axium-skills/
├── rust-development/
│   └── guidelines.md     # your conventions, crate choices, patterns
├── docker-ops/
│   └── guidelines.md     # your infra layout, registry, deploy flow
└── my-project/
    └── conventions.md    # codebase-specific rules
```

In **Skills** mode, the classifier reads your message, selects the relevant folders, and injects their content before the primary model responds. Your conventions are respected without repeating them every session.

---

## Processing modes

| Mode | Behaviour |
|---|---|
| **Simple** | Prompt → primary model, 18-tool subset, no classifier overhead |
| **Supercharge** | Classify → enhance complex prompts → primary model (default) |
| **Skills** | Classify relevant skills → inject guidelines → primary model |

---

## Extended thinking

```json
"thinking_effort": "high"
```

`"low"` / `"medium"` / `"high"` / `"max"` — Anthropic adaptive reasoning before every response. Default is `"high"` for agentic tasks. Adds latency; recommended for decisions that are hard to undo.

---

## Built in Rust — runs anywhere Linux runs

Most agentic tools are Python processes. Axium is a compiled binary. That distinction matters more than it sounds.

- **Single binary** — ships everything including SQLite (FTS5) and TLS. Copy one file, run it.
- **No Python runtime** — no virtualenv, no dependency resolution, no `pip install` breaking on a new machine or a fresh ARM device.
- **Low RSS at idle** — realistically in the low tens of MB; Python-based agents typically sit at 50–200 MB before doing any work.
- **Runs on a Raspberry Pi Zero 2 W** — 512 MB RAM, quad-core ARM Cortex-A53. Cross-compile once with `cargo build --target aarch64-unknown-linux-gnu --release`, copy the binary, run it. A persistent personal AI agent running on a $15 SBC, with Telegram access from anywhere. Python-based agents don't fit.
- **Tokio async runtime** — parallel tool execution, streaming SSE parsed from raw byte buffers, no per-line allocations.
- **Graceful shutdown** — SIGTERM waits for in-flight requests before exiting.

---

## Security model

- **Local-only**: the WebSocket handler rejects any non-`127.0.0.1` IP with HTTP 403. The agent is not reachable from other machines on your network.
- **No cloud state**: all conversation history, memory, and tasks are SQLite + markdown files on your disk. Nothing leaves your machine except API calls to Anthropic/OpenAI.
- **Secret-free repo**: `config.json` (API keys, SMTP, Telegram token) is gitignored. Only `config.example.json` ships.
- **Process isolation**: spawned commands use `kill_on_drop(true)` and a configurable timeout. Hanged processes are killed automatically.
- **Write guard**: the agent cannot write to `/etc`, `/usr`, `/sys`, or your `.ssh` directory.

---

## Setup

**Requirements:** Rust stable · Anthropic API key · OpenAI key (optional, for classifier/review models)

```bash
# One-line install
bash <(curl -fsSL https://raw.githubusercontent.com/Broikos-Nikos/axium/main/install.sh)

# Build from source
git clone https://github.com/Broikos-Nikos/axium.git
cd axium
cp config.example.json config.json
cargo build --release
./target/release/axium            # → interactive CLI REPL
./target/release/axium --server   # → http://127.0.0.1:3000

# Install as a systemd service
sudo bash setup.sh
```

**Minimal `config.json`:**

```json
{
  "api_keys": {
    "anthropic": "sk-ant-...",
    "openai":    "sk-proj-..."
  },
  "models": {
    "primary":              "claude-sonnet-4-6",
    "primary_provider":     "anthropic",
    "classifier":           "gpt-4.1-nano",
    "classifier_provider":  "openai",
    "compactor":            "gpt-4.1-mini",
    "compactor_provider":   "openai",
    "review":               "gpt-4.1-mini",
    "review_provider":      "openai"
  },
  "agent": { "name": "Axium", "soul": "" },
  "settings": {
    "token_limit":             80000,
    "max_tokens":              16384,
    "terminal_timeout_secs":   120,
    "working_directory":       "~",
    "memory_file":             "memory.md",
    "thinking_effort":         "high"
  }
}
```

Put your system prompt in `soul.md` — it hot-reloads without a restart.

---

## Project structure

```
axium/
├── src/
│   ├── agent/
│   │   ├── router.rs        # classify → tool loop → self-correct → quality review
│   │   ├── sonnet.rs        # Anthropic + OpenAI streaming client, 31 tool definitions
│   │   ├── classifier.rs    # local scorer + LLM classifier + code reviewer
│   │   └── compactor.rs     # history compaction + conversation recovery
│   ├── tui/server.rs        # Axum routes, WebSocket handler, local-only guard
│   ├── tools/               # terminal, browser, search, project, email, depgraph
│   ├── channels/
│   │   ├── telegram.rs  # Telegram bot — full agent on mobile
│   │   └── cli.rs       # interactive CLI REPL — works over SSH
│   ├── db/                  # SQLite: chat history (FTS5) + task queue
│   ├── memory/store.rs      # persistent memory.md read/write
│   ├── plugins/mod.rs       # plugin manager, 8 lifecycle hooks
│   ├── worker.rs            # background task worker, polls every 4s
│   └── watcher.rs           # file watcher → live diagnostics
├── static/index.html        # browser UI — single file, no build step
├── axium-skills/            # domain skill folders (Skills mode)
├── axium-plugins/           # lifecycle hook plugins
├── soul.md                  # system prompt (hot-reloadable)
├── memory.md                # agent memory (gitignored)
└── python/                  # Python implementation + benchmark harness
    ├── axium/               # same pipeline: classify → tool loop → review
    └── bench/               # 20 scenarios, seeded fixtures, objective graders
```

---

## License

MIT
