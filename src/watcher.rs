use std::path::Path;
use tokio::sync::broadcast;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Type alias for the project context cache shared with AppState.
type ProjectCache = std::sync::Arc<RwLock<Option<(std::time::Instant, String, String)>>>;

/// Spawn a background file watcher that monitors `working_dir` for source file changes
/// and broadcasts diagnostic messages (syntax errors, etc.) to connected WebSocket clients.
/// Also invalidates the project context cache when source files change.
pub fn spawn_watcher(working_dir: String, broadcast_tx: broadcast::Sender<String>, project_cache: ProjectCache) {
    tokio::spawn(async move {
        if let Err(e) = run_watcher(working_dir, broadcast_tx, project_cache).await {
            warn!(error = %e, "File watcher stopped");
        }
    });
}

async fn run_watcher(
    working_dir: String,
    broadcast_tx: broadcast::Sender<String>,
    project_cache: ProjectCache,
) -> anyhow::Result<()> {
    use notify::{Event, EventKind, RecursiveMode, Watcher};

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            let _ = tx.blocking_send(event);
        }
    })?;

    watcher.watch(Path::new(&working_dir), RecursiveMode::Recursive)?;
    info!(dir = %working_dir, "File watcher started");

    // Simple debounce: track last-seen content hash per path so rapid repeated saves
    // don't trigger repeated diagnostics.
    let mut last_hash: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

    while let Some(event) = rx.recv().await {
        if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
            continue;
        }

        for path in event.paths {
            let path_str = path.to_string_lossy().to_string();

            // Skip noisy paths
            if path_str.contains("/.git/")
                || path_str.contains("/target/")
                || path_str.contains("/.axium/")
                || path_str.contains("/node_modules/")
                || path_str.contains("/__pycache__/")
                || path_str.contains("/.claude/")  // Claude Code internals (plugins, cache, etc.)
            {
                continue;
            }

            let ext = match path.extension().and_then(|e| e.to_str()) {
                Some(e) => e.to_string(),
                None => continue,
            };

            if !["rs", "py", "php", "go", "rb", "sh", "js"].contains(&ext.as_str()) {
                continue;
            }

            // Debounce: skip if file content hasn't changed
            let content_hash = tokio::fs::read(&path)
                .await
                .map(|b| {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    b.hash(&mut h);
                    h.finish()
                })
                .unwrap_or(0);

            if last_hash.get(&path_str) == Some(&content_hash) {
                continue;
            }
            last_hash.insert(path_str.clone(), content_hash);
            // Invalidate project context cache when source files change
            {
                let mut cache = project_cache.write().await;
                *cache = None;
            }
            // Cap hash map size to prevent unbounded growth
            if last_hash.len() > 1000 {
                let keys: Vec<String> = last_hash.keys().take(200).cloned().collect();
                for k in keys { last_hash.remove(&k); }
            }

            let tx2 = broadcast_tx.clone();
            let ext2 = ext.clone();
            tokio::spawn(async move {
                // Small delay so the OS has time to finish the write before we read.
                // Without this we can catch partial writes and get false "EOF" errors.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                if let Some(error) = run_diagnostics(&path_str, &ext2).await {
                    let msg = serde_json::json!({
                        "type": "watcher_diagnostic",
                        "path": path_str,
                        "message": error,
                    })
                    .to_string();
                    let _ = tx2.send(msg);
                }
            });
        }
    }

    Ok(())
}

/// Run a cheap syntax check for a file based on its extension.
/// Returns Some(error_text) on error, None if clean or unknown type.
/// Avoids expensive project-level checks (no cargo check) to stay responsive.
async fn run_diagnostics(path: &str, ext: &str) -> Option<String> {
    match ext {
        "py" => {
            let out = tokio::process::Command::new("python3")
                .args(["-m", "py_compile", path])
                .output()
                .await
                .ok()?;
            if out.status.success() {
                None
            } else {
                Some(format!(
                    "⚠ Python: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
        "php" => {
            let out = tokio::process::Command::new("php")
                .args(["-l", path])
                .output()
                .await
                .ok()?;
            if out.status.success() {
                None
            } else {
                Some(format!(
                    "⚠ PHP: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
        "rb" => {
            let out = tokio::process::Command::new("ruby")
                .args(["-c", path])
                .output()
                .await
                .ok()?;
            if out.status.success() {
                None
            } else {
                Some(format!(
                    "⚠ Ruby: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
        "sh" | "bash" => {
            let out = tokio::process::Command::new("bash")
                .args(["-n", path])
                .output()
                .await
                .ok()?;
            if out.status.success() {
                None
            } else {
                Some(format!(
                    "⚠ Bash: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
        "js" => {
            let out = tokio::process::Command::new("node")
                .args(["--check", path])
                .output()
                .await
                .ok()?;
            if out.status.success() {
                None
            } else {
                Some(format!(
                    "⚠ JS: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
        // Rust: skip cargo check here (too expensive for every save).
        // The agent's write_file/patch_file already runs cargo check.
        _ => None,
    }
}
