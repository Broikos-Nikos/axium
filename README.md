# Axium — Autonomous Linux Assistant

A local-first autonomous AI assistant built in Rust. Runs entirely on your machine at `http://127.0.0.1:3000`. No cloud state, no external databases, no phone-home. Just a fast WebSocket-based chat UI connected to a tool-wielding agent with full system access.

## Install

```bash
bash <(curl -fsSL https://raw.githubusercontent.com/Broikos-Nikos/axium/main/install.sh)
```

That's it. The script checks for `git`, installs it if missing, clones the repo, and runs `setup.sh`.

---

## What it does

Axium is a coding and system automation assistant that lives on your Linux machine and can:

- Execute shell commands, run builds, manage git
- Read, write, patch, and search files
- Browse URLs, fetch documentation
- Run tasks in the background with Telegram notifications
- Maintain persistent memory and project knowledge across sessions
- Review its own code changes after each turn
- Understand your Rust project structure via `rust-analyzer` integration

---

## Requirements

- **Rust** (stable, 2021 edition)
- **`rust-analyzer`** CLI — for symbol extraction and AST-aware rename
  ```bash
  rustup component add rust-analyzer
  ```
- **Anthropic API key** — Claude Sonnet (primary model)
- **OpenAI API key** — optional, for classifier/review models (GPT-4.1-mini)

---

## Setup

### 1. Clone and build

```bash
git clone https://github.com/your-username/axium.git
cd axium
cargo build --release
```

### 2. Create `config.json`

Copy the template and fill in your API keys:

```bash
cp config.example.json config.json
```

```json
{
    "api_keys": {
        "anthropic": "sk-ant-...",
        "openai": "sk-proj-..."
    },
    "models": {
        "primary": "claude-sonnet-4-20250514",
        "primary_provider": "anthropic",
        "continuation": "claude-sonnet-4-20250514",
        "continuation_provider": "anthropic",
        "classifier": "gpt-4.1-mini",
        "classifier_provider": "openai",
        "review": "gpt-4.1-mini",
        "review_provider": "openai",
        "subagent": "claude-sonnet-4-20250514",
        "subagent_provider": "anthropic"
    },
    "agent": {
        "name": "Axium"
    },
    "soul_file": "soul.md",
    "settings": {
        "token_limit": 80000,
        "terminal_timeout_secs": 120,
        "max_output_chars": 15000,
        "max_tool_iterations": 30,
        "max_retries": 2,
        "working_directory": "/home/yourname",
        "conversation_logging": false
    },
    "smtp": {
        "host": "",
        "port": 587,
        "user": "",
        "password": "",
        "from": ""
    },
    "telegram": {
        "bot_token": "",
        "allowed_users": []
    }
}
```

### 3. Customize `soul.md`

The soul file is the agent's system prompt. Edit it freely — it hot-reloads without restart. The included `soul.md` is a good starting point.

### 4. Run

```bash
cargo run --release
# → http://127.0.0.1:3000
```

Or install as a systemd service:

```bash
sudo bash setup.sh
sudo systemctl status axium
```

---

## Processing Modes

Select the mode in the UI between **New Session** and **Settings**:

| Mode | Description |
|------|-------------|
| **Simple** | Prompt goes directly to the primary model. Fast, no overhead. |
| **Supercharge** | GPT-4.1-mini classifies complexity first. Complex tasks get extra planning context. Default for background tasks. |
| **Skills** | LLM scans `axium-skills/` and injects relevant guidelines before responding. Good for domain-specific workflows. |

Mode is stored in browser localStorage — not saved server-side.

---

## Skills System

Create folders under `axium-skills/` with markdown files describing guidelines or domain knowledge:

```
axium-skills/
├── rust-development/
│   └── guidelines.md
├── docker-ops/
│   └── guidelines.md
└── my-project/
    └── conventions.md
```

In **Skills** mode, the classifier reads your message, picks relevant skill folders, and injects their content into the prompt before calling the primary model.

---

## Tool Suite

| Tool | Description |
|------|-------------|
| `run_command` | Execute shell commands with timeout, PTY, kill-on-drop |
| `read_file` | Read files with optional line range |
| `write_file` | Create or overwrite files |
| `patch_file` | Find-and-replace text in a file |
| `search_files` | Regex search with glob filter |
| `list_directory` | Directory listing with sizes |
| `browse_url` | HTTP fetch with HTML stripping |
| `git_command` | Git operations (status, commit, diff, log, etc.) |
| `scan_project` | Annotated file tree with symbol extraction |
| `get_dependency_graph` | File-level import map (who uses what) |
| `find_references` | Project-wide symbol references |
| `rename_symbol` | AST-aware symbol rename (skips comments/strings) |
| `plan_file_changes` | List planned edits for user approval |
| `verify_file_syntax` | Syntax check across 8 languages |
| `update_memory` | Write to persistent memory file |
| `update_project_knowledge` | Save project-specific notes to `.axium/knowledge.md` |
| `queue_task` | Add background task to the task queue |
| `set_autonomous` | Enable autonomous mode (agent loops up to 10 turns) |
| `run_subagent` | Spawn a sub-agent for a bounded sub-task |
| `send_email` | Send email via SMTP |
| `ask_user` | Pause and ask a clarifying question |
| `web_search` | DuckDuckGo search |
| `get_diagnostics` | Fetch VS Code language diagnostics |
| `delete_file` | Delete a file (with confirmation) |
| `move_file` | Move or rename a file |

---

## Project Awareness (Rust Projects)

For Rust projects, Axium uses `rust-analyzer` to build a live architecture map on every turn:

```
[ARCHITECTURE]
  agent/
    router.rs [2467L] — fn run_agent_turn→AgentEvent; impl TurnConfig: ...
    sonnet.rs [886L]  — struct SonnetClient{...}; fn build_tools→Vec<Tool>
  tools/
    project.rs [546L] — fn build_project_context→String; fn scan_project→String
```

This is injected into the system prompt automatically — the agent knows your project structure without needing to read individual files first.

The **dependency graph** tool shows file-level imports:

```
get_dependency_graph("src/agent/router.rs", "dependents")
→ Files that import router.rs:
    tui/server.rs
    worker.rs
    channels/telegram.rs
```

Results are cached in `.axium/architecture_cache.json` by file mtime — subsequent calls are instant.

---

## Background Task Queue

Queue long-running tasks that execute in the background:

```
queue_task("Refactor the authentication module and run all tests")
```

A worker process picks up the task, runs the full agent loop, and notifies you via Telegram (if configured) when done. Results persist in SQLite and are shown when you reconnect.

---

## Telegram Integration

Set `telegram.bot_token` and `telegram.allowed_users` in `config.json`. The Telegram channel runs a parallel agent instance — same tools, same memory, same project context. Background task completions are pushed as Telegram notifications.

---

## Autonomous Mode

```
set_autonomous(true)
```

The agent will continue working on its own for up to 10 turns without waiting for user input. Useful for long refactors or multi-step build pipelines. The UI shows progress in real time.

---

## Code Review

After any turn that modifies files, a secondary model runs a silent code review and optionally generates test suggestions. Results are appended to the turn output. Enabled automatically when `review` model is configured.

---

## Project Structure

```
axium/
├── src/
│   ├── main.rs                 # Entry point, logging, graceful shutdown
│   ├── agent/
│   │   ├── router.rs           # Agent turn: classify → tool loop → self-correct
│   │   ├── sonnet.rs           # LLM API client (Anthropic + OpenAI), tool definitions
│   │   ├── classifier.rs       # Prompt classifier + skills analyzer + code reviewer
│   │   └── compactor.rs        # History compaction + tool output summarization
│   ├── tui/
│   │   └── server.rs           # Axum routes, WebSocket handler, local-only guard
│   ├── tools/
│   │   ├── terminal.rs         # Shell execution
│   │   ├── browser.rs          # URL fetching with HTML stripping
│   │   ├── search.rs           # File search + directory listing
│   │   ├── project.rs          # Project context + architecture map + symbol extraction
│   │   ├── email.rs            # SMTP email sending
│   │   └── depgraph.rs         # File-level dependency graph
│   ├── channels/
│   │   └── telegram.rs         # Telegram message handler
│   ├── db/
│   │   ├── history.rs          # SQLite chat history
│   │   └── tasks.rs            # SQLite task queue
│   ├── memory/
│   │   └── store.rs            # Read/write persistent memory.md
│   ├── config/
│   │   └── loader.rs           # Config + soul loading (hot-reload)
│   ├── worker.rs               # Background task worker (polls every 4s)
│   └── watcher.rs              # File watcher (notify v6)
├── static/
│   └── index.html              # Web UI (single-file HTML/CSS/JS)
├── axium-skills/               # Domain-specific skill files (Skills mode)
├── soul.md                     # Agent system prompt (hot-reloadable)
├── setup.sh                    # Systemd service installer
└── Cargo.toml
```

---

## Security

- **Local-only**: WebSocket handler rejects any non-`127.0.0.1` connection with HTTP 403
- **No cloud state**: All data is SQLite + markdown files on disk
- **Secret-free repo**: `config.json` (API keys, SMTP, Telegram token) is gitignored
- **Process isolation**: Shell commands use `kill_on_drop(true)` and enforced timeouts

---

## License

MIT
