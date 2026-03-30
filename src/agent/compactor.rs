use anyhow::{Context, Result};
use std::sync::Arc;
use super::{detect_provider, Message, Provider};

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
            "Summarize this conversation history concisely. \
             Preserve all key decisions, file paths, code context, \
             and user preferences. Output a bullet-point summary.\n\n{}",
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
            "messages": [{ "role": "user", "content": prompt }],
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

