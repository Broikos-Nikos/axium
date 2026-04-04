use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{info, warn, error};

use crate::agent::classifier::Classifier;
use crate::agent::compactor::Compactor;
use crate::agent::router::{self, AgentEvent, TurnConfig};
use crate::agent::sonnet::SonnetClient;
use crate::agent::Message;
use crate::memory::store::Memory;
use crate::tui::server::AppState;

/// Telegram's maximum message length for sendMessage.
const MAX_MSG_LEN: usize = 4096;

/// A lightweight Telegram bot that bridges incoming messages to the Axium agent.
/// Uses long-polling (getUpdates), same as zeroclaw, no webhooks needed.
pub struct TelegramBot {
    bot_token: String,
    allowed_users: Vec<String>,
    http: Arc<reqwest::Client>,
    state: Arc<AppState>,
    shutdown: tokio::sync::watch::Receiver<bool>,
}

impl TelegramBot {
    /// Spawn the Telegram polling loop as a background task.
    /// Returns immediately; the bot runs until shutdown is signalled.
    pub async fn spawn(state: Arc<AppState>, shutdown: tokio::sync::watch::Receiver<bool>) {
        let cfg = state.config.read().await;
        let token = cfg.settings.telegram_bot_token.clone();
        let users_str = cfg.settings.telegram_allowed_users.clone();
        let enabled = cfg.settings.telegram_enabled;
        drop(cfg);

        if !enabled || token.is_empty() {
            return;
        }

        let allowed: Vec<String> = users_str
            .split(',')
            .map(|s| s.trim().to_lowercase().trim_start_matches('@').to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let mut bot = TelegramBot {
            bot_token: token,
            allowed_users: allowed,
            http: Arc::clone(&state.http),
            state,
            shutdown,
        };

        tokio::spawn(async move {
            if let Err(e) = bot.run().await {
                error!(error = %e, "Telegram bot exited with error");
            }
        });
    }

    fn api_url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.bot_token, method)
    }

    fn is_user_allowed(&self, username: &str, user_id: i64) -> bool {
        if self.allowed_users.is_empty() || self.allowed_users.contains(&"*".to_string()) {
            return true;
        }
        let norm_name = username.to_lowercase();
        let id_str = user_id.to_string();
        self.allowed_users.iter().any(|u| u == &norm_name || u == &id_str)
    }

    /// Main long-polling loop. Modeled after zeroclaw's TelegramChannel.
    async fn run(&mut self) -> Result<()> {
        // Startup probe: claim the getUpdates slot
        let mut offset: i64 = 0;
        info!("Telegram bot starting, claiming polling slot...");

        loop {
            if *self.shutdown.borrow() { return Ok(()); }
            let probe = serde_json::json!({
                "offset": offset,
                "timeout": 0,
                "allowed_updates": ["message"]
            });
            match self.http.post(&self.api_url("getUpdates")).json(&probe).send().await {
                Ok(resp) => {
                    if let Ok(data) = resp.json::<serde_json::Value>().await {
                        let ok = data.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                        if ok {
                            // Advance past queued updates
                            if let Some(results) = data.get("result").and_then(|v| v.as_array()) {
                                for u in results {
                                    if let Some(uid) = u.get("update_id").and_then(|v| v.as_i64()) {
                                        offset = uid + 1;
                                    }
                                }
                            }
                            break;
                        }
                        let code = data.get("error_code").and_then(|v| v.as_i64()).unwrap_or(0);
                        if code == 409 {
                            warn!("Telegram slot busy (409), retrying in 5s");
                        }
                    }
                }
                Err(e) => warn!("Telegram probe error: {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }

        info!("Telegram bot polling active");

        // Main loop
        loop {
            if *self.shutdown.borrow() {
                info!("Telegram bot shutting down");
                return Ok(());
            }

            let body = serde_json::json!({
                "offset": offset,
                "timeout": 30,
                "allowed_updates": ["message"]
            });

            let resp = tokio::select! {
                result = self.http.post(&self.api_url("getUpdates")).json(&body).send() => {
                    match result {
                        Ok(r) => r,
                        Err(e) => {
                            warn!("Telegram poll error: {e}");
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            continue;
                        }
                    }
                }
                _ = self.shutdown.changed() => {
                    info!("Telegram bot shutting down (mid-poll)");
                    return Ok(());
                }
            };

            let data: serde_json::Value = match resp.json().await {
                Ok(d) => d,
                Err(e) => {
                    warn!("Telegram parse error: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            let ok = data.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
            if !ok {
                let code = data.get("error_code").and_then(|v| v.as_i64()).unwrap_or(0);
                let desc = data.get("description").and_then(|v| v.as_str()).unwrap_or("unknown");
                if code == 409 {
                    warn!("Telegram 409 conflict: {desc}. Backing off 35s.");
                    tokio::time::sleep(std::time::Duration::from_secs(35)).await;
                } else {
                    warn!("Telegram API error {code}: {desc}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                continue;
            }

            let results = match data.get("result").and_then(|v| v.as_array()) {
                Some(r) => r.clone(),
                None => continue,
            };

            for update in &results {
                if let Some(uid) = update.get("update_id").and_then(|v| v.as_i64()) {
                    offset = uid + 1;
                }
                self.handle_update(update).await;
            }
        }
    }

    /// Process a single Telegram update.
    async fn handle_update(&self, update: &serde_json::Value) {
        let message = match update.get("message") {
            Some(m) => m,
            None => return,
        };

        let text = match message.get("text").and_then(|v| v.as_str()) {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => return,
        };

        let chat_id = match message.get("chat").and_then(|c| c.get("id")).and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => return,
        };

        let from = message.get("from").unwrap_or(&serde_json::Value::Null);
        let username = from.get("username").and_then(|v| v.as_str()).unwrap_or("");
        let user_id = from.get("id").and_then(|v| v.as_i64()).unwrap_or(0);

        if !self.is_user_allowed(username, user_id) {
            info!(username, user_id, "Telegram: unauthorized user, ignoring");
            return;
        }

        // Handle /new command — clear the Telegram session history
        if text.trim().eq_ignore_ascii_case("/new") {
            let session_key = format!("telegram_{}", user_id);
            if let Ok(session_id) = self.state.chat_db.find_or_create_session(&session_key) {
                match self.state.chat_db.clear_session_messages(&session_id) {
                    Ok(n) => {
                        info!(session = %session_id, deleted = n, "Telegram: session cleared via /new");
                        let _ = self.send_response(chat_id, "Session cleared.").await;
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to clear Telegram session");
                        let _ = self.send_response(chat_id, "Failed to clear session.").await;
                    }
                }
            }
            return;
        }

        // Send typing indicator
        let _ = self.send_typing(chat_id).await;

        // Run through the agent pipeline (same as WebSocket handler)
        match self.process_message(&text, chat_id, user_id).await {
            Ok(response) => {
                if let Err(e) = self.send_response(chat_id, &response).await {
                    error!(error = %e, "Failed to send Telegram response");
                }
            }
            Err(e) => {
                error!(error = %e, "Agent turn failed on Telegram message");
                let _ = self.send_response(chat_id, &format!("Error: {e}")).await;
            }
        }
    }

    /// Run the message through the full agent pipeline.
    async fn process_message(&self, text: &str, chat_id: i64, user_id: i64) -> Result<String> {
        // Use telegram_{user_id} as session key so each user gets their own session
        let session_key = format!("telegram_{}", user_id);

        // Load or create session
        let session_id = match self.state.chat_db.find_or_create_session(&session_key) {
            Ok(id) => id,
            Err(e) => {
                warn!(error = %e, "Failed to get Telegram session, creating new");
                self.state.chat_db.create_session()?
            }
        };

        // Pin the title permanently as "Telegram"
        if self.state.chat_db.get_session_title(&session_id).is_empty() {
            let _ = self.state.chat_db.update_session_title(&session_id, "Telegram");
        }

        // Load history (cap at last 50 messages to avoid unbounded token growth)
        let all_msgs = self.state.chat_db
            .load_session_messages(&session_id)
            .unwrap_or_default();
        let skip = all_msgs.len().saturating_sub(50);
        let mut history: Vec<Message> = all_msgs
            .into_iter()
            .skip(skip)
            .filter(|m| m.role != "system" && !m.content.starts_with("[partial] "))
            .map(|m| Message { role: m.role, content: m.content })
            .collect();

        history.push(Message::user(text));

        // Build agent components from config
        let sudo_pw = self.state.sudo_password.read().await.clone();
        let sudo_note = if !sudo_pw.is_empty() {
            "\n\n## Sudo Access\nA sudo password is configured. When commands need elevated privileges, use `sudo` in run_command — the password is injected automatically and transparently. NEVER ask the user for their password."
        } else { "" };
        let (sonnet, compactor, classifier, soul, turn_cfg, project_ctx, memory_file) = {
            let cfg = self.state.config.read().await;
            let wd = &cfg.settings.working_directory;
            let resolved_wd = if wd.is_empty() || wd == "~" {
                std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
            } else if wd.starts_with("~/") {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{}/{}", home, &wd[2..])
            } else {
                wd.clone()
            };
            let ctx = crate::tui::server::get_project_context(&self.state, &resolved_wd).await;
            (
                SonnetClient::new(
                    &cfg.api_keys.anthropic,
                    &cfg.api_keys.openai,
                    &cfg.models.primary,
                    &cfg.models.primary_provider,
                    cfg.settings.max_tokens,
                    Arc::clone(&self.state.http),
                ),
                Compactor::new(
                    &cfg.api_keys.anthropic,
                    &cfg.api_keys.openai,
                    &cfg.models.compactor,
                    &cfg.models.compactor_provider,
                    Arc::clone(&self.state.http),
                ),
                Classifier::new(
                    &cfg.api_keys.anthropic,
                    &cfg.api_keys.openai,
                    &cfg.models.classifier,
                    &cfg.models.classifier_provider,
                    Arc::clone(&self.state.http),
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
                    http: Arc::clone(&self.state.http),
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
                    plugin_manager: Some(Arc::clone(&self.state.plugin_manager)),
                    compaction_threshold: cfg.settings.compaction_threshold,
                    thinking_effort: cfg.settings.thinking_effort.clone(),
                    fallback_model: cfg.models.fallback.clone(),
                    fallback_provider: cfg.models.fallback_provider.clone(),
                    conv_logger: None,
                    chat_db: Arc::clone(&self.state.chat_db),
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
            You are responding via Telegram. Keep responses concise — Telegram has a 4096 char limit per message. \
            Avoid excessive markdown formatting. Use plain text or minimal formatting.{sudo_note}",
            soul, turn_cfg.working_directory, sudo_note = sudo_note
        );

        let memory = crate::memory::store::load_memory(&memory_file)
            .unwrap_or(Memory {
                path: self.state.memory_path.clone(),
                content: String::new(),
            });

        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let task_db = Arc::clone(&self.state.task_db);
        let mem_lock = Arc::clone(&self.state.memory_lock);

        // Spawn agent turn with 10-minute timeout
        let agent_handle = tokio::spawn(async move {
            let mut hist = history;
            let run_result = tokio::time::timeout(
                std::time::Duration::from_secs(600),
                router::classify_and_run(
                    &classifier,
                    &sonnet,
                    &compactor,
                    &mut hist,
                    &memory,
                    &soul_with_planning,
                    &project_ctx,
                    &task_db,
                    turn_cfg,
                    &tx,
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

        // Collect events to build final response
        let mut response_text = String::new();
        let mut tool_log = String::new();

        // Periodically send typing while waiting
        let typing_chat_id = chat_id;
        let typing_http = Arc::clone(&self.http);
        let typing_token = self.bot_token.clone();
        let typing_cancel = tokio_util::sync::CancellationToken::new();
        let typing_cancel_clone = typing_cancel.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = typing_cancel_clone.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(4)) => {
                        let body = serde_json::json!({
                            "chat_id": typing_chat_id,
                            "action": "typing"
                        });
                        let url = format!("https://api.telegram.org/bot{}/sendChatAction", typing_token);
                        let _ = typing_http.post(&url).json(&body).send().await;
                    }
                }
            }
        });

        let mut file_offers: Vec<(String, String)> = Vec::new();

        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::TextDelta(chunk) => {
                    response_text.push_str(&chunk);
                }
                AgentEvent::Text(text) => {
                    response_text = text;
                }
                AgentEvent::TrivialAnswer(answer) => {
                    response_text = answer;
                }
                AgentEvent::ToolCall { name, input } => {
                    tool_log.push_str(&format!("✓ {}(…)\n", name));
                    let _ = input; // Don't send raw input to Telegram
                }
                AgentEvent::ToolOutput { name, stdout, stderr, code } => {
                    if code != 0 {
                        tool_log.push_str(&format!("✗ {} (exit {})\n", name, code));
                    }
                    let _ = (stdout, stderr);
                }
                AgentEvent::FileOffer { path, caption } => {
                    file_offers.push((path, caption));
                }
                AgentEvent::Error(e) => {
                    if response_text.is_empty() {
                        response_text = format!("⚠ {}", e);
                    } else {
                        response_text.push_str(&format!("\n\n⚠ {}", e));
                    }
                }
                AgentEvent::Done => break,
                _ => {} // Plan, MemoryUpdate, Classified, AskUser — skip for Telegram
            }
        }

        // Stop typing indicator
        typing_cancel.cancel();

        // Wait for agent to finish
        if let Ok(Some((final_text, compacted_hist))) = agent_handle.await {
            if !final_text.is_empty() {
                response_text = final_text;
            }
            if let Some(compacted) = compacted_hist {
                let tuples: Vec<(String, String)> = compacted.iter()
                    .map(|m| (m.role.clone(), m.content.clone()))
                    .collect();
                if let Err(e) = self.state.chat_db.replace_session_messages(&session_id, &tuples) {
                    warn!(error = %e, "Telegram: compaction persist to DB failed");
                }
            }
        }

        // Save to DB
        let _ = self.state.chat_db.save_message(&session_id, "user", text);
        if !response_text.is_empty() {
            let _ = self.state.chat_db.save_message(&session_id, "assistant", &response_text);
        }

        // Deliver any files the agent produced
        let chat_id_str = chat_id.to_string();
        for (path, caption) in &file_offers {
            if let Err(e) = send_document(&self.http, &self.bot_token, &chat_id_str, path, caption).await {
                warn!(error = %e, path, "Failed to send file via Telegram");
            }
        }

        // Append tool log if any
        if !tool_log.is_empty() && !response_text.is_empty() {
            Ok(format!("{}\n\n<tool_log>\n{}</tool_log>", response_text, tool_log))
        } else if response_text.is_empty() {
            Ok("(No response generated)".to_string())
        } else {
            Ok(response_text)
        }
    }

    /// Send typing indicator.
    async fn send_typing(&self, chat_id: i64) -> Result<()> {
        let body = serde_json::json!({
            "chat_id": chat_id,
            "action": "typing"
        });
        let _ = self.http.post(&self.api_url("sendChatAction")).json(&body).send().await;
        Ok(())
    }

    /// Send a text response, chunking if >4096 chars. Falls back to plain text if HTML fails.
    async fn send_response(&self, chat_id: i64, text: &str) -> Result<()> {
        let chunks = split_message(text);
        for (i, chunk) in chunks.iter().enumerate() {
            let display = if chunks.len() > 1 {
                if i == 0 {
                    format!("{}\n\n(continues...)", chunk)
                } else if i == chunks.len() - 1 {
                    format!("(continued)\n\n{}", chunk)
                } else {
                    format!("(continued)\n\n{}\n\n(continues...)", chunk)
                }
            } else {
                chunk.to_string()
            };

            let body = serde_json::json!({
                "chat_id": chat_id,
                "text": display,
            });
            let resp = self.http.post(&self.api_url("sendMessage")).json(&body).send().await?;
            let status = resp.status();
            if !status.is_success() {
                let err = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("Telegram sendMessage failed ({}): {}", status, err));
            }

            if i < chunks.len() - 1 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        Ok(())
    }
}

/// Send a message to a specific Telegram chat (used by the send_telegram tool).
pub async fn send_message(
    http: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    text: &str,
) -> Result<String> {
    if bot_token.is_empty() {
        anyhow::bail!("Telegram bot token not configured");
    }
    if chat_id.is_empty() {
        anyhow::bail!("chat_id is required");
    }
    if text.is_empty() {
        anyhow::bail!("Message text cannot be empty");
    }

    let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
    let chunks = split_message(text);
    for chunk in &chunks {
        let body = serde_json::json!({
            "chat_id": chat_id,
            "text": chunk,
        });
        let resp = http.post(&url).json(&body).send().await?;
        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("Telegram sendMessage failed: {}", err);
        }
    }
    Ok(format!("Message sent to chat {}", chat_id))
}

/// Split a message into chunks respecting Telegram's 4096 limit.
fn split_message(text: &str) -> Vec<String> {
    if text.len() <= MAX_MSG_LEN {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;
    let limit = MAX_MSG_LEN - 60; // Room for continuation markers added by send_response

    while !remaining.is_empty() {
        if remaining.len() <= MAX_MSG_LEN {
            chunks.push(remaining.to_string());
            break;
        }

        let end = remaining
            .char_indices()
            .nth(limit)
            .map_or(remaining.len(), |(i, _)| i);

        let split_at = remaining[..end]
            .rfind('\n')
            .filter(|&p| p >= limit / 2)
            .or_else(|| remaining[..end].rfind(' '))
            .map(|p| p + 1)
            .unwrap_or(end);

        chunks.push(remaining[..split_at].to_string());
        remaining = &remaining[split_at..];
    }

    chunks
}

/// Send a file (document) to a Telegram chat via sendDocument multipart upload.
pub async fn send_document(
    http: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    file_path: &str,
    caption: &str,
) -> Result<()> {
    if bot_token.is_empty() {
        anyhow::bail!("Telegram bot token not configured");
    }

    let path = std::path::Path::new(file_path);
    if !path.exists() {
        anyhow::bail!("File not found: {}", file_path);
    }

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();

    let file_bytes = tokio::fs::read(file_path).await
        .context(format!("Failed to read file: {}", file_path))?;

    let file_part = reqwest::multipart::Part::bytes(file_bytes)
        .file_name(file_name)
        .mime_str("application/octet-stream")?;

    let mut form = reqwest::multipart::Form::new()
        .text("chat_id", chat_id.to_string())
        .part("document", file_part);

    if !caption.is_empty() {
        form = form.text("caption", caption.to_string());
    }

    let url = format!("https://api.telegram.org/bot{}/sendDocument", bot_token);
    let resp = http.post(&url).multipart(form).send().await?;

    if !resp.status().is_success() {
        let err = resp.text().await.unwrap_or_default();
        anyhow::bail!("Telegram sendDocument failed: {}", err);
    }

    Ok(())
}
