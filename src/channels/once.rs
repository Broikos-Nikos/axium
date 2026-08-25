//! `--once`: run a single turn non-interactively and print one JSON line.
//!
//! Every other channel is a conversation, a REPL, a web socket, a Telegram
//! chat. None of them can be driven by a benchmark harness, which needs to hand
//! the agent one message, wait, and read back exactly what happened and what it
//! cost. Without this, `bench-rust/` has nothing to drive.
//!
//! ```text
//! axium --once "fix the discount bug" --workdir C:\builds\shop_1
//! axium --once "what does pricing.py do?" --workdir . --session s1
//! ```
//!
//! One JSON object on **stdout**, everything else on stderr, so a caller can
//! parse stdout without filtering log noise. The object carries the turn's text,
//! the files it changed, the prompt class, and the full `TurnMetrics`, tokens,
//! per-role cost split, tool histogram, wall time.
//!
//! Exit code is 0 when the turn ran, 1 when it could not. A turn that ran and
//! failed still prints its JSON, because "it errored after 14 tool calls" is a
//! benchmark result, not an absence of one.

use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::agent::classifier::Classifier;
use crate::agent::compactor::Compactor;
use crate::agent::metrics;
use crate::agent::router::{self, AgentEvent, TurnConfig};
use crate::agent::sonnet::SonnetClient;
use crate::agent::Message;
use crate::tui::server::AppState;

/// One line of machine-readable turn output.
#[derive(Debug, Serialize)]
pub struct OnceResult {
    pub ok: bool,
    pub text: String,
    /// Workdir-relative paths the turn touched, sorted for a stable diff.
    pub changed: Vec<String>,
    /// What the classifier decided: trivial / simple / medium / complex / skills.
    pub class: String,
    /// Questions the agent asked via `ask_user`. Each was auto-approved so the
    /// run could continue; that it asked at all is what a benchmark measures.
    pub asked: Vec<String>,
    pub error: Option<String>,
    pub metrics: metrics::TurnMetrics,
}

/// Parse `--once <message>` and the flags that go with it.
pub struct OnceArgs {
    pub message: String,
    pub workdir: Option<String>,
    pub session: String,
}

impl OnceArgs {
    /// Returns `None` when `--once` is absent, so `main` can fall through.
    pub fn from_env() -> Option<Self> {
        let args: Vec<String> = std::env::args().collect();
        let pos = args.iter().position(|a| a == "--once")?;
        let message = args.get(pos + 1).cloned().unwrap_or_default();
        let flag = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1))
                .cloned()
        };
        Some(Self {
            message,
            workdir: flag("--workdir"),
            // A stable session id lets a multi-turn scenario share history and
            // memory across separate process invocations, which is exactly what
            // the continuity scenario is testing.
            session: flag("--session").unwrap_or_else(|| "once".to_string()),
        })
    }
}

/// Run one turn and print the JSON line. Returns the process exit code.
pub async fn run(state: Arc<AppState>, args: OnceArgs) -> Result<i32> {
    if args.message.trim().is_empty() {
        eprintln!("--once needs a message: axium --once \"<message>\" [--workdir PATH]");
        return Ok(1);
    }

    let cfg = state.config.read().await.clone();
    let workdir = crate::config::loader::expand_home(
        args.workdir.as_deref().unwrap_or(&cfg.settings.working_directory),
    );

    let meter = metrics::new_handle();
    let http = Arc::clone(&state.http);
    let keys = cfg.api_keys.as_set();

    let sonnet = SonnetClient::new(
        &keys, &cfg.models.primary, &cfg.models.primary_provider,
        cfg.settings.max_tokens, Arc::clone(&http),
    );
    let compactor = Compactor::new(
        &keys, &cfg.models.compactor, &cfg.models.compactor_provider, Arc::clone(&http),
    );
    let classifier = Classifier::new(
        &keys, &cfg.models.classifier, &cfg.models.classifier_provider, Arc::clone(&http),
    )
    .with_meter(Some(Arc::clone(&meter)));

    // History is loaded from and saved to the session, so a multi-turn scenario
    // driven as several `--once` invocations behaves like one conversation.
    let session_id = state.chat_db.find_or_create_session(&args.session).ok();
    let mut history: Vec<Message> = session_id
        .as_ref()
        .and_then(|id| state.chat_db.load_session_messages(id).ok())
        .unwrap_or_default()
        .into_iter()
        .rev()
        .take(cfg.settings.max_history_messages)
        .rev()
        .map(|m| Message { role: m.role, content: m.content })
        .collect();
    history.push(Message::user(&args.message));

    let memory = crate::memory::store::load_memory(&state.memory_path)?;
    let soul = crate::config::loader::load_soul(&cfg.agent.soul);
    let project_context = router::build_project_context_for(&workdir);

    // Events are drained rather than displayed: the classification is worth
    // capturing, the streamed text is already in the final result.
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let class = Arc::new(std::sync::Mutex::new(String::new()));
    let asked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let (class_sink, asked_sink) = (Arc::clone(&class), Arc::clone(&asked));
    let drain = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::Classified { class, .. } => {
                    // First wins. The post-turn pass also emits Classified
                    // events ("facts", "skill") and those must not overwrite
                    // what the classifier decided about the prompt.
                    let mut slot = class_sink.lock().unwrap_or_else(|e| e.into_inner());
                    if slot.is_empty() {
                        // The router names the complex case "enhanced"; the
                        // Python build and the graders call it "complex".
                        *slot = if class == "enhanced" { "complex".into() } else { class };
                    }
                }
                AgentEvent::AskUser { question, reply_tx } => {
                    // Non-interactive: approve so the run continues, and record
                    // it. An agent that asks instead of acting is a measurable
                    // trait, not a stall. Same policy as the Python bench.
                    asked_sink.lock().unwrap_or_else(|e| e.into_inner()).push(question);
                    let _ = reply_tx.send("yes (auto-approved: non-interactive session)".into());
                }
                _ => {}
            }
        }
    });

    let turn_cfg = TurnConfig {
        meter: Some(Arc::clone(&meter)),
        facts: state.durable.facts.clone(),
        checkpoints: state.durable.checkpoints.clone(),
        trajectory: state.durable.trajectory.clone(),
        brain_enabled: cfg.settings.brain_enabled,
        planner_enabled: cfg.settings.planner_enabled,
        distill_skills: cfg.settings.distill_skills,
        skills_dir: cfg.settings.skills_dir.clone(),
        working_directory: workdir.clone(),
        ..router::base_turn_config(&cfg, &state)
    };

    let outcome = router::classify_and_run(
        &classifier, &sonnet, &compactor, &mut history, &memory, &soul,
        &project_context, &state.task_db, turn_cfg, &tx,
    )
    .await;

    drop(tx);
    let _ = drain.await;

    // The checkpoint knows exactly which files the turn touched, so nothing has
    // to track that separately just for this output.
    let changed = state
        .durable
        .checkpoints
        .as_ref()
        .map(|cp| cp.lock().unwrap_or_else(|e| e.into_inner()).last_files())
        .unwrap_or_default();

    let snapshot = meter.lock().unwrap_or_else(|e| e.into_inner()).snapshot();
    let (ran, text, error) = match outcome {
        Ok((text, _ops, _enhanced)) => (true, text, None),
        Err(e) => (false, String::new(), Some(format!("{e:#}"))),
    };
    // `ok` means the turn PRODUCED something, not merely that no Rust error
    // propagated. The router swallows an auth failure and returns empty text, so
    // a bare `Ok(_)` here would report a turn that made four failed API calls and
    // did nothing as a success, and a benchmark would score it as one.
    let produced = !text.trim().is_empty() || !changed.is_empty();
    let ok = ran && produced;
    let error = error.or_else(|| {
        (!ok).then(|| {
            if snapshot.errors.is_empty() {
                "turn produced no text and changed no files".to_string()
            } else {
                snapshot.errors.join("; ")
            }
        })
    });

    if let Some(id) = session_id {
        let _ = state.chat_db.save_message(&id, "user", &args.message);
        if !text.is_empty() {
            let _ = state.chat_db.save_message(&id, "assistant", &text);
        }
    }

    let result = OnceResult {
        ok,
        text,
        changed,
        class: class.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        asked: asked.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        error,
        metrics: snapshot,
    };
    println!("{}", serde_json::to_string(&result)?);
    Ok(if result.ok { 0 } else { 1 })
}
