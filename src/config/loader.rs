use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Config {
    pub api_keys: ApiKeys,
    pub models: Models,
    #[serde(default)]
    pub available_models: serde_json::Value,
    pub agent: AgentConfig,
    pub settings: Settings,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ApiKeys {
    #[serde(default)]
    pub anthropic: String,
    #[serde(default)]
    pub openai: String,
    /// DeepSeek — OpenAI-compatible API at api.deepseek.com.
    #[serde(default)]
    pub deepseek: String,
}

impl ApiKeys {
    pub fn as_set(&self) -> crate::agent::ApiKeySet {
        crate::agent::ApiKeySet::new(&self.anthropic, &self.openai, &self.deepseek)
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Models {
    pub primary: String,
    pub compactor: String,
    #[serde(default = "default_classifier_model")]
    pub classifier: String,
    #[serde(default)]
    pub primary_provider: String,
    #[serde(default)]
    pub compactor_provider: String,
    #[serde(default)]
    pub classifier_provider: String,
    /// Optional cheaper/faster model for tool-continuation turns (leave empty to use primary).
    #[serde(default)]
    pub continuation: String,
    #[serde(default)]
    pub continuation_provider: String,
    /// Model used for code review and test generation after complex tasks.
    #[serde(default = "default_review_model")]
    pub review: String,
    #[serde(default)]
    pub review_provider: String,
    /// Fallback model used when the primary model is unavailable (outage, repeated errors).
    /// Leave empty to disable fallback. Example: "gpt-4.1" as fallback when Anthropic is down.
    #[serde(default)]
    pub fallback: String,
    #[serde(default)]
    pub fallback_provider: String,
}

fn default_classifier_model() -> String { "gpt-4.1-nano".to_string() }
fn default_review_model() -> String { "gpt-5.4-codex".to_string() }

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AgentConfig {
    pub name: String,
    pub soul: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Settings {
    pub token_limit: usize,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    pub max_history_messages: usize,
    pub terminal_timeout_secs: u64,
    pub memory_file: String,
    #[serde(default = "default_max_output_chars")]
    pub max_output_chars: usize,
    #[serde(default = "default_max_tool_iterations")]
    pub max_tool_iterations: usize,
    #[serde(default = "default_max_input_chars")]
    pub max_input_chars: usize,
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    #[serde(default = "default_working_directory")]
    pub working_directory: String,
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    #[serde(default)]
    pub smtp_user: String,
    #[serde(default)]
    pub smtp_password: String,
    #[serde(default)]
    pub smtp_from: String,
    #[serde(default)]
    pub telegram_bot_token: String,
    #[serde(default)]
    pub telegram_allowed_users: String,
    #[serde(default)]
    pub telegram_enabled: bool,
    #[serde(default = "default_conversation_logging")]
    pub conversation_logging: bool,
    /// How many user turns between conversation recovery passes (0 = disabled).
    #[serde(default = "default_recovery_interval")]
    pub recovery_interval: usize,
    /// Percentage of token_limit at which compaction triggers (0-100, default 60).
    /// Lower = compact sooner (more cache-friendly, less context).
    /// Higher = compact later (more context, but bigger prefix = more tokens before cache kicks in).
    /// Set to 100 for the old behavior (compact only when limit is hit).
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: usize,
    /// Anthropic extended thinking effort level: "off", "low", "medium", "high", "max".
    /// "high" is recommended for agentic coding tasks. "off" disables thinking entirely.
    /// Only applies to Anthropic models (Claude 4.6+). Ignored for OpenAI.
    #[serde(default = "default_thinking_effort")]
    pub thinking_effort: String,
    /// Processing mode: "supercharge" (classify + enhance), "simple" (straight
    /// to primary), or "skills" (load matching workflows).
    #[serde(default)]
    pub mode: String,

    // ── durable-context layer ────────────────────────────────────────────────
    // Each flag switches exactly ONE mechanism, so a benchmark can attribute a
    // score change to that mechanism rather than to "the new version". Names and
    // defaults match `python/axium/config.py` so one config.json drives either
    // build. All default ON except distillation, which writes files.
    /// Typed, importance-scored facts extracted after each turn and rendered into
    /// the SYSTEM prompt, where compaction cannot reach them.
    #[serde(default = "default_true")]
    pub facts_enabled: bool,
    /// SQLite file for the fact store, resolved next to the memory file.
    #[serde(default = "default_facts_file")]
    pub facts_file: String,
    /// Per-project `.axium/` profile, fingerprinted overview and journal,
    /// preloaded so the agent stops re-deriving the same project every session.
    #[serde(default = "default_true")]
    pub brain_enabled: bool,
    /// A cheap, Brain-grounded plan before a COMPLEX task starts.
    #[serde(default = "default_true")]
    pub planner_enabled: bool,
    /// Snapshot every file a turn touches so `undo_turn` can revert it exactly.
    #[serde(default = "default_true")]
    pub checkpoints_enabled: bool,
    /// Distil a substantive session into a reusable skill folder.
    ///
    /// Off by default: this one WRITES to the skills tree, and a skill written
    /// from a mediocre session is then selected by name for the rest of the
    /// install's life. Opt in deliberately.
    #[serde(default)]
    pub distill_skills: bool,
    /// Where a distilled skill is written. Empty = the repo's `axium-skills/`.
    #[serde(default)]
    pub skills_dir: String,
}

fn default_max_tokens() -> usize { 4096 }
fn default_max_output_chars() -> usize { 8000 }
fn default_max_tool_iterations() -> usize { 15 }
fn default_max_input_chars() -> usize { 12000 }
fn default_max_retries() -> usize { 2 }
fn default_max_sessions() -> usize { 50 }
fn default_working_directory() -> String { "~".to_string() }
fn default_smtp_port() -> u16 { 587 }
fn default_conversation_logging() -> bool { true }
fn default_recovery_interval() -> usize { 6 }
fn default_compaction_threshold() -> usize { 60 }
fn default_thinking_effort() -> String { "high".to_string() }
fn default_true() -> bool { true }
fn default_facts_file() -> String { "facts.db".to_string() }

impl Settings {
    /// The processing mode, defaulting to "supercharge".
    ///
    /// Empty means "not configured", not "no mode": an empty string would send
    /// the router down its fall-through branch and silently disable
    /// classification, which is the feature the mode exists to select.
    pub fn mode_or_default(&self) -> String {
        if self.mode.trim().is_empty() {
            "supercharge".to_string()
        } else {
            self.mode.trim().to_string()
        }
    }
}

pub fn load_config(path: &str) -> Result<Config> {
    let content = fs::read_to_string(path)?;
    let config: Config = serde_json::from_str(&content)?;

    // Validate at startup so bad config never reaches runtime
    if config.models.primary.is_empty() {
        anyhow::bail!("config: models.primary cannot be empty");
    }
    if config.models.compactor.is_empty() {
        anyhow::bail!("config: models.compactor cannot be empty");
    }
    if config.settings.token_limit == 0 {
        anyhow::bail!("config: token_limit must be > 0");
    }
    if config.settings.max_tokens == 0 {
        anyhow::bail!("config: max_tokens must be > 0");
    }
    if config.settings.terminal_timeout_secs == 0 {
        anyhow::bail!("config: terminal_timeout_secs must be > 0");
    }
    if config.settings.memory_file.is_empty() {
        anyhow::bail!("config: memory_file cannot be empty");
    }
    // Caught at startup rather than at the first fact write, which happens after
    // a turn has already been paid for.
    if config.settings.facts_enabled && config.settings.facts_file.is_empty() {
        anyhow::bail!("config: facts_file cannot be empty when facts_enabled is true");
    }

    Ok(config)
}

pub fn save_config(path: &str, config: &Config) -> Result<()> {
    let json = serde_json::to_string_pretty(config)?;
    let tmp = format!("{}.tmp", path);
    fs::write(&tmp, &json)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Load the agent soul: reads `soul.md` from the binary's working directory if it
/// exists, otherwise falls back to the `agent.soul` value from config.json.
/// This allows hot-editing the soul without restarting or touching config.
pub fn load_soul(fallback: &str) -> String {
    // Try reading soul.md from the working directory
    let candidates = [
        std::path::PathBuf::from("soul.md"),
    ];
    for path in &candidates {
        if let Ok(s) = fs::read_to_string(path) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    fallback.to_string()
}

/// Expand a leading `~` against the home directory.
///
/// This logic was copy-pasted into seven call sites, and the copies had drifted:
/// the bare-`~` branch fell back to `"."` when HOME was unset while the `~/foo`
/// branch fell back to `""`, so on a machine without HOME the same config
/// resolved to the current directory in one place and to the filesystem ROOT in
/// another. For a tool that writes files that is not a cosmetic difference.
/// One fallback, one place.
pub fn expand_home(path: &str) -> String {
    let home = || std::env::var("HOME").ok().filter(|h| !h.is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok().filter(|h| !h.is_empty()))
        .unwrap_or_else(|| ".".to_string());
    if path.is_empty() || path == "~" {
        home()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{}", home(), rest)
    } else {
        path.to_string()
    }
}

/// Resolve a data file (the fact store, say) next to the memory file.
///
/// The memory file is the anchor because it is the one path a user already
/// configures, and keeping the agent's state together means a backup of one
/// directory is a backup of everything it knows.
pub fn resolve_data_path(memory_file: &str, name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let p = std::path::Path::new(name);
    if p.is_absolute() {
        return name.to_string();
    }
    std::path::Path::new(memory_file)
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(name).to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion_is_consistent_between_bare_and_prefixed() {
        // The bug this helper exists to kill: with no HOME, these two used to
        // disagree, one giving "." and the other the filesystem root.
        let prev_home = std::env::var("HOME").ok();
        let prev_profile = std::env::var("USERPROFILE").ok();
        std::env::remove_var("HOME");
        std::env::remove_var("USERPROFILE");

        assert_eq!(expand_home("~"), ".");
        assert_eq!(expand_home(""), ".");
        assert_eq!(expand_home("~/projects"), "./projects");

        std::env::set_var("HOME", "/home/nikos");
        assert_eq!(expand_home("~"), "/home/nikos");
        assert_eq!(expand_home("~/projects"), "/home/nikos/projects");

        if let Some(h) = prev_home { std::env::set_var("HOME", h) } else { std::env::remove_var("HOME") }
        if let Some(u) = prev_profile { std::env::set_var("USERPROFILE", u) } else { std::env::remove_var("USERPROFILE") }
    }

    #[test]
    fn a_path_without_a_tilde_is_left_alone() {
        assert_eq!(expand_home("/var/data"), "/var/data");
        assert_eq!(expand_home("relative/path"), "relative/path");
        // Not a home reference: a file literally named "~something".
        assert_eq!(expand_home("~backup"), "~backup");
    }

    #[test]
    fn data_paths_land_beside_the_memory_file() {
        assert_eq!(
            resolve_data_path("/var/axium/memory.md", "facts.db").replace('\\', "/"),
            "/var/axium/facts.db"
        );
    }

    #[test]
    fn an_absolute_data_path_is_left_alone() {
        assert_eq!(resolve_data_path("/var/axium/memory.md", "/tmp/other.db"), "/tmp/other.db");
    }

    #[test]
    fn a_bare_memory_filename_keeps_the_data_file_bare() {
        assert_eq!(resolve_data_path("memory.md", "facts.db"), "facts.db");
    }

    #[test]
    fn an_empty_name_resolves_to_nothing() {
        assert_eq!(resolve_data_path("/var/axium/memory.md", ""), "");
    }

    /// A config written for the OLD schema must still load, or upgrading the
    /// binary breaks every existing install.
    #[test]
    fn a_config_without_the_new_settings_still_loads() {
        let json = r#"{
            "api_keys": {"deepseek": "k"},
            "models": {"primary": "deepseek-v4-pro", "compactor": "deepseek-v4-flash"},
            "agent": {"name": "Axium", "soul": "s"},
            "settings": {
                "token_limit": 80000, "max_history_messages": 200,
                "terminal_timeout_secs": 120, "memory_file": "memory.md"
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).expect("old config must still parse");
        assert!(cfg.settings.facts_enabled, "facts default on");
        assert!(cfg.settings.brain_enabled, "brain default on");
        assert!(cfg.settings.planner_enabled, "planner default on");
        assert!(cfg.settings.checkpoints_enabled, "checkpoints default on");
        assert!(!cfg.settings.distill_skills, "distillation must be opt-in: it writes files");
        assert_eq!(cfg.settings.facts_file, "facts.db");
        assert_eq!(cfg.settings.skills_dir, "");
    }

    #[test]
    fn the_new_settings_can_be_switched_off_for_an_ablation() {
        let json = r#"{
            "api_keys": {"deepseek": "k"},
            "models": {"primary": "p", "compactor": "c"},
            "agent": {"name": "Axium", "soul": "s"},
            "settings": {
                "token_limit": 80000, "max_history_messages": 200,
                "terminal_timeout_secs": 120, "memory_file": "memory.md",
                "facts_enabled": false, "brain_enabled": false,
                "planner_enabled": false, "checkpoints_enabled": false
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert!(!cfg.settings.facts_enabled);
        assert!(!cfg.settings.brain_enabled);
        assert!(!cfg.settings.planner_enabled);
        assert!(!cfg.settings.checkpoints_enabled);
    }
}
