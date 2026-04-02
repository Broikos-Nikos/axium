use anyhow::{Context, Result};
use futures::StreamExt;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use tokio::sync::mpsc;

use super::{resolve_provider, Provider};

/// Token usage data reported by the API.
#[derive(Debug, Default, Clone, Serialize)]
pub struct ApiUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
}

/// Tool definition sent to the LLM API.
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Anthropic-format tools array (built once).
/// The last tool is tagged with cache_control so Anthropic caches the entire tools array.
static TOOLS_ANTHROPIC: LazyLock<serde_json::Value> = LazyLock::new(|| {
    let mut tools = serde_json::to_value(build_tools()).unwrap();
    if let Some(arr) = tools.as_array_mut() {
        if let Some(last) = arr.last_mut() {
            last["cache_control"] = serde_json::json!({"type": "ephemeral", "ttl": "1h"});
        }
    }
    tools
});

/// OpenAI-format functions array (built once).
static TOOLS_OPENAI: LazyLock<serde_json::Value> = LazyLock::new(|| {
    let tools = build_tools();
    serde_json::json!(tools.iter().map(|t| serde_json::json!({
        "type": "function",
        "function": {
            "name": t.name,
            "description": t.description,
            "parameters": t.input_schema,
        }
    })).collect::<Vec<_>>())
});

/// Tools included in the minimal set (for "simple" mode).
/// Excludes heavy scaffolding tools (subagents, task queuing, code intelligence, destructive ops).
const MINIMAL_TOOL_NAMES: &[&str] = &[
    "run_command", "read_file", "write_file", "append_file", "patch_file",
    "search_files", "list_directory", "scan_project", "browse_url", "web_search",
    "git_command", "update_memory", "update_user_model", "ask_user", "send_email",
    "send_file", "update_project_knowledge", "search_history",
];

fn build_minimal_tools() -> Vec<Tool> {
    build_tools()
        .into_iter()
        .filter(|t| MINIMAL_TOOL_NAMES.contains(&t.name.as_str()))
        .collect()
}

/// Minimal Anthropic-format tools array (for "simple" mode).
/// Last tool tagged with cache_control so Anthropic caches this smaller array too.
static TOOLS_MINIMAL_ANTHROPIC: LazyLock<serde_json::Value> = LazyLock::new(|| {
    let mut tools = serde_json::to_value(build_minimal_tools()).unwrap();
    if let Some(arr) = tools.as_array_mut() {
        if let Some(last) = arr.last_mut() {
            last["cache_control"] = serde_json::json!({"type": "ephemeral", "ttl": "1h"});
        }
    }
    tools
});

/// Minimal OpenAI-format functions array (for "simple" mode).
static TOOLS_MINIMAL_OPENAI: LazyLock<serde_json::Value> = LazyLock::new(|| {
    let tools = build_minimal_tools();
    serde_json::json!(tools.iter().map(|t| serde_json::json!({
        "type": "function",
        "function": {
            "name": t.name,
            "description": t.description,
            "parameters": t.input_schema,
        }
    })).collect::<Vec<_>>())
});

pub struct SonnetClient {
    anthropic_key: String,
    openai_key: String,
    model: String,
    provider: Provider,
    max_tokens: usize,
    http: Arc<reqwest::Client>,
}

impl SonnetClient {
    pub fn new(anthropic_key: &str, openai_key: &str, model: &str, explicit_provider: &str, max_tokens: usize, http: Arc<reqwest::Client>) -> Self {
        let provider = resolve_provider(model, explicit_provider);
        Self {
            anthropic_key: anthropic_key.to_string(),
            openai_key: openai_key.to_string(),
            model: model.to_string(),
            provider,
            max_tokens,
            http,
        }
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }

    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }
}

/// All tools the agent can use (built once, cached in statics above).
fn build_tools() -> Vec<Tool> {
        vec![
            Tool {
                name: "run_command".into(),
                description: "Execute a shell command. Returns stdout, stderr, exit code.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Bash command to run" }
                    },
                    "required": ["command"]
                }),
            },
            Tool {
                name: "read_file".into(),
                description: "Read a file from disk. For large files, use start_line/end_line. Set numbered=false to skip line numbers (saves tokens when browsing).".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path" },
                        "start_line": { "type": "integer", "description": "Start line (1-based, optional)" },
                        "end_line": { "type": "integer", "description": "End line (inclusive, optional)" },
                        "numbered": { "type": "boolean", "description": "Include line numbers (default: true). Set false to save tokens when browsing." }
                    },
                    "required": ["path"]
                }),
            },
            Tool {
                name: "write_file".into(),
                description: "Write content to a file (creates or overwrites). Runs a syntax check after writing for JS, Python, and JSON files.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path" },
                        "content": { "type": "string", "description": "File content" }
                    },
                    "required": ["path", "content"]
                }),
            },
            Tool {
                name: "append_file".into(),
                description: "Append content to a file without overwriting it. Useful for adding lines to configs, logs, CSS, or any file where you only need to add — not replace. Use `after` to insert after a specific marker line instead of at the end.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path (created if it doesn't exist)" },
                        "content": { "type": "string", "description": "Text to append" },
                        "after": { "type": "string", "description": "Optional: insert content after the first line that contains this string, instead of at the end of the file." }
                    },
                    "required": ["path", "content"]
                }),
            },
            Tool {
                name: "patch_file".into(),
                description: "Find and replace text in a file. Tries exact match first, then whitespace-normalised line comparison as a fallback (handles indentation differences). Supports line-range replacement and multi-occurrence.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path" },
                        "old_text": { "type": "string", "description": "Text to find. Not required when using start_line/end_line." },
                        "new_text": { "type": "string", "description": "Replacement text" },
                        "occurrence": { "type": ["integer", "string"], "description": "Which occurrence to replace: 1 (default), 2, 3 … or \"all\" to replace every match." },
                        "start_line": { "type": "integer", "description": "Start line (1-based, inclusive) for line-range replacement. Use with end_line instead of old_text." },
                        "end_line": { "type": "integer", "description": "End line (1-based, inclusive) for line-range replacement." }
                    },
                    "required": ["path", "new_text"]
                }),
            },
            Tool {
                name: "search_files".into(),
                description: "Search for a regex pattern across files. Returns matching lines with paths.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regex pattern" },
                        "path": { "type": "string", "description": "Directory or file to search in (default: current dir)" },
                        "include": { "type": "string", "description": "Glob filter for filenames (e.g. *.rs)" }
                    },
                    "required": ["pattern"]
                }),
            },
            Tool {
                name: "list_directory".into(),
                description: "List files and directories at a path.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Directory path (default: current dir)" }
                    },
                    "required": []
                }),
            },
            Tool {
                name: "scan_project".into(),
                description: "Build an annotated file-tree of a project directory. Returns a tree view with top-level symbols extracted from each source file (functions, classes, structs, etc.). Use this at the start of a coding task to understand what files exist and what they contain — much faster than calling list_directory + read_file on every file.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Root directory to scan (default: current working directory)" },
                        "max_depth": { "type": "integer", "description": "How many directory levels to traverse (default: 4, max recommended: 6)" }
                    },
                    "required": []
                }),
            },
            Tool {
                name: "browse_url".into(),
                description: "Fetch a URL and return its text content. Renders JavaScript via headless Chromium when available.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "URL to fetch" }
                    },
                    "required": ["url"]
                }),
            },
            Tool {
                name: "web_search".into(),
                description: "Search the web using DuckDuckGo. Returns titles, URLs and snippets for the top results. Use this to look up documentation, error messages, troubleshooting steps, or any information you don't have.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query" }
                    },
                    "required": ["query"]
                }),
            },
            Tool {
                name: "git_command".into(),
                description: "Run a git command (commit, diff, log, branch, status, etc).".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "args": { "type": "string", "description": "Git arguments, e.g. 'status --short' or 'commit -m \"msg\"'" }
                    },
                    "required": ["args"]
                }),
            },
            Tool {
                name: "task_manage".into(),
                description: "Manage persistent tasks. Actions: create, update_status, list.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["create", "update_status", "list"] },
                        "title": { "type": "string", "description": "Task title (for create)" },
                        "context": { "type": "string", "description": "Brief context (for create)" },
                        "task_id": { "type": "integer", "description": "Task ID (for update_status)" },
                        "status": { "type": "string", "enum": ["pending", "running", "done", "failed"], "description": "New status" }
                    },
                    "required": ["action"]
                }),
            },
            Tool {
                name: "update_memory".into(),
                description: "Save or update information in your persistent memory. Your memory survives across sessions and conversations. Use this to remember anything important: user preferences, email addresses, names, project details, decisions, or any fact the user tells you to remember. The [MEMORY] section in your system prompt shows your current memory contents. Sections are markdown headings.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["append", "replace"], "description": "'append' adds to a section, 'replace' overwrites it" },
                        "section": { "type": "string", "description": "Section name (e.g. 'User Info', 'Preferences', 'Projects')" },
                        "content": { "type": "string", "description": "Content to write (markdown)" }
                    },
                    "required": ["action", "section", "content"]
                }),
            },
            Tool {
                name: "ask_user".into(),
                description: "Ask the user a clarifying question or request confirmation before a risky action.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "question": { "type": "string", "description": "Question to ask" }
                    },
                    "required": ["question"]
                }),
            },
            Tool {
                name: "spawn_background".into(),
                description: "Start a long-running background process. Returns a handle to check later.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Command to run in background" },
                        "label": { "type": "string", "description": "Human-readable label" }
                    },
                    "required": ["command"]
                }),
            },
            Tool {
                name: "send_email".into(),
                description: "Send an email to someone. Just provide the recipient, subject, and body — the system handles delivery.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "to": { "type": "string", "description": "Recipient email address" },
                        "subject": { "type": "string", "description": "Email subject line" },
                        "body": { "type": "string", "description": "Email body content" },
                        "html": { "type": "boolean", "description": "If true, body is treated as HTML. Default: false" }
                    },
                    "required": ["to", "subject", "body"]
                }),
            },
            Tool {
                name: "send_file".into(),
                description: "Send a file to the user. The file will be delivered through whatever channel the user is connected on (browser download, Telegram, etc). Use this after creating or locating a file the user needs.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute or relative path to the file to send" },
                        "caption": { "type": "string", "description": "Brief description of the file (shown to user)" }
                    },
                    "required": ["path"]
                }),
            },
            Tool {
                name: "run_subagent".into(),
                description: "Delegate a self-contained sub-task to a fresh agent instance. The sub-agent starts with no conversation history or memory — only the task you provide. Use this for isolated, parallel-style work where you want a clean slate (e.g. research a topic, process a file, write a component). The result is returned as a string. Subagents cannot spawn further subagents.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "Complete task description for the sub-agent. Be specific — it has no context from this conversation." },
                        "model": { "type": "string", "enum": ["fast", "primary"], "description": "Model to use: 'fast' (default, uses continuation model — cheaper) or 'primary' (full primary model for complex reasoning)." }
                    },
                    "required": ["task"]
                }),
            },
            Tool {
                name: "plan_file_changes".into(),
                description: "Before modifying more than one file, call this tool to show the user exactly which files will be changed and how, then wait for explicit approval before proceeding. Use this any time you are about to touch 2+ files.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "changes": {
                            "type": "array",
                            "description": "List of planned file operations",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "file":        { "type": "string", "description": "File path" },
                                    "action":      { "type": "string", "enum": ["create", "modify", "delete"], "description": "Operation type" },
                                    "description": { "type": "string", "description": "What will be changed and why" }
                                },
                                "required": ["file", "action", "description"]
                            }
                        }
                    },
                    "required": ["changes"]
                }),
            },
            Tool {
                name: "update_project_knowledge".into(),
                description: "Persist important project facts to .axium/knowledge.md in the current project directory. Use this to store stack details, coding conventions, key decisions, important paths, recurring commands, or anything that should be remembered across sessions for this project.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "content": { "type": "string", "description": "The knowledge to save (markdown format)" },
                        "section": { "type": "string", "description": "Optional section heading (e.g. 'Stack', 'Conventions', 'Commands')" }
                    },
                    "required": ["content"]
                }),
            },
            Tool {
                name: "set_autonomous".into(),
                description: "Enable or disable autonomous mode for this session. When enabled, you will automatically continue working on the current task without waiting for the user to reply after each step. Use this when you have a clear multi-step plan and the user has asked you to work independently. Always disable it when blocked or needing input.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "enabled": { "type": "boolean", "description": "true to enable autonomous mode, false to disable" }
                    },
                    "required": ["enabled"]
                }),
            },
            Tool {
                name: "queue_task".into(),
                description: "Queue a task to run in the background while the user is away. The background worker will execute it autonomously using the full agent. Use this for long-running work so the user can disconnect and check back later.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "Short task title shown to user" },
                        "context": { "type": "string", "description": "Full task description with all context needed to complete it independently" }
                    },
                    "required": ["title", "context"]
                }),
            },
            Tool {
                name: "get_diagnostics".into(),
                description: "Run language-specific diagnostics (type errors, lint warnings, syntax errors) on a file or entire project. Returns structured error output. Use this before and after making changes to catch issues early.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path or project directory to check" }
                    },
                    "required": ["path"]
                }),
            },
            Tool {
                name: "delete_file".into(),
                description: "Delete a file or empty directory.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file or directory to delete" }
                    },
                    "required": ["path"]
                }),
            },
            Tool {
                name: "move_file".into(),
                description: "Move or rename a file or directory.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "source": { "type": "string", "description": "Current file path" },
                        "destination": { "type": "string", "description": "New file path" }
                    },
                    "required": ["source", "destination"]
                }),
            },
            Tool {
                name: "find_references".into(),
                description: "Find all occurrences of a symbol (function, variable, class, type) across the project. Returns file:line:context for each match. More targeted than search_files — designed for code symbols.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string", "description": "The symbol name to find" },
                        "path": { "type": "string", "description": "Project directory to search in (defaults to working directory)" }
                    },
                    "required": ["symbol"]
                }),
            },
            Tool {
                name: "rename_symbol".into(),
                description: "Rename a symbol (function, variable, class, type) across all files in the project. Finds all occurrences and replaces them atomically, then runs diagnostics to verify.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "old_name": { "type": "string", "description": "Current symbol name" },
                        "new_name": { "type": "string", "description": "New symbol name" },
                        "path": { "type": "string", "description": "Project directory to search in (defaults to working directory)" }
                    },
                    "required": ["old_name", "new_name"]
                }),
            },
            Tool {
                name: "get_dependency_graph".into(),
                description: "Show which files import a given file (dependents) and what a file imports (dependencies). Useful for understanding the impact radius of a change before modifying a heavily-used file.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path to analyze (relative to working directory or absolute)" },
                        "direction": {
                            "type": "string",
                            "enum": ["dependents", "dependencies", "both"],
                            "description": "dependents = who imports this file; dependencies = what this file imports; both = show both directions (default)"
                        }
                    },
                    "required": ["path"]
                }),
            },
            Tool {
                name: "rollback_changes".into(),
                description: "Discard all uncommitted file changes in the working directory, restoring to the last git commit. Use this when you realize your approach was wrong and want to start fresh. WARNING: this cannot be undone.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "reason": { "type": "string", "description": "Brief explanation of why you're rolling back" }
                    },
                    "required": ["reason"]
                }),
            },
            Tool {
                name: "search_history".into(),
                description: "Full-text search over past conversation history using the local FTS5 index. Use this to recall what was discussed, decided, or worked on in previous sessions. Returns matching messages with their session context.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search terms or phrase to find in conversation history" },
                        "limit": { "type": "integer", "description": "Maximum number of results to return (default: 10, max: 30)" }
                    },
                    "required": ["query"]
                }),
            },
            Tool {
                name: "update_user_model".into(),
                description: "Proactively update your persistent model of the user: communication style, expertise level, recurring interests, preferences, and inferred patterns. Call this at the end of a session when you've learned something meaningful about the user — no explicit user instruction needed. Be concise and specific.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "observation": { "type": "string", "description": "A concise, specific inference about the user (e.g. 'prefers terse answers', 'deep Rust expertise', 'works on embedded systems')" }
                    },
                    "required": ["observation"]
                }),
            },
        ]
}

/// Build the Anthropic system prompt array, splitting into a stable (soul) block with
/// cache_control and a dynamic block (memory/tasks) without caching.
/// Soul rarely changes → gets cached. Memory/tasks change every turn → not cached.
/// Falls back to a single block if either part is empty (Anthropic rejects empty text blocks).
///
/// Uses 1-hour cache TTL for system blocks — they change infrequently and should survive
/// long tool loops. Message breakpoints use the default 5-minute TTL (set elsewhere).
/// Critical: 1-hour entries MUST appear before 5-minute entries in the request (Anthropic rule).
fn build_system_blocks(system: &str) -> serde_json::Value {
    const MARKER: &str = "\n\n[MEMORY]\n";
    if let Some(idx) = system.find(MARKER) {
        let soul = &system[..idx];
        let dynamic = &system[idx + MARKER.len()..];
        if !soul.is_empty() && !dynamic.trim().is_empty() {
            return serde_json::json!([
                {"type": "text", "text": soul, "cache_control": {"type": "ephemeral", "ttl": "1h"}},
                {"type": "text", "text": dynamic, "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ]);
        }
    }
    serde_json::json!([{"type": "text", "text": system, "cache_control": {"type": "ephemeral", "ttl": "1h"}}])
}

impl SonnetClient {
    /// Return the tools array for a given mode.
    /// "simple" mode gets the minimal set; everything else gets the full set.
    fn tools_for_mode_anthropic(mode: &str) -> &'static serde_json::Value {
        if mode == "simple" { &TOOLS_MINIMAL_ANTHROPIC } else { &TOOLS_ANTHROPIC }
    }

    fn tools_for_mode_openai(mode: &str) -> &'static serde_json::Value {
        if mode == "simple" { &TOOLS_MINIMAL_OPENAI } else { &TOOLS_OPENAI }
    }

    /// Make a streaming API call. Sends text deltas through `delta_tx` as they arrive.
    /// Returns the fully assembled response (identical format to non-streaming `call()`).
    ///
    /// When `force_tool` is true, sets `tool_choice` to require the model to produce at
    /// least one tool call instead of a text-only response.
    ///
    /// `effort` controls Anthropic extended thinking: "off"/"" = disabled,
    /// "low"/"medium"/"high"/"max" = adaptive thinking at that intensity.
    ///
    /// `mode` selects the tool subset: "simple" uses the minimal set (~18 tools);
    /// all other modes use the full set (~31 tools), saving ~4 k tokens per call in simple mode.
    ///
    /// For Anthropic: tags the last message with `cache_control` so the entire conversation
    /// prefix gets cached across tool-loop iterations (up to 90% savings on repeated prefixes).
    pub async fn call_streaming(
        &self,
        system: &str,
        messages: &mut Vec<serde_json::Value>,
        delta_tx: &mpsc::UnboundedSender<String>,
        force_tool: bool,
        effort: &str,
        mode: &str,
    ) -> Result<serde_json::Value> {
        match self.provider {
            Provider::Anthropic => {
                tag_last_message_for_cache(messages);
                let result = self.call_anthropic_streaming(system, messages, delta_tx, force_tool, effort, mode).await;
                untag_last_message_cache(messages);
                result
            }
            Provider::OpenAI => self.call_openai_streaming(system, messages, delta_tx, force_tool, mode).await,
        }
    }

    // ── Streaming implementations ──────────────────────────────────────

    async fn call_anthropic_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        delta_tx: &mpsc::UnboundedSender<String>,
        force_tool: bool,
        effort: &str,
        mode: &str,
    ) -> Result<serde_json::Value> {
        let thinking_enabled = matches!(effort, "low" | "medium" | "high" | "max");
        let max_tokens = if thinking_enabled {
            // Adaptive thinking needs headroom for both thinking + output
            self.max_tokens.max(16384)
        } else {
            self.max_tokens
        };
        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "system": build_system_blocks(system),
            "messages": messages,
            "tools": Self::tools_for_mode_anthropic(mode),
            "stream": true,
        });
        if thinking_enabled {
            body["thinking"] = serde_json::json!({"type": "adaptive"});
            body["output_config"] = serde_json::json!({"effort": effort});
        }
        if force_tool {
            body["tool_choice"] = serde_json::json!({"type": "any"});
        }

        let resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.anthropic_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "prompt-caching-2024-07-31")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to reach Anthropic API")?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp.headers().get("retry-after")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let text = resp.text().await?;
            anyhow::bail!("Anthropic API error ({}): {} retry-after:{}", status, text, retry_after);
        }

        let mut content_blocks: Vec<serde_json::Value> = Vec::new();
        let mut stop_reason = "end_turn".to_string();
        let mut current_text = String::new();
        let mut current_thinking = String::new();
        let mut current_tool_json = String::new();
        let mut current_tool_id = String::new();
        let mut current_tool_name = String::new();
        let mut in_tool_block = false;
        let mut in_thinking_block = false;
        let mut usage = ApiUsage::default();

        let mut stream = resp.bytes_stream();
        let mut raw_buf: Vec<u8> = Vec::new();
        const MAX_BUF: usize = 16 * 1024 * 1024;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Stream read error")?;
            raw_buf.extend_from_slice(&chunk);
            if raw_buf.len() > MAX_BUF {
                anyhow::bail!("Anthropic stream buffer exceeded 16 MB — aborting");
            }

            // Process complete lines from the byte buffer
            let mut start = 0;
            'inner: loop {
                match raw_buf[start..].iter().position(|&b| b == b'\n') {
                    None => break 'inner,
                    Some(rel_pos) => {
                        let newline_abs = start + rel_pos;
                        let line_bytes = &raw_buf[start..newline_abs];
                        start = newline_abs + 1;

                        let line = match std::str::from_utf8(line_bytes) {
                            Ok(s) => s.trim_end_matches('\r'),
                            Err(_) => continue, // skip malformed line
                        };

                if line.is_empty() || line.starts_with("event:") {
                    continue;
                }
                if !line.starts_with("data: ") {
                    continue;
                }

                let data = &line[6..];
                let event: serde_json::Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                match event["type"].as_str() {
                    Some("message_start") => {
                        if let Some(u) = event["message"]["usage"].as_object() {
                            usage.input_tokens = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                            usage.cache_creation_tokens = u.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                            usage.cache_read_tokens = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        }
                    }
                    Some("content_block_start") => {
                        let block = &event["content_block"];
                        match block["type"].as_str() {
                            Some("thinking") => {
                                current_thinking.clear();
                                in_thinking_block = true;
                                in_tool_block = false;
                            }
                            Some("text") => {
                                current_text.clear();
                                in_tool_block = false;
                                in_thinking_block = false;
                            }
                            Some("tool_use") => {
                                current_tool_id = block["id"].as_str().unwrap_or("").to_string();
                                current_tool_name = block["name"].as_str().unwrap_or("").to_string();
                                current_tool_json.clear();
                                in_tool_block = true;
                                in_thinking_block = false;
                            }
                            _ => {}
                        }
                    }
                    Some("content_block_delta") => {
                        let delta = &event["delta"];
                        match delta["type"].as_str() {
                            Some("thinking_delta") => {
                                if let Some(text) = delta["thinking"].as_str() {
                                    current_thinking.push_str(text);
                                    // Stream thinking to UI wrapped in tags so the frontend can style it
                                    let _ = delta_tx.send(format!("<think>{}</think>", text));
                                }
                            }
                            Some("text_delta") => {
                                if let Some(text) = delta["text"].as_str() {
                                    current_text.push_str(text);
                                    let _ = delta_tx.send(text.to_string());
                                }
                            }
                            Some("input_json_delta") => {
                                if let Some(json) = delta["partial_json"].as_str() {
                                    current_tool_json.push_str(json);
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("content_block_stop") => {
                        if in_thinking_block && !current_thinking.is_empty() {
                            // Include thinking blocks in the response so they can be passed
                            // back on the next tool-loop iteration (interleaved thinking).
                            content_blocks.push(serde_json::json!({
                                "type": "thinking",
                                "thinking": current_thinking.clone()
                            }));
                            current_thinking.clear();
                            in_thinking_block = false;
                        }
                        if !in_tool_block && !in_thinking_block && !current_text.is_empty() {
                            content_blocks.push(serde_json::json!({
                                "type": "text",
                                "text": current_text.clone()
                            }));
                            current_text.clear();
                        }
                        if in_tool_block && !current_tool_id.is_empty() {
                            let input: serde_json::Value = serde_json::from_str(&current_tool_json)
                                .unwrap_or(serde_json::json!({}));
                            content_blocks.push(serde_json::json!({
                                "type": "tool_use",
                                "id": current_tool_id,
                                "name": current_tool_name,
                                "input": input
                            }));
                            current_tool_id.clear();
                            current_tool_name.clear();
                            current_tool_json.clear();
                            in_tool_block = false;
                        }
                    }
                    Some("message_delta") => {
                        if let Some(sr) = event["delta"]["stop_reason"].as_str() {
                            stop_reason = sr.to_string();
                        }
                        if let Some(out) = event["usage"]["output_tokens"].as_u64() {
                            usage.output_tokens = out;
                        }
                    }
                    _ => {}
                }
                    } // end Some(rel_pos)
                } // end match
            } // end 'inner loop
            raw_buf.drain(..start);
        }

        Ok(serde_json::json!({
            "content": content_blocks,
            "stop_reason": stop_reason,
            "usage": {
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cache_creation_input_tokens": usage.cache_creation_tokens,
                "cache_read_input_tokens": usage.cache_read_tokens,
            }
        }))
    }

    async fn call_openai_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        delta_tx: &mpsc::UnboundedSender<String>,
        force_tool: bool,
        mode: &str,
    ) -> Result<serde_json::Value> {
        let mut oai_msgs = vec![serde_json::json!({"role": "system", "content": system})];
        for m in messages {
            oai_msgs.extend(anthropic_msg_to_openai(m));
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": oai_msgs,
            "tools": Self::tools_for_mode_openai(mode),
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if force_tool {
            body["tool_choice"] = serde_json::json!("required");
        }

        let resp = self
            .http
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.openai_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to reach OpenAI API")?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp.headers().get("retry-after")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let text = resp.text().await?;
            anyhow::bail!("OpenAI API error ({}): {} retry-after:{}", status, text, retry_after);
        }

        let mut content_text = String::new();
        let mut finish_reason = "stop".to_string();
        let mut tool_call_map: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
        let mut usage = ApiUsage::default();

        let mut stream = resp.bytes_stream();
        let mut raw_buf: Vec<u8> = Vec::new();
        const MAX_BUF: usize = 16 * 1024 * 1024;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Stream read error")?;
            raw_buf.extend_from_slice(&chunk);
            if raw_buf.len() > MAX_BUF {
                anyhow::bail!("OpenAI stream buffer exceeded 16 MB — aborting");
            }

            // Process complete lines from byte buffer
            let mut start = 0;
            'inner: loop {
                match raw_buf[start..].iter().position(|&b| b == b'\n') {
                    None => break 'inner,
                    Some(rel_pos) => {
                        let newline_abs = start + rel_pos;
                        let line_bytes = &raw_buf[start..newline_abs];
                        start = newline_abs + 1;

                        let line = match std::str::from_utf8(line_bytes) {
                            Ok(s) => s.trim_end_matches('\r'),
                            Err(_) => continue, // skip malformed line
                        };

                if line.is_empty() {
                    continue;
                }
                if !line.starts_with("data: ") {
                    continue;
                }

                let data = &line[6..];
                if data == "[DONE]" {
                    break 'inner;
                }

                let event: serde_json::Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let choice = &event["choices"][0];
                let delta = &choice["delta"];

                if let Some(fr) = choice["finish_reason"].as_str() {
                    finish_reason = fr.to_string();
                }

                // OpenAI sends usage in the final chunk when stream_options.include_usage is set
                if let Some(u) = event["usage"].as_object() {
                    usage.input_tokens = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    usage.output_tokens = u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                }

                if let Some(text) = delta["content"].as_str() {
                    content_text.push_str(text);
                    let _ = delta_tx.send(text.to_string());
                }

                if let Some(tcs) = delta["tool_calls"].as_array() {
                    for tc in tcs {
                        let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                        let entry = tool_call_map
                            .entry(idx)
                            .or_insert_with(|| (String::new(), String::new(), String::new()));
                        if let Some(id) = tc["id"].as_str() {
                            entry.0 = id.to_string();
                        }
                        if let Some(name) = tc["function"]["name"].as_str() {
                            entry.1 = name.to_string();
                        }
                        if let Some(args) = tc["function"]["arguments"].as_str() {
                            entry.2.push_str(args);
                        }
                    }
                }
                    } // end Some(rel_pos)
                } // end match
            } // end 'inner loop
            raw_buf.drain(..start);
        }

        // Build response in Anthropic-compatible format
        let mut content_blocks: Vec<serde_json::Value> = Vec::new();
        if !content_text.is_empty() {
            content_blocks.push(serde_json::json!({
                "type": "text",
                "text": content_text
            }));
        }

        for (_idx, (id, name, args)) in &tool_call_map {
            let input: serde_json::Value =
                serde_json::from_str(args).unwrap_or(serde_json::json!({}));
            content_blocks.push(serde_json::json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input
            }));
        }

        let stop_reason = match finish_reason.as_str() {
            "tool_calls" => "tool_use",
            "length" => "max_tokens",
            "content_filter" => {
                tracing::warn!("OpenAI content filter triggered");
                "end_turn"
            }
            _ => "end_turn",
        };

        Ok(serde_json::json!({
            "content": content_blocks,
            "stop_reason": stop_reason,
            "usage": {
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cache_creation_input_tokens": usage.cache_creation_tokens,
                "cache_read_input_tokens": usage.cache_read_tokens,
            }
        }))
    }
}

/// Tag the last message in the array with `cache_control` so Anthropic caches the entire
/// conversation prefix. Within a tool loop, iterations 2-N get ~90% cache hits on the prefix.
/// Uses 1 of Anthropic's 4 allowed cache breakpoints.
fn tag_last_message_for_cache(messages: &mut [serde_json::Value]) {
    let Some(last) = messages.last_mut() else { return };
    if let Some(content_str) = last["content"].as_str().map(|s| s.to_string()) {
        // Convert string content to block format with cache_control
        last["content"] = serde_json::json!([{
            "type": "text",
            "text": content_str,
            "cache_control": {"type": "ephemeral"}
        }]);
    } else if let Some(arr) = last["content"].as_array_mut() {
        // Content is already block format (e.g. tool_result blocks) — tag the last block
        if let Some(last_block) = arr.last_mut() {
            last_block["cache_control"] = serde_json::json!({"type": "ephemeral"});
        }
    }
}

/// Remove the cache_control tag added by `tag_last_message_for_cache` so the breakpoint
/// can move forward on the next iteration.
fn untag_last_message_cache(messages: &mut [serde_json::Value]) {
    let Some(last) = messages.last_mut() else { return };
    if let Some(arr) = last["content"].as_array_mut() {
        // If we created a single-element text array, convert back to plain string
        if arr.len() == 1 && arr[0]["type"].as_str() == Some("text") {
            if arr[0].get("cache_control").is_some() {
                if let Some(text) = arr[0]["text"].as_str().map(|s| s.to_string()) {
                    last["content"] = serde_json::json!(text);
                    return;
                }
            }
        }
        // Otherwise just strip cache_control from the last block
        if let Some(last_block) = arr.last_mut() {
            if let Some(obj) = last_block.as_object_mut() {
                obj.remove("cache_control");
            }
        }
    }
}

/// Convert an Anthropic-format message to OpenAI-format message(s).
///
/// Anthropic assistant messages may contain `tool_use` blocks in `content`,
/// which OpenAI expects as `tool_calls` on the assistant message.
/// Anthropic user messages may contain `tool_result` blocks,
/// which OpenAI expects as separate `role: "tool"` messages.
fn anthropic_msg_to_openai(msg: &serde_json::Value) -> Vec<serde_json::Value> {
    let role = msg["role"].as_str().unwrap_or("user");

    // Simple string content — pass through
    if msg["content"].is_string() {
        return vec![msg.clone()];
    }

    let content_arr = match msg["content"].as_array() {
        Some(arr) => arr,
        None => return vec![msg.clone()],
    };

    if role == "assistant" {
        // Collect text blocks and tool_use blocks
        let mut text_parts = String::new();
        let mut tool_calls: Vec<serde_json::Value> = Vec::new();
        for block in content_arr {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(t) = block["text"].as_str() {
                        text_parts.push_str(t);
                    }
                }
                Some("tool_use") => {
                    let id = block["id"].as_str().unwrap_or("");
                    let name = block["name"].as_str().unwrap_or("");
                    let input = &block["input"];
                    tool_calls.push(serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": input.to_string(),
                        }
                    }));
                }
                _ => {}
            }
        }

        let mut msg_out = serde_json::json!({ "role": "assistant" });
        if !text_parts.is_empty() {
            msg_out["content"] = serde_json::json!(text_parts);
        } else if !tool_calls.is_empty() {
            // OpenAI requires content: null when only tool_calls are present
            msg_out["content"] = serde_json::Value::Null;
        }
        if !tool_calls.is_empty() {
            msg_out["tool_calls"] = serde_json::json!(tool_calls);
        }
        vec![msg_out]
    } else {
        // User messages: split tool_result blocks into separate "tool" messages,
        // and keep text blocks as a regular user message
        let mut out = Vec::new();
        let mut text_parts = String::new();

        for block in content_arr {
            match block["type"].as_str() {
                Some("tool_result") => {
                    let tool_call_id = block["tool_use_id"].as_str().unwrap_or("");
                    let content = block["content"].as_str().unwrap_or("");
                    out.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": tool_call_id,
                        "content": content,
                    }));
                }
                Some("text") => {
                    if let Some(t) = block["text"].as_str() {
                        text_parts.push_str(t);
                    }
                }
                _ => {
                    // Fallback: treat as text
                    if let Some(t) = block["content"].as_str() {
                        text_parts.push_str(t);
                    }
                }
            }
        }

        if !text_parts.is_empty() {
            out.push(serde_json::json!({
                "role": "user",
                "content": text_parts,
            }));
        }

        if out.is_empty() {
            // Shouldn't happen, but fallback to pass-through
            vec![msg.clone()]
        } else {
            out
        }
    }
}
