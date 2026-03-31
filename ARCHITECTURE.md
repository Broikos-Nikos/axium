# AXIUM — Autonomous Linux Assistant

## Overview

**Axium** (`axiom` binary) is a local-first autonomous Linux assistant written in Rust. It runs on your machine at `http://127.0.0.1:3000` with a WebSocket-based web UI. The primary reasoning model is **Claude Sonnet** (or any Anthropic/OpenAI model). Secondary models handle classification, history compaction, and post-turn code review.

Axium is designed for autonomy — it classifies prompts, plans before acting, self-corrects on errors, executes tools in parallel, manages background tasks, and persists all state across restarts.

---

## Core Principles

1. **Local-only** — no cloud state, no external databases. SQLite + markdown on disk. Binds to `127.0.0.1` only.
2. **Secure by default** — WebSocket handler rejects any non-loopback IP with HTTP 403. API keys stay in `config.json`.
3. **Autonomous** — classifies complexity, plans before acting, self-corrects on failures, supports unattended background task execution.
4. **Minimal tokens** — compresses history aggressively, truncates tool outputs, summarizes large results.
5. **Full system access** — terminal commands, file operations, git, web fetching, background processes, email, Telegram.
6. **Persistent memory** — `memory.md` + SQLite chat history + task queue survive restarts.
7. **Transparent execution** — plans, tool calls, outputs, and streamed responses are visible in real time.

---

## Architecture Diagram

```
┌──────────────────────────────────────────────────────────────────────┐
│                       Web UI (index.html)                             │
│  ┌──────────────────────────────┐  ┌──────────────────────────────┐   │
│  │         Chat Panel           │  │       Terminal Panel          │   │
│  │  User ▸ ...                  │  │  ▸ run_command(cargo build)   │   │
│  │  ▸ Plan: ...                 │  │  stdout: ...                  │   │
│  │  ▸ [tool_call] ...           │  │  ▸ git_command(status)        │   │
│  │  Axium ▸ ... (streamed)      │  │  exit: 0                      │   │
│  │  [input bar]                 │  │                               │   │
│  └──────────────────────────────┘  └──────────────────────────────┘   │
│  [🔵 Simple  🟣 Supercharge  🧠 Skills]  [New Session]  [⚙ Settings]  │
└──────────────────────────────────────────────────────────────────────┘
              │ WebSocket (/ws)                    │ HTTP (/api/*)
              ▼                                    ▼
┌──────────────────────────────────────────────────────────────────────┐
│                         Axum Web Server (server.rs)                   │
│  Routes, WebSocket handler, local-only guard (ConnectInfo → 403)     │
│  API: /api/config, /api/sessions, /api/export, /api/file-download    │
│       /api/autostart, /api/stop, /api/shutdown, /api/reboot          │
└────────────┬─────────────────────────────────────────────────────────┘
             │
             ▼
┌──────────────────────────────────────────────────────────────────────┐
│                    Classifier (classifier.rs)                          │
│  1. analyze_skills()  — pick relevant axium-skills/ folders          │
│  2. quick_classify()  — regex fast-path (trivial / greeting / etc.)  │
│  3. classify()        — LLM classification (OpenAI)                  │
│     → Trivial / Simple / Long / Code / Task / Research / ...        │
│  4. CodeReviewer      — post-turn git diff → LLM review + test gen  │
└────────────┬─────────────────────────────────────────────────────────┘
             │ mode: simple | supercharge | skills
             ▼
┌──────────────────────────────────────────────────────────────────────┐
│                    Agent Router (router.rs)                            │
│  classify_and_run():                                                  │
│    simple     → skip classifier, call run_agent_turn directly        │
│    supercharge→ LLM classify, inject complexity hints, run_agent_turn│
│    skills     → analyze_skills, inject skill context, run_agent_turn │
│                                                                       │
│  run_agent_turn():                                                    │
│    1. Build system prompt (soul + memory + project + tasks)          │
│    2. Check tokens → compact if over limit                           │
│    3. Tool loop (max N iterations):                                  │
│       - Call LLM API (stream)                                        │
│       - Parse tool_use blocks                                        │
│       - Execute tools IN PARALLEL (tokio::spawn)                     │
│       - Self-correct on errors (retry with reflection)               │
│       - Heartbeat check → retry if response looks incomplete         │
│       - Truncate outputs > max_output_chars                          │
│    4. Stream AgentEvents via mpsc channel                            │
│    5. Post-turn: CodeReviewer runs on git diff (async, parallel)     │
└──────────┬───────────────────────────────────────────────────────────┘
           │
     ┌─────┼──────────────────────────────┐
     ▼     ▼                              ▼
┌─────────────┐ ┌─────────────┐ ┌───────────────────────────────────────┐
│ LLM Client  │ │  Compactor  │ │            Tool Suite                 │
│ (sonnet.rs) │ │(compactor.rs│ │                                       │
│             │ │             │ │ run_command     scan_project          │
│ Anthropic   │ │ Summarize   │ │ read_file       web_search            │
│ OpenAI      │ │ history &   │ │ write_file      get_dependency_graph  │
│ auto-detect │ │ tool output │ │ append_file     find_references       │ 
│ streaming   │ │             │ │ patch_file      rename_symbol         │
│ 28 tools    │ │             │ │ search_files    plan_file_changes     │
└─────────────┘ └─────────────┘ │ list_directory  verify_file_syntax    │ 
                                │ browse_url      get_diagnostics       │
                                │ git_command     set_autonomous        │
                                │ task_manage     queue_task            │
                                │ update_memory   run_subagent          │
                                │ update_project_ send_email            │
                                │   knowledge     send_file             │
                                │ spawn_background delete_file          │
                                │ ask_user        move_file             │
                                └───────────────────────────────────────┘
           │
     ┌─────┼──────────┬───────────────────┐
     ▼     ▼          ▼                   ▼
┌────────┐ ┌──────┐ ┌──────────┐ ┌──────────────────┐
│ SQLite │ │Memory│ │ Task DB  │ │ Telegram Channel │
│ Chat   │ │ .md  │ │ (SQLite) │ │ (telegram.rs)    │
│History │ │      │ │ + worker │ │ bg notify        │
└────────┘ └──────┘ └──────────┘ └──────────────────┘
```

---

## Processing Modes

Selected in the UI between **New Session** and **Settings**; stored in browser localStorage only.

| Mode | Behaviour |
|------|-----------|
| `simple` | Prompt → primary model directly. No classifier overhead. |
| `supercharge` | GPT classifier first, complexity hints injected, then primary model. Default for background tasks and Telegram. |
| `skills` | LLM scans `axium-skills/`, injects relevant skill guidelines, then primary model. |

---

## Module Map

```
assistant/
├── ARCHITECTURE.md            # this file
├── README.md                  # user-facing documentation
├── config.example.json        # safe template (no real keys)
├── config.json                # API keys + settings (gitignored)
├── soul.md                    # agent system prompt (hot-reload)
├── memory.md                  # agent-managed persistent memory (gitignored)
├── axium-skills/              # domain skill folders (Skills mode)
│   └── rust-development/
│       └── guidelines.md
├── static/
│   └── index.html             # web UI (single-file HTML/CSS/JS)
├── Cargo.toml
└── src/
    ├── main.rs                # entry point, logging, graceful shutdown
    ├── tui/
    │   └── server.rs          # Axum routes, WebSocket handler, local-only guard
    │                          # API endpoints, autonomous loop, task push on reconnect
    ├── agent/
    │   ├── mod.rs             # AgentEvent enum, Provider enum, token estimation
    │   ├── sonnet.rs          # LLM API client (Anthropic + OpenAI), 28 tool definitions
    │   ├── compactor.rs       # History compaction + tool output summarization
    │   ├── classifier.rs      # Classifier (LLM + quick_classify), CodeReviewer,
    │   │                      # analyze_skills() for Skills mode
    │   └── router.rs          # classify_and_run, run_agent_turn, execute_tool,
    │                          # run_subagent_task, compress_tool_log, get_dead_zones_rs
    ├── tools/
    │   ├── mod.rs
    │   ├── terminal.rs        # Shell execution (PTY, kill_on_drop, timeout, background)
    │   ├── browser.rs         # HTTP URL fetching with HTML stripping
    │   ├── search.rs          # Regex file search + directory listing
    │   ├── project.rs         # Project context builder, architecture map,
    │   │                      # extract_symbols_ra (rust-analyzer CLI), build_project_context
    │   ├── depgraph.rs        # File-level import graph (crate:: use-statement parsing)
    │   └── email.rs           # SMTP email via lettre
    ├── channels/
    │   └── telegram.rs        # Telegram bot: receive messages, run agent, send results
    ├── db/
    │   ├── history.rs         # SQLite: chat messages per session
    │   └── tasks.rs           # SQLite: task queue (pending/running/done/failed),
    │                          # claim_pending, save_task_result, unread_completed
    ├── memory/
    │   └── store.rs           # Read/write/append to memory.md
    ├── config/
    │   └── loader.rs          # load_soul() (hot-reload), Config struct, TurnConfig
    ├── worker.rs              # Background task worker — polls every 4s, claims pending
    │                          # tasks, runs full agent turn, broadcasts result via WS
    └── watcher.rs             # File watcher (notify v6) — broadcasts diagnostics via
                               # broadcast_tx on source file changes
```

---

## Key Components

### TurnConfig (`config/loader.rs`)

Carries all per-turn runtime settings. `#[derive(Clone)]`.

```rust
pub struct TurnConfig {
    pub mode: String,                 // "simple" | "supercharge" | "skills"
    pub token_limit: usize,
    pub terminal_timeout: u64,
    pub max_output_chars: usize,
    pub max_tool_iterations: usize,
    pub max_retries: u32,
    pub sudo_password: String,
    pub working_directory: String,
    pub smtp_host / smtp_port / smtp_user / smtp_password / smtp_from,
    pub telegram_bot_token: String,
    pub conversation_logging: bool,
    pub http: reqwest::Client,
    pub anthropic_key / openai_key: String,
    pub primary_model / primary_provider: String,
    pub subagent_depth: u32,
    pub continuation_model / continuation_provider: String,
    pub classifier_model / classifier_provider: String,
    pub review_model / review_provider: String,
}
```

### AgentEvent (`agent/mod.rs`)

Events streamed from the agent turn to the WebSocket handler.

| Event | Description |
|-------|-------------|
| `TextDelta(String)` | Streamed text chunk |
| `Text(String)` | Final complete agent response |
| `ToolCall { name, input }` | Agent invoking a tool |
| `ToolOutput { name, stdout, stderr, code }` | Tool result |
| `Plan(String)` | Agent plan before acting |
| `MemoryUpdate { section, content }` | Memory file was written |
| `AskUser { question, reply_tx }` | Agent pausing for user input |
| `Classified { class, detail }` | Classifier result |
| `TrivialAnswer(String)` | Classifier answered directly (skip main model) |
| `FileOffer { path, caption }` | Agent delivers a file (download + Telegram) |
| `ModelUsed(String)` | Which model handled this turn |
| `Error(String)` | Error occurred |
| `Retry` | Heartbeat rejected incomplete response; UI clears streaming text |
| `SetAutonomous { enabled }` | Toggle autonomous mode for the session |
| `TaskQueued { id, title }` | Background task added to queue |
| `Done` | Turn complete |

---

## Tool Suite (28 tools)

| Tool | Module | Description |
|------|--------|-------------|
| `run_command` | terminal.rs | Shell commands with PTY, timeout, kill-on-drop |
| `read_file` | router.rs | Read files with optional line range |
| `write_file` | router.rs | Create or overwrite files |
| `append_file` | router.rs | Append text to a file |
| `patch_file` | router.rs | Find-and-replace in file |
| `search_files` | search.rs | Regex search with glob filter |
| `list_directory` | search.rs | Directory listing with sizes |
| `scan_project` | project.rs | Annotated file tree + symbol extraction (rust-analyzer) |
| `browse_url` | browser.rs | HTTP fetch with HTML stripping |
| `web_search` | browser.rs | DuckDuckGo search (urlencoding) |
| `git_command` | router.rs | Git operations |
| `task_manage` | db/tasks.rs | Create, update, list persistent tasks |
| `update_memory` | memory/store.rs | Write to memory.md |
| `update_project_knowledge` | router.rs | Write to `.axium/knowledge.md` |
| `ask_user` | router.rs | Pause and request user input |
| `spawn_background` | terminal.rs | Start detached background process |
| `send_email` | email.rs | SMTP email |
| `send_file` | router.rs | Deliver file to browser (download) + Telegram |
| `run_subagent` | router.rs | Spawn bounded sub-agent (depth guard, events forwarded) |
| `plan_file_changes` | router.rs | List planned edits for user approval |
| `set_autonomous` | router.rs | Enable/disable autonomous mode (max 10 turns) |
| `queue_task` | db/tasks.rs | Add task to background queue |
| `get_diagnostics` | router.rs | VS Code language diagnostics via LSP |
| `delete_file` | router.rs | Delete a file |
| `move_file` | router.rs | Move or rename a file |
| `find_references` | router.rs | Project-wide symbol references |
| `rename_symbol` | router.rs | AST-aware symbol rename (skips comments/strings via rust-analyzer) |
| `get_dependency_graph` | depgraph.rs | File-level `crate::` import map (dependents/dependencies/both) |

---

## Project Awareness (Rust Projects)

For Rust projects, `scan_project` uses the `rust-analyzer` CLI to extract symbols from each source file:

```
rust-analyzer symbols < src/agent/router.rs
→ StructureNode { label: "run_agent_turn", kind: Function, detail: Some("AgentEvent"), ... }
```

Results are cached in `.axium/architecture_cache.json` per-file by mtime. The agent receives an `[ARCHITECTURE]` section in its system prompt every turn:

```
[ARCHITECTURE]
  agent/
    router.rs [2554L] — fn run_agent_turn→AgentEvent; fn classify_and_run; …
    sonnet.rs [902L]  — struct SonnetClient{}; fn build_tools→Vec<Tool>
  tools/
    project.rs [545L] — fn build_project_context→String; fn scan_project→String
```

`get_dependency_graph` parses `use crate::` statements to build a bidirectional file import map, answering "who imports this file" or "what does this file import".

`rename_symbol` uses `rust-analyzer parse` to extract COMMENT and STRING byte ranges as dead zones — replacements skip those ranges to avoid corrupting comments and string literals.

---

## Background Task Queue

`queue_task` inserts a row into the SQLite task table with status `pending`. The worker (`worker.rs`) polls every 4 seconds, claims pending tasks with a transaction, runs a full `classify_and_run` turn, saves the result, and broadcasts the completion event via the broadcast channel. Unread completed tasks are pushed to the UI on WebSocket reconnect. Telegram notifications are sent on completion when enabled.

---

## Autonomous Mode

`set_autonomous(true)` sets a session flag in `AppState`. The `pending_auto` loop in `server.rs` checks the flag after each turn and re-triggers the agent (up to 10 turns) without waiting for user input. The agent clears the flag by calling `set_autonomous(false)` or reaching the turn limit.

---

## Code Review

After any turn that modifies files, `CodeReviewer` in `classifier.rs` runs a `git diff` and sends it to the review model (OpenAI) for feedback and test generation. Runs in parallel with the turn response. Results are appended as a separate `[Code Review]` block.

---

## Security

- **Local-only guard**: `server.rs` `lan_guard` middleware rejects any non-`127.0.0.1` IP with HTTP 403
- **Process isolation**: spawned commands use `kill_on_drop(true)` and configurable timeout
- **Input caps**: user messages truncated at `max_input_chars`; tool outputs at `max_output_chars`
- **Secret isolation**: `config.json` is gitignored; the repo contains only `config.example.json`
- **Sub-agent depth guard**: `run_subagent` checks `subagent_depth < 3` to prevent runaway recursion

---

## Config File (`config.json`)

```json
{
  "api_keys": { "anthropic": "...", "openai": "..." },
  "models": {
    "primary": "claude-sonnet-4-6",
    "primary_provider": "anthropic",
    "compactor": "gpt-4.1-mini",
    "classifier": "gpt-4.1-nano",
    "continuation": "",
    "review": "gpt-4.1-mini",
    "review_provider": "openai"
  },
  "available_models": { "anthropic": [...], "openai": [...] },
  "agent": { "name": "Axium", "soul": "" },
  "soul_file": "soul.md",
  "settings": {
    "token_limit": 30000,
    "max_tokens": 16384,
    "max_history_messages": 200,
    "terminal_timeout_secs": 120,
    "max_output_chars": 8000,
    "max_tool_iterations": 100,
    "max_input_chars": 12000,
    "max_retries": 2,
    "max_sessions": 50,
    "working_directory": "/home/yourname",
    "smtp_host": "", "smtp_port": 587, "smtp_user": "", "smtp_password": "", "smtp_from": "",
    "telegram_bot_token": "", "telegram_allowed_users": "", "telegram_enabled": false,
    "conversation_logging": false
  }
}
```

---

## Rust Crates

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime (full features) |
| `axum` | Web framework + WebSocket |
| `reqwest` | HTTP client for LLM APIs and URL fetching (rustls) |
| `serde` / `serde_json` | JSON serialization |
| `rusqlite` | SQLite (bundled) — chat history + task queue |
| `lettre` | SMTP email (tokio + rustls) |
| `notify` | File system watcher (v6) |
| `tracing` + `tracing-subscriber` | Structured logging with env filter |
| `anyhow` | Error handling |
| `chrono` | Timestamps |
| `axum` | Web + WebSocket |
| `futures` | Async stream/sink utilities |
| `glob` | File pattern matching |
| `regex` | Regex search |
| `tokio-util` | Codec + async I/O utilities |
| `urlencoding` | URL encoding for web_search |
| `url` | URL parsing |
| `libc` | POSIX signal handling |

---

## Build & Run

```bash
cd assistant
cargo build --release
./target/release/axiom
# → http://127.0.0.1:3000

# Install as systemd service:
sudo bash setup.sh

# Force rebuild + restart:
sudo bash setup.sh --rebuild
```

Set log level: `RUST_LOG=axiom=debug cargo run --release`

Graceful shutdown: Ctrl+C or SIGTERM — waits for in-flight requests.
