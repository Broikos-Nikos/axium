mod agent;
mod channels;
mod config;
mod db;
mod memory;
mod plugins;
mod tools;
mod tui;
mod watcher;
mod worker;

use std::sync::Arc;
use anyhow::Result;
use tokio::sync::RwLock;
use tracing::{info, error, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {

    // Simple structured logging to stdout only
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("axiom=info"))
        .with_target(false)
        .compact()
        .init();

    let config_path = "config.json";
    let mut cfg = config::loader::load_config(config_path)?;

    // Environment variable fallback for API keys
    if cfg.api_keys.anthropic.is_empty() {
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            if !key.is_empty() {
                info!("Using ANTHROPIC_API_KEY from environment");
                cfg.api_keys.anthropic = key;
            }
        }
    }
    if cfg.api_keys.openai.is_empty() {
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            if !key.is_empty() {
                info!("Using OPENAI_API_KEY from environment");
                cfg.api_keys.openai = key;
            }
        }
    }

    if cfg.api_keys.anthropic.is_empty() && cfg.api_keys.openai.is_empty() {
        error!("No API keys configured in config.json");
        eprintln!("Error: No API keys configured. Add at least one API key then run again.");
        return Err(anyhow::anyhow!("No API keys configured. Add at least one API key to config.json then restart."));
    }

    let memory_path = cfg.settings.memory_file.clone();

    // Ensure memory file exists
    let _ = memory::store::load_memory(&memory_path)?;

    // Initialize SQLite databases
    let chat_db = Arc::new(db::history::ChatDb::open("chat_history.db")?);
    let task_db = Arc::new(db::tasks::TaskDb::open("chat_history.db")?);
    info!("SQLite database initialized");

    // Prune old sessions on startup and periodically (every 6 hours)
    let max_sessions = cfg.settings.max_sessions;
    let prune_db = Arc::clone(&chat_db);
    tokio::spawn(async move {
        loop {
            match prune_db.session_count() {
                Ok(count) if count > max_sessions => {
                    match prune_db.prune_old_sessions(max_sessions) {
                        Ok(pruned) => info!(pruned, kept = max_sessions, "Pruned old sessions"),
                        Err(e) => warn!(error = %e, "Failed to prune sessions"),
                    }
                }
                _ => {}
            }
            tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
        }
    });

    let http = Arc::new(reqwest::Client::builder()
        .pool_max_idle_per_host(4)
        .timeout(std::time::Duration::from_secs(120))
        .build()?);

    let (broadcast_tx, _) = tokio::sync::broadcast::channel::<String>(64);

    // Load plugin manager
    let plugin_manager = Arc::new(RwLock::new(plugins::PluginManager::load()));

    // Telegram bot shutdown channel (replaced on each settings save)
    let (tg_shutdown_tx, tg_shutdown_rx) = tokio::sync::watch::channel(false);

    let project_context_cache = std::sync::Arc::new(RwLock::new(None));

    let state = Arc::new(tui::server::AppState {
        config: RwLock::new(cfg),
        config_path: config_path.to_string(),
        memory_path,
        chat_db,
        task_db,
        http,
        memory_lock: Arc::new(tokio::sync::Mutex::new(())),
        sudo_password: RwLock::new(String::new()),
        broadcast_tx: broadcast_tx.clone(),
        telegram_shutdown: tokio::sync::Mutex::new(tg_shutdown_tx),
        plugin_manager,
        project_context_cache: project_context_cache.clone(),
        conv_log_buffer: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        flush_notify: Arc::new(tokio::sync::Notify::new()),
        task_file_buffers: Arc::new(RwLock::new(std::collections::HashMap::new())),
    });

    // Spawn background file watcher for the configured working directory
    {
        let raw_wd = state.config.read().await.settings.working_directory.clone();
        let working_dir = if raw_wd.is_empty() || raw_wd == "~" {
            std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
        } else if raw_wd.starts_with("~/") {
            let home = std::env::var("HOME").unwrap_or_default();
            format!("{}/{}", home, &raw_wd[2..])
        } else {
            raw_wd
        };
        watcher::spawn_watcher(working_dir, broadcast_tx, project_context_cache);
    }
    worker::spawn_worker(state.clone());

    // Background flush task: wakes every 30s or when signalled (buffer threshold / /api/flush)
    {
        let flush_state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = flush_state.flush_notify.notified() => {}
                }
                tui::server::flush_conv_log(&flush_state).await;
                tui::server::flush_task_buffers(&flush_state).await;
            }
        });
    }

    let app = tui::server::build_router(state.clone());

    let addr: std::net::SocketAddr = "0.0.0.0:3000".parse()?;
    {
        let cfg = state.config.read().await;
        let lan_ip = local_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "<unknown>".to_string());
        info!(name = %cfg.agent.name, addr = %addr, "Starting server");
        println!();
        println!("  ┌─────────────────────────────────────────────┐");
        println!("  │  {} ready                                    ", cfg.agent.name);
        println!("  │");
        println!("  │  Local:   http://localhost:3000");
        println!("  │  Network: http://{}:3000", lan_ip);
        println!("  │");
        println!("  │  SSH tunnel from your PC:");
        println!("  │  ssh -L 3000:localhost:3000 pi@{}", lan_ip);
        println!("  │  then open http://localhost:3000");
        println!("  └─────────────────────────────────────────────┘");
        println!();
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Shared shutdown signal for graceful cleanup
    let shutdown_state = state.clone();

    // Graceful shutdown on SIGINT/SIGTERM
    let shutdown = async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM");
        #[cfg(unix)]
        let sigterm_recv = sigterm.recv();
        #[cfg(not(unix))]
        let sigterm_recv = std::future::pending::<Option<()>>();

        tokio::select! {
            _ = ctrl_c => info!("Received SIGINT, shutting down..."),
            _ = sigterm_recv => info!("Received SIGTERM, shutting down..."),
        }

        // Stop the Telegram bot cleanly
        let _ = shutdown_state.telegram_shutdown.lock().await.send(true);
    };

    // Spawn Telegram bot if configured
    channels::telegram::TelegramBot::spawn(state.clone(), tg_shutdown_rx).await;

    info!("Server ready");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;

    // Flush any remaining in-memory write buffers before exiting
    tui::server::flush_conv_log(&state).await;
    tui::server::flush_task_buffers(&state).await;
    info!("Server stopped");
    Ok(())
}

/// Determine the machine's outbound LAN IP without sending any data.
fn local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}
