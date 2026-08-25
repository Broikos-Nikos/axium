pub mod brain;
pub mod checkpoints;
pub mod classifier;
pub mod compactor;
pub mod metrics;
pub mod planner;
pub mod router;
pub mod sonnet;
pub mod trajectory;

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
        (m.content.len() * 2).div_ceil(7) + 4
    }).sum::<usize>()
}

/// Which API provider a model belongs to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Provider {
    Anthropic,
    OpenAI,
    /// DeepSeek, OpenAI-compatible wire format, different base URL and key.
    DeepSeek,
}

impl Provider {
    /// Base URL for providers that speak the OpenAI chat-completions format.
    /// Returns `None` for Anthropic, which has its own request shape.
    /// Base URL, honouring an override.
    ///
    /// `AXIUM_BASE_URL_<PROVIDER>` redirects this provider's traffic. It exists
    /// so a benchmark can put a recording proxy in front of the API and measure
    /// tokens from the provider's own responses rather than from any harness's
    /// self-report - three of which turned out to count cache differently.
    /// Unset in normal operation, so production behaviour is unchanged.
    pub fn base_url(&self) -> Option<String> {
        let key = format!("AXIUM_BASE_URL_{}", self.as_str().to_uppercase());
        if let Ok(v) = std::env::var(&key) {
            if !v.trim().is_empty() {
                return Some(v.trim().trim_end_matches('/').to_string());
            }
        }
        self.openai_compatible_base().map(|s| s.to_string())
    }

    pub fn openai_compatible_base(&self) -> Option<&'static str> {
        match self {
            Provider::OpenAI => Some("https://api.openai.com/v1"),
            Provider::DeepSeek => Some("https://api.deepseek.com/v1"),
            Provider::Anthropic => None,
        }
    }

    /// Stable lowercase name, matching the `*_provider` config strings.
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAI => "openai",
            Provider::DeepSeek => "deepseek",
        }
    }
}

/// Auto-detect the provider from a model ID string.
pub fn detect_provider(model: &str) -> Provider {
    if model.starts_with("claude-") {
        Provider::Anthropic
    } else if model.starts_with("deepseek") {
        Provider::DeepSeek
    } else {
        Provider::OpenAI
    }
}

/// Resolve the provider from an explicit override or by auto-detecting from the model name.
/// `SonnetClient`, `Classifier` and `Compactor` all route through this so the mapping
/// lives in exactly one place.
pub fn resolve_provider(model: &str, explicit: &str) -> Provider {
    if !explicit.is_empty() {
        match explicit {
            "anthropic" => Provider::Anthropic,
            "deepseek" => Provider::DeepSeek,
            "openai" => Provider::OpenAI,
            // Unknown label: fall back to sniffing the model id rather than
            // silently mis-routing a deepseek-* model to OpenAI.
            _ => detect_provider(model),
        }
    } else {
        detect_provider(model)
    }
}

/// API credentials for every supported provider, passed as one unit so adding a
/// provider does not mean threading another `&str` through a dozen constructors.
#[derive(Debug, Clone, Default)]
pub struct ApiKeySet {
    pub anthropic: String,
    pub openai: String,
    pub deepseek: String,
}

impl ApiKeySet {
    pub fn new(anthropic: &str, openai: &str, deepseek: &str) -> Self {
        Self {
            anthropic: anthropic.to_string(),
            openai: openai.to_string(),
            deepseek: deepseek.to_string(),
        }
    }

    /// The bearer/API key for a given provider.
    pub fn for_provider(&self, p: Provider) -> &str {
        match p {
            Provider::Anthropic => &self.anthropic,
            Provider::OpenAI => &self.openai,
            Provider::DeepSeek => &self.deepseek,
        }
    }
}
