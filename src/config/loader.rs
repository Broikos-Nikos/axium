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
    pub anthropic: String,
    pub openai: String,
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
