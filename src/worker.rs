use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::agent::classifier::Classifier;
use crate::agent::compactor::Compactor;
use crate::agent::router::{self, AgentEvent, TurnConfig};
use crate::agent::sonnet::SonnetClient;
use crate::agent::Message;
use crate::db::tasks::Task;
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

/// Resolve the tasks/ directory path from the config working directory.
fn tasks_dir(working_dir: &str) -> PathBuf {
    let base = if working_dir.is_empty() || working_dir == "~" {
        std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
    } else if working_dir.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/{}", home, &working_dir[2..])
    } else {
        working_dir.to_string()
    };
    PathBuf::from(base).join("tasks")
}

/// Write or overwrite a task state file.
fn write_task_file(dir: &PathBuf, task: &Task, progress_log: &str, result_text: &str) {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("task_{}.md", task.id));
    let status_label = match task.status.as_str() {
        "running" => "running",
        "done" => "done",
        "failed" => "failed",
        _ => "pending",
    };
    let mut content = format!(
        "# Task #{}: {}\n\
        **Status**: {}  \n\
        **Attempt**: {}/{}  \n\
        **Created**: {}  \n\
        **Updated**: {}  \n\n",
        task.id, task.title,
        status_label,
        task.attempt + 1, task.max_attempts,
        task.created_at, task.updated_at,
    );
    if !task.context.is_empty() {
        content.push_str(&format!("## Context\n{}\n\n", task.context));
    }
    if !progress_log.is_empty() {
        content.push_str(&format!("## Progress\n{}\n\n", progress_log));
    }
    if !result_text.is_empty() {
        content.push_str(&format!("## Result\n{}\n", result_text));
    }
    if let Err(e) = std::fs::write(&path, &content) {
        warn!(error = %e, path = %path.display(), "Failed to write task state file");
    }
}

/// Append a timestamped line to a task's progress log file.
fn append_task_log(dir: &PathBuf, task_id: i64, line: &str) {
    let path = dir.join(format!("task_{}.md", task_id));
    let ts = chrono::Local::now().format("%H:%M:%S").to_string();
    let entry = format!("- [{}] {}\n", ts, line);
    // Append to existing file (if it exists)
    if let Ok(mut existing) = std::fs::read_to_string(&path) {
        // Insert before "## Result" section if it exists, otherwise append
        if let Some(pos) = existing.find("## Result\n") {
            existing.insert_str(pos, &entry);
        } else {
            existing.push_str(&entry);
        }
        let _ = std::fs::write(&path, existing);
    }
}

/// Build attempt-aware guidance text for the worker soul.
fn attempt_guidance(attempt: i64, max_attempts: i64, prev_failure: &str) -> String {
    if attempt == 0 {
        "Take your time and be thorough. Verify your work before finishing. \
        Use tools to actually perform the task — do not just describe what you would do."
            .to_string()
    } else if attempt + 1 >= max_attempts {
        format!(
            "FINAL ATTEMPT. Previous attempt failed: {}\n\
            You MUST deliver a concrete result NOW. If you cannot complete perfectly, \
            deliver what you can with clear notes on what remains incomplete. \
            Do NOT end your turn without producing real output — files created, commands run, \
            or concrete deliverables.",
            prev_failure
        )
    } else {
        format!(
            "Previous attempt failed: {}\n\
            Address the specific failure reason above and complete the task. \
            Focus on what was missing and fix it.",
            prev_failure
        )
    }
}

async fn run_next_task(state: &Arc<AppState>) -> anyhow::Result<()> {
    let task = match state.task_db.claim_pending()? {
        Some(t) => t,
        None => return Ok(()),
    };

    info!(id = task.id, title = %task.title, attempt = task.attempt, "Background worker picked up task");

    // Plugin hook: on_task_start
    {
        let pm = state.plugin_manager.read().await;
        let _ = pm.run_hook("on_task_start", &serde_json::json!({
            "task_id": task.id,
            "title": task.title,
            "context": task.context,
            "attempt": task.attempt,
        })).await;
    }

    // Notify connected clients that task is starting
    let start_msg = serde_json::json!({
        "type": "task_started",
        "id": task.id,
        "title": task.title,
        "attempt": task.attempt + 1,
        "max_attempts": task.max_attempts,
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
            plugin_manager: Some(Arc::clone(&state.plugin_manager)),
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
        let project_ctx = crate::tui::server::get_project_context(&state, &working_dir).await;
        let tdir = tasks_dir(raw_wd);

        // Determine previous failure reason (stored in result if retrying)
        let prev_failure = if task.attempt > 0 && !task.result.is_empty() {
            task.result.clone()
        } else {
            String::new()
        };

        let guidance = attempt_guidance(task.attempt, task.max_attempts, &prev_failure);

        // Worker soul — focused on autonomous execution with attempt awareness
        let worker_soul = format!(
            "{}\n\n[WORKER MODE — AGENTIC TASK EXECUTION]\n\
            You are running as a background task worker. Complete the task fully and autonomously.\n\
            Do not ask for clarification — make reasonable assumptions and proceed.\n\
            \n## Task\n{}\n\
            \n## Context\n{}\n\
            \n## Attempt {}/{}\n{}\n",
            soul, task.title, task.context,
            task.attempt + 1, task.max_attempts,
            guidance,
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

        // Write initial task state file
        write_task_file(&tdir, &task, "- Starting task execution\n", "");

        // Drain events in background — forward progress to UI and task file
        let broadcast_tx = state.broadcast_tx.clone();
        let task_id = task.id;
        let tdir_clone = tdir.clone();
        let drain = tokio::spawn(async move {
            let mut output = String::new();
            while let Some(event) = rx.recv().await {
                match event {
                    AgentEvent::Text(text) => { output = text; }
                    AgentEvent::AskUser { reply_tx, .. } => {
                        // Auto-approve in worker mode — no human to ask
                        let _ = reply_tx.send("yes".to_string());
                    }
                    AgentEvent::ToolCall { ref name, .. } => {
                        append_task_log(&tdir_clone, task_id, &format!("Tool: {}", name));
                        let _ = broadcast_tx.send(serde_json::json!({
                            "type": "task_progress",
                            "id": task_id,
                            "tool": name,
                        }).to_string());
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

        let (raw_status, raw_result) = match run_result {
            Ok(Ok((text, _))) => {
                let t = if text.is_empty() { agent_output } else { text };
                ("done", t)
            }
            Ok(Err(e)) => ("failed", format!("Task failed: {}", e)),
            Err(_) => ("failed", "Task failed: timed out after 10 minutes".to_string()),
        };

        // ── Verification pass ──
        if raw_status == "done" {
            let (verified, reason) = classifier.verify_task(
                &task.title, &task.context, &raw_result,
            ).await;

            if verified {
                info!(id = task.id, "Task verified successfully");
                append_task_log(&tdir, task.id, "Verification: PASSED");
                // Write final task file
                let mut final_task = task.clone();
                final_task.status = "done".to_string();
                write_task_file(&tdir, &final_task, "", &raw_result);
                ("done".to_string(), raw_result)
            } else {
                // Check if we can retry
                let new_attempt = state.task_db.increment_attempt(task.id)
                    .unwrap_or(task.attempt + 1);
                if new_attempt >= task.max_attempts {
                    // Force finish — deliver partial results
                    info!(id = task.id, attempt = new_attempt, "Task verification failed on final attempt — force finishing");
                    append_task_log(&tdir, task.id, &format!("Verification FAILED (final attempt): {}", reason));
                    let partial = format!("[PARTIAL — verification failed: {}]\n\n{}", reason, raw_result);
                    let mut final_task = task.clone();
                    final_task.status = "done".to_string();
                    final_task.attempt = new_attempt;
                    write_task_file(&tdir, &final_task, "", &partial);
                    ("done".to_string(), partial)
                } else {
                    // Retry — set back to pending with failure context
                    info!(id = task.id, attempt = new_attempt, reason = %reason, "Task verification failed — scheduling retry");
                    append_task_log(&tdir, task.id, &format!("Verification FAILED: {} — retrying", reason));
                    let failure_ctx = format!("PREVIOUS FAILURE (attempt {}): {}", new_attempt, reason);
                    let _ = state.task_db.save_task_result(task.id, &failure_ctx, "pending");
                    // Broadcast retry event
                    let _ = state.broadcast_tx.send(serde_json::json!({
                        "type": "task_retry",
                        "id": task.id,
                        "title": task.title,
                        "attempt": new_attempt + 1,
                        "max_attempts": task.max_attempts,
                        "reason": reason,
                    }).to_string());
                    return Ok(());
                }
            }
        } else {
            // Agent itself failed — check retry eligibility
            let new_attempt = state.task_db.increment_attempt(task.id)
                .unwrap_or(task.attempt + 1);
            if new_attempt >= task.max_attempts {
                append_task_log(&tdir, task.id, &format!("FAILED (final attempt): {}", raw_result));
                let mut final_task = task.clone();
                final_task.status = "failed".to_string();
                final_task.attempt = new_attempt;
                write_task_file(&tdir, &final_task, "", &raw_result);
                ("failed".to_string(), raw_result)
            } else {
                info!(id = task.id, attempt = new_attempt, "Task execution failed — scheduling retry");
                append_task_log(&tdir, task.id, &format!("Execution FAILED: {} — retrying", raw_result));
                let failure_ctx = format!("PREVIOUS FAILURE (attempt {}): {}", new_attempt, raw_result);
                let _ = state.task_db.save_task_result(task.id, &failure_ctx, "pending");
                let _ = state.broadcast_tx.send(serde_json::json!({
                    "type": "task_retry",
                    "id": task.id,
                    "title": task.title,
                    "attempt": new_attempt + 1,
                    "max_attempts": task.max_attempts,
                    "reason": raw_result,
                }).to_string());
                return Ok(());
            }
        }
    };

    if let Err(e) = state.task_db.save_task_result(task.id, &result, &status) {
        warn!(id = task.id, error = %e, "Failed to save task result — resetting to pending");
        let _ = state.task_db.update_task_status(task.id, "pending");
        return Err(e.into());
    }

    info!(id = task.id, status = %status, "Background task complete");

    // Plugin hook: on_task_complete
    {
        let pm = state.plugin_manager.read().await;
        let _ = pm.run_hook("on_task_complete", &serde_json::json!({
            "task_id": task.id,
            "title": task.title,
            "status": status,
            "result": if result.len() > 1000 { &result[..1000] } else { &result },
        })).await;
    }

    // Broadcast completion to connected clients
    let done_msg = serde_json::json!({
        "type": "task_completed",
        "id": task.id,
        "title": task.title,
        "status": status,
        "result": if result.len() > 500 { let mut b=500; while b>0 && !result.is_char_boundary(b) { b-=1; } format!("{}…", &result[..b]) } else { result.clone() },
    }).to_string();
    let _ = state.broadcast_tx.send(done_msg);

    // Telegram notification — extract values first so the read lock is dropped before the await
    let tg_notify = {
        let cfg = state.config.read().await;
        if cfg.settings.telegram_enabled && !cfg.settings.telegram_bot_token.is_empty() {
            Some((cfg.settings.telegram_bot_token.clone(), cfg.settings.telegram_allowed_users.clone()))
        } else {
            None
        }
    };
    if let Some((tg_token, tg_users)) = tg_notify {
        let summary = if result.len() > 300 { let mut b=300; while b>0 && !result.is_char_boundary(b) { b-=1; } format!("{}…", &result[..b]) } else { result.clone() };
        let icon = if status == "done" { "✅" } else { "❌" };
        let message = format!("{} Task #{} {}: {}\n\n{}", icon, task.id, status, task.title, summary);
        let _ = notify_telegram(&tg_token, &tg_users, &message, &state.http).await;
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
