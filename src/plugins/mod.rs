use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Known hook names. A plugin provides a hook by placing an executable file
/// named `<hook_name>.*` (e.g. `on_message.sh`, `on_tool_before.py`) in its folder.
pub const HOOK_NAMES: &[&str] = &[
    "on_message",
    "on_classified",
    "on_tool_before",
    "on_tool_after",
    "on_response",
    "on_session_start",
    "on_task_start",
    "on_task_complete",
];

const PLUGINS_DIR: &str = "axium-plugins";
const STATE_FILE: &str = "plugins.json";
const HOOK_TIMEOUT_SECS: u64 = 5;
const MAX_STDOUT_BYTES: usize = 10_240;

// ── Data structures ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub author: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginState {
    pub folder: String,
    pub enabled: bool,
    pub order: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateFile {
    plugins: Vec<PluginState>,
}

/// Full plugin info returned to the UI / API.
#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub folder: String,
    pub enabled: bool,
    pub order: usize,
    pub manifest: PluginManifest,
    pub hooks: Vec<String>, // which hook files exist
}

/// Runtime entry combining state + resolved paths.
#[derive(Debug, Clone)]
struct LoadedPlugin {
    folder: String,
    enabled: bool,
    order: usize,
    manifest: PluginManifest,
    hooks: Vec<(String, PathBuf)>, // (hook_name, script_path)
}

// ── PluginManager ────────────────────────────────────────────────────

pub struct PluginManager {
    plugins_dir: PathBuf,
    state_file: PathBuf,
    plugins: Vec<LoadedPlugin>,
}

impl PluginManager {
    /// Scan `axium-plugins/` and load `plugins.json`. New folders are auto-added as disabled.
    pub fn load() -> Self {
        let plugins_dir = PathBuf::from(PLUGINS_DIR);
        let state_file = PathBuf::from(STATE_FILE);

        // Read existing state
        let existing_state: Vec<PluginState> = std::fs::read_to_string(&state_file)
            .ok()
            .and_then(|s| serde_json::from_str::<StateFile>(&s).ok())
            .map(|sf| sf.plugins)
            .unwrap_or_default();

        // Scan plugin directories
        let mut discovered: Vec<String> = Vec::new();
        if plugins_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            // Must have a plugin.json
                            if path.join("plugin.json").is_file() {
                                discovered.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
        discovered.sort();

        // Merge: keep state for existing, add new as disabled
        let max_order = existing_state.iter().map(|s| s.order).max().unwrap_or(0);
        let mut plugins = Vec::new();
        let mut next_order = max_order + 1;

        for folder_name in &discovered {
            let state = existing_state.iter().find(|s| &s.folder == folder_name);
            let (enabled, order) = match state {
                Some(s) => (s.enabled, s.order),
                None => {
                    let o = next_order;
                    next_order += 1;
                    (false, o) // new plugins default to disabled
                }
            };

            let folder_path = plugins_dir.join(folder_name);

            // Read manifest
            let manifest = std::fs::read_to_string(folder_path.join("plugin.json"))
                .ok()
                .and_then(|s| serde_json::from_str::<PluginManifest>(&s).ok())
                .unwrap_or(PluginManifest {
                    name: folder_name.clone(),
                    description: String::new(),
                    version: String::new(),
                    author: String::new(),
                });

            // Discover hook files
            let mut hooks = Vec::new();
            if let Ok(files) = std::fs::read_dir(&folder_path) {
                for file_entry in files.flatten() {
                    let fpath = file_entry.path();
                    if !fpath.is_file() {
                        continue;
                    }
                    if let Some(stem) = fpath.file_stem().and_then(|s| s.to_str()) {
                        if HOOK_NAMES.contains(&stem) {
                            hooks.push((stem.to_string(), fpath));
                        }
                    }
                }
            }
            hooks.sort_by(|a, b| a.0.cmp(&b.0));

            plugins.push(LoadedPlugin {
                folder: folder_name.clone(),
                enabled,
                order,
                manifest,
                hooks,
            });
        }

        // Sort by order
        plugins.sort_by_key(|p| p.order);

        let mgr = Self {
            plugins_dir,
            state_file,
            plugins,
        };
        // Persist state (captures newly discovered plugins)
        let _ = mgr.save_state();

        info!(count = mgr.plugins.len(), "Plugin manager loaded");
        mgr
    }

    /// List all plugins for the API/UI.
    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|p| PluginInfo {
                folder: p.folder.clone(),
                enabled: p.enabled,
                order: p.order,
                manifest: p.manifest.clone(),
                hooks: p.hooks.iter().map(|(name, _)| name.clone()).collect(),
            })
            .collect()
    }

    /// Enable or disable a plugin by folder name.
    pub fn set_enabled(&mut self, folder: &str, enabled: bool) -> bool {
        if let Some(p) = self.plugins.iter_mut().find(|p| p.folder == folder) {
            p.enabled = enabled;
            info!(folder, enabled, "Plugin toggled");
            let _ = self.save_state();
            true
        } else {
            false
        }
    }

    /// Reorder plugins. `order` is a list of folder names in the desired order.
    pub fn set_order(&mut self, order: &[String]) {
        for (i, folder) in order.iter().enumerate() {
            if let Some(p) = self.plugins.iter_mut().find(|p| &p.folder == folder) {
                p.order = i;
            }
        }
        self.plugins.sort_by_key(|p| p.order);
        let _ = self.save_state();
        info!("Plugins reordered");
    }

    /// Run a hook across all enabled plugins (in order). Returns the last
    /// non-empty JSON modification, or None if no plugin modified anything.
    pub async fn run_hook(
        &self,
        hook_name: &str,
        input: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        let mut current_input = input.clone();
        let mut any_modification = false;

        for plugin in &self.plugins {
            if !plugin.enabled {
                continue;
            }
            // Find the hook script for this plugin
            let script = match plugin.hooks.iter().find(|(name, _)| name == hook_name) {
                Some((_, path)) => path,
                None => continue,
            };

            match run_hook_script(script, &current_input).await {
                Some(mods) => {
                    // Merge modifications into current_input for chaining
                    if let Some(obj) = mods.as_object() {
                        if let Some(cur) = current_input.as_object_mut() {
                            for (k, v) in obj {
                                cur.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    any_modification = true;
                }
                None => {} // no output or error — pass through
            }
        }

        if any_modification {
            Some(current_input)
        } else {
            None
        }
    }

    /// Reload plugin list from disk (useful after adding/removing folders).
    pub fn reload(&mut self) {
        let fresh = Self::load();
        self.plugins = fresh.plugins;
    }

    fn save_state(&self) -> Result<()> {
        let state = StateFile {
            plugins: self
                .plugins
                .iter()
                .map(|p| PluginState {
                    folder: p.folder.clone(),
                    enabled: p.enabled,
                    order: p.order,
                })
                .collect(),
        };
        let json = serde_json::to_string_pretty(&state)?;
        std::fs::write(&self.state_file, json)?;
        Ok(())
    }
}

// ── Hook script execution ────────────────────────────────────────────

/// Execute a single hook script, piping `input` as JSON on stdin.
/// Returns parsed JSON from stdout, or None on error/timeout/empty output.
async fn run_hook_script(
    script_path: &Path,
    input: &serde_json::Value,
) -> Option<serde_json::Value> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let input_json = serde_json::to_string(input).ok()?;

    let mut child = match Command::new(script_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(
                script = %script_path.display(),
                error = %e,
                "Failed to spawn hook script"
            );
            return None;
        }
    };

    // Write input to stdin
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input_json.as_bytes()).await;
        drop(stdin);
    }

    // Save PID before child is moved into wait_with_output()
    let child_pid = child.id();

    // Wait with timeout
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(HOOK_TIMEOUT_SECS),
        child.wait_with_output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(
                    script = %script_path.display(),
                    exit_code = output.status.code().unwrap_or(-1),
                    stderr = %stderr.chars().take(200).collect::<String>(),
                    "Hook script exited with error"
                );
                return None;
            }
            let stdout = &output.stdout;
            if stdout.is_empty() || stdout.len() > MAX_STDOUT_BYTES {
                return None;
            }
            let text = String::from_utf8_lossy(stdout);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            match serde_json::from_str::<serde_json::Value>(trimmed) {
                Ok(v) => Some(v),
                Err(e) => {
                    warn!(
                        script = %script_path.display(),
                        error = %e,
                        "Hook script output is not valid JSON"
                    );
                    None
                }
            }
        }
        Ok(Err(e)) => {
            warn!(script = %script_path.display(), error = %e, "Hook script IO error");
            None
        }
        Err(_) => {
            warn!(script = %script_path.display(), "Hook script timed out ({}s)", HOOK_TIMEOUT_SECS);
            if let Some(pid) = child_pid {
                let _ = tokio::process::Command::new("kill").arg(pid.to_string()).status().await;
            }
            None
        }
    }
}

/// Convenience function: run hooks on an optional PluginManager reference.
/// Used from router.rs and worker.rs where the manager is behind Option<Arc<RwLock<>>>.
pub async fn run_hooks(
    pm: &Option<std::sync::Arc<RwLock<PluginManager>>>,
    hook_name: &str,
    input: &serde_json::Value,
) -> Option<serde_json::Value> {
    match pm {
        Some(pm) => pm.read().await.run_hook(hook_name, input).await,
        None => None,
    }
}
