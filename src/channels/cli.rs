use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{info, warn, error};

use crate::agent::classifier::Classifier;
use crate::agent::compactor::Compactor;
use crate::agent::router::{self, AgentEvent, TurnConfig};
use crate::agent::sonnet::SonnetClient;
use crate::agent::Message;
use crate::memory::store::Memory;
use crate::tui::server::AppState;

/// Filter streaming chunks, hiding `<think>…</think>` blocks.
/// Maintains state across chunk boundaries via `in_think`, `buf`, `post_think`.
fn filter_think(chunk: &str, in_think: &mut bool, buf: &mut String, post_think: &mut bool) -> String {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    buf.push_str(chunk);
    let mut out = String::new();
    loop {
        if *in_think {
            match buf.find(CLOSE) {
                Some(pos) => {
                    *in_think = false;
                    *post_think = true;
                    buf.drain(..pos + CLOSE.len());
                }
                None => {
                    if buf.len() > CLOSE.len() - 1 {
                        buf.drain(..buf.len() - (CLOSE.len() - 1));
                    }
                    break;
                }
            }
        } else {
            match buf.find(OPEN) {
                Some(pos) => {
                    if pos > 0 {
                        if *post_think { out.push_str("\n\n"); *post_think = false; }
                        out.push_str(&buf[..pos]);
                    }
                    *in_think = true;
                    buf.drain(..pos + OPEN.len());
                }
                None => {
                    let safe = if buf.len() >= OPEN.len() { buf.len() - (OPEN.len() - 1) } else { 0 };
                    if safe > 0 {
                        if *post_think { out.push_str("\n\n"); *post_think = false; }
                        out.push_str(&buf[..safe]);
                        buf.drain(..safe);
                    }
                    break;
                }
            }
        }
    }
    out
}

/// Strip all `<think>…</think>` blocks from completed text (for DB storage).
fn strip_think(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        match rest.find("<think>") {
            None => { out.push_str(rest); break; }
            Some(start) => {
                out.push_str(&rest[..start]);
                let after_open = &rest[start + "<think>".len()..];
                match after_open.find("</think>") {
                    None => break,
                    Some(end) => { rest = &after_open[end + "</think>".len()..]; }
                }
            }
        }
    }
    out.trim_start_matches('\n').to_string()
}

/// Run an interactive CLI REPL that talks to the agent via stdin/stdout.
pub async fn run(state: Arc<AppState>) {
    let session_id = match state.chat_db.find_or_create_session("cli") {
        Ok(id) => id,
        Err(e) => {
            error!(error = %e, "Failed to create CLI session");
            return;
        }
    };
    if state.chat_db.get_session_title(&session_id).is_empty() {
        let _ = state.chat_db.update_session_title(&session_id, "CLI");
    }

    let stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut lines = stdin.lines();

    println!("CLI mode — type a message and press Enter. /new to reset, /quit to exit.");

    loop {
        let _ = stdout.write_all(b"\n> ").await;
        let _ = stdout.flush().await;

        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break, // EOF
            Err(e) => {
                error!(error = %e, "Failed to read stdin");
                break;
            }
        };
        let text = line.trim();
        if text.is_empty() { continue; }

        match text {
            "/quit" | "/exit" => break,
            "/new" => {
                let _ = state.chat_db.clear_session_messages(&session_id);
                println!("Session cleared.");
                continue;
            }
            _ => {}
        }

        // Load history (cap at last 50 messages)
        let all_msgs = state.chat_db.load_session_messages(&session_id).unwrap_or_default();
        let skip = all_msgs.len().saturating_sub(50);
        let mut history: Vec<Message> = all_msgs
            .into_iter()
            .skip(skip)
            .filter(|m| m.role != "system" && !m.content.starts_with("[partial] "))
            .map(|m| Message { role: m.role, content: m.content })
            .collect();
        history.push(Message::user(text));

        // Build agent components from config
        let sudo_pw = state.sudo_password.read().await.clone();
        let sudo_note = if !sudo_pw.is_empty() {
            "\n\n## Sudo Access\nA sudo password is configured. When commands need elevated privileges, use `sudo` in run_command — the password is injected automatically and transparently. NEVER ask the user for their password."
        } else { "" };
        let (sonnet, compactor, classifier, soul, turn_cfg, project_ctx, memory_file) = {
            let cfg = state.config.read().await;
            let wd = &cfg.settings.working_directory;
            let resolved_wd = if wd.is_empty() || wd == "~" {
                std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
            } else if wd.starts_with("~/") {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{}/{}", home, &wd[2..])
            } else {
                wd.clone()
            };
            let ctx = crate::tui::server::get_project_context(&state, &resolved_wd).await;
            (
                SonnetClient::new(
                    &cfg.api_keys.anthropic, &cfg.api_keys.openai,
                    &cfg.models.primary, &cfg.models.primary_provider,
                    cfg.settings.max_tokens, Arc::clone(&state.http),
                ),
                Compactor::new(
                    &cfg.api_keys.anthropic, &cfg.api_keys.openai,
                    &cfg.models.compactor, &cfg.models.compactor_provider,
                    Arc::clone(&state.http),
                ),
                Classifier::new(
                    &cfg.api_keys.anthropic, &cfg.api_keys.openai,
                    &cfg.models.classifier, &cfg.models.classifier_provider,
                    Arc::clone(&state.http),
                ),
                crate::config::loader::load_soul(&cfg.agent.soul),
                TurnConfig {
                    token_limit: cfg.settings.token_limit,
                    terminal_timeout: cfg.settings.terminal_timeout_secs,
                    max_output_chars: cfg.settings.max_output_chars,
                    max_tool_iterations: cfg.settings.max_tool_iterations,
                    max_retries: cfg.settings.max_retries,
                    sudo_password: sudo_pw,
                    working_directory: resolved_wd.clone(),
                    smtp_host: cfg.settings.smtp_host.clone(),
                    smtp_port: cfg.settings.smtp_port,
                    smtp_user: cfg.settings.smtp_user.clone(),
                    smtp_password: cfg.settings.smtp_password.clone(),
                    smtp_from: cfg.settings.smtp_from.clone(),
                    telegram_bot_token: cfg.settings.telegram_bot_token.clone(),
                    conversation_logging: cfg.settings.conversation_logging,
                    http: Arc::clone(&state.http),
                    anthropic_key: cfg.api_keys.anthropic.clone(),
                    openai_key: cfg.api_keys.openai.clone(),
                    primary_model: cfg.models.primary.clone(),
                    primary_provider: cfg.models.primary_provider.clone(),
                    subagent_depth: 0,
                    continuation_model: cfg.models.continuation.clone(),
                    continuation_provider: cfg.models.continuation_provider.clone(),
                    classifier_model: cfg.models.classifier.clone(),
                    classifier_provider: cfg.models.classifier_provider.clone(),
                    review_model: cfg.models.review.clone(),
                    review_provider: cfg.models.review_provider.clone(),
                    compactor_model: cfg.models.compactor.clone(),
                    compactor_provider: cfg.models.compactor_provider.clone(),
                    mode: "supercharge".to_string(),
                    plugin_manager: Some(Arc::clone(&state.plugin_manager)),
                    compaction_threshold: cfg.settings.compaction_threshold,
                    thinking_effort: cfg.settings.thinking_effort.clone(),
                    fallback_model: cfg.models.fallback.clone(),
                    fallback_provider: cfg.models.fallback_provider.clone(),
                    conv_logger: None,
                    chat_db: Arc::clone(&state.chat_db),
                },
                ctx,
                cfg.settings.memory_file.clone(),
            )
        };

        let soul_with_planning = format!(
            "{}\n\n[WORKING DIRECTORY]\n{}\n\n[INSTRUCTIONS]\n\
            Before taking any action, briefly outline your plan (2-3 lines).\n\
            If a tool fails, analyze the error and try a different approach.\n\
            Use tools in parallel when they are independent.\n\
            Confirm with the user before destructive operations (rm, overwrite, drop).\n\
            Never re-do work that already succeeded in this conversation. If the user follows up, check prior assistant messages and continue from where you left off.\n\n\
            ## Persistent Memory\n\
            You have a PERSISTENT MEMORY that survives across all sessions and conversations. \
            Your current memory contents appear in the [MEMORY] section of this system prompt. \
            PROACTIVELY save to memory whenever the conversation reveals something with lasting value: \
            user details (name, contact, preferences, habits), project facts (paths, stack, commands, decisions), \
            system info (IPs, hostnames, services, hardware), recurring workflows, or anything the user mentions \
            about themselves or their work that would genuinely help in future sessions. \
            Only save durable, reusable facts — not one-off task details or things mentioned in passing. \
            You MUST use update_memory — this is the only way to retain information across sessions. \
            Organize into sections: User Info, Preferences, Projects, Workflows, System, Notes.\n\n\
            ## Tool Use Protocol\n\
            CRITICAL: You are an EXECUTION agent, not a narration agent.\n\
            - When a task requires creating files, calling commands, or any action: call write_file, run_command, etc. IMMEDIATELY.\n\
            - NEVER say \"I'll write the script now\" or \"Let me create the file\" and then end your turn. \
            The moment you decide to do something, emit the tool_use call in the SAME response.\n\
            - Do NOT output code in markdown blocks — instead, use write_file to save it and run_command to execute it.\n\
            - Your turn is NOT complete until you have called ALL necessary tools and delivered the final result.\n\
            - Think of each response as: plan (brief) → tool calls → result summary. Never stop at \"plan\".\n\n\
            ## Channel\n\
            You are responding via CLI terminal. Keep responses focused and use plain text or minimal formatting.{sudo_note}",
            soul, turn_cfg.working_directory, sudo_note = sudo_note
        );

        let memory = crate::memory::store::load_memory(&memory_file)
            .unwrap_or(Memory { path: state.memory_path.clone(), content: String::new() });

        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let task_db = Arc::clone(&state.task_db);
        let mem_lock = Arc::clone(&state.memory_lock);

        let agent_handle = tokio::spawn(async move {
            let mut hist = history;
            let run_result = tokio::time::timeout(
                std::time::Duration::from_secs(600),
                router::classify_and_run(
                    &classifier, &sonnet, &compactor,
                    &mut hist, &memory, &soul_with_planning,
                    &project_ctx, &task_db, turn_cfg, &tx,
                ),
            ).await;
            let result = match run_result {
                Ok(r) => r,
                Err(_) => {
                    let _ = tx.send(AgentEvent::Error("Agent timed out after 10 minutes.".into()));
                    let _ = tx.send(AgentEvent::Done);
                    return None;
                }
            };
            match result {
                Ok((text, memory_ops, compacted)) => {
                    if !memory_ops.is_empty() {
                        let _lock = mem_lock.lock().await;
                        let mut mem = memory;
                        for op in memory_ops {
                            if let Err(e) = match op.action.as_str() {
                                "replace" => mem.replace_section(&op.section, &op.content),
                                _ => mem.append_to_section(&op.section, &op.content),
                            } {
                                error!(error = %e, section = %op.section, "Memory save failed");
                            }
                        }
                    }
                    let compacted_hist = if compacted { Some(hist) } else { None };
                    Some((text, compacted_hist))
                }
                Err(e) => {
                    error!(error = %e, "Agent turn failed");
                    let _ = tx.send(AgentEvent::Error(format!("{:#}", e)));
                    let _ = tx.send(AgentEvent::Done);
                    None
                }
            }
        });

        // Drain events — stream text to stdout in real time
        let mut response_text = String::new();
        let mut streaming = false;
        let mut in_think = false;
        let mut think_buf = String::new();
        let mut post_think = false;

        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::TextDelta(chunk) => {
                    if !streaming {
                        streaming = true;
                    }
                    let visible = filter_think(&chunk, &mut in_think, &mut think_buf, &mut post_think);
                    if !visible.is_empty() {
                        print!("{}", visible);
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                    response_text.push_str(&chunk);
                }
                AgentEvent::Text(text) => {
                    response_text = text;
                }
                AgentEvent::TrivialAnswer(answer) => {
                    print!("{}", answer);
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                    response_text = answer;
                }
                AgentEvent::ToolCall { name, .. } => {
                    eprintln!("\x1b[2m[tool: {}]\x1b[0m", name);
                }
                AgentEvent::ToolOutput { name, code, .. } => {
                    if code != 0 {
                        eprintln!("\x1b[31m[{} failed (exit {})]\x1b[0m", name, code);
                    }
                }
                AgentEvent::FileOffer { path, caption } => {
                    eprintln!("[file: {} — {}]", path, caption);
                }
                AgentEvent::AskUser { question, reply_tx } => {
                    println!("\n{}", question);
                    let _ = stdout.write_all(b">> ").await;
                    let _ = stdout.flush().await;
                    let reply = match lines.next_line().await {
                        Ok(Some(l)) => l,
                        _ => "yes".to_string(),
                    };
                    let _ = reply_tx.send(reply);
                }
                AgentEvent::Error(e) => {
                    eprintln!("\x1b[31mError: {}\x1b[0m", e);
                }
                AgentEvent::Done => {
                    // Flush any buffered text that didn't contain a complete tag
                    if !think_buf.is_empty() && !in_think {
                        if post_think { print!("\n\n"); }
                        print!("{}", think_buf);
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                        think_buf.clear();
                    }
                    break;
                }
                _ => {}
            }
        }

        // Wait for agent to finish
        if let Ok(Some((final_text, compacted_hist))) = agent_handle.await {
            let final_stripped = strip_think(&final_text);
            if !final_stripped.is_empty() && !streaming {
                print!("{}", final_stripped);
                response_text = final_stripped;
            } else if !final_stripped.is_empty() {
                response_text = final_stripped;
            }
            if let Some(compacted) = compacted_hist {
                let tuples: Vec<(String, String)> = compacted.iter()
                    .map(|m| (m.role.clone(), m.content.clone()))
                    .collect();
                if let Err(e) = state.chat_db.replace_session_messages(&session_id, &tuples) {
                    warn!(error = %e, "CLI: compaction persist to DB failed");
                }
            }
        }
        println!();

        // Save to DB
        let _ = state.chat_db.save_message(&session_id, "user", text);
        if !response_text.is_empty() {
            let _ = state.chat_db.save_message(&session_id, "assistant", &strip_think(&response_text));
        }
    }

    info!("CLI session ended");
}
