use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::{Html, Json};
use axum::routing::{get, post};
use axum::Router;
use futures::stream::StreamExt;
use futures::SinkExt;
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{info, warn, error};

use crate::agent::classifier::Classifier;
use crate::agent::compactor::Compactor;
use crate::agent::router::{self, AgentEvent, TurnConfig};
use crate::agent::sonnet::SonnetClient;
use crate::agent::Message;
use crate::config::loader::{self, Config};
use crate::db::history::ChatDb;
use crate::db::tasks::TaskDb;
use crate::memory::store::Memory;
use crate::tools::project;

pub struct AppState {
    pub config: RwLock<Config>,
    pub config_path: String,
    pub memory_path: String,
    pub chat_db: Arc<ChatDb>,
    pub task_db: Arc<TaskDb>,
    pub http: Arc<reqwest::Client>,
    pub memory_lock: Arc<Mutex<()>>,
    pub sudo_password: RwLock<String>,
    /// Broadcasts watcher diagnostic events to all connected WebSocket clients.
    pub broadcast_tx: tokio::sync::broadcast::Sender<String>,
    /// Shutdown sender for the Telegram bot — send `true` to stop the current bot.
    pub telegram_shutdown: Mutex<tokio::sync::watch::Sender<bool>>,
    /// Plugin manager for hook execution.
    pub plugin_manager: Arc<RwLock<crate::plugins::PluginManager>>,
    /// Cached project context: (timestamp, working_dir, content). Invalidated by watcher or after 60s.
    pub project_context_cache: Arc<RwLock<Option<(std::time::Instant, String, String)>>>,
    /// Buffered conversation log entries. Flushed periodically, on /api/flush, and on shutdown.
    pub conv_log_buffer: Arc<Mutex<Vec<String>>>,
    /// Wakes the background flush task immediately (100-entry threshold or manual trigger).
    pub flush_notify: Arc<tokio::sync::Notify>,
    /// In-flight task file contents keyed by task_id → (absolute path, current file content).
    pub task_file_buffers: Arc<RwLock<std::collections::HashMap<i64, (std::path::PathBuf, String)>>>,
}

/// Get project context, using the cached value if still valid (< 60s and same working_dir),
/// otherwise rebuilding from disk.
pub async fn get_project_context(state: &AppState, working_dir: &str) -> String {
    const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60);

    // Check cache
    {
        let cache = state.project_context_cache.read().await;
        if let Some((ts, ref cached_wd, ref ctx)) = *cache {
            if cached_wd == working_dir && ts.elapsed() < MAX_AGE {
                return ctx.clone();
            }
        }
    }

    // Cache miss — rebuild
    let ctx = project::build_project_context(working_dir);
    {
        let mut cache = state.project_context_cache.write().await;
        *cache = Some((std::time::Instant::now(), working_dir.to_string(), ctx.clone()));
    }
    ctx
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/ws", get(ws_handler))
        .route("/api/config", get(get_config))
        .route("/api/config", post(set_config))
        .route("/api/export", get(export_handler))
        .route("/api/files", get(file_download_handler))
        .route("/api/sessions", get(list_sessions_handler))
        .route("/api/sessions/delete", post(delete_session_handler))
        .route("/api/autostart", get(get_autostart_handler))
        .route("/api/autostart", post(set_autostart_handler))
        .route("/api/action/stop", post(stop_action_handler))
        .route("/api/action/shutdown", post(shutdown_action_handler))
        .route("/api/action/reboot", post(reboot_action_handler))
        .route("/api/health", get(health_handler))
        .route("/api/skills", get(list_skills_handler))
        .route("/api/skills/folder", post(create_skill_folder_handler))
        .route("/api/skills/folder/delete", post(delete_skill_folder_handler))
        .route("/api/skills/file", get(get_skill_file_handler))
        .route("/api/skills/file", post(save_skill_file_handler))
        .route("/api/skills/file/delete", post(delete_skill_file_handler))
        .route("/api/plugins", get(list_plugins_handler))
        .route("/api/plugins/toggle", post(toggle_plugin_handler))
        .route("/api/plugins/reorder", post(reorder_plugins_handler))
        .route("/api/flush", post(flush_handler))

        .layer(axum::middleware::from_fn(lan_guard))
        .with_state(state)
}

/// Middleware: reject non-LAN connections for all routes.
async fn lan_guard(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if !is_lan_or_loopback(addr.ip()) {
        warn!(remote = %addr, "Rejected non-LAN HTTP request");
        return axum::response::Response::builder()
            .status(403)
            .body("Forbidden: LAN connections only".into())
            .unwrap();
    }
    next.run(request).await
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("../../static/index.html"))
}

async fn get_config(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let has_sudo = !state.sudo_password.read().await.is_empty();
    let cfg = state.config.read().await;
    Json(serde_json::json!({
        "agent_name": cfg.agent.name,
        "models": {
            "primary": cfg.models.primary,
            "compactor": cfg.models.compactor,
            "classifier": cfg.models.classifier,
        },
        "available_models": cfg.available_models,
        "has_anthropic_key": !cfg.api_keys.anthropic.is_empty(),
        "has_openai_key": !cfg.api_keys.openai.is_empty(),
        "has_sudo_password": has_sudo,
        "settings": {
            "max_output_chars": cfg.settings.max_output_chars,
            "max_tool_iterations": cfg.settings.max_tool_iterations,
            "max_input_chars": cfg.settings.max_input_chars,
            "working_directory": cfg.settings.working_directory,
            "smtp_host": cfg.settings.smtp_host,
            "smtp_port": cfg.settings.smtp_port,
            "smtp_user": cfg.settings.smtp_user,
            "has_smtp_password": !cfg.settings.smtp_password.is_empty(),
            "smtp_from": cfg.settings.smtp_from,
            "telegram_enabled": cfg.settings.telegram_enabled,
            "has_telegram_token": !cfg.settings.telegram_bot_token.is_empty(),
            "telegram_allowed_users": cfg.settings.telegram_allowed_users,
            "conversation_logging": cfg.settings.conversation_logging,
        },
    }))
}

async fn set_config(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    // Handle sudo password (memory-only, never saved to disk)
    if let Some(pw) = body["sudo_password"].as_str() {
        *state.sudo_password.write().await = pw.to_string();
    }

    let mut cfg = state.config.write().await;

    if let Some(primary) = body["primary"].as_str() {
        if !primary.is_empty() {
            cfg.models.primary = primary.to_string();
        }
    }
    if let Some(compactor) = body["compactor"].as_str() {
        if !compactor.is_empty() {
            cfg.models.compactor = compactor.to_string();
        }
    }
    if let Some(classifier) = body["classifier"].as_str() {
        if !classifier.is_empty() {
            cfg.models.classifier = classifier.to_string();
        }
    }
    if let Some(continuation) = body["continuation"].as_str() {
        cfg.models.continuation = continuation.to_string();
    }
    if let Some(key) = body["anthropic_key"].as_str() {
        if !key.is_empty() {
            cfg.api_keys.anthropic = key.to_string();
        }
    }
    if let Some(key) = body["openai_key"].as_str() {
        if !key.is_empty() {
            cfg.api_keys.openai = key.to_string();
        }
    }
    if let Some(name) = body["agent_name"].as_str() {
        let name: String = name.trim().chars().take(50).collect();
        if !name.is_empty() {
            cfg.agent.name = name;
        }
    }
    if let Some(iterations) = body["max_tool_iterations"].as_u64() {
        cfg.settings.max_tool_iterations = (iterations as usize).clamp(5, 100);
    }
    if let Some(wd) = body["working_directory"].as_str() {
        let wd: String = wd.trim().to_string();
        if !wd.is_empty() {
            cfg.settings.working_directory = wd;
        }
    }
    if let Some(v) = body["smtp_host"].as_str() {
        cfg.settings.smtp_host = v.trim().to_string();
    }
    if let Some(v) = body["smtp_port"].as_u64() {
        cfg.settings.smtp_port = (v as u16).max(1);
    }
    if let Some(v) = body["smtp_user"].as_str() {
        cfg.settings.smtp_user = v.trim().to_string();
    }
    if let Some(v) = body["smtp_password"].as_str() {
        if !v.is_empty() {
            cfg.settings.smtp_password = v.to_string();
        }
    }
    if let Some(v) = body["smtp_from"].as_str() {
        cfg.settings.smtp_from = v.trim().to_string();
    }
    if let Some(v) = body["telegram_bot_token"].as_str() {
        if !v.is_empty() {
            cfg.settings.telegram_bot_token = v.to_string();
        }
    }
    if let Some(v) = body["telegram_allowed_users"].as_str() {
        cfg.settings.telegram_allowed_users = v.trim().to_string();
    }
    if let Some(v) = body["telegram_enabled"].as_bool() {
        cfg.settings.telegram_enabled = v;
    }
    if let Some(v) = body["conversation_logging"].as_bool() {
        cfg.settings.conversation_logging = v;
    }
    let save_result = loader::save_config(&state.config_path, &cfg);
    // Drop the write lock BEFORE calling restart_telegram, which needs a read lock.
    drop(cfg);
    match save_result {
        Ok(_) => {
            info!("Config saved");
            // Restart telegram in the background so we don't block the HTTP response
            let state_clone = Arc::clone(&state);
            tokio::spawn(async move { restart_telegram(&state_clone).await });
            Json(serde_json::json!({"ok": true}))
        }
        Err(e) => Json(serde_json::json!({"ok": false, "error": format!("{:#}", e)})),
    }
}

/// Stop the current Telegram bot (if any) and re-spawn it if enabled.
async fn restart_telegram(state: &Arc<AppState>) {
    // Signal the old bot to stop
    {
        let tx = state.telegram_shutdown.lock().await;
        let _ = tx.send(true);
    }
    // Small delay to let the old polling loop exit
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    // Create a fresh shutdown channel and spawn
    let (new_tx, new_rx) = tokio::sync::watch::channel(false);
    *state.telegram_shutdown.lock().await = new_tx;
    crate::channels::telegram::TelegramBot::spawn(Arc::clone(state), new_rx).await;
    info!("Telegram bot restarted");
}

async fn export_handler(
    State(state): State<Arc<AppState>>,
) -> axum::response::Response {
    let session_id = match state.chat_db.latest_session() {
        Ok(Some(id)) => id,
        _ => {
            return axum::response::Response::builder()
                .status(404)
                .body("No session found".into())
                .unwrap();
        }
    };
    let messages = state.chat_db.load_session_messages(&session_id).unwrap_or_default();
    let mut markdown = format!("# Chat Export — {}\n\n", session_id);
    for m in &messages {
        let role_label = match m.role.as_str() {
            "user" => "**User**",
            "assistant" => "**Assistant**",
            _ => "**System**",
        };
        markdown.push_str(&format!("### {} ({})\n\n{}\n\n---\n\n", role_label, m.timestamp, m.content));
    }
    axum::response::Response::builder()
        .header("Content-Type", "text/markdown; charset=utf-8")
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}.md\"", session_id),
        )
        .body(markdown.into())
        .unwrap()
}

/// Serve a file for download. Only allows files under $HOME.
async fn file_download_handler(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let path = match params.get("path") {
        Some(p) => p,
        None => {
            return axum::response::Response::builder()
                .status(400)
                .body("Missing 'path' parameter".into())
                .unwrap();
        }
    };

    // Security: resolve to canonical path and block traversal
    let canonical = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => {
            return axum::response::Response::builder()
                .status(404)
                .body("File not found".into())
                .unwrap();
        }
    };

    if !canonical.is_file() {
        return axum::response::Response::builder()
            .status(404)
            .body("Not a file".into())
            .unwrap();
    }

    // Restrict downloads to files under $HOME
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && !canonical.to_string_lossy().starts_with(&home) {
        return axum::response::Response::builder()
            .status(403)
            .body("Access denied".into())
            .unwrap();
    }

    let bytes = match std::fs::read(&canonical) {
        Ok(b) => b,
        Err(_) => {
            return axum::response::Response::builder()
                .status(500)
                .body("Failed to read file".into())
                .unwrap();
        }
    };

    let file_name = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download");

    let content_type = match canonical.extension().and_then(|e| e.to_str()) {
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("zip") => "application/zip",
        Some("gz") | Some("tgz") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("json") => "application/json",
        Some("csv") => "text/csv",
        Some("txt") | Some("md") | Some("log") => "text/plain; charset=utf-8",
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("rs") | Some("py") | Some("js") | Some("ts") | Some("sh") | Some("toml") | Some("yaml") | Some("yml") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };

    axum::response::Response::builder()
        .header("Content-Type", content_type)
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", file_name),
        )
        .body(bytes.into())
        .unwrap()
}

async fn list_sessions_handler(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    match state.chat_db.list_sessions() {
        Ok(sessions) => Json(serde_json::json!({ "sessions": sessions })),
        Err(e) => Json(serde_json::json!({ "sessions": [], "error": format!("{}", e) })),
    }
}

async fn delete_session_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let id = body["session_id"].as_str().unwrap_or("");
    if id.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "Missing session_id" }));
    }
    match state.chat_db.delete_session(id) {
        Ok(_) => Json(serde_json::json!({ "ok": true })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": format!("{}", e) })),
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> axum::response::Response {
    // Allow loopback and private LAN addresses; reject public internet connections
    if !is_lan_or_loopback(addr.ip()) {
        warn!(remote = %addr, "Rejected non-LAN WebSocket connection");
        return axum::response::Response::builder()
            .status(403)
            .body("Forbidden: LAN connections only".into())
            .unwrap();
    }
    info!(remote = %addr, "WebSocket connection accepted");
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let mut history: Vec<Message> = Vec::new();
    // Subscribe to broadcast (file watcher diagnostics etc.)
    let mut bcast_rx = state.broadcast_tx.subscribe();

    // Create or resume session
    let mut session_id = match state.chat_db.latest_session() {
        Ok(Some(id)) => {
            // Load previous messages
            if let Ok(msgs) = state.chat_db.load_session_messages(&id) {
                for m in msgs {
                    // Skip system-role messages (watcher diagnostics etc.) — not for LLM
                    if m.role == "system" { continue; }
                    // Skip [partial] messages saved during disconnects — incomplete data
                    if m.content.starts_with("[partial] ") { continue; }
                    let content = if m.role == "assistant" {
                        crate::agent::router::strip_think_tags(&m.content)
                    } else { m.content };
                    history.push(Message { role: m.role, content });
                }
            }
            id
        }
        _ => state.chat_db.create_session().unwrap_or_else(|_| "default".into()),
    };

    // Send initial greeting
    let (agent_name, model_name) = {
        let cfg = state.config.read().await;
        (cfg.agent.name.clone(), cfg.models.primary.clone())
    };
    let session_title = state.chat_db.get_session_title(&session_id);
    let greeting = serde_json::json!({
        "type": "system",
        "text": format!("{} v0.3 — connected. Model: {} | Session: {} | History: {} msgs",
            agent_name, model_name, session_id, history.len()),
        "session_id": session_id,
        "session_title": session_title,
    });
    let _ = ws_tx.send(WsMessage::Text(greeting.to_string().into())).await;

    // Send session history to client for reconnection
    for m in &history {
        let msg = serde_json::json!({
            "type": if m.role == "user" { "history_user" } else { "history_assistant" },
            "text": m.content,
        });
        if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
            return;
        }
    }

    // On connect: push any unread completed background tasks to this client
    if let Ok(unread) = state.task_db.unread_completed() {
        for task in &unread {
            let m = serde_json::json!({
                "type": "task_completed",
                "id": task.id,
                "title": task.title,
                "status": task.status,
                "result": if task.result.len() > 500 { let mut b=500; while b>0 && !task.result.is_char_boundary(b) { b-=1; } format!("{}…", &task.result[..b]) } else { task.result.clone() },
            });
            let _ = ws_tx.send(WsMessage::Text(m.to_string().into())).await;
            let _ = state.task_db.mark_read(task.id);
        }
    }

    let mut auto_mode = false;
    let mut auto_turn_count: u8 = 0;
    let mut pending_auto: Option<String> = None;
    let mut turns_since_recovery: usize = 0;
    let mut recovery_window_start: usize = 0; // index into history where current recovery window begins
    let keepalive = tokio::time::Duration::from_secs(30);
    let mut ping_interval = tokio::time::interval(keepalive);
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // Drain pending autonomous prompt without blocking on WebSocket input
        let incoming: serde_json::Value = if let Some(auto_text) = pending_auto.take() {
            auto_turn_count += 1;
            let _ = ws_tx.send(WsMessage::Text(
                serde_json::json!({"type": "autonomous_turn", "turn": auto_turn_count}).to_string().into()
            )).await;
            serde_json::json!({"type": "message", "text": auto_text})
        } else {
            // Select between incoming WS messages and broadcast (watcher diagnostics).
            let msg = tokio::select! {
                m = ws_rx.next() => m,
                result = bcast_rx.recv() => {
                    match result {
                        Ok(bcast) => {
                            // Persist watcher diagnostics so they survive page reload
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&bcast) {
                                if v["type"] == "watcher_diagnostic" {
                                    let text = format!("⚠ {}: {}", v["path"].as_str().unwrap_or("?"), v["message"].as_str().unwrap_or("?"));
                                    let _ = state.chat_db.save_message(&session_id, "system", &text);
                                }
                            }
                            let _ = ws_tx.send(WsMessage::Text(bcast.into())).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("Broadcast receiver lagged, skipped {} messages", n);
                        }
                        Err(_) => {} // channel closed
                    }
                    continue;
                }
                _ = ping_interval.tick() => {
                    if ws_tx.send(WsMessage::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                    continue;
                }
            };
            let Some(Ok(msg)) = msg else { break };
            let text = match msg {
                WsMessage::Text(t) => t.to_string(),
                WsMessage::Close(_) => break,
                _ => continue,
            };
            match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            }
        };

        if incoming["type"] == "message" {
            let user_text = incoming["text"].as_str().unwrap_or("").to_string();
            if user_text.is_empty() {
                continue;
            }
            let mode = incoming["mode"].as_str().unwrap_or("supercharge").to_string();

            // Truncate user input if over limit, read config once, and build agent
            let sudo_pw = state.sudo_password.read().await.clone();
            let sudo_note = if !sudo_pw.is_empty() {
                "\n\n## Sudo Access\nA sudo password is configured. When commands need elevated privileges, use `sudo` in run_command — the password is injected automatically and transparently. NEVER ask the user for their password."
            } else { "" };
            let (user_text, sonnet, compactor, classifier, memory_path, soul, turn_cfg, project_ctx, memory_file) = {
                let cfg = state.config.read().await;
                let max = cfg.settings.max_input_chars;
                let user_text = if user_text.len() > max {
                    let mut b = max; while b > 0 && !user_text.is_char_boundary(b) { b -= 1; }
                    format!("{}\n[INPUT TRUNCATED at {} bytes]", &user_text[..b], max)
                } else {
                    user_text
                };
                let wd = &cfg.settings.working_directory;
                let resolved_wd = if wd.is_empty() || wd == "~" {
                    std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
                } else if wd.starts_with("~/") {
                    let home = std::env::var("HOME").unwrap_or_default();
                    format!("{}/{}", home, &wd[2..])
                } else {
                    wd.clone()
                };
                let ctx = get_project_context(&state, &resolved_wd).await;
                (
                    user_text,
                    SonnetClient::new(
                        &cfg.api_keys.anthropic,
                        &cfg.api_keys.openai,
                        &cfg.models.primary,
                        &cfg.models.primary_provider,
                        cfg.settings.max_tokens,
                        Arc::clone(&state.http),
                    ),
                    Compactor::new(
                        &cfg.api_keys.anthropic,
                        &cfg.api_keys.openai,
                        &cfg.models.compactor,
                        &cfg.models.compactor_provider,
                        Arc::clone(&state.http),
                    ),
                    Classifier::new(
                        &cfg.api_keys.anthropic,
                        &cfg.api_keys.openai,
                        &cfg.models.classifier,
                        &cfg.models.classifier_provider,
                        Arc::clone(&state.http),
                    ),
                    state.memory_path.clone(),
                    crate::config::loader::load_soul(&cfg.agent.soul),
                    TurnConfig {
                        token_limit: cfg.settings.token_limit,
                        terminal_timeout: cfg.settings.terminal_timeout_secs,
                        max_output_chars: cfg.settings.max_output_chars,
                        max_tool_iterations: cfg.settings.max_tool_iterations,
                        max_retries: cfg.settings.max_retries,
                        sudo_password: sudo_pw,
                        working_directory: resolved_wd,
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
                        mode: mode.clone(),
                        plugin_manager: Some(Arc::clone(&state.plugin_manager)),
                        compaction_threshold: cfg.settings.compaction_threshold,
                        thinking_effort: cfg.settings.thinking_effort.clone(),
                        fallback_model: cfg.models.fallback.clone(),
                        fallback_provider: cfg.models.fallback_provider.clone(),
                        conv_logger: if cfg.settings.conversation_logging {
                            Some(crate::agent::router::ConvLogger {
                                buffer: Arc::clone(&state.conv_log_buffer),
                                notify: Arc::clone(&state.flush_notify),
                            })
                        } else {
                            None
                        },
                        chat_db: Arc::clone(&state.chat_db),
                    },
                    ctx,
                    cfg.settings.memory_file.clone(),
                )
            };

            info!(len = user_text.len(), "User message received");

            // Plugin hook: on_message — may rewrite user_text
            let user_text = {
                let hook_input = serde_json::json!({
                    "user_text": user_text,
                    "mode": mode,
                    "session_id": session_id,
                });
                let pm = state.plugin_manager.read().await;
                if let Some(mods) = pm.run_hook("on_message", &hook_input).await {
                    mods["user_text"].as_str().unwrap_or(&user_text).to_string()
                } else {
                    user_text
                }
            };

            // Push to history but DON'T save to DB yet — deferred until agent succeeds
            history.push(Message::user(&user_text));
            let user_text_for_db = user_text.clone();
            let mut user_msg_saved = false;

            // Channel for agent events
            let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();

            let hist_clone = history.clone();
            let memory = crate::memory::store::load_memory(&memory_file)
                .unwrap_or(Memory {
                    path: memory_path.clone(),
                    content: String::new(),
                });
            let task_db = Arc::clone(&state.task_db);

            // Inject planning instruction into the soul for this turn
            let soul_with_planning = format!(
                "{}\n\n[WORKING DIRECTORY]\n{}\n\n[INSTRUCTIONS]\n\
                Before taking any action, wrap a brief plan (2-3 lines) in <think>...</think> tags.\n\
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
                - Do NOT output code in markdown blocks (```python, ```bash) — instead, use write_file to save it and run_command to execute it.\n\
                - Your turn is NOT complete until you have called ALL necessary tools and delivered the final result.\n\
                - Think of each response as: <think>plan</think> → tool calls → result summary. Never stop at the plan.\n\n\
                ## Pre-flight Rule\n\
                Before starting any multi-step build task, identify ALL unknowns (credentials, names, paths, choices). \
                Ask them ALL in a single message BEFORE touching any tools. Never stop mid-task to ask something you could have asked upfront. \
                If you have enough context to proceed without asking, just do it.\n\n\
                ## Response Style\n\
                Keep answers concise. Bullet points over paragraphs. No preambles (\"Sure!\", \"Of course!\", \"Great question!\"). \
                For tasks: act first, give a brief summary after. Omit obvious filler.{sudo_note}",
                soul, turn_cfg.working_directory, sudo_note = sudo_note
            );

            // Spawn agent work
            let mem_lock = Arc::clone(&state.memory_lock);
            let classifier_for_title = classifier.clone();
            let classifier_for_recovery = classifier.clone();
            let agent_handle = tokio::spawn(async move {
                let mut hist = hist_clone;
                let result = router::classify_and_run(
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
                )
                .await;
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
                        // Return compacted history alongside text so the WS handler
                        // can persist it to SQLite (surviving browser close/restart).
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

            // Stream agent events to WebSocket
            // Track partial text for reconnection resilience
            let mut partial_text = String::new();
            let mut pending_ask: Option<tokio::sync::oneshot::Sender<String>> = None;
            let mut had_error = false;
            let mut history_pushed = false;
            let abort_handle = agent_handle.abort_handle();

            let agent_timeout = tokio::time::sleep(std::time::Duration::from_secs(600));
            tokio::pin!(agent_timeout);

            loop {
                tokio::select! {
                    event = rx.recv() => {
                        match event {
                            Some(AgentEvent::TextDelta(chunk)) => {
                                // First successful content from API — now commit user msg to DB
                                if !user_msg_saved {
                                    let _ = state.chat_db.save_message(&session_id, "user", &user_text_for_db);
                                    user_msg_saved = true;
                                }
                                partial_text.push_str(&chunk);
                                let msg = serde_json::json!({
                                    "type": "text_delta",
                                    "text": chunk,
                                });
                                if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                    // Connection lost — save partial text and abort agent
                                    if !partial_text.is_empty() {
                                        info!("DB save [assistant/partial] from TextDelta disconnect handler");
                                        let _ = state.chat_db.save_message(&session_id, "assistant", &format!("[partial] {}", partial_text));
                                    }
                                    abort_handle.abort();
                                    return;
                                }
                            }
                            Some(AgentEvent::Text(text)) => {
                                // Full text from router — update partial_text so Done handler can finalize.
                                // Don't save to DB here; Done handler saves once to avoid duplicates.
                                partial_text = text.clone();
                                let msg = serde_json::json!({
                                    "type": "assistant",
                                    "text": text,
                                });
                                if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                            Some(AgentEvent::Plan(plan)) => {
                                let msg = serde_json::json!({
                                    "type": "plan",
                                    "text": plan,
                                });
                                if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                            Some(AgentEvent::ToolCall { name, input }) => {
                                // Agent is working — commit user msg to DB
                                if !user_msg_saved {
                                    let _ = state.chat_db.save_message(&session_id, "user", &user_text_for_db);
                                    user_msg_saved = true;
                                }
                                let msg = serde_json::json!({
                                    "type": "tool_call",
                                    "name": name,
                                    "input": input,
                                });
                                if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                            Some(AgentEvent::ToolOutput { name, stdout, stderr, code }) => {
                                let msg = serde_json::json!({
                                    "type": "tool_output",
                                    "name": name,
                                    "stdout": crate::agent::router::strip_ansi(&stdout),
                                    "stderr": crate::agent::router::strip_ansi(&stderr),
                                    "code": code,
                                });
                                if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                            Some(AgentEvent::MemoryUpdate { section, content }) => {
                                let msg = serde_json::json!({
                                    "type": "memory_update",
                                    "section": section,
                                    "content": content,
                                });
                                if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                            Some(AgentEvent::FileOffer { path, caption }) => {
                                let file_name = std::path::Path::new(&path)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("file")
                                    .to_string();
                                let msg = serde_json::json!({
                                    "type": "file_offer",
                                    "path": path,
                                    "name": file_name,
                                    "caption": caption,
                                    "url": format!("/api/files?path={}", urlencoded(&path)),
                                });
                                if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                            Some(AgentEvent::AskUser { question, reply_tx }) => {
                                // Save the question as an assistant message so the conversation
                                // is preserved in DB even if the final response is tool-only.
                                if !user_msg_saved {
                                    let _ = state.chat_db.save_message(&session_id, "user", &user_text_for_db);
                                    user_msg_saved = true;
                                }
                                let _ = state.chat_db.save_message(&session_id, "assistant", &question);
                                let msg = serde_json::json!({
                                    "type": "ask_user",
                                    "text": question,
                                });
                                if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                    return;
                                }
                                pending_ask = Some(reply_tx);
                            }
                            Some(AgentEvent::Classified { class, detail }) => {
                                let msg = serde_json::json!({
                                    "type": "classified",
                                    "class": class,
                                    "detail": detail,
                                });
                                if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                            Some(AgentEvent::TrivialAnswer(answer)) => {
                                // Commit user message for trivial answers too
                                if !user_msg_saved {
                                    let _ = state.chat_db.save_message(&session_id, "user", &user_text_for_db);
                                    user_msg_saved = true;
                                }
                                // Push to history so LLM has context of the Q&A.
                                // Don't save assistant to DB here — the Text+Done events that
                                // follow from the router will handle the single DB save.
                                history.push(Message::assistant(&answer));
                                history_pushed = true;
                                let msg = serde_json::json!({
                                    "type": "trivial_answer",
                                    "text": answer,
                                });
                                if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                            Some(AgentEvent::Error(e)) => {
                                warn!(error = %e, "Agent error");
                                had_error = true;
                                // Save to DB so the error survives a page reload
                                if !user_msg_saved {
                                    let _ = state.chat_db.save_message(&session_id, "user", &user_text_for_db);
                                    user_msg_saved = true;
                                }
                                info!("DB save [assistant/error] from Error handler");
                                let _ = state.chat_db.save_message(&session_id, "assistant", &format!("⚠ Error: {}", e));
                                let msg = serde_json::json!({
                                    "type": "error",
                                    "text": e,
                                });
                                if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                            Some(AgentEvent::ModelUsed(model)) => {
                                let msg = serde_json::json!({"type": "model_used", "model": model});
                                if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                            Some(AgentEvent::SetAutonomous { enabled }) => {
                                auto_mode = enabled;
                                if !enabled {
                                    auto_turn_count = 0;
                                    pending_auto = None;
                                }
                                let msg = serde_json::json!({"type": "autonomous_mode", "enabled": enabled});
                                let _ = ws_tx.send(WsMessage::Text(msg.to_string().into())).await;
                            }
                            Some(AgentEvent::TaskQueued { id, title }) => {
                                let msg = serde_json::json!({"type": "task_queued", "id": id, "title": title});
                                let _ = ws_tx.send(WsMessage::Text(msg.to_string().into())).await;
                            }
                            Some(AgentEvent::TokenUsage { input, output, cache_read, cache_write, model }) => {
                                let msg = serde_json::json!({
                                    "type": "token_usage",
                                    "input": input,
                                    "output": output,
                                    "cache_read": cache_read,
                                    "cache_write": cache_write,
                                    "model": model,
                                });
                                let _ = ws_tx.send(WsMessage::Text(msg.to_string().into())).await;
                            }
                            Some(AgentEvent::Retry) => {
                                // Heartbeat decided the response was incomplete.
                                // Discard streamed text that the UI already rendered and start fresh.
                                partial_text.clear();
                                let msg = serde_json::json!({"type": "retry"});
                                if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                            Some(AgentEvent::Done) | None => {
                                // If agent never produced content, roll back the user message
                                if !user_msg_saved && partial_text.is_empty() {
                                    history.pop();
                                    info!("Rolled back user message — agent produced no output");
                                }
                                // Save final text to DB and history (once).
                                // Use a non-blocking check on agent_handle to avoid freezing the WS.
                                if !partial_text.is_empty() {
                                    let final_text = if agent_handle.is_finished() {
                                        match agent_handle.await {
                                            Ok(Some((full_text, compacted_hist))) => {
                                                // Persist compaction to SQLite so it survives reconnects.
                                                if let Some(compacted) = compacted_hist {
                                                    let tuples: Vec<(String, String)> = compacted.iter()
                                                        .map(|m| (m.role.clone(), m.content.clone()))
                                                        .collect();
                                                    if let Err(e) = state.chat_db.replace_session_messages(&session_id, &tuples) {
                                                        error!(error = %e, "Compaction: failed to persist to DB");
                                                    } else {
                                                        // Replace in-memory history so the next turn's
                                                        // hist_clone starts from the compacted state.
                                                        history.clear();
                                                        history.extend(compacted);
                                                        info!("Compaction persisted to DB and applied to in-memory history");
                                                    }
                                                }
                                                full_text
                                            }
                                            _ => std::mem::take(&mut partial_text),
                                        }
                                    } else {
                                        std::mem::take(&mut partial_text)
                                    };
                                    let clean = crate::agent::router::compress_tool_log(&crate::agent::router::strip_think_tags(&final_text));

                                    // Plugin hook: on_response — may modify response text
                                    let clean = {
                                        let hook_input = serde_json::json!({
                                            "response": clean,
                                            "session_id": session_id,
                                        });
                                        let pm = state.plugin_manager.read().await;
                                        if let Some(mods) = pm.run_hook("on_response", &hook_input).await {
                                            mods["response"].as_str().unwrap_or(&clean).to_string()
                                        } else {
                                            clean
                                        }
                                    };

                                    if !clean.trim().is_empty() {
                                        if !history_pushed {
                                            history.push(Message::assistant(&clean));
                                        }
                                        info!("DB save [assistant/final] from Done handler");
                                        let _ = state.chat_db.save_message(&session_id, "assistant", &clean);
                                    } else {
                                        info!("Skipped saving empty assistant message after stripping think/tool tags");
                                    }
                                }
                                let _ = ws_tx.send(WsMessage::Text(
                                    serde_json::json!({"type": "done"}).to_string().into()
                                )).await;

                                // Conversation recovery: periodically clean up correction noise
                                if !had_error {
                                    turns_since_recovery += 1;
                                    let recovery_interval = {
                                        state.config.read().await.settings.recovery_interval
                                    };
                                    if recovery_interval > 0
                                        && turns_since_recovery >= recovery_interval
                                        && history.len() >= 6
                                    {
                                        let window_start = recovery_window_start;
                                        let window: Vec<(String, String)> = history[window_start..]
                                            .iter()
                                            .map(|m| (m.role.clone(), m.content.clone()))
                                            .collect();

                                        if window.len() >= 4 {
                                            info!(
                                                window_size = window.len(),
                                                window_start = window_start,
                                                "Conversation recovery: starting cleanup"
                                            );
                                            match classifier_for_recovery
                                                .conversation_recovery(&window)
                                                .await
                                            {
                                                Some(cleaned) => {
                                                    let removed = window.len() - cleaned.len();
                                                    // Rebuild history: keep messages before window, append cleaned
                                                    history.truncate(window_start);
                                                    for (role, content) in &cleaned {
                                                        history.push(Message {
                                                            role: role.clone(),
                                                            content: content.clone(),
                                                        });
                                                    }
                                                    // Persist to DB: replace entire session messages
                                                    let full: Vec<(String, String)> = history
                                                        .iter()
                                                        .map(|m| (m.role.clone(), m.content.clone()))
                                                        .collect();
                                                    if let Err(e) = state
                                                        .chat_db
                                                        .replace_session_messages(&session_id, &full)
                                                    {
                                                        error!(error = %e, "Recovery: DB replace failed");
                                                    }
                                                    info!(
                                                        removed = removed,
                                                        "Conversation recovery: cleaned"
                                                    );
                                                    let _ = ws_tx
                                                        .send(WsMessage::Text(
                                                            serde_json::json!({
                                                                "type": "recovery",
                                                                "removed": removed,
                                                            })
                                                            .to_string()
                                                            .into(),
                                                        ))
                                                        .await;
                                                    recovery_window_start = history.len();
                                                }
                                                None => {
                                                    // No cleanup needed — advance window anyway
                                                    recovery_window_start = history.len();
                                                }
                                            }
                                        } else {
                                            recovery_window_start = history.len();
                                        }
                                        turns_since_recovery = 0;
                                    }
                                }

                                // Autonomous mode: queue next turn or end (skip on error)
                                if auto_mode && !had_error {
                                    if auto_turn_count < 10 {
                                        pending_auto = Some(
                                            "Continue with the next step autonomously. \
                                            If all steps are complete, call set_autonomous with enabled=false \
                                            and summarize what was accomplished.".to_string()
                                        );
                                    } else {
                                        auto_mode = false;
                                        auto_turn_count = 0;
                                        let _ = ws_tx.send(WsMessage::Text(
                                            serde_json::json!({"type": "autonomous_done", "message": "Autonomous mode reached maximum turns."}).to_string().into()
                                        )).await;
                                    }
                                }
                                break;
                            }
                        }
                    }
                    // Read incoming WS messages: handle cancel at any time, ask_reply when pending
                    ws_msg = ws_rx.next() => {
                        if let Some(Ok(WsMessage::Text(t))) = ws_msg {
                            let v: serde_json::Value = serde_json::from_str(&t.to_string()).unwrap_or_default();
                            if v["type"] == "cancel" {
                                abort_handle.abort();
                                // Roll back the user message so the next turn
                                // doesn't see a dangling unpaired user message.
                                if !user_msg_saved {
                                    history.pop();
                                } else {
                                    // User msg already in DB — save a cancellation note
                                    info!("DB save [assistant/cancelled] from Cancel handler");
                                    let _ = state.chat_db.save_message(&session_id, "assistant", "[cancelled]");
                                }
                                if auto_mode {
                                    auto_mode = false;
                                    auto_turn_count = 0;
                                    pending_auto = None;
                                }
                                let _ = ws_tx.send(WsMessage::Text(
                                    serde_json::json!({"type":"cancelled"}).to_string().into()
                                )).await;
                                let _ = ws_tx.send(WsMessage::Text(
                                    serde_json::json!({"type":"done"}).to_string().into()
                                )).await;
                                break;
                            }
                            if (v["type"] == "message" || v["type"] == "ask_reply") && pending_ask.is_some() {
                                let reply = v["text"].as_str().unwrap_or("").to_string();
                                history.push(Message::user(&reply));
                                let _ = state.chat_db.save_message(&session_id, "user", &reply);
                                if let Some(reply_tx) = pending_ask.take() {
                                    let _ = reply_tx.send(reply);
                                }
                            }
                        } else if matches!(ws_msg, Some(Ok(WsMessage::Close(_))) | None) {
                            abort_handle.abort();
                            return;
                        }
                    }
                    _ = &mut agent_timeout => {
                        abort_handle.abort();
                        let _ = ws_tx.send(WsMessage::Text(
                            serde_json::json!({"type":"error","text":"Agent turn timed out after 10 minutes."}).to_string().into()
                        )).await;
                        let _ = ws_tx.send(WsMessage::Text(
                            serde_json::json!({"type":"done"}).to_string().into()
                        )).await;
                        break;
                    }
                }
            }

            // Session title: generate eagerly on first 10 messages, then every 10 after.
            // Use DB count so reconnects don't reset the counter.
            // Skip Telegram sessions — they keep their permanent "Telegram" title.
            let db_msg_count = state.chat_db.message_count(&session_id);
            let no_title = state.chat_db.get_session_title(&session_id).is_empty();
            if !session_id.starts_with("telegram_") && db_msg_count >= 10 && (no_title || db_msg_count % 10 == 0) {
                let snippet: Vec<(String, String)> = history.iter()
                    .take(10)
                    .map(|m| (m.role.clone(), m.content.chars().take(150).collect()))
                    .collect();
                // Spawn title generation in background to avoid blocking the WS event loop
                let title_sid = session_id.clone();
                let title_db = Arc::clone(&state.chat_db);
                let title_bcast = state.broadcast_tx.clone();
                tokio::spawn(async move {
                    let title = classifier_for_title.generate_session_title(&snippet).await;
                    if !title.is_empty() {
                        let _ = title_db.update_session_title(&title_sid, &title);
                        let _ = title_bcast.send(
                            serde_json::json!({
                                "type": "session_title_updated",
                                "session_id": title_sid,
                                "title": title,
                            }).to_string()
                        );
                    }
                });
            }

        } else if incoming["type"] == "new_session" {
            // Start a fresh session
            history.clear();
            auto_mode = false;
            auto_turn_count = 0;
            pending_auto = None;
            turns_since_recovery = 0;
            recovery_window_start = 0;
            session_id = state.chat_db.create_session().unwrap_or_else(|_| "default".into());

            // Plugin hook: on_session_start
            {
                let pm = state.plugin_manager.read().await;
                let _ = pm.run_hook("on_session_start", &serde_json::json!({
                    "session_id": session_id,
                })).await;
            }

            let msg = serde_json::json!({
                "type": "system",
                "text": format!("New session started: {}", session_id),
                "session_id": session_id,
            });
            let _ = ws_tx.send(WsMessage::Text(msg.to_string().into())).await;
        } else if incoming["type"] == "switch_session" {
            let target_id = incoming["session_id"].as_str().unwrap_or("").to_string();
            if !target_id.is_empty() {
                history.clear();
                auto_mode = false;
                auto_turn_count = 0;
                pending_auto = None;
                turns_since_recovery = 0;
                if let Ok(msgs) = state.chat_db.load_session_messages(&target_id) {
                    for m in msgs {
                        // Skip system-role messages (watcher diagnostics etc.) — not for LLM
                        if m.role == "system" { continue; }
                        // Skip [partial] messages saved during disconnects — incomplete data
                        if m.content.starts_with("[partial] ") { continue; }
                        let content = if m.role == "assistant" {
                            crate::agent::router::strip_think_tags(&m.content)
                        } else { m.content };
                        history.push(Message { role: m.role, content });
                    }
                }
                session_id = target_id.clone();
                recovery_window_start = history.len(); // start fresh for switched session

                let stored_title = state.chat_db.get_session_title(&target_id);
                let sys_msg = serde_json::json!({
                    "type": "session_switched",
                    "session_id": target_id,
                    "session_title": stored_title,
                });
                let _ = ws_tx.send(WsMessage::Text(sys_msg.to_string().into())).await;

                // Replay session history
                for m in &history {
                    let msg = serde_json::json!({
                        "type": if m.role == "user" { "history_user" } else { "history_assistant" },
                        "text": m.content,
                    });
                    if ws_tx.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
    info!("WebSocket connection closed");
}

/// Check whether the axium systemd service is enabled for autostart.
async fn get_autostart_handler() -> Json<serde_json::Value> {
    let output = tokio::process::Command::new("systemctl")
        .args(["is-enabled", "axium"])
        .output()
        .await;

    match output {
        Ok(out) => {
            let status = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let enabled = status == "enabled";
            Json(serde_json::json!({ "enabled": enabled, "status": status }))
        }
        Err(_) => Json(serde_json::json!({
            "enabled": false,
            "status": "unavailable",
            "error": "systemctl not found — not running on a systemd system"
        })),
    }
}

/// Enable or disable the axium systemd service using the stored sudo password.
async fn set_autostart_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let enabled = body["enabled"].as_bool().unwrap_or(false);
    let sudo_pw = state.sudo_password.read().await.clone();

    if sudo_pw.is_empty() {
        return Json(serde_json::json!({
            "ok": false,
            "error": "Sudo password not set. Enter it in Settings first."
        }));
    }

    let action = if enabled { "enable" } else { "disable" };

    let mut child = match tokio::process::Command::new("sudo")
        .args(["-S", "systemctl", action, "axium"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Json(serde_json::json!({ "ok": false, "error": format!("Failed to run sudo: {}", e) })),
    };

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(format!("{}\n", sudo_pw).as_bytes()).await;
    }

    match child.wait_with_output().await {
        Ok(out) if out.status.success() => Json(serde_json::json!({ "ok": true })),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Json(serde_json::json!({ "ok": false, "error": stderr }))
        }
        Err(e) => Json(serde_json::json!({ "ok": false, "error": format!("{}", e) })),
    }
}

/// Stop the agent process cleanly (exits with 0 so systemd Restart=on-failure won't revive it).
async fn stop_action_handler() -> Json<serde_json::Value> {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        std::process::exit(0);
    });
    Json(serde_json::json!({ "ok": true }))
}

/// Shut down the host machine immediately.
async fn shutdown_action_handler(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let sudo_pw = state.sudo_password.read().await.clone();
    if sudo_pw.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "Sudo password not set in Settings." }));
    }
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        if let Ok(mut child) = tokio::process::Command::new("sudo")
            .args(["-S", "shutdown", "-h", "now"])
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(format!("{}\n", sudo_pw).as_bytes()).await;
            }
        }
    });
    Json(serde_json::json!({ "ok": true }))
}

/// Reboot the host machine immediately.
async fn reboot_action_handler(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let sudo_pw = state.sudo_password.read().await.clone();
    if sudo_pw.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "Sudo password not set in Settings." }));
    }
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        if let Ok(mut child) = tokio::process::Command::new("sudo")
            .args(["-S", "reboot"])
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(format!("{}\n", sudo_pw).as_bytes()).await;
            }
        }
    });
    Json(serde_json::json!({ "ok": true }))
}

async fn health_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

// ── Skills Management API ────────────────────────────────────────────

const SKILLS_DIR: &str = "axium-skills";

/// Sanitize a user-provided name into a safe folder/file component.
fn sanitize_name(input: &str) -> String {
    let s: String = input
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    // collapse multiple hyphens
    let mut out = String::new();
    let mut prev_hyphen = false;
    for c in s.chars() {
        if c == '-' {
            if !prev_hyphen { out.push(c); }
            prev_hyphen = true;
        } else {
            out.push(c);
            prev_hyphen = false;
        }
    }
    if out.len() > 64 { out.truncate(64); }
    out
}

/// Validate that path components stay within axium-skills/.
fn validate_skill_components(components: &[&str]) -> bool {
    for c in components {
        if c.is_empty() || *c == "." || *c == ".."
            || c.contains('/') || c.contains('\\') || c.contains('\0')
        {
            return false;
        }
    }
    true
}

async fn list_skills_handler() -> Json<serde_json::Value> {
    let skills_dir = std::path::Path::new(SKILLS_DIR);
    if !skills_dir.is_dir() {
        let _ = std::fs::create_dir_all(skills_dir);
        return Json(serde_json::json!({ "skills": [] }));
    }
    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(skills_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                let mut files = Vec::new();
                if let Ok(file_entries) = std::fs::read_dir(entry.path()) {
                    for fe in file_entries.flatten() {
                        if fe.path().is_file() {
                            let fname = fe.file_name().to_string_lossy().to_string();
                            let size = fe.metadata().map(|m| m.len()).unwrap_or(0);
                            files.push(serde_json::json!({ "name": fname, "size": size }));
                        }
                    }
                }
                files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                skills.push(serde_json::json!({ "name": name, "files": files }));
            }
        }
    }
    skills.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Json(serde_json::json!({ "skills": skills }))
}

async fn create_skill_folder_handler(
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let raw = match body["name"].as_str() {
        Some(n) if !n.trim().is_empty() => n,
        _ => return Json(serde_json::json!({"ok": false, "error": "Missing folder name"})),
    };
    let name = sanitize_name(raw);
    if name.is_empty() {
        return Json(serde_json::json!({"ok": false, "error": "Invalid folder name"}));
    }
    let path = std::path::Path::new(SKILLS_DIR).join(&name);
    if path.exists() {
        return Json(serde_json::json!({"ok": false, "error": "Folder already exists"}));
    }
    match std::fs::create_dir_all(&path) {
        Ok(_) => Json(serde_json::json!({"ok": true, "name": name})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": format!("{}", e)})),
    }
}

async fn delete_skill_folder_handler(
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let name = match body["name"].as_str() {
        Some(n) if !n.is_empty() => n,
        _ => return Json(serde_json::json!({"ok": false, "error": "Missing folder name"})),
    };
    if !validate_skill_components(&[name]) {
        return Json(serde_json::json!({"ok": false, "error": "Invalid folder name"}));
    }
    let path = std::path::Path::new(SKILLS_DIR).join(name);
    if !path.is_dir() {
        return Json(serde_json::json!({"ok": false, "error": "Folder not found"}));
    }
    // double-check canonical path stays within skills dir
    if let (Ok(base), Ok(target)) = (std::fs::canonicalize(SKILLS_DIR), std::fs::canonicalize(&path)) {
        if !target.starts_with(&base) {
            return Json(serde_json::json!({"ok": false, "error": "Access denied"}));
        }
    }
    match std::fs::remove_dir_all(&path) {
        Ok(_) => Json(serde_json::json!({"ok": true})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": format!("{}", e)})),
    }
}

async fn get_skill_file_handler(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let folder = match params.get("folder") {
        Some(f) if !f.is_empty() => f.as_str(),
        _ => return Json(serde_json::json!({"ok": false, "error": "Missing folder"})),
    };
    let file = match params.get("file") {
        Some(f) if !f.is_empty() => f.as_str(),
        _ => return Json(serde_json::json!({"ok": false, "error": "Missing file"})),
    };
    if !validate_skill_components(&[folder, file]) {
        return Json(serde_json::json!({"ok": false, "error": "Invalid path"}));
    }
    let path = std::path::Path::new(SKILLS_DIR).join(folder).join(file);
    if !path.is_file() {
        return Json(serde_json::json!({"ok": false, "error": "File not found"}));
    }
    if let (Ok(base), Ok(target)) = (std::fs::canonicalize(SKILLS_DIR), std::fs::canonicalize(&path)) {
        if !target.starts_with(&base) {
            return Json(serde_json::json!({"ok": false, "error": "Access denied"}));
        }
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => Json(serde_json::json!({"ok": true, "content": content, "folder": folder, "file": file})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": format!("{}", e)})),
    }
}

async fn save_skill_file_handler(
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let folder = match body["folder"].as_str() {
        Some(f) if !f.is_empty() => f,
        _ => return Json(serde_json::json!({"ok": false, "error": "Missing folder"})),
    };
    let file = match body["file"].as_str() {
        Some(f) if !f.is_empty() => f,
        _ => return Json(serde_json::json!({"ok": false, "error": "Missing file name"})),
    };
    let content = body["content"].as_str().unwrap_or("");
    if !validate_skill_components(&[folder, file]) {
        return Json(serde_json::json!({"ok": false, "error": "Invalid path"}));
    }
    let base = match std::fs::canonicalize(SKILLS_DIR) {
        Ok(b) => b,
        Err(_) => return Json(serde_json::json!({"ok": false, "error": "Skills directory not found"})),
    };
    let folder_path = base.join(folder);
    if !folder_path.is_dir() || !folder_path.starts_with(&base) {
        return Json(serde_json::json!({"ok": false, "error": "Invalid folder"}));
    }
    let file_path = folder_path.join(file);
    if !file_path.starts_with(&folder_path) {
        return Json(serde_json::json!({"ok": false, "error": "Invalid file name"}));
    }
    match std::fs::write(&file_path, content) {
        Ok(_) => Json(serde_json::json!({"ok": true})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": format!("{}", e)})),
    }
}

async fn delete_skill_file_handler(
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let folder = match body["folder"].as_str() {
        Some(f) if !f.is_empty() => f,
        _ => return Json(serde_json::json!({"ok": false, "error": "Missing folder"})),
    };
    let file = match body["file"].as_str() {
        Some(f) if !f.is_empty() => f,
        _ => return Json(serde_json::json!({"ok": false, "error": "Missing file name"})),
    };
    if !validate_skill_components(&[folder, file]) {
        return Json(serde_json::json!({"ok": false, "error": "Invalid path"}));
    }
    let path = std::path::Path::new(SKILLS_DIR).join(folder).join(file);
    if !path.is_file() {
        return Json(serde_json::json!({"ok": false, "error": "File not found"}));
    }
    if let (Ok(base), Ok(target)) = (std::fs::canonicalize(SKILLS_DIR), std::fs::canonicalize(&path)) {
        if !target.starts_with(&base) {
            return Json(serde_json::json!({"ok": false, "error": "Access denied"}));
        }
    }
    match std::fs::remove_file(&path) {
        Ok(_) => Json(serde_json::json!({"ok": true})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": format!("{}", e)})),
    }
}

// ── Plugin management handlers ──────────────────────────────────────────

async fn list_plugins_handler(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let pm = state.plugin_manager.read().await;
    Json(serde_json::json!({"ok": true, "plugins": pm.list_plugins()}))
}

async fn toggle_plugin_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let name = match body["name"].as_str() {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return Json(serde_json::json!({"ok": false, "error": "Missing plugin name"})),
    };
    let enabled = body["enabled"].as_bool().unwrap_or(true);
    let mut pm = state.plugin_manager.write().await;
    if pm.set_enabled(&name, enabled) {
        Json(serde_json::json!({"ok": true}))
    } else {
        Json(serde_json::json!({"ok": false, "error": "Plugin not found"}))
    }
}

async fn reorder_plugins_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let order: Vec<String> = match body["order"].as_array() {
        Some(arr) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        None => return Json(serde_json::json!({"ok": false, "error": "Missing order array"})),
    };
    let mut pm = state.plugin_manager.write().await;
    pm.set_order(&order);
    Json(serde_json::json!({"ok": true}))
}

async fn flush_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    flush_conv_log(&state).await;
    flush_task_buffers(&state).await;
    Json(serde_json::json!({"ok": true}))
}

/// Atomically drain the conversation log buffer and append to disk.
pub async fn flush_conv_log(state: &AppState) {
    let entries: Vec<String> = {
        let mut guard = state.conv_log_buffer.lock().await;
        if guard.is_empty() { return; }
        std::mem::take(&mut *guard)
    };
    let log_path = std::path::Path::new(&state.memory_path)
        .parent()
        .map(|d| d.join("conversation.log"))
        .unwrap_or_else(|| std::path::PathBuf::from("conversation.log"));
    if let Some(parent) = log_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let content: String = entries.concat();
    use tokio::io::AsyncWriteExt;
    match tokio::fs::OpenOptions::new().create(true).append(true).open(&log_path).await {
        Ok(mut f) => { if let Err(e) = f.write_all(content.as_bytes()).await {
            warn!(error = %e, "Failed to flush conversation log buffer");
        }}
        Err(e) => warn!(error = %e, path = %log_path.display(), "Failed to open conversation log for flush"),
    }
}

/// Snapshot all in-flight task file buffers and write them to disk.
pub async fn flush_task_buffers(state: &AppState) {
    let snapshots: Vec<(i64, std::path::PathBuf, String)> = {
        let guard = state.task_file_buffers.read().await;
        guard.iter().map(|(id, (p, c))| (*id, p.clone(), c.clone())).collect()
    };
    for (_task_id, path, content) in snapshots {
        if let Err(e) = tokio::fs::write(&path, &content).await {
            warn!(error = %e, path = %path.display(), "Failed to flush task buffer");
        }
    }
}

/// Returns true for loopback and RFC-1918 private addresses.
/// Rejects everything else (public internet).
fn is_lan_or_loopback(ip: std::net::IpAddr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private(),
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            // fe80::/10 — link-local
            (seg[0] & 0xffc0) == 0xfe80
            // fc00::/7 — unique local (ULA)
            || (seg[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Minimal percent-encoding for URL query parameter values.
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}
