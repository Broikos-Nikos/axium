# AXIUM — Autonomous Linux Assistant

## Overview

**Axium** is a local-first autonomous Linux assistant written in Rust. It runs entirely on your machine at `http://127.0.0.1:3000` with a web-based UI. The primary reasoning agent uses **Claude Sonnet** (or any Anthropic/OpenAI model). A secondary compaction layer uses a cheaper model (e.g. **GPT-4.1-mini**) for history summarization and tool output summarization.

Axium is designed for autonomy — it plans before acting, self-corrects on errors, executes tools in parallel, and persists all state across restarts.

---

## Core Principles

1. **Local-only** — no cloud state, no external databases. SQLite + markdown on disk. Binds to `127.0.0.1` only.
2. **Secure by default** — local-only socket verification rejects non-loopback connections. No messaging app integrations. API keys in config (not transmitted externally).
3. **Autonomous** — plans before acting, self-corrects on failures, manages tasks, maintains project awareness.
4. **Minimal tokens** — compresses history aggressively, truncates tool outputs, summarizes large results.
5. **Full system access** — terminal commands, file operations, git, web fetching, background processes.
6. **Persistent memory** — markdown memory file + SQLite chat history + task tracking survive restarts.
7. **Transparent execution** — the user sees plans, tool calls, outputs, and streamed responses in real time.

---

## Architecture Diagram

```
┌──────────────────────────────────────────────────────────────┐
│                   Web UI (index.html)                        │
│  ┌──────────────────────────┐  ┌──────────────────────────┐  │
│  │      Chat Panel          │  │    Terminal Panel         │  │
│  │                          │  │                           │  │
│  │  User ▸ ...              │  │  ▸ run_command(ls -la)    │  │
│  │  ▸ Plan: ...             │  │  stdout: ...              │  │
│  │  Axium ▸ ... (streamed)  │  │  ▸ git_command(status)    │  │
│  │                          │  │  exit: 0                  │  │
│  │  [input bar]             │  │                           │  │
│  └──────────────────────────┘  └──────────────────────────┘  │
│  [Status: connected | msgs | tokens] [New Session] [⚙]      │
└──────────────────────────────────────────────────────────────┘
         │ WebSocket (/ws)                    │ HTTP (/api/*)
         ▼                                    ▼
┌──────────────────────────────────────────────────────────────┐
│                    Axum Web Server                            │
│  server.rs — routes, WebSocket handler, local-only guard     │
│  ConnectInfo<SocketAddr> → reject non-127.0.0.1              │
└─────────┬────────────────────────────────────────────────────┘
          │
          ▼
┌──────────────────────────────────────────────────────────────┐
│                    Agent Router (router.rs)                    │
│  1. Build system prompt (soul + memory + project + tasks)    │
│  2. Check tokens → compact if over limit                     │
│  3. Planning instruction injected into soul                  │
│  4. Tool loop (max N iterations):                            │
│     - Call LLM API                                           │
│     - Parse tool_use blocks                                  │
│     - Execute tools IN PARALLEL (tokio::spawn)               │
│     - Self-correct on errors (retry with reflection)         │
│     - Truncate outputs > max_output_chars                    │
│  5. Stream events back via mpsc channel                      │
└──────────────────────────────────────────────────────────────┘
          │
    ┌─────┼────────────────────┐
    ▼     ▼                    ▼
┌────────────┐ ┌────────────┐ ┌──────────────┐
│ Sonnet API │ │ Compactor  │ │ Tool Suite   │
│ (sonnet.rs)│ │(compactor) │ │              │
│            │ │            │ │ run_command   │
│ Anthropic  │ │ Summarize  │ │ read_file    │
│ or OpenAI  │ │ history &  │ │ write_file   │
│ auto-detect│ │ tool output│ │ patch_file   │
└────────────┘ └────────────┘ │ search_files │
                              │ list_directory│
                              │ browse_url   │
                              │ git_command  │
                              │ task_manage  │
                              │ update_memory│
                              │ ask_user     │
                              │ spawn_background│
                              └──────────────┘
          │
    ┌─────┼──────────┐
    ▼     ▼          ▼
┌────────┐ ┌──────┐ ┌──────────┐
│ SQLite │ │Memory│ │ Task DB  │
│ Chat   │ │ .md  │ │ (SQLite) │
│History │ │      │ │          │
└────────┘ └──────┘ └──────────┘
```

---

## Module Map

```
assistant/
├── ARCHITECTURE.md          # this file
├── memory.md                # agent-managed persistent memory
├── config.json              # API keys, soul prompt, settings
├── chat_history.db          # SQLite — chat history + tasks
├── Cargo.toml
├── static/
│   └── index.html           # web UI (HTML + CSS + JS, single file)
└── src/
    ├── main.rs              # entry point, logging, graceful shutdown
    ├── tui/
    │   ├── mod.rs
    │   └── server.rs        # Axum routes, WebSocket handler, local-only guard
    ├── agent/
    │   ├── mod.rs           # Message type, token estimation, Provider enum
    │   ├── sonnet.rs        # LLM API client (Anthropic + OpenAI), tool definitions
    │   ├── compactor.rs     # History compaction + tool output summarization
    │   └── router.rs        # Agent turn: plan → compact → tool loop → self-correct
    ├── tools/
    │   ├── mod.rs
    │   ├── terminal.rs      # Shell execution (with kill_on_drop, timeout, background)
    │   ├── browser.rs       # HTTP URL fetching with HTML stripping
    │   ├── search.rs        # Regex file search + directory listing
    │   └── project.rs       # Project context builder (git, files, directory)
    ├── db/
    │   ├── mod.rs
    │   ├── history.rs       # SQLite chat message persistence
    │   └── tasks.rs         # SQLite task tracking (create/update/list)
    ├── memory/
    │   ├── mod.rs
    │   └── store.rs         # Read/write/append to memory.md
    └── config/
        ├── mod.rs
        └── loader.rs        # Parse config.json, typed config structs
```

---

## Key Features

### Security
- **Local-only socket verification**: WebSocket handler checks `ConnectInfo<SocketAddr>` — rejects any non-loopback IP with HTTP 403
- **No messaging app integrations**: No Discord/Slack/Telegram attack surface
- **Process isolation**: `kill_on_drop(true)` on spawned commands, timeout enforcement
- **Input truncation**: User messages capped at `max_input_chars` setting
- **Output truncation**: Tool results capped at `max_output_chars` setting

### Autonomy
- **Planning step**: System prompt instructs the agent to outline a plan before acting
- **Self-correction**: On API or tool errors, the agent retries with the error context visible (up to `max_retries`)
- **Parallel tool execution**: Independent tool calls from a single LLM response are executed concurrently via `tokio::spawn`
- **Task management**: Persistent task tracking (pending/running/done/failed) via SQLite
- **Project awareness**: Auto-detects git status, recent commits, project type, and directory structure

### Persistence
- **SQLite chat history**: All messages saved per session, loaded on reconnect
- **Session management**: Create new sessions or resume the latest one
- **Memory file**: Markdown-based long-term memory the agent reads and updates
- **Task state**: Survives restarts — the agent sees active tasks on every turn

### Token Management
- **Estimation**: ~3.5 chars/token with per-message framing overhead (more accurate than /4)
- **Compaction**: When history exceeds `token_limit`, older messages are summarized by the compactor model
- **Output caps**: Tool outputs truncated before being sent back to the LLM
- **Project context**: Kept compact (git status + key files + shallow directory only)

---

## Tool Suite

| Tool | Description |
|------|-------------|
| `run_command` | Execute bash commands with timeout and kill-on-drop |
| `read_file` | Read files with optional line range (start_line/end_line) |
| `write_file` | Create or overwrite files (auto-creates parent dirs) |
| `patch_file` | Find-and-replace text in a file |
| `search_files` | Regex search across files with glob filtering |
| `list_directory` | List directory contents with file sizes |
| `browse_url` | HTTP fetch with HTML stripping |
| `git_command` | Git operations (status, commit, diff, log, branch, etc.) |
| `task_manage` | Create, update, and list persistent tasks |
| `update_memory` | Append or replace sections in memory.md |
| `ask_user` | Pause and request user clarification |
| `spawn_background` | Start detached background processes |

---

## Config File (`config.json`)

```json
{
    "api_keys": {
        "anthropic": "sk-ant-...",
        "openai": "sk-proj-..."
    },
    "models": {
        "primary": "claude-sonnet-4-20250514",
        "compactor": "gpt-4.1-mini"
    },
    "agent": {
        "name": "Axium",
        "soul": "You are Axium, a precise and proactive Linux assistant..."
    },
    "settings": {
        "token_limit": 30000,
        "compaction_threshold": 25000,
        "max_history_messages": 200,
        "terminal_timeout_secs": 120,
        "memory_file": "memory.md",
        "max_output_chars": 8000,
        "max_tool_iterations": 15,
        "max_input_chars": 12000,
        "max_retries": 2
    }
}
```

---

## Rust Crates

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime (full features) |
| `axum` | Web framework + WebSocket support |
| `reqwest` | HTTP client for LLM APIs and URL fetching |
| `serde` / `serde_json` | JSON serialization |
| `rusqlite` | SQLite for chat history and task persistence |
| `tracing` + `tracing-subscriber` | Structured logging with env filter |
| `anyhow` | Error handling |
| `chrono` | Timestamps |
| `tower-http` | HTTP middleware |
| `futures` | Async utilities (stream, sink) |
| `glob` | File pattern matching |
| `regex` | Regex search in files |
| `tokio-stream` | Async stream utilities |

---

## Build & Run

```bash
cd assistant
cargo build --release
cargo run --release
# → http://127.0.0.1:3000
```

Set log level: `RUST_LOG=axiom=debug cargo run --release`

Graceful shutdown: Ctrl+C or SIGTERM.

---

## Event Flow (WebSocket)

Events sent from server to UI:

| Event | Description |
|-------|-------------|
| `system` | System messages (greeting, session info) |
| `text_delta` | Streamed text chunk from agent |
| `assistant` | Final complete agent response |
| `plan` | Agent's plan before acting |
| `tool_call` | Agent requesting a tool execution |
| `tool_output` | Tool result (stdout, stderr, exit code) |
| `memory_update` | Memory file was updated |
| `ask_user` | Agent asking a clarifying question |
| `error` | Error message |
| `done` | Turn complete |

Events sent from UI to server:

| Event | Description |
|-------|-------------|
| `message` | User chat message |
| `new_session` | Start a fresh session |
