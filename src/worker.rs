use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::agent::classifier::Classifier;
use crate::agent::compactor::Compactor;
use crate::agent::router::{self, AgentEvent, TurnConfig};
use crate::agent::sonnet::SonnetClient;
use crate::agent::Message;
use crate::memory::store::Memory;
use crate::tui::server::AppState;

/// Spawn the background task worker.
/// Polls for pending tasks every few seconds and runs them as autonomous agent turns.
pub fn spawn_worker(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            // Catch panics so the worker loop survives individual task failures
            let state_clone = Arc::clone(&state);
            let result = tokio::spawn(async move {
                run_next_task(&state_clone).await
            }).await;
            match result {
                Ok(Err(e)) => warn!(error = %e, "Background worker error"),
                Err(e) => warn!(error = %e, "Background worker task panicked — recovering"),
                _ => {}
            }
        }
    });
}

async fn run_next_task(state: &Arc<AppState>) -> anyhow::Result<()> {
    let task = match state.task_db.claim_pending()? {
        Some(t) => t,
        None => return Ok(()),
    };

    info!(id = task.id, title = %task.title, "Background worker picked up task");

    // Notify connected clients that task is starting
    let start_msg = serde_json::json!({
        "type": "task_started",
        "id": task.id,
        "title": task.title,
    }).to_string();
    let _ = state.broadcast_tx.send(start_msg);

    // Build agent components from current config
    let (status, result) = {
        let cfg = state.config.read().await;
        let sonnet = SonnetClient::new(
            &cfg.api_keys.anthropic,
            &cfg.api_keys.openai,
            &cfg.models.primary,
            &cfg.models.primary_provider,
            cfg.settings.max_tokens,
            Arc::clone(&state.http),
        );
        let compactor = Compactor::new(
            &cfg.api_keys.anthropic,
            &cfg.api_keys.openai,
            &cfg.models.compactor,
            &cfg.models.compactor_provider,
            Arc::clone(&state.http),
        );
        let classifier = Classifier::new(
            &cfg.api_keys.anthropic,
            &cfg.api_keys.openai,
            &cfg.models.classifier,
            &cfg.models.classifier_provider,
            Arc::clone(&state.http),
        );

        let turn_cfg = TurnConfig {
            token_limit: cfg.settings.token_limit,
            terminal_timeout: cfg.settings.terminal_timeout_secs,
            max_output_chars: cfg.settings.max_output_chars,
            max_tool_iterations: cfg.settings.max_tool_iterations,
            max_retries: cfg.settings.max_retries,
            sudo_password: String::new(),
            working_directory: cfg.settings.working_directory.clone(),
            conversation_logging: false,
            anthropic_key: cfg.api_keys.anthropic.clone(),
            openai_key: cfg.api_keys.openai.clone(),
            primary_model: cfg.models.primary.clone(),
            primary_provider: cfg.models.primary_provider.clone(),
            http: Arc::clone(&state.http),
            smtp_host: cfg.settings.smtp_host.clone(),
            smtp_port: cfg.settings.smtp_port,
            smtp_user: cfg.settings.smtp_user.clone(),
            smtp_password: cfg.settings.smtp_password.clone(),
            smtp_from: cfg.settings.smtp_from.clone(),
            telegram_bot_token: cfg.settings.telegram_bot_token.clone(),
            subagent_depth: 0,
            continuation_model: cfg.models.continuation.clone(),
            continuation_provider: cfg.models.continuation_provider.clone(),
            classifier_model: cfg.models.classifier.clone(),
            classifier_provider: cfg.models.classifier_provider.clone(),
            review_model: cfg.models.review.clone(),
            review_provider: cfg.models.review_provider.clone(),
            mode: "supercharge".to_string(),
        };

        let soul = crate::config::loader::load_soul(&cfg.agent.soul);
        let raw_wd = &cfg.settings.working_directory;
        let working_dir = if raw_wd.is_empty() || raw_wd == "~" {
            std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
        } else if raw_wd.starts_with("~/") {
            let home = std::env::var("HOME").unwrap_or_default();
            format!("{}/{}", home, &raw_wd[2..])
        } else {
            raw_wd.clone()
        };
        let project_ctx = crate::tools::project::build_project_context(&working_dir);

        // Worker soul — focused on autonomous execution
        let worker_soul = format!(
            "{}\n\n[WORKER MODE]\nYou are running as a background task worker. \
            Complete the task fully and autonomously. \
            Do not ask for clarification — make reasonable assumptions and proceed. \
            The task is: {}\n\nAdditional context: {}",
            soul, task.title, task.context
        );

        let memory = crate::memory::store::load_memory(&cfg.settings.memory_file)
            .unwrap_or(Memory { path: cfg.settings.memory_file.clone(), content: String::new() });

        let mut history = vec![Message {
            role: "user".to_string(),
            content: if task.context.is_empty() {
                task.title.clone()
            } else {
                format!("{}\n\nContext:\n{}", task.title, task.context)
            },
        }];

        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let task_db = Arc::clone(&state.task_db);

        // Drain events in background while agent runs
        let drain = tokio::spawn(async move {
            let mut output = String::new();
            while let Some(event) = rx.recv().await {
                match event {
                    AgentEvent::Text(text) => { output = text; }
                    AgentEvent::AskUser { reply_tx, .. } => {
                        // Auto-approve in worker mode — no human to ask
                        let _ = reply_tx.send("yes".to_string());
                    }
                    _ => {}
                }
            }
            output
        });

        drop(cfg); // release read lock before async work

        let run_result = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            router::classify_and_run(
                &classifier,
                &sonnet,
                &compactor,
                &mut history,
                &memory,
                &worker_soul,
                &project_ctx,
                &task_db,
                turn_cfg,
                &tx,
            ),
        ).await;

        drop(tx); // signal drain to finish
        let agent_output = drain.await.unwrap_or_default();

        match run_result {
            Ok(Ok((text, _))) => {
                let t = if text.is_empty() { agent_output } else { text };
                ("done", t)
            }
            Ok(Err(e)) => ("failed", format!("Task failed: {}", e)),
            Err(_) => ("failed", "Task failed: timed out after 10 minutes".to_string()),
        }
    };

    if let Err(e) = state.task_db.save_task_result(task.id, &result, status) {
        warn!(id = task.id, error = %e, "Failed to save task result — resetting to pending");
        let _ = state.task_db.update_task_status(task.id, "pending");
        return Err(e.into());
    }

    info!(id = task.id, status, "Background task complete");

    // Broadcast completion to connected clients
    let done_msg = serde_json::json!({
        "type": "task_completed",
        "id": task.id,
        "title": task.title,
        "status": status,
        "result": if result.len() > 500 { let mut b=500; while b>0 && !result.is_char_boundary(b) { b-=1; } format!("{}…", &result[..b]) } else { result.clone() },
    }).to_string();
    let _ = state.broadcast_tx.send(done_msg);

    // Telegram notification
    {
        let cfg = state.config.read().await;
        if cfg.settings.telegram_enabled && !cfg.settings.telegram_bot_token.is_empty() {
            let summary = if result.len() > 300 { let mut b=300; while b>0 && !result.is_char_boundary(b) { b-=1; } format!("{}…", &result[..b]) } else { result.clone() };
            let message = format!("✅ Task #{} complete: {}\n\n{}", task.id, task.title, summary);
            let _ = notify_telegram(&cfg.settings.telegram_bot_token, &cfg.settings.telegram_allowed_users, &message, &state.http).await;
        }
    }

    Ok(())
}

async fn notify_telegram(token: &str, allowed_users: &str, message: &str, http: &reqwest::Client) -> anyhow::Result<()> {
    // Get updates to find chat IDs for allowed users
    let updates_url = format!("https://api.telegram.org/bot{}/getUpdates?limit=10", token);
    let updates: serde_json::Value = http.get(&updates_url).send().await?.json().await?;

    let allowed: Vec<&str> = allowed_users.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();

    for update in updates["result"].as_array().unwrap_or(&vec![]) {
        let username = update["message"]["from"]["username"].as_str().unwrap_or("");
        let chat_id = update["message"]["chat"]["id"].as_i64().unwrap_or(0);
        if chat_id != 0 && (allowed.is_empty() || allowed.contains(&username)) {
            let send_url = format!("https://api.telegram.org/bot{}/sendMessage", token);
            let _ = http.post(&send_url)
                .json(&serde_json::json!({ "chat_id": chat_id, "text": message }))
                .send().await;
        }
    }

    Ok(())
}
