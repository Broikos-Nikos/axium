use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn, error};
use chrono::Local;

use super::classifier::{Classifier, PromptClass};
use super::compactor::Compactor;
use super::sonnet::SonnetClient;
use super::{estimate_tokens, Message};
use crate::db::tasks::TaskDb;
use crate::memory::store::Memory;
use crate::tools;

/// Events sent from the agent loop back to the UI.
pub enum AgentEvent {
    /// Streamed text chunk from the agent.
    TextDelta(String),
    /// Agent produced final text output.
    Text(String),
    /// Agent is calling a tool.
    ToolCall { name: String, input: String },
    /// Tool produced output.
    ToolOutput { name: String, stdout: String, stderr: String, code: i32 },
    /// Agent generated a plan before acting.
    Plan(String),
    /// Memory was updated.
    MemoryUpdate { section: String, content: String },
    /// Agent is asking the user a question; reply via the oneshot sender.
    AskUser {
        question: String,
        reply_tx: tokio::sync::oneshot::Sender<String>,
    },
    /// Classifier determined the prompt class.
    Classified { class: String, detail: String },
    /// Trivial answer from classifier (skip main model).
    TrivialAnswer(String),
    /// Agent wants to deliver a file to the user (browser download + Telegram document).
    FileOffer { path: String, caption: String },
    /// An error occurred.
    Error(String),
    /// The model that handled this turn.
    ModelUsed(String),
    /// Agent turn is complete.
    Done,
    /// Heartbeat decided the response was incomplete — text is being discarded and retried.
    /// The UI should clear any already-rendered streaming text.
    Retry,
    /// Agent requests autonomous mode on/off for this session.
    SetAutonomous { enabled: bool },
    /// Agent queued a background task.
    TaskQueued { id: i64, title: String },
}

impl std::fmt::Debug for AgentEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TextDelta(_) => write!(f, "TextDelta(...)"),
            Self::Text(_) => write!(f, "Text(...)"),
            Self::ToolCall { name, .. } => write!(f, "ToolCall({})", name),
            Self::ToolOutput { name, .. } => write!(f, "ToolOutput({})", name),
            Self::Plan(_) => write!(f, "Plan(...)"),
            Self::MemoryUpdate { section, .. } => write!(f, "MemoryUpdate({})", section),
            Self::AskUser { question, .. } => write!(f, "AskUser({:?})", question),
            Self::Classified { class, .. } => write!(f, "Classified({})", class),
            Self::TrivialAnswer(_) => write!(f, "TrivialAnswer(...)"),
            Self::FileOffer { path, .. } => write!(f, "FileOffer({})", path),
            Self::Error(e) => write!(f, "Error({})", e),
            Self::ModelUsed(m) => write!(f, "ModelUsed({})", m),
            Self::Done => write!(f, "Done"),
            Self::Retry => write!(f, "Retry"),
            Self::SetAutonomous { enabled } => write!(f, "SetAutonomous({})", enabled),
            Self::TaskQueued { id, .. } => write!(f, "TaskQueued({})", id),
        }
    }
}

/// Configuration for a single agent turn.
#[derive(Clone)]
pub struct TurnConfig {
    pub token_limit: usize,
    pub terminal_timeout: u64,
    pub max_output_chars: usize,
    pub max_tool_iterations: usize,
    pub max_retries: usize,
    pub sudo_password: String,
    pub working_directory: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_user: String,
    pub smtp_password: String,
    pub smtp_from: String,
    pub telegram_bot_token: String,
    pub conversation_logging: bool,
    // Subagent fields — needed to spawn a fresh agent with the same credentials
    pub http: Arc<reqwest::Client>,
    pub anthropic_key: String,
    pub openai_key: String,
    pub primary_model: String,
    pub primary_provider: String,
    pub subagent_depth: u8,
    /// Optional cheaper model for tool-continuation turns. Empty = use primary.
    pub continuation_model: String,
    pub continuation_provider: String,
    /// Model used for classification / quality-review turns (should be cheap, e.g. gpt-4.1-nano).
    pub classifier_model: String,
    pub classifier_provider: String,
    /// UI-selected processing mode: "simple", "supercharge", or "skills".
    /// Absent/empty defaults to "supercharge" (current behavior).
    pub mode: String,
    /// Model used for post-turn code review and test generation.
    pub review_model: String,
    pub review_provider: String,
    /// Plugin manager for hook execution (None in sub-agents).
    pub plugin_manager: Option<std::sync::Arc<tokio::sync::RwLock<crate::plugins::PluginManager>>>,
}

/// A pending memory operation returned from the agent loop.
#[derive(Debug)]
pub struct MemoryOp {
    pub action: String,
    pub section: String,
    pub content: String,
}

/// Classify the user's prompt, then either answer trivially or run the full agent turn.
/// Returns (final_text, memory_ops, was_enhanced).
pub async fn classify_and_run(
    classifier: &Classifier,
    sonnet: &SonnetClient,
    compactor: &Compactor,
    history: &mut Vec<Message>,
    memory: &Memory,
    soul: &str,
    project_context: &str,
    task_db: &Arc<TaskDb>,
    cfg: TurnConfig,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<(String, Vec<MemoryOp>)> {
    // Get the latest user message
    let user_msg = history.last()
        .map(|m| m.content.clone())
        .unwrap_or_default();

    // Derive conversation log path from memory file location
    let log_path = std::path::Path::new(&memory.path)
        .parent()
        .map(|p| p.join("conversation.log"))
        .unwrap_or_else(|| std::path::PathBuf::from("conversation.log"));

    // Skip classification for very short follow-ups or tool-result continuations
    let should_classify = user_msg.len() > 2 && !user_msg.starts_with("[Previous conversation summary]");

    let mut is_complex = false;
    let mut enhanced_msg: Option<String> = None;
    let mut skill_context = String::new();

    match cfg.mode.as_str() {
        "simple" => {
            // Simple mode: skip classification entirely, go straight to primary
            info!(mode = "simple", "Mode: direct pass-through, no classifier");
            let _ = tx.send(AgentEvent::Classified {
                class: "simple".into(),
                detail: "Simple mode — direct to primary model".into(),
            });
        }
        "skills" => {
            // Skills mode: analyze prompt for relevant skills, load them, pass prompt unmodified
            if should_classify {
                info!(mode = "skills", "Mode: skills analysis");
                let _ = tx.send(AgentEvent::Classified {
                    class: "skills".into(),
                    detail: "Analyzing skills needed...".into(),
                });
                match classifier.analyze_skills(&user_msg).await {
                    Ok(ctx) if !ctx.is_empty() => {
                        info!(skill_len = ctx.len(), "Skills loaded successfully");
                        skill_context = ctx;
                    }
                    Ok(_) => {
                        info!("No relevant skills found for this prompt");
                    }
                    Err(e) => {
                        warn!(error = %e, "Skills analysis failed, proceeding without skills");
                    }
                }
            }
        }
        _ => {
            // Supercharge mode (default): existing classification behavior
            if should_classify {
                match classifier.classify(&user_msg).await {
                    Ok(PromptClass::Trivial(answer)) => {
                        info!(class = "trivial", "Classifier: answering directly");
                        let _ = tx.send(AgentEvent::Classified {
                            class: "trivial".into(),
                            detail: "Answering directly with fast model".into(),
                        });
                        let _ = tx.send(AgentEvent::TrivialAnswer(answer.clone()));
                        let _ = tx.send(AgentEvent::Text(answer.clone()));
                        let _ = tx.send(AgentEvent::Done);
                        if cfg.conversation_logging {
                            log_turn(&log_path, &user_msg, None, "classifier (trivial)", &answer);
                        }
                        return Ok((answer, Vec::new()));
                    }
                    Ok(PromptClass::Complex(enhanced)) => {
                        is_complex = true;
                        info!(class = "complex", enhanced_len = enhanced.len(), "Classifier: enhancing prompt");
                        let _ = tx.send(AgentEvent::Classified {
                            class: "enhanced".into(),
                            detail: "Prompt supercharged for better results".into(),
                        });
                        // Replace the last user message with the enhanced version,
                        // but keep the original intent visible
                        if let Some(last) = history.last_mut() {
                            last.content = format!(
                                "[Original request: {}]\n\n{}",
                                user_msg, enhanced
                            );
                        }
                        enhanced_msg = Some(enhanced);
                    }
                    Ok(PromptClass::Simple) => {
                        info!(class = "simple", "Classifier: pass-through");
                        let _ = tx.send(AgentEvent::Classified {
                            class: "simple".into(),
                            detail: "Direct to primary model".into(),
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "Classification failed, falling through to primary");
                    }
                }
            }
        }
    }

    // Plugin hook: on_classified
    if let Some(ref pm) = cfg.plugin_manager {
        let pm = pm.read().await;
        let _ = pm.run_hook("on_classified", &serde_json::json!({
            "mode": cfg.mode,
            "is_complex": is_complex,
            "user_text": user_msg,
        })).await;
    }

    // Inject skill context into soul if skills mode loaded anything
    let effective_soul;
    let soul_ref = if !skill_context.is_empty() {
        effective_soul = format!("{}\n\n[LOADED SKILLS]\n{}", soul, skill_context);
        &effective_soul
    } else {
        soul
    };

    // ── Verification loop for complex tasks ──────────────────────────────
    // Simple/trivial: single pass. Complex: plan → execute → 1 review round max.
    const MAX_VERIFY_ROUNDS: usize = 2;
    let mut all_memory_ops: Vec<MemoryOp> = Vec::new();
    let mut combined_text = String::new();
    let mut combined_tool_log = String::new();

    // Track history length before the loop — messages added during review rounds
    // are ephemeral (assistant drafts + [SYSTEM REVIEW] continuations) and MUST be
    // removed after the loop. Leaving them causes the model to see stale [SYSTEM REVIEW]
    // injections in future turns and misidentify real user messages as prompt injections.
    let history_len_before_loop = history.len();

    for round in 0..MAX_VERIFY_ROUNDS {
        let (text, mem_ops) = run_agent_turn(
            classifier, sonnet, compactor, history, memory, soul_ref, project_context, task_db, &cfg, tx,
        ).await?;

        all_memory_ops.extend(mem_ops);
        combined_text = text.clone();

        // Extract tool log from the text for the reviewer
        if let Some(start) = text.find("<tool_log>") {
            if let Some(end_rel) = text[start..].find("</tool_log>") {
                let log_section = &text[start + 10..start + end_rel];
                if !combined_tool_log.is_empty() {
                    combined_tool_log.push('\n');
                }
                combined_tool_log.push_str(log_section.trim());
            }
        }

        // Append assistant response to history for context in next round.
        // Strip tool_log — reviewer already has it; keeping it inflates context on every turn.
        history.push(Message { role: "assistant".into(), content: strip_tool_log(&text) });

        // Only run quality review loop for complex tasks
        if !is_complex || round >= MAX_VERIFY_ROUNDS - 1 {
            break;
        }

        // Quality review via small model.
        // Strip tool_log from the text — the reviewer already has tool info in combined_tool_log.
        // Without stripping, the 800-char tail sent to the reviewer may be entirely tool entries.
        let feedback = classifier.quality_review(
            &user_msg,
            &combined_tool_log,
            &strip_tool_log(&combined_text),
        ).await;

        match feedback {
            None => {
                info!(round, "Quality review: DONE");
                break;
            }
            Some(what_remains) => {
                info!(round, feedback = %what_remains, "Quality review: CONTINUE");
                let _ = tx.send(AgentEvent::Plan(
                    format!("Review round {}: {}", round + 1, what_remains)
                ));
                // Inject feedback as a follow-up user message for the next round only
                let continuation = format!(
                    "The previous work is not yet complete.\n\
                    What remains: {}\n\n\
                    Continue from where you left off. Do NOT repeat work already done. \
                    Focus only on what's missing.",
                    what_remains
                );
                history.push(Message::user(&continuation));
            }
        }
    }

    // Remove ephemeral review-loop messages (intermediate assistant drafts and
    // continuation prompts). Only the original user message should remain; the
    // final combined text will be added by the caller (server.rs Done handler).
    history.truncate(history_len_before_loop);

    if cfg.conversation_logging {
        log_turn(&log_path, &user_msg, enhanced_msg.as_deref(), sonnet.model_name(), &combined_text);
    }

    // Post-turn: code review + test generation for complex tasks with git-tracked changes
    if is_complex && !combined_tool_log.is_empty() && !cfg.review_model.is_empty() {
        let diff = tokio::process::Command::new("git")
            .args(["diff"])
            .current_dir(&cfg.working_directory)
            .output().await
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();

        if diff.trim().len() > 50 {
            let reviewer = crate::agent::classifier::CodeReviewer::new(
                &cfg.anthropic_key,
                &cfg.openai_key,
                &cfg.review_model,
                &cfg.review_provider,
                Arc::clone(&cfg.http),
            );
            let (review_result, test_result) = tokio::join!(
                reviewer.code_review(&diff, &user_msg),
                reviewer.generate_tests(&diff, &user_msg),
            );
            if let Some(review) = review_result {
                info!("Code review found issues");
                let _ = tx.send(AgentEvent::Text(format!("\n\n**[CODE REVIEW]**\n{}", review)));
                combined_text.push_str(&format!("\n\n[CODE REVIEW]\n{}", review));
            }
            if let Some(tests) = test_result {
                info!("Test suggestions generated");
                let _ = tx.send(AgentEvent::Text(format!("\n\n**[TEST SUGGESTIONS]**\n{}", tests)));
                combined_text.push_str(&format!("\n\n[TEST SUGGESTIONS]\n{}", tests));
            }
        }

        // Auto-run existing project tests to catch regressions
        if let Some((cmd, args)) = detect_test_command(&cfg.working_directory) {
            info!(cmd = %cmd, "Auto-running project tests");
            let _ = tx.send(AgentEvent::Text("\n\n**[RUNNING TESTS]**...".to_string()));
            let out = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                tokio::process::Command::new(&cmd)
                    .args(&args)
                    .current_dir(&cfg.working_directory)
                    .kill_on_drop(true)
                    .output(),
            ).await;
            if let Ok(Ok(output)) = out {
                if output.status.success() {
                    let _ = tx.send(AgentEvent::Text("\n**[TESTS PASSED]**".to_string()));
                    combined_text.push_str("\n\n[TESTS PASSED]");
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let combined_out = format!("{}\n{}", stdout, stderr);
                    let capped = truncate_tail(&combined_out, 3000);
                    let fail_msg = format!("\n\n[TEST FAILURES]\n{}", capped.trim());
                    let _ = tx.send(AgentEvent::Text(fail_msg.clone()));
                    combined_text.push_str(&fail_msg);
                }
            }
        }
    }

    Ok((combined_text, all_memory_ops))
}

/// Run the full agent turn: plan → compaction → tool loop → heartbeat → final text.
pub async fn run_agent_turn(
    classifier: &Classifier,
    sonnet: &SonnetClient,
    compactor: &Compactor,
    history: &[Message],
    memory: &Memory,
    soul: &str,
    project_context: &str,
    task_db: &Arc<TaskDb>,
    cfg: &TurnConfig,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<(String, Vec<MemoryOp>)> {
    // Build active tasks summary (kept compact)
    let tasks_summary = match task_db.list_active_tasks() {
        Ok(tasks) if !tasks.is_empty() => {
            let lines: Vec<String> = tasks.iter().map(|t| {
                format!("  #{} [{}] {}", t.id, t.status, t.title)
            }).collect();
            format!("\n[ACTIVE TASKS]\n{}", lines.join("\n"))
        }
        _ => String::new(),
    };

    // Build system prompt
    let system = format!(
        "{}\n\n[MEMORY]\n{}\n{}\n{}",
        soul, memory.content, project_context, tasks_summary
    );

    // --- Compaction check ---
    let token_est = estimate_tokens(history);
    let mut api_msgs: Vec<serde_json::Value> = if token_est > cfg.token_limit && history.len() > 3 {
        info!(tokens = token_est, limit = cfg.token_limit, "Compacting conversation history");
        let split = history.len() - 3;
        let (old, recent) = history.split_at(split);

        let summary = compactor.compact(old).await.unwrap_or_else(|e| {
            warn!(error = %e, "Compaction failed");
            let _ = tx.send(AgentEvent::Error(
                format!("Context compaction failed ({}). Conversation history may be incomplete.", e)
            ));
            format!("[compaction failed: {}]", e)
        });

        let mut msgs = vec![serde_json::json!({
            "role": "user",
            "content": format!("[Previous conversation summary]\n{}", summary)
        })];
        for m in recent {
            msgs.push(serde_json::json!({"role": m.role, "content": m.content}));
        }
        msgs
    } else {
        history
            .iter()
            .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
            .collect()
    };

    // Token budget warning: if approaching the limit but not yet compacted,
    // hint the model to be concise.
    let system = if token_est > cfg.token_limit * 80 / 100 && token_est <= cfg.token_limit {
        format!("{}\n\n[CONTEXT BUDGET] Conversation is approaching the token limit. Be concise.", system)
    } else {
        system
    };

    let mut memory_ops: Vec<MemoryOp> = Vec::new();
    let mut final_text = String::new();
    let mut iterations = 0;
    let mut consecutive_errors = 0;
    let mut tool_log: Vec<String> = Vec::new();
    let mut nudge_count: usize = 0;
    let mut force_tool_next = false;
    let mut reset_text_on_next = false;
    let mut last_was_tool = false; // true after a tool-use round — triggers cheaper model
    let mut last_model_used = sonnet.model_name().to_string();
    const MAX_NUDGES: usize = 2;

    // Build an optional cheaper continuation client (used after tool results)
    let continuation_client: Option<SonnetClient> =
        if !cfg.continuation_model.is_empty() && cfg.continuation_model != cfg.primary_model {
            Some(SonnetClient::new(
                &cfg.anthropic_key, &cfg.openai_key,
                &cfg.continuation_model, &cfg.continuation_provider,
                4096, Arc::clone(&cfg.http),
            ))
        } else {
            None
        };

    // Extract original user request for heartbeat checks
    let user_request = history.last()
        .map(|m| m.content.clone())
        .unwrap_or_default();

    // --- Tool loop with iteration cap and self-correction ---
    loop {
        // When the heartbeat nudged the previous iteration, discard that response
        // and start fresh. Tool-use continuations must NOT reset (they accumulate
        // text before and after tool calls into one coherent answer).
        if reset_text_on_next {
            final_text.clear();
            reset_text_on_next = false;
            // Tell the UI to discard the already-streamed text — a new attempt is starting.
            let _ = tx.send(AgentEvent::Retry);
        }

        if iterations >= cfg.max_tool_iterations {
            warn!(iterations, "Hit tool iteration limit");
            let _ = tx.send(AgentEvent::Error(
                format!("Stopped after {} tool iterations to prevent runaway.", iterations)
            ));
            break;
        }
        iterations += 1;

        // Create a channel for streaming text deltas
        let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<String>();

        // Spawn forwarder to stream text deltas to the UI in real-time
        let tx_fwd = tx.clone();
        let fwd_handle = tokio::spawn(async move {
            while let Some(chunk) = delta_rx.recv().await {
                let _ = tx_fwd.send(AgentEvent::TextDelta(chunk));
            }
        });

        let active_sonnet = if last_was_tool {
            continuation_client.as_ref().unwrap_or(sonnet)
        } else {
            sonnet
        };
        last_was_tool = false; // reset; set again if this turn ends in tool_use
        last_model_used = active_sonnet.model_name().to_string();

        let response = match active_sonnet.call_streaming(&system, &api_msgs, &delta_tx, force_tool_next).await {
            Ok(r) => {
                consecutive_errors = 0;
                force_tool_next = false;
                drop(delta_tx);
                let _ = fwd_handle.await;
                r
            }
            Err(e) => {
                drop(delta_tx);
                let _ = fwd_handle.await;
                match parse_http_status(&e) {
                    Some(401) | Some(403) => {
                        let _ = tx.send(AgentEvent::Error(
                            format!("API authentication failed — check your API key in Settings. ({})", e)
                        ));
                        break;
                    }
                    Some(429) | Some(529) => {
                        let wait = parse_retry_after(&e.to_string()).unwrap_or(20);
                        warn!(wait, "Rate limited — backing off");
                        let _ = tx.send(AgentEvent::ToolOutput {
                            name: "system".into(),
                            stdout: format!("Rate limited — retrying in {}s...", wait),
                            stderr: String::new(),
                            code: 0,
                        });
                        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                        // Don't increment consecutive_errors, but DO count against iterations
                        // to prevent infinite retry on persistent rate limiting
                        continue;
                    }
                    _ => {
                        consecutive_errors += 1;
                        error!(error = %e, attempt = consecutive_errors, "API call failed");
                        if consecutive_errors > cfg.max_retries {
                            let _ = tx.send(AgentEvent::Error(format!("API failed after {} retries: {}", cfg.max_retries, e)));
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        continue;
                    }
                }
            }
        };

        let stop_reason = response["stop_reason"]
            .as_str()
            .unwrap_or("end_turn")
            .to_string();

        let content = response["content"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        // Append assistant message
        api_msgs.push(serde_json::json!({
            "role": "assistant",
            "content": content
        }));

        // Accumulate text and detect planning markers
        // (text was already streamed to UI; here we just collect for history)
        let any_tool_blocks = content.iter().any(|b| b["type"] == "tool_use");
        for block in &content {
            if block["type"] == "text" {
                if let Some(t) = block["text"].as_str() {
                    if t.contains("<plan>") || t.starts_with("Plan:") || t.starts_with("## Plan") {
                        let _ = tx.send(AgentEvent::Plan(t.to_string()));
                    }
                    final_text.push_str(t);
                }
            }
        }

        if stop_reason == "max_tokens" {
            // Model was cut off mid-response — it didn't choose to stop.
            // Always continue unless we've exhausted nudges.
            info!(
                iteration = iterations,
                stop_reason = "max_tokens",
                final_text_len = final_text.len(),
                nudge_count = nudge_count,
                "Model hit max_tokens — auto-continuing"
            );
            if nudge_count < MAX_NUDGES && iterations < cfg.max_tool_iterations {
                nudge_count += 1;
                api_msgs.push(serde_json::json!({
                    "role": "user",
                    "content": "[SYSTEM] Your response was cut off by the token limit. \
                    Continue EXACTLY where you left off. If you were in the middle of a tool call, \
                    make the tool call now with the complete content. Do not repeat what you already said."
                }));
                // Only force tool use if the model was already using tools (not mid-text)
                force_tool_next = any_tool_blocks;
                continue;
            }
            warn!(nudge_count, "Exhausted max_tokens continuations");
            break;
        }

        if stop_reason != "tool_use" {
            // Model chose to end its turn.
            let any_tools_called = !tool_log.is_empty();
            let tool_log_str = tool_log.join("\n");

            // Heartbeat check: verify the agent actually completed the task.
            // Only meaningful when tools were called AND the agent produced minimal text —
            // i.e. the agent planned/described work but didn't execute it.
            // If the agent already wrote a substantial response (>= 200 chars of visible text),
            // the task was informational and the response should never be nuked.
            let visible_text = strip_think_tags(&final_text);
            let visible_len = visible_text.trim().len();

            const HEARTBEAT_TOOL_THRESHOLD: usize = 2;
            const SUBSTANTIAL_RESPONSE: usize = 200;
            let is_complete = if !any_tools_called {
                true
            } else if tool_log.len() < HEARTBEAT_TOOL_THRESHOLD {
                true // short chain — skip expensive heartbeat
            } else if visible_len >= SUBSTANTIAL_RESPONSE {
                true // agent already wrote a real answer — don't nuke it
            } else if nudge_count >= MAX_NUDGES || iterations >= cfg.max_tool_iterations {
                true // budget exhausted, accept whatever we have
            } else {
                // Timeout heartbeat classifier to avoid hanging on API issues
                match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    classifier.heartbeat(&user_request, &tool_log_str, &final_text),
                ).await {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::warn!("Heartbeat classifier timed out after 15s — assuming complete");
                        true
                    }
                }
            };

            info!(
                iteration = iterations,
                stop_reason = %stop_reason,
                final_text_len = final_text.len(),
                tools_called = any_tools_called,
                nudge_count = nudge_count,
                is_complete = is_complete,
                "Heartbeat check"
            );

            if !is_complete {
                reset_text_on_next = true;
                nudge_count += 1;

                // Cap to last 5 tool entries so the nudge message stays lean.
                const NUDGE_LOG_CAP: usize = 5;
                let nudge_log: String = tool_log.iter()
                    .rev().take(NUDGE_LOG_CAP).rev()
                    .cloned().collect::<Vec<_>>().join("\n");

                let nudge_msg = if any_tools_called {
                    format!(
                        "[SYSTEM] You have not finished the user's request.\n\
                        Tools executed so far:\n{}\n\
                        Review what the user asked and complete the remaining work. \
                        Do NOT repeat tools you already called — use those results to finish.",
                        nudge_log
                    )
                } else {
                    "[SYSTEM] You described what you will do but did not call any tools.\n\
                    Call write_file, run_command, or other tools NOW to complete the task.\n\
                    Do NOT produce more text or explanations — respond ONLY with tool_use blocks.".to_string()
                };

                info!(nudge = nudge_count, "Heartbeat: incomplete — nudging agent");
                api_msgs.push(serde_json::json!({
                    "role": "user",
                    "content": nudge_msg
                }));
                force_tool_next = !any_tools_called;
                continue;
            }
            break;
        }

        // Execute tool calls in parallel (single-pass filter + spawn)
        let mut handles = Vec::new();
        let mut subagent_calls: Vec<(String, String, serde_json::Value)> = Vec::new();
        let tool_blocks: Vec<_> = content.iter().filter(|b| b["type"] == "tool_use").collect();
        if tool_blocks.is_empty() {
            // Malformed response: stop_reason was tool_use but no tool_use blocks found
            warn!("stop_reason=tool_use but no tool_use blocks in response");
            break;
        }
        for block in tool_blocks {
            let tool_id = block["id"].as_str().unwrap_or("").to_string();
            let tool_name = block["name"].as_str().unwrap_or("").to_string();
            let input = block["input"].clone();

            if tool_id.is_empty() {
                warn!("Skipping tool_use block with empty id (name={})", tool_name);
                continue;
            }

            let _ = tx.send(AgentEvent::ToolCall {
                name: tool_name.clone(),
                input: input.to_string(),
            });

            // Plugin hook: on_tool_before
            if let Some(ref pm) = cfg.plugin_manager {
                let pm = pm.read().await;
                let _ = pm.run_hook("on_tool_before", &serde_json::json!({
                    "tool": tool_name,
                    "input": input,
                })).await;
            }

            if tool_name == "run_subagent" {
                subagent_calls.push((tool_id, tool_name, input));
            } else if tool_name == "ask_user" || tool_name == "plan_file_changes" {
                // These tools wait for user interaction — must NOT use the generic
                // 90-second spawn timeout. Run them inline after parallel tools finish.
                subagent_calls.push((tool_id, tool_name, input));
            } else {
                let tx_clone = tx.clone();
                let task_db_clone = Arc::clone(task_db);
                let cfg_clone = cfg.clone();
                let tool_id_for_timeout = tool_id.clone();
                let handle = tokio::spawn(async move {
                    let result = execute_tool(
                        &tool_name, &input, &cfg_clone, &task_db_clone, &tx_clone,
                    ).await;
                    (tool_id, tool_name, result)
                });
                handles.push((tool_id_for_timeout, handle));
            }
        }

        // Collect all results
        let mut tool_results: Vec<serde_json::Value> = Vec::new();
        let mut had_errors = false;
        for (timeout_id, handle) in handles {
            let abort = handle.abort_handle();
            match tokio::time::timeout(std::time::Duration::from_secs(90), handle).await {
                Ok(Ok((tool_id, tool_name, result))) => {
                    match result {
                        Ok((text, maybe_op)) => {
                            if let Some(op) = maybe_op {
                                memory_ops.push(op);
                            }
                            let truncated = if text.len() > 200 {
                                let mut b = 200;
                                while b > 0 && !text.is_char_boundary(b) { b -= 1; }
                                format!("{}...", &text[..b])
                            } else {
                                text.clone()
                            };
                            tool_log.push(format!("✓ {}(…) → {}", tool_name, truncated));
                            info!(tool = %tool_name, result_len = text.len(), "Tool completed");
                            // Plugin hook: on_tool_after (parallel success)
                            if let Some(ref pm) = cfg.plugin_manager {
                                let pm = pm.read().await;
                                let _ = pm.run_hook("on_tool_after", &serde_json::json!({
                                    "tool": tool_name,
                                    "success": true,
                                    "result": if text.len() > 1000 { &text[..1000] } else { text.as_str() },
                                })).await;
                            }
                            tool_results.push(serde_json::json!({
                                "type": "tool_result",
                                "tool_use_id": tool_id,
                                "content": strip_ansi(&text),
                            }));
                        }
                        Err(e) => {
                            had_errors = true;
                            let err_text = format!("Error: {}", e);
                            tool_log.push(format!("✗ {}(…) → {}", tool_name, err_text));
                            info!(tool = %tool_name, error = %e, "Tool failed");
                            // Plugin hook: on_tool_after (parallel error)
                            if let Some(ref pm) = cfg.plugin_manager {
                                let pm = pm.read().await;
                                let _ = pm.run_hook("on_tool_after", &serde_json::json!({
                                    "tool": tool_name,
                                    "success": false,
                                    "result": err_text,
                                })).await;
                            }
                            tool_results.push(serde_json::json!({
                                "type": "tool_result",
                                "tool_use_id": tool_id,
                                "content": err_text,
                            }));
                        }
                    }
                }
                Ok(Err(e)) => {
                    error!(error = %e, "Tool task panicked");
                    had_errors = true;
                    let _ = tx.send(AgentEvent::Error(format!("Tool crashed: {}", e)));
                    // Still push a tool_result so the tool_use block is never orphaned
                    tool_results.push(serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": timeout_id,
                        "content": format!("Tool crashed: {}", e),
                    }));
                }
                Err(_elapsed) => {
                    warn!("Tool timed out after 90s");
                    abort.abort(); // Kill the orphaned task
                    had_errors = true;
                    tool_results.push(serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": timeout_id,
                        "content": "Tool timed out after 90s.",
                    }));
                }
            }
        }

        // Run subagent / interactive tool requests directly (not in spawn — avoids Send
        // constraint for subagents, and avoids the 90-second timeout for ask_user/plan_file_changes).
        for (tool_id, tool_name, input) in subagent_calls {
            let result = if tool_name == "run_subagent" {
                let task = input["task"].as_str().unwrap_or("").to_string();
                match tokio::time::timeout(
                    std::time::Duration::from_secs(300),
                    run_subagent_task(&task, cfg, task_db, tx),
                ).await {
                    Ok(r) => r,
                    Err(_) => "Error: subagent timed out after 5 minutes.".to_string(),
                }
            } else {
                // ask_user / plan_file_changes — run inline with no wrapping timeout
                // (they have their own internal timeouts: 300s / 120s)
                match execute_tool(&tool_name, &input, cfg, task_db, tx).await {
                    Ok((text, maybe_op)) => {
                        if let Some(op) = maybe_op {
                            memory_ops.push(op);
                        }
                        text
                    }
                    Err(e) => {
                        had_errors = true;
                        format!("Error: {}", e)
                    }
                }
            };
            let truncated = if result.len() > 200 {
                let mut b = 200; while b > 0 && !result.is_char_boundary(b) { b -= 1; }
                format!("{}...", &result[..b])
            } else { result.clone() };
            tool_log.push(format!("✓ {}(…) → {}", tool_name, truncated));
            info!(tool = %tool_name, result_len = result.len(), "Inline tool completed");
            // Plugin hook: on_tool_after (inline)
            if let Some(ref pm) = cfg.plugin_manager {
                let pm = pm.read().await;
                let _ = pm.run_hook("on_tool_after", &serde_json::json!({
                    "tool": tool_name,
                    "success": true,
                    "result": if result.len() > 1000 { &result[..1000] } else { result.as_str() },
                })).await;
            }
            tool_results.push(serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_id,
                "content": result,
            }));
        }

        // Self-correction: if tools had errors, inject reflection into the tool_results message.
        if had_errors && consecutive_errors < cfg.max_retries {
            info!("Tool errors detected, enabling self-correction");
            tool_results.push(serde_json::json!({
                "type": "text",
                "text": "Some tools had errors. Analyze what went wrong and try a different approach."
            }));
        }

        // Warn the model when approaching the tool iteration limit so it can wrap up gracefully.
        if iterations >= cfg.max_tool_iterations.saturating_sub(3) {
            let remaining = cfg.max_tool_iterations.saturating_sub(iterations);
            tool_results.push(serde_json::json!({
                "type": "text",
                "text": format!(
                    "[SYSTEM] You have {} iteration(s) remaining before the hard limit. \
                    Wrap up now: finish any in-progress work and provide your final response to the user. \
                    Do not start new tool chains.",
                    remaining
                )
            }));
        }

        // Add tool results as a user message (tool_result blocks + optional text)
        api_msgs.push(serde_json::json!({
            "role": "user",
            "content": tool_results,
        }));
        last_was_tool = true; // next call is a continuation → use cheaper model
    }

    // Append tool execution summary to final text so history preserves what happened.
    // This prevents the "hallucination loop" where the model keeps re-planning because
    // it can't see its prior tool results in flat text history.
    if !tool_log.is_empty() {
        final_text.push_str("\n\n<tool_log>\n");
        for entry in &tool_log {
            final_text.push_str(entry);
            final_text.push('\n');
        }
        final_text.push_str("</tool_log>");
    }

    // Check if final_text has any visible content after stripping internal tags.
    // If only tool work was done and the model produced no user-visible text,
    // synthesize a brief summary so the user sees something.
    let visible = compress_tool_log(&strip_think_tags(&final_text));
    if visible.trim().is_empty() && !tool_log.is_empty() {
        let summary = format!("Done. (used {} tool{})", tool_log.len(), if tool_log.len() == 1 { "" } else { "s" });
        final_text.push_str(&summary);
    }

    let _ = tx.send(AgentEvent::Text(final_text.clone()));
    let _ = tx.send(AgentEvent::ModelUsed(last_model_used));
    let _ = tx.send(AgentEvent::Done);

    info!(iterations, text_len = final_text.len(), mem_ops = memory_ops.len(), "Agent turn complete");
    Ok((final_text, memory_ops))
}

/// Maximum file size for read_file tool (1 MB).
const MAX_READ_FILE_SIZE: u64 = 1_048_576;

/// Paths that the agent must not write to.
fn is_write_safe(path: &str) -> bool {
    let resolved = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => {
            // File doesn't exist yet — canonicalize parent
            if let Some(parent) = std::path::Path::new(path).parent() {
                let file_name = match std::path::Path::new(path).file_name() {
                    Some(f) => f,
                    None => return false,
                };
                match std::fs::canonicalize(parent) {
                    Ok(p) => p.join(file_name),
                    Err(_) => return false,
                }
            } else {
                return false;
            }
        }
    };
    let s = resolved.to_string_lossy();
    let blocked = [
        "/etc", "/usr", "/boot", "/sys", "/proc",
        "/var/run", "/var/log", "/var/lib", "/var/spool",
        "/sbin", "/bin", "/lib", "/lib64",
    ];
    for b in &blocked {
        if s.starts_with(b) {
            return false;
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && !s.starts_with(&home) {
        return false;
    }
    let blocked_home = [".ssh", ".gnupg"];
    for dir in &blocked_home {
        if s.starts_with(&format!("{}/{}", home, dir)) {
            return false;
        }
    }
    true
}

/// Paths that the agent must not read (sensitive system/user files).
fn is_read_safe(path: &str) -> bool {
    let resolved = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => return true, // file doesn't exist — let the read error naturally
    };
    let s = resolved.to_string_lossy();
    let blocked_exact = ["/etc/shadow", "/etc/gshadow"];
    for b in &blocked_exact {
        if s.as_ref() == *b { return false; }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        let blocked_home = [".ssh", ".gnupg", ".aws", ".kube"];
        for dir in &blocked_home {
            if s.starts_with(&format!("{}/{}", home, dir)) {
                return false;
            }
        }
    }
    true
}
// ── patch_file helpers ─────────────────────────────────────────────────────

/// Return lines of `s` with common leading whitespace removed.
fn normalize_indent(s: &str) -> Vec<String> {
    let lines: Vec<&str> = s.lines().collect();
    let min_ws = lines.iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines.iter()
        .map(|l| if l.len() >= min_ws { l[min_ws..].trim_end().to_string() }
                 else { l.trim_end().to_string() })
        .collect()
}

/// Apply old_text→new_text in `content`, at the nth occurrence (1-based).
/// Tries exact match first, then whitespace-normalised line comparison.
/// Returns the patched content or a descriptive error string with context.
fn patch_apply(content: &str, old_text: &str, new_text: &str, nth: usize) -> Result<String, String> {
    if old_text.is_empty() {
        return Err("Error: old_text is empty.".to_string());
    }

    // ── 1. Exact byte match ───────────────────────────────────────────────
    let mut exact_positions: Vec<usize> = Vec::new();
    let mut search = 0;
    while let Some(found) = content[search..].find(old_text) {
        let abs = search + found;
        exact_positions.push(abs);
        search = abs + old_text.len().max(1);
    }
    if !exact_positions.is_empty() {
        if nth == 0 || nth > exact_positions.len() {
            return Err(format!(
                "Error: occurrence {} requested but {} found.",
                nth, exact_positions.len()
            ));
        }
        let idx = exact_positions[nth - 1];
        let mut out = content.to_string();
        out.replace_range(idx..idx + old_text.len(), new_text);
        return Ok(out);
    }

    // ── 2. Whitespace-normalised line match ────────────────────────────────
    let norm_old = normalize_indent(old_text);
    let n = norm_old.len();
    if n == 0 {
        return Err("Error: old_text has no non-empty lines.".to_string());
    }

    let file_lines: Vec<&str> = content.lines().collect();
    let mut match_starts: Vec<usize> = Vec::new();

    if n > file_lines.len() {
        return Err(format!(
            "Error: old_text has {} lines but the file only has {} lines.",
            n, file_lines.len()
        ));
    }

    // Pre-strip each line for fast rejection: most windows fail on trimmed content
    // before we pay the cost of join + normalize_indent per window.
    let file_trimmed: Vec<&str> = file_lines.iter().map(|l| l.trim()).collect();
    let old_trimmed: Vec<&str>  = norm_old.iter().map(|l| l.trim()).collect();

    for start in 0..=file_lines.len().saturating_sub(n) {
        // O(1) per window for the common false case
        if file_trimmed[start..start + n] != old_trimmed[..n] {
            continue;
        }
        // Full normalize only for candidate windows (rare)
        let window = file_lines[start..start + n].join("\n");
        let norm_win = normalize_indent(&window);
        if norm_win == norm_old {
            match_starts.push(start);
        }
    }

    if match_starts.is_empty() {
        // Give a helpful hint: show lines that share at least one non-empty token
        let first_sig = norm_old.iter()
            .find(|l| !l.trim().is_empty())
            .cloned()
            .unwrap_or_default();
        let hint = if !first_sig.is_empty() {
            let similar: Vec<String> = file_lines.iter().enumerate()
                .filter(|(_, l)| l.contains(first_sig.trim()))
                .take(3)
                .map(|(i, l)| format!("  line {}: {}", i + 1, l.trim()))
                .collect();
            if similar.is_empty() {
                String::new()
            } else {
                format!("\nSimilar lines found:\n{}", similar.join("\n"))
            }
        } else {
            String::new()
        };
        return Err(format!(
            "Error: old_text not found (exact or whitespace-normalised).{}", hint
        ));
    }

    if nth == 0 || nth > match_starts.len() {
        return Err(format!(
            "Error: occurrence {} requested but {} found.", nth, match_starts.len()
        ));
    }

    let start = match_starts[nth - 1];
    let end = start + n;

    // Preserve the original indentation of the first matched line
    let orig_indent = {
        let first = file_lines[start];
        let ws = first.len() - first.trim_start().len();
        " ".repeat(ws)
    };

    // Re-indent new_text: first line keeps its own indent, subsequent lines
    // get orig_indent prepended after stripping the old_text's own indent
    let new_norm = normalize_indent(new_text);
    let reindented: Vec<String> = new_norm.iter().map(|line| {
        if line.trim().is_empty() {
            String::new()
        } else {
            format!("{}{}", orig_indent, line)
        }
    }).collect();

    let before = if start > 0 { file_lines[..start].join("\n") } else { String::new() };
    let after  = if end < file_lines.len() { file_lines[end..].join("\n") } else { String::new() };
    let mid = reindented.join("\n");

    let mut result = String::new();
    if !before.is_empty() { result.push_str(&before); result.push('\n'); }
    result.push_str(&mid);
    if !after.is_empty()  { result.push('\n'); result.push_str(&after); }
    if content.ends_with('\n') && !result.ends_with('\n') { result.push('\n'); }

    Ok(result)
}

/// Replace lines start_line..=end_line (1-based, inclusive) with new_text.
fn patch_line_range(content: &str, start_line: usize, end_line: usize, new_text: &str) -> Result<String, String> {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if start_line < 1 || start_line > total {
        return Err(format!("Error: start_line {} out of range (file has {} lines).", start_line, total));
    }
    let s = start_line - 1;
    let e = end_line.min(total);
    let before = lines[..s].join("\n");
    let after  = if e < total { lines[e..].join("\n") } else { String::new() };
    let mut result = if before.is_empty() { String::new() } else { format!("{}\n", before) };
    result.push_str(new_text);
    if !after.is_empty() { result.push('\n'); result.push_str(&after); }
    if content.ends_with('\n') && !result.ends_with('\n') { result.push('\n'); }
    Ok(result)
}

/// Execute a single tool and return (result_string, optional_memory_op).
async fn execute_tool(
    tool_name: &str,
    input: &serde_json::Value,
    cfg: &TurnConfig,
    task_db: &Arc<TaskDb>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<(String, Option<MemoryOp>)> {
    let terminal_timeout = cfg.terminal_timeout;
    let max_output_chars = cfg.max_output_chars;
    let sudo_password = cfg.sudo_password.as_str();
    let working_directory = cfg.working_directory.as_str();
    let smtp_config = tools::email::SmtpConfig {
        host: cfg.smtp_host.clone(),
        port: cfg.smtp_port,
        user: cfg.smtp_user.clone(),
        password: cfg.smtp_password.clone(),
        from: cfg.smtp_from.clone(),
    };
    let telegram_bot_token = cfg.telegram_bot_token.as_str();

    // Resolve ~ to home directory
    let work_dir = if working_directory.is_empty() || working_directory == "~" {
        std::env::var("HOME").unwrap_or_default()
    } else if working_directory.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/{}", home, &working_directory[2..])
    } else {
        working_directory.to_string()
    };
    let wd = if work_dir.is_empty() { None } else { Some(work_dir.as_str()) };

    let result = match tool_name {
        "run_command" => {
            let cmd = input["command"].as_str().unwrap_or("");
            let pw = if sudo_password.is_empty() { None } else { Some(sudo_password) };

            // Detect reboot/shutdown — run with a 5s delay so the agent can finish
            // its turn and save the response before the system goes down.
            let is_system_power = {
                let c = cmd.trim().to_lowercase();
                (c.contains("reboot") || c.contains("shutdown") || c.contains("init 6") || c.contains("systemctl reboot"))
                && !c.starts_with('#') && !c.starts_with("echo")
            };
            let (effective_cmd, is_deferred) = if is_system_power {
                // Strip sudo prefix so we can rebuild with password if needed
                let base = cmd.trim()
                    .trim_start_matches("sudo ")
                    .trim_start_matches("sudo  ");
                let deferred = format!("(sleep 5 && sudo {}) &", base);
                (deferred, true)
            } else {
                (cmd.to_string(), false)
            };

            match tools::terminal::run_command(&effective_cmd, terminal_timeout, pw, wd).await {
                Ok((stdout, stderr, code)) => {
                    let (display_stdout, display_stderr) = if is_deferred {
                        ("Scheduled in 5 seconds.".into(), String::new())
                    } else {
                        (truncate_tail(&stdout, 2000), truncate_tail(&stderr, 1000))
                    };
                    let _ = tx.send(AgentEvent::ToolOutput {
                        name: "run_command".into(),
                        stdout: display_stdout.clone(),
                        stderr: display_stderr.clone(),
                        code,
                    });
                    format!("exit code: {}\nstdout:\n{}\nstderr:\n{}", code, display_stdout, display_stderr)
                }
                Err(e) => format!("Error: {}", e),
            }
        }
        "read_file" => {
            let raw_path = input["path"].as_str().unwrap_or("");
            let path = resolve_path(raw_path, wd);
            let path = path.as_str();
            if !is_read_safe(path) {
                format!("Error: reading {} is blocked for safety.", path)
            } else {
            // File size guard
            match std::fs::metadata(path) {
                Ok(meta) if meta.len() > MAX_READ_FILE_SIZE => {
                    format!(
                        "Error: file is {} bytes (max {}). Use start_line/end_line for large files.",
                        meta.len(),
                        MAX_READ_FILE_SIZE
                    )
                }
                Err(e) => format!("Error reading {}: {}", path, e),
                _ => {
                    match std::fs::read_to_string(path) {
                        Ok(c) => {
                            let start = input["start_line"].as_u64().unwrap_or(0) as usize;
                            let end = input["end_line"].as_u64().unwrap_or(0) as usize;
                            let all_lines: Vec<&str> = c.lines().collect();
                            let (slice, offset) = if start > 0 {
                                let s = (start - 1).min(all_lines.len());
                                let e = if end > 0 { end.min(all_lines.len()) } else { all_lines.len() };
                                if s > e {
                                    return Ok((format!("Error: invalid line range {}..{}", start, end), None));
                                }
                                (&all_lines[s..e], s)
                            } else {
                                (&all_lines[..], 0)
                            };
                            let numbered = input["numbered"].as_bool().unwrap_or(true);
                            if numbered {
                                add_line_numbers(slice, offset + 1)
                            } else {
                                slice.join("\n")
                            }
                        }
                        Err(e) => format!("Error reading {}: {}", path, e),
                    }
                }
            }
            }
        }
        "write_file" => {
            let raw_path = input["path"].as_str().unwrap_or("");
            let path = resolve_path(raw_path, wd);
            let path = path.as_str();
            if !is_write_safe(path) {
                format!("Error: writing to {} is blocked for safety.", path)
            } else {
                let file_content = input["content"].as_str().unwrap_or("");
                if let Some(parent) = std::path::Path::new(path).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(path, file_content) {
                    Ok(_) => {
                        let note = verify_file_syntax(path).await.unwrap_or_default();
                        if note.is_empty() { format!("Written to {}", path) }
                        else { format!("Written to {}\n{}", path, note) }
                    }
                    Err(e) => format!("Error writing {}: {}", path, e),
                }
            }
        }
        "patch_file" => {
            let raw_path = input["path"].as_str().unwrap_or("");
            let path = resolve_path(raw_path, wd);
            let path = path.as_str();
            if !is_write_safe(path) {
                format!("Error: writing to {} is blocked for safety.", path)
            } else {
                let old_text = input["old_text"].as_str().unwrap_or("");
                let new_text = input["new_text"].as_str().unwrap_or("");
                let start_line = input["start_line"].as_u64().map(|n| n as usize);
                let end_line   = input["end_line"].as_u64().map(|n| n as usize);
                let occurrence = input["occurrence"].as_u64().unwrap_or(1) as usize;
                let replace_all_occ = input["occurrence"].as_str()
                    .map(|s| s == "all")
                    .unwrap_or(false);

                match std::fs::read_to_string(path) {
                    Ok(content) => {
                        let result = if let (Some(sl), Some(el)) = (start_line, end_line) {
                            patch_line_range(&content, sl, el, new_text)
                        } else if replace_all_occ {
                            if old_text.is_empty() {
                                Err("Error: old_text required for occurrence=all.".to_string())
                            } else {
                                Ok(content.replace(old_text, new_text))
                            }
                        } else {
                            patch_apply(&content, old_text, new_text, occurrence)
                        };
                        match result {
                            Ok(updated) => match std::fs::write(path, &updated) {
                                Ok(_) => {
                                    let note = verify_file_syntax(path).await.unwrap_or_default();
                                    if note.is_empty() { format!("Patched {}", path) }
                                    else { format!("Patched {}\n{}", path, note) }
                                }
                                Err(e) => format!("Error writing {}: {}", path, e),
                            },
                            Err(e) => e,
                        }
                    }
                    Err(e) => format!("Error reading {}: {}", path, e),
                }
            }
        }
        "append_file" => {
            let raw_path = input["path"].as_str().unwrap_or("");
            let path = resolve_path(raw_path, wd);
            let path = path.as_str();
            if !is_write_safe(path) {
                format!("Error: writing to {} is blocked for safety.", path)
            } else {
                let content = input["content"].as_str().unwrap_or("");
                let after_marker = input["after"].as_str();
                if let Some(parent) = std::path::Path::new(path).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let write_result = if let Some(marker) = after_marker {
                    // Insert content after the first occurrence of `marker`
                    match std::fs::read_to_string(path) {
                        Ok(existing) => {
                            if let Some(pos) = existing.find(marker) {
                                let eol = existing[pos..].find('\n')
                                    .map(|i| pos + i + 1)
                                    .unwrap_or(existing.len());
                                let mut new_content = existing[..eol].to_string();
                                new_content.push_str(content);
                                if !content.ends_with('\n') { new_content.push('\n'); }
                                new_content.push_str(&existing[eol..]);
                                std::fs::write(path, &new_content)
                                    .map(|_| format!("Inserted after marker in {}", path))
                                    .unwrap_or_else(|e| format!("Error writing {}: {}", path, e))
                            } else {
                                format!("Error: marker {:?} not found in {}", marker, path)
                            }
                        }
                        Err(_) => {
                            // File doesn't exist yet — just write
                            std::fs::write(path, content)
                                .map(|_| format!("Written to {}", path))
                                .unwrap_or_else(|e| format!("Error writing {}: {}", path, e))
                        }
                    }
                } else {
                    // Pure append to end of file
                    use std::io::Write as _;
                    std::fs::OpenOptions::new().create(true).append(true).open(path)
                        .and_then(|mut f| {
                            f.write_all(content.as_bytes())?;
                            if !content.ends_with('\n') { f.write_all(b"\n")?; }
                            Ok(format!("Appended to {}", path))
                        })
                        .unwrap_or_else(|e| format!("Error opening {}: {}", path, e))
                };
                // Syntax-check if succeeded
                if write_result.starts_with("Error") {
                    write_result
                } else {
                    let note = verify_file_syntax(path).await.unwrap_or_default();
                    if note.is_empty() { write_result }
                    else { format!("{}\n{}", write_result, note) }
                }
            }
        }
        "search_files" => {
            let pattern = input["pattern"].as_str().unwrap_or("");
            let raw_path = input["path"].as_str().unwrap_or("");
            let search_path = if raw_path.is_empty() { work_dir.clone() } else { resolve_path(raw_path, wd) };
            let include = input["include"].as_str().unwrap_or("");
            tools::search::search_files(pattern, &search_path, include).await
        }
        "list_directory" => {
            let raw_path = input["path"].as_str().unwrap_or("");
            let path = if raw_path.is_empty() { work_dir.clone() } else { resolve_path(raw_path, wd) };
            tools::search::list_directory(&path)
        }
        "scan_project" => {
            let raw_path = input["path"].as_str().unwrap_or("");
            let path = if raw_path.is_empty() { work_dir.clone() } else { resolve_path(raw_path, wd) };
            let depth = input["max_depth"].as_u64().unwrap_or(4) as usize;
            crate::tools::project::scan_project(&path, depth)
        }
        "browse_url" => {
            let url = input["url"].as_str().unwrap_or("");
            match tools::browser::browse_url(url).await {
                Ok(text) => text,
                Err(e) => format!("Error browsing {}: {}", url, e),
            }
        }
        "web_search" => {
            let query = input["query"].as_str().unwrap_or("");
            match tools::browser::web_search(query, &cfg.http).await {
                Ok(results) => results,
                Err(e) => format!("Search error: {}", e),
            }
        }
        "git_command" => {
            let args = input["args"].as_str().unwrap_or("status");
            // Reject shell metacharacters to prevent command injection
            let has_shell_meta = args.chars().any(|c| matches!(c, ';' | '|' | '&' | '$' | '`' | '(' | ')' | '{' | '}' | '<' | '>' | '\n'));
            if has_shell_meta {
                "Error: git args contain shell metacharacters. Use simple git arguments only.".to_string()
            } else {
            let full_cmd = format!("git {}", args);
            match tools::terminal::run_command(&full_cmd, 30, None, wd).await {
                Ok((stdout, stderr, code)) => {
                    let _ = tx.send(AgentEvent::ToolOutput {
                        name: "git_command".into(),
                        stdout: truncate_str(&stdout, 2000),
                        stderr: truncate_str(&stderr, 1000),
                        code,
                    });
                    format!("exit: {}\n{}{}", code, stdout, stderr)
                }
                Err(e) => format!("Git error: {}", e),
            }
            }
        }
        "task_manage" => {
            let action = input["action"].as_str().unwrap_or("list");
            match action {
                "create" => {
                    let title = input["title"].as_str().unwrap_or("Untitled");
                    let context = input["context"].as_str().unwrap_or("");
                    match task_db.create_task(title, context) {
                        Ok(id) => format!("Task #{} created: {}", id, title),
                        Err(e) => format!("Error creating task: {}", e),
                    }
                }
                "update_status" => {
                    let task_id = input["task_id"].as_i64().unwrap_or(0);
                    let status = input["status"].as_str().unwrap_or("done");
                    match task_db.update_task_status(task_id, status) {
                        Ok(_) => format!("Task #{} → {}", task_id, status),
                        Err(e) => format!("Error: {}", e),
                    }
                }
                "list" => {
                    match task_db.list_recent_tasks(10) {
                        Ok(tasks) => {
                            if tasks.is_empty() {
                                "No tasks.".to_string()
                            } else {
                                tasks.iter()
                                    .map(|t| format!("#{} [{}] {} — {}", t.id, t.status, t.title, t.context))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            }
                        }
                        Err(e) => format!("Error: {}", e),
                    }
                }
                _ => format!("Unknown task action: {}", action),
            }
        }
        "update_memory" => {
            let action = input["action"].as_str().unwrap_or("append");
            let section = input["section"].as_str().unwrap_or("");
            let mem_content = input["content"].as_str().unwrap_or("");
            let _ = tx.send(AgentEvent::MemoryUpdate {
                section: section.to_string(),
                content: mem_content.to_string(),
            });
            let op = MemoryOp {
                action: action.to_string(),
                section: section.to_string(),
                content: mem_content.to_string(),
            };
            return Ok((
                truncate_str(
                    &format!("Memory section '{}' updated ({}).", section, action),
                    max_output_chars,
                ),
                Some(op),
            ));
        }
        "ask_user" => {
            let question = input["question"].as_str().unwrap_or("Can you clarify?");
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(AgentEvent::AskUser {
                question: question.to_string(),
                reply_tx,
            });
            // Wait for user response (with 5 minute timeout)
            match tokio::time::timeout(
                std::time::Duration::from_secs(300),
                reply_rx,
            )
            .await
            {
                Ok(Ok(reply)) => format!("User replied: {}", reply),
                Ok(Err(_)) => "User did not reply (connection may have closed).".to_string(),
                Err(_) => "User did not reply within 5 minutes.".to_string(),
            }
        }
        "plan_file_changes" => {
            let changes = match input["changes"].as_array() {
                Some(a) if !a.is_empty() => a.clone(),
                _ => return Ok((format!("Error: 'changes' array is required."), None)),
            };
            let mut plan = String::from("**Planned file changes:**\n\n");
            for change in &changes {
                let file   = change["file"].as_str().unwrap_or("?");
                let action = change["action"].as_str().unwrap_or("modify");
                let desc   = change["description"].as_str().unwrap_or("");
                plan.push_str(&format!("  [{action}] `{file}` — {desc}\n"));
            }
            plan.push_str("\nProceed with these changes? (yes/no)");
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(AgentEvent::AskUser { question: plan, reply_tx });
            match tokio::time::timeout(std::time::Duration::from_secs(120), reply_rx).await {
                Ok(Ok(reply)) => {
                    let r = reply.trim().to_lowercase();
                    if r == "yes" || r == "y" || r.contains("ok") || r.contains("proceed") || r.contains("sure") {
                        format!("Plan approved — proceed with all {} changes.", changes.len())
                    } else {
                        format!("Plan rejected: {}. Stop and ask the user what to change.", reply)
                    }
                }
                Ok(Err(_)) => "User did not reply (connection closed).".to_string(),
                Err(_) => "Timed out waiting for plan approval — aborting.".to_string(),
            }
        }
        "update_project_knowledge" => {
            let content = input["content"].as_str().unwrap_or("").trim().to_string();
            let section = input["section"].as_str().unwrap_or("").trim().to_string();
            if content.is_empty() {
                return Ok(("Error: content is required.".to_string(), None));
            }
            let base = wd.as_deref().unwrap_or(".");
            let axium_dir = std::path::Path::new(base).join(".axium");
            let _ = std::fs::create_dir_all(&axium_dir);
            let knowledge_path = axium_dir.join("knowledge.md");
            let existing = std::fs::read_to_string(&knowledge_path).unwrap_or_default();
            let new_content = if section.is_empty() {
                format!("{}\n{}\n", existing.trim_end(), content)
            } else {
                format!("{}\n## {}\n{}\n", existing.trim_end(), section, content)
            };
            match std::fs::write(&knowledge_path, new_content.trim_start()) {
                Ok(_) => format!("Project knowledge saved to {}", knowledge_path.display()),
                Err(e) => format!("Error writing project knowledge: {}", e),
            }
        }
        "set_autonomous" => {
            let enabled = input["enabled"].as_bool().unwrap_or(false);
            let _ = tx.send(AgentEvent::SetAutonomous { enabled });
            if enabled {
                "Autonomous mode enabled — I will continue working through the task without waiting for user input after each step.".to_string()
            } else {
                "Autonomous mode disabled — I will pause for user input after each step.".to_string()
            }
        }
        "queue_task" => {
            let title = input["title"].as_str().unwrap_or("").to_string();
            let context = input["context"].as_str().unwrap_or("").to_string();
            if title.is_empty() {
                return Ok(("Error: title is required.".to_string(), None));
            }
            match task_db.create_task(&title, &context) {
                Ok(id) => {
                    let _ = tx.send(AgentEvent::TaskQueued { id, title: title.clone() });
                    format!("Task #{} queued: {}", id, title)
                }
                Err(e) => format!("Error creating task: {}", e),
            }
        }
        "get_diagnostics" => {
            let path = input["path"].as_str().unwrap_or("");
            let full_path = resolve_path(path, wd);
            let p = std::path::Path::new(&full_path);
            if p.is_file() {
                // Single-file diagnostics
                match verify_file_syntax(&full_path).await {
                    Some(msg) => msg,
                    None => format!("No issues found in {}", full_path),
                }
            } else if p.is_dir() {
                // Project-level diagnostics
                let mut results = Vec::new();
                if p.join("Cargo.toml").exists() {
                    let out = tokio::process::Command::new("cargo")
                        .args(["check", "--message-format=short"])
                        .current_dir(&full_path)
                        .output().await;
                    if let Ok(o) = out {
                        if !o.status.success() {
                            let stderr = String::from_utf8_lossy(&o.stderr);
                            for line in stderr.lines().filter(|l| l.contains("error") || l.contains("warning[")).take(30) {
                                results.push(line.to_string());
                            }
                        }
                    }
                } else if p.join("go.mod").exists() {
                    let out = tokio::process::Command::new("go")
                        .args(["vet", "./..."])
                        .current_dir(&full_path)
                        .output().await;
                    if let Ok(o) = out {
                        if !o.status.success() {
                            results.push(String::from_utf8_lossy(&o.stderr).trim().to_string());
                        }
                    }
                } else if p.join("package.json").exists() {
                    // Try eslint if available
                    let out = tokio::process::Command::new("npx")
                        .args(["--no-install", "eslint", ".", "--format=compact", "--no-color"])
                        .current_dir(&full_path)
                        .output().await;
                    if let Ok(o) = out {
                        if !o.status.success() {
                            let stdout = String::from_utf8_lossy(&o.stdout);
                            for line in stdout.lines().take(30) {
                                results.push(line.to_string());
                            }
                        }
                    }
                }
                if results.is_empty() {
                    format!("No issues found in {}", full_path)
                } else {
                    results.join("\n")
                }
            } else {
                format!("Path not found: {}", full_path)
            }
        }
        "delete_file" => {
            let path = input["path"].as_str().unwrap_or("");
            let full_path = resolve_path(path, wd);
            if !is_write_safe(&full_path) {
                format!("Error: deleting {} is blocked for safety.", full_path)
            } else {
                let p = std::path::Path::new(&full_path);
                if p.is_file() {
                    match std::fs::remove_file(&full_path) {
                        Ok(_) => format!("Deleted {}", full_path),
                        Err(e) => format!("Error deleting {}: {}", full_path, e),
                    }
                } else if p.is_dir() {
                    match std::fs::remove_dir(&full_path) {
                        Ok(_) => format!("Deleted directory {}", full_path),
                        Err(e) => format!("Error deleting directory {} (must be empty): {}", full_path, e),
                    }
                } else {
                    format!("Path not found: {}", full_path)
                }
            }
        }
        "move_file" => {
            let source = input["source"].as_str().unwrap_or("");
            let destination = input["destination"].as_str().unwrap_or("");
            let src = resolve_path(source, wd);
            let dst = resolve_path(destination, wd);
            if !is_write_safe(&src) || !is_write_safe(&dst) {
                format!("Error: move blocked for safety ({} → {})", src, dst)
            } else if !std::path::Path::new(&src).exists() {
                format!("Source not found: {}", src)
            } else {
                // Ensure destination parent exists
                if let Some(parent) = std::path::Path::new(&dst).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::rename(&src, &dst) {
                    Ok(_) => format!("Moved {} → {}", src, dst),
                    Err(_) => {
                        // rename fails across filesystems — fallback to copy+delete
                        match std::fs::copy(&src, &dst) {
                            Ok(_) => {
                                if let Err(e) = std::fs::remove_file(&src) {
                                    format!("Copied {} → {} but failed to remove source: {}", src, dst, e)
                                } else {
                                    format!("Moved {} → {}", src, dst)
                                }
                            }
                            Err(e) => format!("Error moving: {}", e),
                        }
                    }
                }
            }
        }
        "find_references" => {
            let symbol = input["symbol"].as_str().unwrap_or("");
            let search_dir = input["path"].as_str()
                .map(|p| resolve_path(p, wd))
                .unwrap_or_else(|| wd.as_deref().unwrap_or(".").to_string());
            if symbol.is_empty() {
                "Error: symbol is required.".to_string()
            } else {
                find_symbol_references(symbol, &search_dir)
            }
        }
        "rename_symbol" => {
            let old_name = input["old_name"].as_str().unwrap_or("");
            let new_name = input["new_name"].as_str().unwrap_or("");
            let search_dir = input["path"].as_str()
                .map(|p| resolve_path(p, wd))
                .unwrap_or_else(|| wd.as_deref().unwrap_or(".").to_string());
            if old_name.is_empty() || new_name.is_empty() {
                "Error: old_name and new_name are required.".to_string()
            } else {
                rename_symbol_in_project(old_name, new_name, &search_dir).await
            }
        }
        "get_dependency_graph" => {
            let path = input["path"].as_str().unwrap_or("");
            let direction = input["direction"].as_str().unwrap_or("both");
            let working = wd.as_deref().unwrap_or(".");
            if path.is_empty() {
                "Error: path is required.".to_string()
            } else {
                tools::depgraph::get_dependency_graph(path, direction, working)
            }
        }
        "send_email" => {
            let to = input["to"].as_str().unwrap_or("");
            let subject = input["subject"].as_str().unwrap_or("");
            let body_text = input["body"].as_str().unwrap_or("");
            let html = input["html"].as_bool().unwrap_or(false);
            match tools::email::send_email(&smtp_config, to, subject, body_text, html).await {
                Ok(msg) => msg,
                Err(e) => format!("Email error: {}", e),
            }
        }
        "send_file" => {
            let path = input["path"].as_str().unwrap_or("");
            let caption = input["caption"].as_str().unwrap_or("");
            // Resolve relative paths against working directory
            let full_path = if path.starts_with('/') {
                path.to_string()
            } else if let Some(w) = wd {
                format!("{}/{}", w, path)
            } else {
                path.to_string()
            };
            if !std::path::Path::new(&full_path).exists() {
                format!("Error: file not found: {}", full_path)
            } else {
                let _ = tx.send(AgentEvent::FileOffer {
                    path: full_path.clone(),
                    caption: caption.to_string(),
                });
                format!("File delivered to user: {}", full_path)
            }
        }
        "send_telegram" => {
            let chat_id = input["chat_id"].as_str().unwrap_or("");
            let text = input["text"].as_str().unwrap_or("");
            match crate::channels::telegram::send_message(
                &cfg.http, telegram_bot_token, chat_id, text
            ).await {
                Ok(msg) => msg,
                Err(e) => format!("Telegram error: {}", e),
            }
        }
        "spawn_background" => {
            let cmd = input["command"].as_str().unwrap_or("");
            let label = input["label"].as_str().unwrap_or(cmd);
            match tools::terminal::spawn_background(cmd, wd).await {
                Ok(pid) => {
                    let _ = tx.send(AgentEvent::ToolOutput {
                        name: "spawn_background".into(),
                        stdout: format!("Background process started: {} (PID: {})", label, pid),
                        stderr: String::new(),
                        code: 0,
                    });
                    format!("Background process '{}' started with PID {}", label, pid)
                }
                Err(e) => format!("Error spawning background process: {}", e),
            }
        }
        _ => format!("Unknown tool: {}", tool_name),
    };

    // Truncate output if too large
    Ok((truncate_str(&result, max_output_chars), None))
}

/// Run a subagent task directly (not inside tokio::spawn — avoids Send constraints).
/// Returns the subagent's final text output. Uses Box::pin to break the recursive type cycle.
fn run_subagent_task<'a>(
    task: &'a str,
    cfg: &'a TurnConfig,
    task_db: &'a Arc<TaskDb>,
    tx: &'a mpsc::UnboundedSender<AgentEvent>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
    Box::pin(async move {
    if cfg.subagent_depth >= 1 {
        return "Error: subagents cannot spawn sub-subagents (max depth 1).".to_string();
    }
    if task.is_empty() {
        return "Error: 'task' parameter is required.".to_string();
    }

    let sub_sonnet = super::sonnet::SonnetClient::new(
        &cfg.anthropic_key, &cfg.openai_key,
        &cfg.primary_model, &cfg.primary_provider,
        4096, Arc::clone(&cfg.http),
    );
    let sub_classifier = super::classifier::Classifier::new(
        &cfg.anthropic_key, &cfg.openai_key,
        &cfg.classifier_model, &cfg.classifier_provider,
        Arc::clone(&cfg.http),
    );
    let sub_compactor = super::compactor::Compactor::new(
        &cfg.anthropic_key, &cfg.openai_key,
        &cfg.primary_model, &cfg.primary_provider,
        Arc::clone(&cfg.http),
    );

    let sub_history = vec![super::Message {
        role: "user".to_string(),
        content: task.to_string(),
    }];
    let sub_memory = Memory { path: String::new(), content: String::new() };

    let mut sub_cfg = cfg.clone();
    sub_cfg.subagent_depth = cfg.subagent_depth + 1;
    sub_cfg.conversation_logging = false;

    let (sub_tx, sub_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let sub_soul = "You are a focused sub-agent. Complete the task you are given efficiently and return the result. Do not ask clarifying questions.";

    // Bridge sub-agent events to the parent UI in real-time as they happen.
    let tx_bridge = tx.clone();
    let bridge = tokio::spawn(async move {
        let mut rx = sub_rx;
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::ToolCall { name, input } => {
                    let _ = tx_bridge.send(AgentEvent::ToolCall {
                        name: format!("[sub] {}", name), input,
                    });
                }
                AgentEvent::ToolOutput { name, stdout, stderr, code } => {
                    let _ = tx_bridge.send(AgentEvent::ToolOutput {
                        name: format!("[sub] {}", name), stdout, stderr, code,
                    });
                }
                AgentEvent::Error(e) => {
                    let _ = tx_bridge.send(AgentEvent::Error(format!("[sub] {}", e)));
                }
                _ => {}
            }
        }
    });

    match run_agent_turn(
        &sub_classifier, &sub_sonnet, &sub_compactor,
        &sub_history, &sub_memory, sub_soul, "",
        task_db, &sub_cfg, &sub_tx,
    ).await {
        Ok((output, _memory_ops)) => {
            drop(sub_tx); // close channel so bridge drains and exits
            let _ = bridge.await;
            output
        }
        Err(e) => {
            drop(sub_tx);
            let _ = bridge.await;
            format!("Subagent error: {}", e)
        }
    }
    }) // end Box::pin
}

/// Format file lines with line numbers: `   1│ content`.
fn add_line_numbers(lines: &[&str], start: usize) -> String {
    let width = (start + lines.len()).to_string().len().max(3);
    lines.iter().enumerate()
        .map(|(i, line)| format!("{:>width$}│ {}", start + i, line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// After writing a file, run a quick syntax check appropriate for its type.
/// Returns `Some(error_text)` if a problem is detected, `None` if OK or unknown type.
/// Uses tokio::process::Command to avoid blocking the async runtime.
async fn verify_file_syntax(path: &str) -> Option<String> {
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    // Helper: run a command with a 30-second timeout
    async fn check(mut cmd: tokio::process::Command) -> Option<std::process::Output> {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            cmd.kill_on_drop(true).output(),
        ).await.ok()?.ok()
    }
    match ext {
        "json" => {
            let content = std::fs::read_to_string(path).ok()?;
            serde_json::from_str::<serde_json::Value>(&content).err()
                .map(|e| format!("⚠ JSON syntax error: {}", e))
        }
        "js" | "mjs" | "cjs" => {
            let mut cmd = tokio::process::Command::new("node");
            cmd.args(["--check", path]);
            let out = check(cmd).await?;
            if out.status.success() { None }
            else { Some(format!("⚠ JS syntax error:\n{}", String::from_utf8_lossy(&out.stderr).trim())) }
        }
        "py" => {
            let mut cmd = tokio::process::Command::new("python3");
            cmd.args(["-m", "py_compile", path]);
            let out = check(cmd).await?;
            if out.status.success() { None }
            else { Some(format!("⚠ Python syntax error:\n{}", String::from_utf8_lossy(&out.stderr).trim())) }
        }
        "php" => {
            let mut cmd = tokio::process::Command::new("php");
            cmd.args(["-l", path]);
            let out = check(cmd).await?;
            if out.status.success() { None }
            else { Some(format!("⚠ PHP syntax error:\n{}", String::from_utf8_lossy(&out.stderr).trim())) }
        }
        "rs" => {
            let project_root = find_project_root(path, "Cargo.toml")?;
            let mut cmd = tokio::process::Command::new("cargo");
            cmd.args(["check", "--message-format=short"]).current_dir(&project_root);
            let out = check(cmd).await?;
            if out.status.success() { None } else {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let errors: String = stderr.lines()
                    .filter(|l| l.contains("error") || l.contains("warning["))
                    .take(20)
                    .collect::<Vec<_>>().join("\n");
                if errors.is_empty() { None }
                else { Some(format!("⚠ Rust errors:\n{}", errors)) }
            }
        }
        "go" => {
            let project_root = find_project_root(path, "go.mod")?;
            let mut cmd = tokio::process::Command::new("go");
            cmd.args(["vet", "./..."]).current_dir(&project_root);
            let out = check(cmd).await?;
            if out.status.success() { None }
            else { Some(format!("⚠ Go vet errors:\n{}", String::from_utf8_lossy(&out.stderr).trim())) }
        }
        "rb" => {
            let mut cmd = tokio::process::Command::new("ruby");
            cmd.args(["-c", path]);
            let out = check(cmd).await?;
            if out.status.success() { None }
            else { Some(format!("⚠ Ruby syntax error:\n{}", String::from_utf8_lossy(&out.stderr).trim())) }
        }
        "sh" | "bash" => {
            let mut cmd = tokio::process::Command::new("bash");
            cmd.args(["-n", path]);
            let out = check(cmd).await?;
            if out.status.success() { None }
            else { Some(format!("⚠ Bash syntax error:\n{}", String::from_utf8_lossy(&out.stderr).trim())) }
        }
        _ => None,
    }
}

/// Walk up the directory tree from `path` to find a directory containing `marker` file.
fn find_project_root(path: &str, marker: &str) -> Option<std::path::PathBuf> {
    let mut dir = std::path::Path::new(path).parent()?;
    loop {
        if dir.join(marker).exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// Snap a byte offset down to the nearest char boundary (for head truncation).
fn snap_floor(s: &str, pos: usize) -> usize {
    let mut b = pos.min(s.len());
    while b > 0 && !s.is_char_boundary(b) { b -= 1; }
    b
}

/// Snap a byte offset up to the nearest char boundary (for tail truncation).
fn snap_ceil(s: &str, pos: usize) -> usize {
    let mut b = pos.min(s.len());
    while b < s.len() && !s.is_char_boundary(b) { b += 1; }
    b
}

/// Safe head-truncation: return `&s[..n]` snapped to a char boundary.
fn safe_head(s: &str, n: usize) -> &str {
    &s[..snap_floor(s, n)]
}

/// Truncate a string to max_chars, appending a truncation notice.
fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        let head = safe_head(s, max_chars);
        format!("{}\n\n[OUTPUT TRUNCATED — {} chars total, showing first {}]", head, s.len(), head.len())
    }
}

/// Tail-first truncation — keeps the LAST max_chars of output.
/// Better for command output: errors/results are always at the end, not the start.
fn truncate_tail(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        let start = snap_ceil(s, s.len() - max_chars);
        // Then snap to a newline boundary so we don't cut mid-line
        let snapped = s[start..].find('\n').map(|i| start + i + 1).unwrap_or(start);
        format!("[OUTPUT TRUNCATED — {} chars total, showing last {}]\n{}", s.len(), s.len() - snapped, &s[snapped..])
    }
}

/// Append a conversation turn to the conversation log file.
fn log_turn(
    log_path: &std::path::Path,
    user_msg: &str,
    enhanced_msg: Option<&str>,
    model: &str,
    reply: &str,
) {
    use std::io::Write;
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let separator = "═".repeat(80);

    let mut entry = String::new();
    entry.push_str(&format!("\n{separator}\n"));
    entry.push_str(&format!("[{ts}] USER\n{user_msg}\n"));
    if let Some(enh) = enhanced_msg {
        entry.push_str(&format!("\n[{ts}] SUPERCHARGED\n{enh}\n"));
    }
    entry.push_str(&format!("\n[{ts}] ASSISTANT [{model}]\n{reply}\n"));

    match std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
        Ok(mut f) => { let _ = f.write_all(entry.as_bytes()); }
        Err(e) => warn!(error = %e, path = %log_path.display(), "Failed to write conversation log"),
    }
}

/// Strip <think>...</think> blocks from assistant responses before storing in history.
/// Prevents planning tokens from re-consuming context on subsequent turns.
pub(crate) fn strip_think_tags(s: &str) -> String {
    let mut out = s.to_string();
    // Strip both <think>...</think> and <thinking>...</thinking> variants
    for (open, close) in &[("<thinking>", "</thinking>"), ("<think>", "</think>")] {
        loop {
            if let Some(start) = out.find(open) {
                if let Some(end_rel) = out[start..].find(close) {
                    out.replace_range(start..start + end_rel + close.len(), "");
                } else {
                    out.truncate(start); // unclosed tag — drop to end
                    break;
                }
            } else {
                break;
            }
        }
    }
    let trimmed = out.trim().to_string();
    // If trimmed is empty the model produced only think-block content (no visible output).
    // Return empty string — the caller decides how to handle it. Do NOT return the raw
    // tagged text, which would leak <think> blocks into history or the UI.
    trimmed
}

/// Compress <tool_log>...</tool_log> into a compact <tool_trace> for history.
/// Keeps one summary line per tool call (truncated to ~60 chars) so the agent
/// remembers what it did in previous turns without inflating context with full output.
pub(crate) fn compress_tool_log(s: &str) -> String {
    const RESULT_LIMIT: usize = 60;
    let out = s.to_string();
    let mut result = String::new();
    let mut search_start = 0;

    loop {
        match out[search_start..].find("<tool_log>") {
            None => {
                result.push_str(&out[search_start..]);
                break;
            }
            Some(rel) => {
                let abs = search_start + rel;
                // Append text before the tag
                result.push_str(&out[search_start..abs]);

                let inner_start = abs + 10; // len("<tool_log>")
                match out[inner_start..].find("</tool_log>") {
                    None => {
                        // Unclosed tag — drop to end
                        break;
                    }
                    Some(rel_end) => {
                        let inner = &out[inner_start..inner_start + rel_end];
                        let lines: Vec<&str> = inner.lines()
                            .map(|l| l.trim())
                            .filter(|l| !l.is_empty())
                            .collect();

                        if !lines.is_empty() {
                            result.push_str("\n\n<tool_trace>\n");
                            for line in lines {
                                // Trim everything after "→ " to RESULT_LIMIT chars
                                if let Some(arrow) = line.find("→ ") {
                                    let split = arrow + "→ ".len();
                                    let prefix = &line[..split];
                                    let rest = &line[split..];
                                    let trimmed = if rest.len() > RESULT_LIMIT {
                                        format!("{}{}…", prefix, safe_head(rest, RESULT_LIMIT))
                                    } else {
                                        line.to_string()
                                    };
                                    result.push_str(&trimmed);
                                } else {
                                    // No arrow — keep as-is but cap length
                                    if line.len() > RESULT_LIMIT * 2 {
                                        result.push_str(safe_head(line, RESULT_LIMIT * 2));
                                        result.push('…');
                                    } else {
                                        result.push_str(line);
                                    }
                                }
                                result.push('\n');
                            }
                            result.push_str("</tool_trace>");
                        }

                        search_start = inner_start + rel_end + 11; // len("</tool_log>")
                    }
                }
            }
        }
    }

    result.trim_end().to_string()
}

/// Strip <tool_log>...</tool_log> from assistant responses before storing in history.
/// Tool logs are only needed for the quality reviewer; keeping them in history
/// inflates context on every subsequent turn without adding reasoning value.
pub(crate) fn strip_tool_log(s: &str) -> String {
    let mut out = s.to_string();
    loop {
        if let Some(start) = out.find("<tool_log>") {
            if let Some(end_rel) = out[start..].find("</tool_log>") {
                out.replace_range(start..start + end_rel + 11, "");
            } else {
                out.truncate(start); // unclosed tag — drop to end
                break;
            }
        } else {
            break;
        }
    }
    out.trim_end().to_string()
}

/// Strip ANSI/VT100 escape sequences from a string.
/// Handles CSI sequences (ESC [ ... letter) and simple 2-char escapes.
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next(); // consume '['
                    // skip until a final byte (ASCII letter or a few special chars)
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() || c == '@' || c == '`' {
                            break;
                        }
                    }
                }
                Some(_) => { chars.next(); } // skip one char for 2-char sequences
                None => {}
            }
        } else {
            out.push(ch);
        }
    }
    out
}
fn parse_http_status(e: &anyhow::Error) -> Option<u16> {
    let s = e.to_string();
    let start = s.find('(')? + 1;
    let end = s[start..].find(')')?;
    s[start..start + end].parse().ok()
}

/// Try to parse a Retry-After value (seconds) from an error message body.
fn parse_retry_after(body: &str) -> Option<u64> {
    // Use the lowercase version throughout to avoid byte offset mismatch on non-ASCII
    let lower = body.to_lowercase();
    for keyword in &["retry-after:", "retry_after:", "retryafter:"] {
        if let Some(pos) = lower.find(keyword) {
            let rest = lower[pos + keyword.len()..].trim_start();
            if let Some(num_end) = rest.find(|c: char| !c.is_ascii_digit()) {
                if num_end > 0 {
                    if let Ok(n) = rest[..num_end].parse::<u64>() {
                        return Some(n);
                    }
                }
            } else if !rest.is_empty() {
                // Entire rest is digits
                return rest.parse::<u64>().ok();
            }
        }
    }
    None
}

/// Detect the appropriate test command for a project based on marker files.
/// Returns Some((command, args)) or None if no test framework is detected.
fn detect_test_command(working_dir: &str) -> Option<(String, Vec<String>)> {
    let dir = std::path::Path::new(working_dir);

    // Rust — Cargo.toml
    if dir.join("Cargo.toml").exists() {
        return Some(("cargo".into(), vec!["test".into(), "--".into(), "--color=never".into()]));
    }

    // Go — go.mod
    if dir.join("go.mod").exists() {
        return Some(("go".into(), vec!["test".into(), "./...".into()]));
    }

    // Python — pytest / unittest
    if dir.join("pytest.ini").exists()
        || dir.join("pyproject.toml").exists()
        || dir.join("setup.py").exists()
    {
        // Check if pytest is available
        if dir.join("pytest.ini").exists() || std::process::Command::new("pytest").arg("--version").output().is_ok() {
            return Some(("pytest".into(), vec!["-x".into(), "--tb=short".into(), "-q".into()]));
        }
        return Some(("python3".into(), vec!["-m".into(), "unittest".into(), "discover".into(), "-q".into()]));
    }

    // PHP — phpunit.xml
    if dir.join("phpunit.xml").exists() || dir.join("phpunit.xml.dist").exists() {
        if dir.join("vendor/bin/phpunit").exists() {
            return Some(("./vendor/bin/phpunit".into(), vec!["--no-interaction".into()]));
        }
        return Some(("phpunit".into(), vec!["--no-interaction".into()]));
    }

    // Node — package.json with test script
    if dir.join("package.json").exists() {
        if let Ok(pkg) = std::fs::read_to_string(dir.join("package.json")) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&pkg) {
                if json["scripts"]["test"].is_string() {
                    let test_script = json["scripts"]["test"].as_str().unwrap_or("");
                    if !test_script.is_empty() && test_script != "echo \"Error: no test specified\" && exit 1" {
                        return Some(("npm".into(), vec!["test".into(), "--".into(), "--color=false".into()]));
                    }
                }
            }
        }
    }

    // Ruby — Rakefile or spec/
    if dir.join("Gemfile").exists() && dir.join("spec").is_dir() {
        return Some(("bundle".into(), vec!["exec".into(), "rspec".into(), "--no-color".into()]));
    }

    None
}

/// Resolve a potentially relative path against the working directory.
fn resolve_path(path: &str, wd: Option<&str>) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else if path == "~" {
        std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
    } else if path.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/{}", home, &path[2..])
    } else if let Some(w) = wd {
        format!("{}/{}", w, path)
    } else {
        path.to_string()
    }
}

/// Source file extensions worth searching for symbol references.
const SOURCE_EXTS: &[&str] = &[
    "rs", "py", "php", "go", "rb", "sh", "js", "jsx", "ts", "tsx",
    "java", "kt", "c", "cpp", "h", "hpp", "cs", "swift", "lua",
    "toml", "yaml", "yml", "json", "html", "css", "scss", "sql",
];

const SKIP_SEARCH_DIRS: &[&str] = &[
    ".git", "target", "node_modules", "__pycache__", ".next", "dist",
    "build", "vendor", ".venv", "venv", ".axium",
];

/// Find all references to a symbol across a project directory.
fn find_symbol_references(symbol: &str, search_dir: &str) -> String {
    let pattern = format!(r"\b{}\b", regex::escape(symbol));
    let re = match regex::Regex::new(&pattern) {
        Ok(r) => r,
        Err(e) => return format!("Invalid symbol: {}", e),
    };

    let mut results = Vec::new();
    collect_symbol_refs(std::path::Path::new(search_dir), &re, &mut results, 0);

    if results.is_empty() {
        format!("No references to '{}' found in {}", symbol, search_dir)
    } else {
        let total = results.len();
        results.truncate(80);
        let mut out = format!("Found {} references to '{}':\n\n", total, symbol);
        out.push_str(&results.join("\n"));
        if total > 80 {
            out.push_str(&format!("\n... and {} more", total - 80));
        }
        out
    }
}

fn collect_symbol_refs(
    dir: &std::path::Path,
    re: &regex::Regex,
    results: &mut Vec<String>,
    depth: usize,
) {
    if depth > 10 || results.len() > 200 { return; }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if name.starts_with('.') || SKIP_SEARCH_DIRS.contains(&name.as_str()) {
            continue;
        }

        if path.is_dir() {
            collect_symbol_refs(&path, re, results, depth + 1);
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !SOURCE_EXTS.contains(&ext) { continue; }

            if let Ok(content) = std::fs::read_to_string(&path) {
                for (i, line) in content.lines().enumerate() {
                    // Skip comment-only lines for Rust to reduce false positives
                    if ext == "rs" {
                        let t = line.trim();
                        if t.starts_with("//") || t.starts_with("///") || t.starts_with("/*") { continue; }
                    }
                    if re.is_match(line) {
                        results.push(format!(
                            "  {}:{}:  {}",
                            path.display(),
                            i + 1,
                            line.trim()
                        ));
                        if results.len() > 200 { return; }
                    }
                }
            }
        }
    }
}

/// Rename a symbol across all source files in a project, then run diagnostics.
async fn rename_symbol_in_project(old_name: &str, new_name: &str, search_dir: &str) -> String {
    let pattern = format!(r"\b{}\b", regex::escape(old_name));
    let re = match regex::Regex::new(&pattern) {
        Ok(r) => r,
        Err(e) => return format!("Invalid symbol: {}", e),
    };

    let mut files_changed = Vec::new();
    let mut total_replacements: usize = 0;

    // Collect all files that contain the symbol
    let mut file_list = Vec::new();
    collect_files_with_symbol(std::path::Path::new(search_dir), &re, &mut file_list, 0);

    for file_path in &file_list {
        if let Ok(content) = std::fs::read_to_string(file_path) {
            let is_rust = file_path.extension().and_then(|e| e.to_str()) == Some("rs");
            let new_content = if is_rust {
                // For Rust files, skip replacements inside comments and string literals
                let dead_zones = get_dead_zones_rs(&content);
                re.replace_all(&content, |caps: &regex::Captures| -> String {
                    let start = caps.get(0).unwrap().start();
                    if dead_zones.iter().any(|r| r.contains(&start)) {
                        caps.get(0).unwrap().as_str().to_string()
                    } else {
                        new_name.to_string()
                    }
                }).to_string()
            } else {
                re.replace_all(&content, new_name).to_string()
            };
            if new_content != content {
                let count = if is_rust {
                    // Count only non-dead-zone matches
                    let dead_zones = get_dead_zones_rs(&content);
                    re.find_iter(&content)
                        .filter(|m| !dead_zones.iter().any(|r| r.contains(&m.start())))
                        .count()
                } else {
                    re.find_iter(&content).count()
                };
                if count > 0 && std::fs::write(file_path, &new_content).is_ok() {
                    files_changed.push(format!("  {} ({} replacements)", file_path.display(), count));
                    total_replacements += count;
                }
            }
        }
    }

    if files_changed.is_empty() {
        return format!("No occurrences of '{}' found.", old_name);
    }

    let mut out = format!(
        "Renamed '{}' → '{}': {} replacements in {} files:\n\n{}\n",
        old_name,
        new_name,
        total_replacements,
        files_changed.len(),
        files_changed.join("\n")
    );

    // Run diagnostics to verify the rename didn't break anything
    if std::path::Path::new(search_dir).join("Cargo.toml").exists() {
        let check = tokio::process::Command::new("cargo")
            .args(["check", "--message-format=short"])
            .current_dir(search_dir)
            .output().await;
        if let Ok(o) = check {
            if !o.status.success() {
                let errors: String = String::from_utf8_lossy(&o.stderr)
                    .lines()
                    .filter(|l| l.contains("error"))
                    .take(10)
                    .collect::<Vec<_>>()
                    .join("\n");
                if !errors.is_empty() {
                    out.push_str(&format!("\n⚠ Post-rename diagnostics found errors:\n{}", errors));
                }
            } else {
                out.push_str("\n✓ Diagnostics passed — no errors.");
            }
        }
    }

    out
}

/// Run `rust-analyzer parse` on Rust source content and return byte ranges of
/// COMMENT, BLOCK_COMMENT, and STRING nodes. Used to skip replacements inside
/// comments and string literals during symbol renaming.
fn get_dead_zones_rs(content: &str) -> Vec<std::ops::Range<usize>> {
    use std::io::Write as _;
    let mut child = match std::process::Command::new("rust-analyzer")
        .arg("parse")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(content.as_bytes());
        }
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let mut zones = Vec::new();

    for line in text.lines() {
        let t = line.trim();
        let is_dead = t.starts_with("COMMENT@")
            || t.starts_with("BLOCK_COMMENT@")
            || t.starts_with("STRING@");
        if !is_dead { continue; }

        if let Some(at) = t.find('@') {
            let range_str = &t[at + 1..];
            let end = range_str.find(' ').unwrap_or(range_str.len());
            let range_part = &range_str[..end];
            if let Some(sep) = range_part.find("..") {
                let start: usize = range_part[..sep].parse().unwrap_or(0);
                let end: usize = range_part[sep + 2..].parse().unwrap_or(0);
                if end > start { zones.push(start..end); }
            }
        }
    }

    zones
}

fn collect_files_with_symbol(
    dir: &std::path::Path,
    re: &regex::Regex,
    files: &mut Vec<std::path::PathBuf>,
    depth: usize,
) {
    if depth > 10 { return; }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if name.starts_with('.') || SKIP_SEARCH_DIRS.contains(&name.as_str()) {
            continue;
        }

        if path.is_dir() {
            collect_files_with_symbol(&path, re, files, depth + 1);
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !SOURCE_EXTS.contains(&ext) { continue; }

            if let Ok(content) = std::fs::read_to_string(&path) {
                if re.is_match(&content) {
                    files.push(path);
                }
            }
        }
    }
}

