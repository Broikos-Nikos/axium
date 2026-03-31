pub mod classifier;
pub mod compactor;
pub mod router;
pub mod sonnet;

use serde::{Deserialize, Serialize};

/// A simplified message for history tracking (user-facing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn user(content: &str) -> Self {
        Self { role: "user".into(), content: content.into() }
    }

    pub fn assistant(content: &str) -> Self {
        Self { role: "assistant".into(), content: content.into() }
    }
}

/// Estimate tokens from text length.
/// Uses ~3.5 chars/token for English (more accurate than /4).
/// Accounts for message framing overhead and system prompt + tool definitions.
pub fn estimate_tokens(messages: &[Message]) -> usize {
    // System prompt (~1700 tokens) + 28 tool definitions (~4300 tokens)
    const SYSTEM_OVERHEAD: usize = 6000;
    SYSTEM_OVERHEAD + messages.iter().map(|m| {
        // ~3.5 chars per token + 4 tokens per message framing (round up)
        (m.content.len() * 2 + 6) / 7 + 4
    }).sum::<usize>()
}

/// Which API provider a model belongs to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Provider {
    Anthropic,
    OpenAI,
}

/// Auto-detect the provider from a model ID string.
pub fn detect_provider(model: &str) -> Provider {
    if model.starts_with("claude-") {
        Provider::Anthropic
    } else {
        Provider::OpenAI
    }
}

/// Resolve the provider from an explicit override or by auto-detecting from the model name.
/// Both `SonnetClient` and `Classifier` use this to avoid duplicating the same logic.
pub fn resolve_provider(model: &str, explicit: &str) -> Provider {
    if !explicit.is_empty() {
        match explicit {
            "anthropic" => Provider::Anthropic,
            _ => Provider::OpenAI,
        }
    } else {
        detect_provider(model)
    }
}
