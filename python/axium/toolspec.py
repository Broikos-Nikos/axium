"""Tool schemas, in a provider-neutral shape.

`providers.tools_anthropic` / `tools_openai` convert these at call time. Names and
descriptions deliberately match the Rust build so benchmark numbers are comparable
between the two implementations.
"""


def _t(name, description, properties, required=()):
    return {"name": name, "description": description,
            "input_schema": {"type": "object", "properties": properties,
                             "required": list(required)}}


TOOLS = [
    _t("run_command", "Execute a shell command. Returns stdout, stderr and exit code.",
       {"command": {"type": "string", "description": "Shell command to run"}},
       ["command"]),

    _t("read_file",
       "Read a file from disk. For large files use start_line/end_line. "
       "Set numbered=false to omit line numbers and save tokens when browsing.",
       {"path": {"type": "string", "description": "File path"},
        "start_line": {"type": "integer", "description": "Start line, 1-based (optional)"},
        "end_line": {"type": "integer", "description": "End line, inclusive (optional)"},
        "numbered": {"type": "boolean", "description": "Include line numbers (default true)"}},
       ["path"]),

    _t("write_file",
       "Write content to a file, creating or overwriting it. Runs a syntax check "
       "afterwards for Python, JSON and PHP files. Refuses to overwrite an existing "
       "file when the new content would drop definitions it already has: use "
       "patch_file or append_file to change part of a file, or pass replace=true "
       "when the whole file really should go.",
       {"path": {"type": "string"}, "content": {"type": "string"},
        "replace": {"type": "boolean",
                    "description": "Allow a full rewrite that removes existing "
                                   "definitions"}},
       ["path", "content"]),

    _t("append_file",
       "Append text to a file without overwriting it. Use `after` to insert "
       "immediately below the first line containing that marker instead.",
       {"path": {"type": "string"}, "content": {"type": "string"},
        "after": {"type": "string", "description": "Insert after the first line containing this text"}},
       ["path", "content"]),

    _t("patch_file",
       "Find and replace text in a file. Tries an exact match first, then a "
       "whitespace-normalised line match (handles indentation drift). Supports "
       "line-range replacement via start_line/end_line and multi-occurrence via "
       "occurrence.",
       {"path": {"type": "string"},
        "old_text": {"type": "string", "description": "Text to find. Not needed with start_line/end_line."},
        "new_text": {"type": "string", "description": "Replacement text"},
        "occurrence": {"type": ["integer", "string"],
                       "description": "Which match to replace: 1 (default), 2, 3 ... or \"all\""},
        "start_line": {"type": "integer"}, "end_line": {"type": "integer"}},
       ["path", "new_text"]),

    _t("search_files",
       "Search for a regex pattern across files. Returns matching lines with paths.",
       {"pattern": {"type": "string"},
        "path": {"type": "string", "description": "Directory or file to search (default: working dir)"},
        "include": {"type": "string", "description": "Filename glob filter, e.g. *.py"}},
       ["pattern"]),

    _t("list_directory", "List files and directories at a path.",
       {"path": {"type": "string"}}),

    _t("scan_project",
       "Build an annotated file tree of a project: each source file is listed with "
       "the top-level symbols it defines. Much cheaper than listing then reading "
       "every file — use this first on an unfamiliar codebase.",
       {"path": {"type": "string"}, "max_depth": {"type": "integer", "description": "Default 4"}}),

    _t("git_command", "Run a git command (status, diff, log, add, commit, ...).",
       {"args": {"type": "string", "description": "Git arguments, e.g. 'status --short'"}},
       ["args"]),

    _t("delete_file", "Delete a file or an empty directory.",
       {"path": {"type": "string"}}, ["path"]),

    _t("move_file", "Move or rename a file or directory.",
       {"source": {"type": "string"}, "destination": {"type": "string"}},
       ["source", "destination"]),

    _t("find_references",
       "Find every occurrence of a symbol (function, class, variable, constant) "
       "across the project. Returns file:line:context. More targeted than search_files.",
       {"symbol": {"type": "string"}, "path": {"type": "string"}},
       ["symbol"]),

    _t("get_diagnostics",
       "Run language-specific diagnostics (syntax and type errors) on a file or "
       "directory. Use before and after edits to catch breakage early.",
       {"path": {"type": "string"}}, ["path"]),

    _t("get_dependency_graph",
       "Show which files import a given file (dependents) and what it imports "
       "(dependencies) — the impact radius of changing it.",
       {"path": {"type": "string"},
        "direction": {"type": "string", "enum": ["dependents", "dependencies", "both"]}},
       ["path"]),

    _t("update_memory",
       "Save or update a section of your persistent memory, which survives across "
       "sessions. Use it for durable facts: user details, project conventions, "
       "decisions. Not for one-off task state.",
       {"action": {"type": "string", "enum": ["append", "replace"]},
        "section": {"type": "string", "description": "Section heading, e.g. 'User Info'"},
        "content": {"type": "string"}},
       ["action", "section", "content"]),

    _t("search_history",
       "Full-text search over past conversations (local SQLite FTS index). Use it "
       "to recall what was discussed or decided in earlier sessions.",
       {"query": {"type": "string"}, "limit": {"type": "integer", "description": "Default 10, max 30"}},
       ["query"]),

    _t("update_project_knowledge",
       "Persist project facts to .axium/knowledge.md in the working directory: "
       "stack, conventions, key paths, recurring commands.",
       {"content": {"type": "string"}, "section": {"type": "string"}},
       ["content"]),

    _t("task_manage", "Manage persistent background tasks.",
       {"action": {"type": "string", "enum": ["create", "update_status", "list"]},
        "title": {"type": "string"}, "context": {"type": "string"},
        "task_id": {"type": "integer"},
        "status": {"type": "string", "enum": ["pending", "running", "done", "failed"]}},
       ["action"]),

    _t("ask_user", "Ask the user a clarifying question or confirm a risky action.",
       {"question": {"type": "string"}}, ["question"]),

    _t("run_subagent",
       "Delegate a self-contained sub-task to a fresh agent with no conversation "
       "history. Give it everything it needs — it has no context from this chat. "
       "Sub-agents cannot spawn further sub-agents.",
       {"task": {"type": "string"},
        "model": {"type": "string", "enum": ["fast", "primary"],
                  "description": "'fast' (default, continuation model) or 'primary'"}},
       ["task"]),

    _t("set_autonomous",
       "Turn autonomous mode on or off for this session. When on, you keep working "
       "through a multi-step plan without waiting for the user between steps.",
       {"enabled": {"type": "boolean"}}, ["enabled"]),

    _t("undo_turn",
       "Revert file changes from an earlier turn using the snapshots taken before "
       "each write: edited files are restored byte-for-byte and files that turn "
       "created are removed. Use this when asked to put something back or undo "
       "what you just did — it is exact, unlike rewriting the files from memory. "
       "action='list' shows what can be undone.",
       {"action": {"type": "string", "enum": ["undo", "list"],
                   "description": "Default 'undo'"},
        "checkpoint_id": {"type": "string",
                          "description": "A specific checkpoint from action='list'. "
                                         "Omit for the most recent."}}),

    _t("remember_fact",
       "Store one durable fact with a type and an importance. Use it for a rule, "
       "threshold, convention or decision that must still hold many turns from "
       "now. Facts are shown to you at the top of every turn and survive "
       "compaction, unlike anything said in the conversation.",
       {"value": {"type": "string",
                  "description": "The fact as one self-contained sentence, with "
                                 "any number stated verbatim"},
        "type": {"type": "string",
                 "enum": ["rule", "convention", "decision", "preference",
                          "gotcha", "reference", "note"]},
        "key": {"type": "string",
                "description": "Stable dotted id for dedup, e.g. shipping.free_threshold"},
        "importance": {"type": "number",
                       "description": "0.0-1.0. A hard rule or a user correction: 0.9"}},
       ["value"]),

    _t("recall",
       "Search your durable facts for a rule, threshold or decision. Faster and "
       "more precise than search_history. Omit the query to list the most "
       "important facts.",
       {"query": {"type": "string"}, "limit": {"type": "integer",
                                               "description": "Default 10, max 30"}}),

    _t("learn_project",
       "Rebuild the Project Brain in .axium/: refresh the annotated overview and "
       "optionally write PROFILE.md. Run this once on an unfamiliar project so "
       "later sessions start oriented instead of re-exploring.",
       {"profile": {"type": "string",
                    "description": "Optional PROFILE.md body: stack, entry points, "
                                   "key files, conventions, deploy target"}}),
]

# "simple" mode drops the heavy scaffolding tools, saving roughly 2k prompt tokens
# on every call of the loop.
MINIMAL_TOOL_NAMES = {
    "run_command", "read_file", "write_file", "append_file", "patch_file",
    "search_files", "list_directory", "scan_project", "git_command",
    "update_memory", "ask_user", "search_history",
    # undo_turn earns its schema even in the minimal set: reverting a turn from
    # snapshots is one call, and reconstructing the same files by hand is dozens.
    "undo_turn", "recall",
}

TOOLS_BY_NAME = {t["name"]: t for t in TOOLS}
MINIMAL_TOOLS = [t for t in TOOLS if t["name"] in MINIMAL_TOOL_NAMES]


def tools_for_mode(mode):
    return MINIMAL_TOOLS if mode == "simple" else TOOLS
