use anyhow::{Context, Result};
use std::sync::Arc;
use super::{detect_provider, Message, Provider};

/// System prompt for the compactor model. Gives it context about the agent so it
/// produces higher-quality summaries that preserve actionable information.
const COMPACTOR_SYSTEM: &str = "\
You are summarizing conversations from an autonomous Linux assistant that uses tools \
(run_command, read_file, write_file, patch_file, search_files, git_command, etc.) to \
complete coding and system tasks. Your summary replaces the old messages — the assistant \
will only see your summary plus the most recent messages.\n\n\
Preserve:\n\
- File paths created/edited/read and what was done to them\n\
- Commands run and their outcomes (especially errors)\n\
- Decisions made and user preferences stated\n\
- Task status: what is done, what is still pending\n\
- <tool_trace> blocks verbatim — they are already compressed\n\n\
Omit:\n\
- Pleasantries, acknowledgements, verbose explanations\n\
- Plans that were already executed (keep the result, drop the plan)\n\
- Redundant file contents — note the file and purpose, not the code";

pub struct Compactor {
    anthropic_key: String,
    openai_key: String,
    model: String,
    provider: Provider,
    http: Arc<reqwest::Client>,
}

impl Compactor {
    pub fn new(anthropic_key: &str, openai_key: &str, model: &str, explicit_provider: &str, http: Arc<reqwest::Client>) -> Self {
        let provider = if !explicit_provider.is_empty() {
            match explicit_provider {
                "anthropic" => Provider::Anthropic,
                _ => Provider::OpenAI,
            }
        } else {
            detect_provider(model)
        };
        Self {
            anthropic_key: anthropic_key.to_string(),
            openai_key: openai_key.to_string(),
            model: model.to_string(),
            provider,
            http,
        }
    }

    /// Summarize old messages into a compact bullet-point summary.
    pub async fn compact(&self, old_messages: &[Message]) -> Result<String> {
        let mut history_text = String::new();
        for m in old_messages {
            history_text.push_str(&m.role);
            history_text.push_str(": ");
            history_text.push_str(&m.content);
            history_text.push('\n');
            // Cap at ~100K chars to avoid sending enormous payloads to the compaction model
            if history_text.len() > 100_000 {
                history_text.push_str("\n[... truncated for compaction ...]\n");
                break;
            }
        }

        let prompt = format!(
            "You are summarizing an AI assistant conversation for context carry-over. \
             The summary will be injected at the start of the next turn so the assistant \
             can continue without the full history.\n\n\
             Rules:\n\
             - One bullet per distinct fact. No narrative.\n\
             - Keep: file paths edited, commands run, errors encountered, decisions made, \
               user preferences stated, task status (done/pending/failed).\n\
             - Omit: pleasantries, explanations already acted on, superseded plans.\n\
             - If code was written, note the file and what it does — not the code itself.\n\
             - Be terse. Every word must earn its place.\n\n\
             Conversation to summarize:\n{}",
            history_text
        );

        self.call_llm(&prompt, 2048).await
    }

    async fn call_llm(&self, prompt: &str, max_tokens: usize) -> Result<String> {
        match self.provider {
            Provider::OpenAI => self.call_openai(prompt, max_tokens).await,
            Provider::Anthropic => self.call_anthropic(prompt, max_tokens).await,
        }
    }

    async fn call_openai(&self, prompt: &str, max_tokens: usize) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": COMPACTOR_SYSTEM },
                { "role": "user", "content": prompt },
            ],
            "max_tokens": max_tokens,
        });

        let resp = self
            .http
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.openai_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to reach OpenAI API for compaction")?;

        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("OpenAI compaction error ({}): {}", status, text);
        }

        let json: serde_json::Value =
            serde_json::from_str(&text).context("Failed to parse OpenAI response")?;
        Ok(json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("[compaction failed]")
            .to_string())
    }

    async fn call_anthropic(&self, prompt: &str, max_tokens: usize) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "system": COMPACTOR_SYSTEM,
            "messages": [{ "role": "user", "content": prompt }],
        });

        let resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.anthropic_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to reach Anthropic API for compaction")?;

        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("Anthropic compaction error ({}): {}", status, text);
        }

        let json: serde_json::Value =
            serde_json::from_str(&text).context("Failed to parse Anthropic response")?;
        Ok(json["content"][0]["text"]
            .as_str()
            .unwrap_or("[compaction failed]")
            .to_string())
    }
}

