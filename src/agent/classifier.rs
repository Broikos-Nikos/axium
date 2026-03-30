use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::{info, warn};

use super::{resolve_provider, Provider};

/// Classification result from the cheap model.
#[derive(Debug, Clone)]
pub enum PromptClass {
    /// Trivial question — answer directly with the classifier model, skip primary.
    Trivial(String),
    /// Simple question — pass through to primary unchanged.
    Simple,
    /// Complex task — enhanced prompt replaces the original for the primary model.
    Complex(String),
}

#[derive(Clone)]
pub struct Classifier {
    anthropic_key: String,
    openai_key: String,
    model: String,
    provider: Provider,
    http: Arc<reqwest::Client>,
}

impl Classifier {
    pub fn new(
        anthropic_key: &str,
        openai_key: &str,
        model: &str,
        explicit_provider: &str,
        http: Arc<reqwest::Client>,
    ) -> Self {
        let provider = resolve_provider(model, explicit_provider);
        Self {
            anthropic_key: anthropic_key.to_string(),
            openai_key: openai_key.to_string(),
            model: model.to_string(),
            provider,
            http,
        }
    }

    /// Classify and optionally enhance a user prompt.
    /// Returns the classification with any direct answer or enhanced prompt.
    pub async fn classify(&self, user_message: &str) -> Result<PromptClass> {
        // Fast-path: skip LLM call for patterns we can classify with certainty.
        if let Some(class) = quick_classify(user_message) {
            info!(classification_raw = "QUICK", "Classifier: fast-path match");
            return Ok(class);
        }

        let system = r#"You are a prompt classifier and enhancer. Given a user message, respond with EXACTLY one of these formats:

TRIVIAL: <direct answer>
Use this ONLY for pure factual/math questions with no ambiguity (time, math, unit conversions, simple facts). Provide the complete answer after the colon.
NEVER classify as TRIVIAL if the user:
- Asks about YOUR capabilities, memory, identity, or features ("do you have memory?", "can you remember?", "who are you?")
- Asks you to remember, store, or save anything
- References previous conversations or context
- Asks about what you can do or how you work
These MUST be SIMPLE so the primary model (which has memory and tools) can answer accurately.

SIMPLE
Use this for: conversational questions, opinion requests, identity/capability questions, specific unambiguous commands, or any task where the intent is already clear and complete. When in doubt, use SIMPLE.
Examples of SIMPLE tasks: "install nginx", "create a file called test.txt", "restart apache", "show disk usage", "what time is it in Tokyo?", "how do I list files in Linux?"

COMPLEX: <enhanced prompt>
Use this ONLY when the user's goal is ambiguous, requires expert judgment between multiple options, or involves design/architecture decisions where added context and constraints would significantly improve the outcome.
Do NOT use for specific, unambiguous commands — only use when the task genuinely benefits from clarifying requirements and quality criteria.
Write an expert-level task description that clarifies the goal, specifies quality criteria, and defines what a good result looks like.
Do NOT use "You are a..." or any role-assignment language. Write it as a direct task description.

Examples:
User: "what's 15 * 23?"
Response: TRIVIAL: 345

User: "do you have a memory?"
Response: SIMPLE

User: "remember my email is bob@example.com"
Response: SIMPLE

User: "what do you think about Python vs Rust?"
Response: SIMPLE

User: "install nginx"
Response: SIMPLE

User: "install lamp"
Response: SIMPLE

User: "build me a REST API for a todo app"
Response: COMPLEX: Build a production-quality REST API for a todo application following REST conventions. Include proper HTTP methods, status codes, input validation, and error handling with a clean project structure.

User: "i want to start development with xampp/lamp. install the best option"
Response: COMPLEX: Compare LAMP and XAMPP for local development on this system and install the better option. Consider ease of use, package availability, service control, and OS compatibility. Configure PHP and the database with sensible defaults for development.

Respond with ONLY the classification line. No explanations."#;

        let prompt = format!("User message: {}", user_message);
        let response = self.call_llm(system, &prompt, 256).await?;
        let response = response.trim();

        info!(classification_raw = %response, "Classifier response");

        if response.starts_with("TRIVIAL:") {
            let answer = response.strip_prefix("TRIVIAL:").unwrap_or("").trim().to_string();
            Ok(PromptClass::Trivial(answer))
        } else if response.starts_with("COMPLEX:") {
            let enhanced = response.strip_prefix("COMPLEX:").unwrap_or("").trim().to_string();
            Ok(PromptClass::Complex(enhanced))
        } else if response == "SIMPLE" {
            Ok(PromptClass::Simple)
        } else {
            warn!(response = %response, "Classifier returned unexpected value — falling back to Simple");
            Ok(PromptClass::Simple)
        }
    }

    async fn call_llm(&self, system: &str, prompt: &str, max_tokens: usize) -> Result<String> {
        match self.provider {
            Provider::OpenAI => self.call_openai(system, prompt, max_tokens).await,
            Provider::Anthropic => self.call_anthropic(system, prompt, max_tokens).await,
        }
    }

    async fn call_openai(&self, system: &str, prompt: &str, max_tokens: usize) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": prompt },
            ],
            "max_tokens": max_tokens,
            "temperature": 0.3,
        });

        let resp = self
            .http
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.openai_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to reach OpenAI API for classification")?;

        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("OpenAI classifier error ({}): {}", status, text);
        }

        let json: serde_json::Value =
            serde_json::from_str(&text).context("Failed to parse OpenAI classifier response")?;
        Ok(json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("SIMPLE")
            .to_string())
    }

    async fn call_anthropic(&self, system: &str, prompt: &str, max_tokens: usize) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": [{ "role": "user", "content": prompt }],
            "temperature": 0.3,
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
            .context("Failed to reach Anthropic API for classification")?;

        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("Anthropic classifier error ({}): {}", status, text);
        }

        let json: serde_json::Value =
            serde_json::from_str(&text).context("Failed to parse Anthropic classifier response")?;
        Ok(json["content"][0]["text"]
            .as_str()
            .unwrap_or("SIMPLE")
            .to_string())
    }

    /// Heartbeat check: did the agent complete the user's request?
    /// Returns true if the task appears complete, false if more work is needed.
    /// Fails open (returns true) on any error to avoid blocking the agent.
    pub async fn heartbeat(&self, user_request: &str, tool_log: &str, agent_text: &str) -> bool {
        let system = r#"You are a task-completion auditor. Given a user request, the tools the agent executed, and the agent's final text, decide if the task is DONE.

Rules:
- If the user's message is conversational — a greeting, check-in, acknowledgment, or social message (e.g. "hi", "ok", "thanks", "just checking", "hello", "good morning") — and the agent replied appropriately → COMPLETE
- If the user asked a question and the agent answered it → COMPLETE
- If the user asked for something to be created/written/built AND the tool log shows it was done → COMPLETE
- If the agent delivered a file/email the user asked for → COMPLETE
- If the user gave a real task AND the agent only described what it WOULD do without calling any tools → INCOMPLETE
- If the agent ran some tools but clearly hasn't finished all parts of the request → INCOMPLETE
- When in doubt, lean toward COMPLETE to avoid loops

Respond with EXACTLY one word: COMPLETE or INCOMPLETE"#;

        // Truncate agent text to avoid wasting tokens
        let text_tail = if agent_text.len() > 600 {
            let mut b = agent_text.len() - 600;
            while b < agent_text.len() && !agent_text.is_char_boundary(b) { b += 1; }
            &agent_text[b..]
        } else {
            agent_text
        };

        let prompt = build_review_prompt(
            user_request,
            tool_log,
            text_tail
        );

        match self.call_llm(system, &prompt, 16).await {
            Ok(resp) => {
                let word = resp.trim().to_uppercase();
                info!(heartbeat = %word, "Heartbeat check result");
                if word == "COMPLETE" {
                    true
                } else if word.contains("INCOMPLETE") {
                    false
                } else {
                    warn!(response = %word, "Heartbeat returned unexpected value — treating as complete");
                    true
                }
            }
            Err(e) => {
                warn!(error = %e, "Heartbeat check failed, assuming complete");
                true
            }
        }
    }

    /// Generate a short title for a session based on early conversation messages.
    /// Returns a 3-5 word title, or empty string on error.
    pub async fn generate_session_title(&self, messages: &[(String, String)]) -> String {
        let system = "You are a session labeler. Given conversation excerpts, reply with ONLY a concise 2-5 word title that captures the main topic. No quotes, no punctuation at the end, no explanations.";
        let snippet = messages.iter()
            .filter(|(role, _)| role == "user" || role == "assistant")
            .take(6)
            .map(|(role, content)| {
            let preview = if content.len() > 120 {
                    let mut b = 120; while b > 0 && !content.is_char_boundary(b) { b -= 1; }
                    &content[..b]
                } else { content };
                format!("[{}]: {}", role, preview)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!("Conversation:\n{}", snippet);
        match self.call_llm(system, &prompt, 20).await {
            Ok(t) => t.trim().trim_matches('"').trim_matches('\'').to_string(),
            Err(e) => {
                tracing::warn!(error = %e, "Session title generation failed");
                String::new()
            }
        }
    }

    /// Analyze a user prompt to determine which skills are needed, then load them.
    /// Scans the `axium-skills/` directory for available skill folders, asks the LLM
    /// which are relevant, and returns the concatenated content of matched skill files.
    /// Returns an empty string if no skills match or the directory doesn't exist.
    pub async fn analyze_skills(&self, user_message: &str) -> Result<String> {
        // Discover available skill folders
        let skills_dir = std::path::Path::new("axium-skills");
        if !skills_dir.is_dir() {
            info!("Skills directory not found, skipping skill loading");
            return Ok(String::new());
        }

        let mut skill_names: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(skills_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        skill_names.push(name.to_string());
                    }
                }
            }
        }

        if skill_names.is_empty() {
            info!("No skill folders found in axium-skills/");
            return Ok(String::new());
        }

        skill_names.sort();
        let available = skill_names.join(", ");

        let system = r#"You are a skill selector for an AI agent. Given a user prompt and a list of available skill folders, determine which skills are relevant to help the agent fulfill the request.

Rules:
- Only select skills that are DIRECTLY relevant to the user's request
- When in doubt, select NONE
- Do NOT select skills just because they're tangentially related

Respond with EXACTLY one of these formats:
NONE
SKILLS: skill1, skill2"#;

        let prompt = format!(
            "Available skills: {}\n\nUser prompt: {}",
            available, user_message
        );

        let response = self.call_llm(system, &prompt, 128).await?;
        let response = response.trim();

        if response.starts_with("NONE") || !response.starts_with("SKILLS:") {
            info!(response = response, "Skills analysis: no skills selected");
            return Ok(String::new());
        }

        // Parse selected skill names
        let selected_str = response.trim_start_matches("SKILLS:").trim();
        let selected: Vec<&str> = selected_str
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        info!(selected = ?selected, "Skills analysis: loading selected skills");

        // Load skill contents
        let mut result = String::new();
        let max_total = 8000; // Cap total skill content

        for skill_name in &selected {
            // Validate skill name exists in available list
            if !skill_names.iter().any(|n| n == skill_name) {
                warn!(skill = skill_name, "Classifier selected unknown skill, skipping");
                continue;
            }

            let skill_path = skills_dir.join(skill_name);
            if !skill_path.is_dir() {
                continue;
            }

            result.push_str(&format!("\n## Skill: {}\n", skill_name));

            if let Ok(files) = std::fs::read_dir(&skill_path) {
                for file_entry in files.flatten() {
                    let path = file_entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let fname = path.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("unknown");
                        result.push_str(&format!("### {}\n{}\n", fname, content));

                        if result.len() >= max_total {
                            result.truncate(max_total);
                            result.push_str("\n[SKILL CONTENT TRUNCATED]");
                            return Ok(result);
                        }
                    }
                }
            }
        }

        Ok(result)
    }

    /// Quality review for complex tasks: assess whether the deliverable is done and good.
    /// Returns None if complete, or Some(feedback) with what needs improvement.
    /// Fails open (returns None) on error.
    pub async fn quality_review(
        &self,
        user_request: &str,
        tool_log: &str,
        agent_text: &str,
    ) -> Option<String> {
        let system = r#"You are a senior QA reviewer. An AI agent just attempted a complex task. Assess whether the deliverable is DONE to a high standard.

Respond in EXACTLY one of these formats:

DONE
Use when: all parts of the request are complete, files written, commands run, output verified, nothing left to do.

CONTINUE: <1-2 sentence description of what's missing or needs improvement>
Use when: the agent started but hasn't finished key parts, or there's an obvious gap.

Rules:
- Be pragmatic: if the core deliverable exists and works, say DONE even if minor polish is possible.
- Only say CONTINUE for genuine gaps: missing files, untested code that should be tested, incomplete multi-part deliverables, errors in output.
- NEVER ask for documentation, comments, or refactoring unless the user explicitly asked.
- NEVER ask for tests unless the user explicitly asked for tests.
- If in doubt, say DONE. Avoid perfectionism."#;

        let text_tail = if agent_text.len() > 800 {
            let mut b = agent_text.len() - 800;
            while b < agent_text.len() && !agent_text.is_char_boundary(b) { b += 1; }
            &agent_text[b..]
        } else {
            agent_text
        };

        let prompt = build_review_prompt(
            user_request,
            tool_log,
            text_tail
        );

        match self.call_llm(system, &prompt, 64).await {
            Ok(resp) => {
                let trimmed = resp.trim();
                info!(review = %trimmed, "Quality review result");
                if trimmed.starts_with("CONTINUE:") {
                    let feedback = trimmed.strip_prefix("CONTINUE:").unwrap_or("").trim();
                    if feedback.is_empty() {
                        None
                    } else {
                        Some(feedback.to_string())
                    }
                } else {
                    None // DONE or unrecognized → accept
                }
            }
            Err(e) => {
                warn!(error = %e, "Quality review failed, assuming done");
                None
            }
        }
    }
}

/// One-shot code reviewer and test generator using a dedicated code model.
pub struct CodeReviewer {
    anthropic_key: String,
    openai_key: String,
    model: String,
    provider: Provider,
    http: Arc<reqwest::Client>,
}

impl CodeReviewer {
    pub fn new(
        anthropic_key: &str,
        openai_key: &str,
        model: &str,
        explicit_provider: &str,
        http: Arc<reqwest::Client>,
    ) -> Self {
        let provider = resolve_provider(model, explicit_provider);
        Self {
            anthropic_key: anthropic_key.to_string(),
            openai_key: openai_key.to_string(),
            model: model.to_string(),
            provider,
            http,
        }
    }

    /// Review a git diff for bugs, edge cases and security issues.
    /// Returns review notes or None if the code looks clean.
    pub async fn code_review(&self, diff: &str, user_request: &str) -> Option<String> {
        if diff.trim().is_empty() { return None; }
        let diff_cap = if diff.len() > 8000 {
            let mut b = 8000; while b > 0 && !diff.is_char_boundary(b) { b -= 1; } &diff[..b]
        } else { diff };
        let system = "You are a senior software engineer doing a focused code review. Analyze the git diff and give specific, actionable feedback on: 1) bugs or logic errors, 2) edge cases not handled, 3) security issues, 4) anything incomplete. Be concise — bullet points only. If the code looks correct, respond with exactly: LGTM";
        let prompt = format!("Task: {}\n\nDiff:\n```\n{}\n```", user_request, diff_cap);
        match self.call_llm(system, &prompt, 1024).await {
            Ok(resp) => {
                let trimmed = resp.trim();
                if trimmed == "LGTM" || trimmed.is_empty() { None }
                else { Some(trimmed.to_string()) }
            }
            Err(e) => { warn!(error = %e, "Code review failed"); None }
        }
    }

    /// Generate test cases for code changes in the diff.
    /// Returns test suggestions or None if not applicable.
    pub async fn generate_tests(&self, diff: &str, user_request: &str) -> Option<String> {
        if diff.trim().is_empty() { return None; }
        let diff_cap = if diff.len() > 8000 {
            let mut b = 8000; while b > 0 && !diff.is_char_boundary(b) { b -= 1; } &diff[..b]
        } else { diff };
        let system = "You are a test engineer. Given a git diff, write specific test cases that should be added for the changed code. Return concrete test functions or clear test descriptions. If the changes are configuration-only or trivial, respond with exactly: NO_TESTS";
        let prompt = format!("Task: {}\n\nChanges:\n```\n{}\n```", user_request, diff_cap);
        match self.call_llm(system, &prompt, 2048).await {
            Ok(resp) => {
                let trimmed = resp.trim();
                if trimmed == "NO_TESTS" || trimmed.is_empty() { None }
                else { Some(trimmed.to_string()) }
            }
            Err(e) => { warn!(error = %e, "Test generation failed"); None }
        }
    }

    async fn call_llm(&self, system: &str, prompt: &str, max_tokens: usize) -> Result<String> {
        match self.provider {
            Provider::OpenAI => self.call_openai(system, prompt, max_tokens).await,
            Provider::Anthropic => self.call_anthropic(system, prompt, max_tokens).await,
        }
    }

    async fn call_openai(&self, system: &str, prompt: &str, max_tokens: usize) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": prompt },
            ],
            "max_tokens": max_tokens,
        });
        let resp = self.http
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.openai_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send().await.context("OpenAI code review request failed")?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("OpenAI code review error ({}): {}", status, if text.len() > 300 { let mut b = 300; while b > 0 && !text.is_char_boundary(b) { b -= 1; } &text[..b] } else { &text });
        }
        let json: serde_json::Value = serde_json::from_str(&text)?;
        Ok(json["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string())
    }

    async fn call_anthropic(&self, system: &str, prompt: &str, max_tokens: usize) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": [{ "role": "user", "content": prompt }],
        });
        let resp = self.http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.anthropic_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send().await.context("Anthropic code review request failed")?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("Anthropic code review error ({}): {}", status, if text.len() > 300 { let mut b = 300; while b > 0 && !text.is_char_boundary(b) { b -= 1; } &text[..b] } else { &text });
        }
        let json: serde_json::Value = serde_json::from_str(&text)?;
        Ok(json["content"][0]["text"].as_str().unwrap_or("").to_string())
    }
}

/// Build the shared prompt body used by both `heartbeat` and `quality_review`.
fn build_review_prompt(user_request: &str, tool_log: &str, text_tail: &str) -> String {
    format!(
        "USER REQUEST: {}\n\nTOOL LOG:\n{}\n\nAGENT RESPONSE (tail):\n{}",
        user_request,
        if tool_log.is_empty() { "(no tools called)" } else { tool_log },
        text_tail
    )
}

/// Fast-path classifier: returns Some(class) for messages we can classify
/// without an LLM call (greetings, acks, clear single-action commands).
/// Returns None to fall through to the LLM classifier.
fn quick_classify(msg: &str) -> Option<PromptClass> {
    let trimmed = msg.trim();
    if trimmed.is_empty() {
        return Some(PromptClass::Simple);
    }
    let lower = trimmed.to_lowercase();
    let lower = lower.trim_end_matches(['!', '.', '?', ' ']).trim();

    // Short greetings and acknowledgements — no LLM needed, let main model reply
    const GREETINGS: &[&str] = &[
        "hi", "hello", "hey", "thanks", "thank you", "thank you so much",
        "ok", "okay", "sure", "alright", "got it", "noted", "understood",
        "done", "yep", "yup", "nope", "no", "yes", "nice", "great", "cool",
        "good morning", "good afternoon", "good evening", "good night",
        "sounds good", "makes sense", "perfect", "awesome",
    ];
    if GREETINGS.contains(&lower) {
        return Some(PromptClass::Simple);
    }

    // Single-action commands with a clear specific target and no ambiguity words
    // e.g. "restart nginx", "update vscode", "install curl", "show disk usage"
    // but NOT "install the best option" or "update everything"
    const AMBIGUOUS: &[&str] = &[
        "best", "recommend", "which", "vs", "versus", "better", "worse",
        "should i", " or ", "option", "alternative", "compare",
        "suggest", "how should", "what should",
    ];
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.len() >= 2 && words.len() <= 6 {
        const SIMPLE_VERBS: &[&str] = &[
            "restart", "reboot", "start", "stop", "enable", "disable",
            "update", "upgrade", "install", "uninstall", "remove",
            "show", "list", "check", "clear", "run", "open", "close",
            "delete", "move", "copy", "rename", "kill", "reload",
        ];
        let first = words[0].to_lowercase();
        if SIMPLE_VERBS.contains(&first.as_str()) {
            let has_ambiguity = AMBIGUOUS.iter().any(|a| lower.contains(a));
            if !has_ambiguity {
                return Some(PromptClass::Simple);
            }
        }
    }

    None
}
