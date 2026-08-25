use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::{info, warn};

use super::{planner, resolve_provider, trajectory, ApiKeySet, Provider};
use crate::memory::facts;

/// Pull the assistant text out of a raw provider response.
///
/// Falls back to "SIMPLE" rather than erroring: an unparseable classifier reply
/// should route conservatively, not kill the turn.
fn extract_text(v: &serde_json::Value, provider: Provider) -> String {
    let slot = match provider {
        Provider::Anthropic => &v["content"][0]["text"],
        _ => &v["choices"][0]["message"]["content"],
    };
    slot.as_str().unwrap_or("SIMPLE").to_string()
}

/// First `max` chars. Char-based, not byte-based: `&s[..600]` panics the moment a
/// user writes Greek, and the existing call sites in this file each hand-roll a
/// `is_char_boundary` walk to avoid it.
fn truncate_head(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Last `max` chars — the tail of an agent reply is the part that says what
/// happened, so a long reply is trimmed from the front.
fn truncate_tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        s.chars().skip(n - max).collect()
    }
}

/// Classification result from the cheap model.
#[derive(Debug, Clone)]
pub enum PromptClass {
    /// Trivial question — answer directly with the classifier model, skip primary.
    Trivial(String),
    /// Simple question — pass through to primary unchanged.
    Simple,
    /// Medium task — uses primary model but skips quality review and code review.
    /// Tests still run if detected. Covers clear code tasks that don't need enhancement.
    Medium,
    /// Complex task — enhanced prompt replaces the original for the primary model.
    Complex(String),
}

/// Result of the local weighted prompt scorer.
struct ScoreResult {
    score: f64,
    confidence: f64,
}

#[derive(Clone)]
pub struct Classifier {
    keys: ApiKeySet,
    model: String,
    provider: Provider,
    http: Arc<reqwest::Client>,
    /// Where this classifier's calls are billed. `None` outside a metered
    /// turn — every cheap pass in this file bills through it, which is what
    /// makes the per-role cost split able to prove cheap routing pays off.
    meter: Option<crate::agent::metrics::MeterHandle>,
}

impl Classifier {
    pub fn new(
        keys: &ApiKeySet,
        model: &str,
        explicit_provider: &str,
        http: Arc<reqwest::Client>,
    ) -> Self {
        let provider = resolve_provider(model, explicit_provider);
        Self {
            keys: keys.clone(),
            model: model.to_string(),
            provider,
            http,
            meter: None,
        }
    }

    /// Bill this classifier's calls to `meter`.
    pub fn with_meter(mut self, meter: Option<crate::agent::metrics::MeterHandle>) -> Self {
        self.meter = meter;
        self
    }

    /// Classify and optionally enhance a user prompt.
    /// Returns the classification with any direct answer or enhanced prompt.
    /// Route one prompt.
    ///
    /// `facts_block` is the same `[FACTS]` text the agent gets. It matters
    /// because the TRIVIAL path answers from HERE and never reaches the agent,
    /// so without it a question about something the user stated two turns ago
    /// ("what is our free-shipping threshold?") is classified trivial and
    /// answered "I don't have that information" by a model that was never shown
    /// the fact. Caught by bench scenario M1: the fact was stored correctly and
    /// the routing walked straight past it.
    pub async fn classify(&self, user_message: &str, facts_block: &str) -> Result<PromptClass> {
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

        let system = if facts_block.is_empty() {
            system.to_string()
        } else {
            format!(
                "{system}\n\n[STANDING FACTS]\nRules and decisions from earlier in \
                 this session. If the user is asking about something stated here, \
                 answer it as TRIVIAL using the fact VERBATIM, including any number. \
                 Never say you lack information that appears below.\n{facts_block}"
            )
        };
        let prompt = format!("User message: {}", user_message);
        let response = self.call_llm(&system, &prompt, 256).await?;
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
        self.call_llm_as("classifier", system, prompt, max_tokens).await
    }

    /// Every cheap pass in this file goes through here, so metering is structural
    /// rather than something each new pass has to remember to add.
    async fn call_llm_as(
        &self,
        role: &str,
        system: &str,
        prompt: &str,
        max_tokens: usize,
    ) -> Result<String> {
        let started = std::time::Instant::now();
        let raw = match self.provider {
            Provider::Anthropic => self.call_anthropic_raw(system, prompt, max_tokens).await,
            // OpenAI and DeepSeek share the chat-completions shape.
            _ => self.call_openai_compatible_raw(system, prompt, max_tokens).await,
        };
        let latency = started.elapsed().as_secs_f64();
        match raw {
            Ok(v) => {
                crate::agent::metrics::record(
                    self.meter.as_ref(),
                    role,
                    &self.model,
                    crate::agent::metrics::usage_from_response(&v),
                    latency,
                    None,
                );
                Ok(extract_text(&v, self.provider))
            }
            Err(e) => {
                crate::agent::metrics::record(
                    self.meter.as_ref(),
                    role,
                    &self.model,
                    crate::agent::metrics::Usage::default(),
                    latency,
                    Some(e.to_string()),
                );
                Err(e)
            }
        }
    }

    async fn call_openai_compatible_raw(&self, system: &str, prompt: &str, max_tokens: usize) -> Result<serde_json::Value> {
        let base = self.provider.base_url()
            .ok_or_else(|| anyhow::anyhow!("provider has no OpenAI-compatible endpoint"))?;
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
            .post(format!("{}/chat/completions", base))
            .header("Authorization", format!("Bearer {}", self.keys.for_provider(self.provider)))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Failed to reach {} API for classification", self.provider.as_str()))?;

        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("{} classifier error ({}): {}", self.provider.as_str(), status, text);
        }

        serde_json::from_str(&text).context("Failed to parse classifier response")
    }

    async fn call_anthropic_raw(&self, system: &str, prompt: &str, max_tokens: usize) -> Result<serde_json::Value> {
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
            .header("x-api-key", &self.keys.anthropic)
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
        Ok(json)
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

        // Truncate agent text to avoid wasting tokens.
        let text_tail = truncate_tail(agent_text, 600);
        let prompt = build_review_prompt(user_request, tool_log, &text_tail);

        match self.call_llm_as("heartbeat", system, &prompt, 16).await {
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
                format!("[{}]: {}", role, truncate_head(content, 120))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!("Conversation:\n{}", snippet);
        match self.call_llm_as("title", system, &prompt, 20).await {
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

        let response = self.call_llm_as("skills", system, &prompt, 128).await?;
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

    // ── durable-context passes ──────────────────────────────────────────────
    // All four run on the cheap model. Each one is optional and fails soft: a
    // failure in the learning layer must cost a fact, a plan or a journal line,
    // never the turn that already succeeded.

    /// Pull durable statements out of one turn.
    ///
    /// This is the pass that makes a rule stated in turn 1 survive to turn 20:
    /// nothing here depends on the model having remembered to call a tool. Pass
    /// `correction = true` when the user turn looks like a correction, which
    /// floors the importance of whatever comes back — the thing an agent is most
    /// expensive to forget.
    pub async fn extract_facts(
        &self,
        user_message: &str,
        agent_text: &str,
        correction: bool,
    ) -> Vec<facts::ExtractedFact> {
        let convo = format!(
            "USER: {}\n\nAGENT: {}",
            truncate_head(user_message, 2000),
            truncate_tail(agent_text, 1500)
        );
        let raw = match self.call_llm(facts::EXTRACT_SYSTEM, &convo, 400).await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "Fact extraction failed; the turn keeps its result");
                return Vec::new();
            }
        };
        let mut rows = facts::parse_extraction(&raw);
        if correction {
            for r in rows.iter_mut() {
                r.importance = r.importance.max(facts::CORRECTION_FLOOR);
            }
        }
        if !rows.is_empty() {
            info!(learned = rows.len(), correction, "Facts extracted from turn");
        }
        rows
    }

    /// A short, brain-grounded plan for a complex task.
    ///
    /// Returns "" when the model declined or produced something that would steer
    /// nothing — an unusable plan still costs tokens on every call of the loop.
    pub async fn plan(&self, task: &str, brain_context: &str, facts_block: &str) -> String {
        let prompt = planner::build_prompt(task, brain_context, facts_block);
        match self
            .call_llm_as("planner", planner::PLAN_SYSTEM, &prompt, planner::MAX_PLAN_TOKENS)
            .await
        {
            Ok(p) if planner::is_useful(&p) => p.trim().to_string(),
            Ok(_) => {
                info!("Planner produced nothing usable; proceeding unplanned");
                String::new()
            }
            Err(e) => {
                warn!(error = %e, "Planner call failed; proceeding unplanned");
                String::new()
            }
        }
    }

    /// One line for the project journal.
    ///
    /// Deliberately not the agent's own prose: that is written to be read by a
    /// human mid-session and reads as noise a week later.
    pub async fn summarise_turn(&self, user_message: &str, agent_text: &str) -> String {
        let system = "Summarise what was DONE in one line, under 30 words. State the outcome, \
                      not the intent. No preamble.";
        let prompt = format!(
            "REQUEST: {}\n\nAGENT: {}",
            truncate_head(user_message, 600),
            truncate_tail(agent_text, 1200)
        );
        match self.call_llm_as("journal", system, &prompt, 80).await {
            Ok(s) => truncate_head(s.trim().lines().next().unwrap_or(""), 200),
            Err(e) => {
                warn!(error = %e, "Journal summary failed; no entry written");
                String::new()
            }
        }
    }

    /// Turn a session trace into a reusable skill. `None` when it declined.
    pub async fn distill_skill(&self, trace: &str) -> Option<trajectory::DistilledSkill> {
        match self
            .call_llm_as("distill", trajectory::DISTILL_SYSTEM, trace, 1200)
            .await
        {
            Ok(raw) => trajectory::parse_skill(&raw),
            Err(e) => {
                warn!(error = %e, "Skill distillation failed");
                None
            }
        }
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
    /// Conversation recovery: clean up a window of messages by merging correction
    /// sequences and removing noise while preserving the true meaning.
    /// Returns None if no cleanup needed, or Some(cleaned_messages) as (role, content) pairs.
    pub async fn conversation_recovery(
        &self,
        messages: &[(String, String)],
    ) -> Option<Vec<(String, String)>> {
        if messages.len() < 4 {
            return None;
        }

        let system = r#"You are a conversation editor. You receive a sequence of user/assistant messages from an AI agent conversation. Clean up noise while preserving meaning.

MERGE these patterns:
- User asks something vague → agent misunderstands → user corrects → agent redoes
  → Merge into: user's clarified request + agent's final correct response
- User says "no, I meant X" or "that's wrong, do Y instead"
  → Merge the original request + correction into one clear user message, keep only the correct assistant response
- Agent gives a wrong answer then corrects itself after user feedback
  → Keep only the correct final version with the clarified request

PRESERVE exactly:
- All file paths, code changes, command outputs, and tool results from the FINAL correct action
- All decisions the user confirmed or made
- All context needed to continue the conversation (what was built, installed, configured)
- The chronological order of distinct topics
- Messages that are already clean (no correction pattern) — return them unchanged
- <tool_trace> blocks in assistant messages — these are compressed tool logs, keep them verbatim

NEVER:
- Remove messages containing unique information not repeated elsewhere
- Merge messages about unrelated topics into one
- Add information that wasn't in the original messages
- Change the meaning or outcome of any exchange
- Drop standalone questions/answers that have no correction

Output: Return a JSON array of {"role":"...","content":"..."} objects with the cleaned messages.
If NO cleanup is needed (all messages are already clean), respond with exactly: NO_CHANGE"#;

        // Build the conversation text
        let mut conv = String::new();
        for (i, (role, content)) in messages.iter().enumerate() {
            let preview = if content.len() > 2000 {
                let mut b = 2000;
                while b > 0 && !content.is_char_boundary(b) {
                    b -= 1;
                }
                format!("{}…[truncated]", &content[..b])
            } else {
                content.clone()
            };
            conv.push_str(&format!("MSG {}: [{}]\n{}\n\n", i + 1, role, preview));
        }

        let prompt = format!(
            "Here are {} messages to review:\n\n{}",
            messages.len(),
            conv
        );

        match self.call_llm(system, &prompt, 4096).await {
            Ok(resp) => {
                let trimmed = resp.trim();
                if trimmed == "NO_CHANGE" || trimmed.starts_with("NO_CHANGE") {
                    info!("Conversation recovery: no changes needed");
                    return None;
                }

                // Try to parse JSON array from response
                // The LLM might wrap it in ```json ... ```, so strip that
                let json_str = trimmed
                    .trim_start_matches("```json")
                    .trim_start_matches("```")
                    .trim_end_matches("```")
                    .trim();

                match serde_json::from_str::<Vec<serde_json::Value>>(json_str) {
                    Ok(arr) => {
                        let cleaned: Vec<(String, String)> = arr
                            .iter()
                            .filter_map(|v| {
                                let role = v["role"].as_str()?.to_string();
                                let content = v["content"].as_str()?.to_string();
                                if role.is_empty() || content.is_empty() {
                                    return None;
                                }
                                Some((role, content))
                            })
                            .collect();

                        if cleaned.is_empty() {
                            warn!("Conversation recovery: parsed empty array, skipping");
                            return None;
                        }

                        // Only apply if we actually reduced message count
                        if cleaned.len() >= messages.len() {
                            info!(
                                original = messages.len(),
                                cleaned = cleaned.len(),
                                "Conversation recovery: no reduction, skipping"
                            );
                            return None;
                        }

                        info!(
                            original = messages.len(),
                            cleaned = cleaned.len(),
                            "Conversation recovery: cleaned messages"
                        );
                        Some(cleaned)
                    }
                    Err(e) => {
                        warn!(error = %e, response = %trimmed, "Conversation recovery: failed to parse JSON");
                        None
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Conversation recovery LLM call failed");
                None
            }
        }
    }

    /// Verify whether a background task was truly completed.
    /// Returns Ok(true) if verified, Ok(false) with reason appended if failed.
    /// Fails open (returns true) on LLM errors to avoid blocking the worker.
    pub async fn verify_task(&self, task_title: &str, task_context: &str, result: &str) -> (bool, String) {
        let system = r#"You are a task completion verifier for an AI agent. Given a task description and the agent's result, determine if the task was TRULY completed.

Check:
- Did the agent actually PERFORM the requested actions (not just describe what it would do)?
- Are the claimed results specific and concrete (file paths, command outputs, tool traces)?
- Is there evidence of real tool usage (<tool_trace> blocks, command outputs, file writes)?
- Does the result address ALL parts of the task, not just some?
- Would the user consider this task done if they read the result?

An agent that only DESCRIBES what to do without actually doing it is NOT complete.
An agent that completed some parts but skipped others is NOT complete.

Respond in EXACTLY one of these formats:
VERIFIED
FAILED: <1-2 sentence reason why the task is not complete>"#;

        let result_preview = if result.len() > 3000 {
            let mut b = 3000;
            while b > 0 && !result.is_char_boundary(b) { b -= 1; }
            format!("{}…[truncated]", &result[..b])
        } else {
            result.to_string()
        };

        let prompt = format!(
            "TASK: {}\nCONTEXT: {}\n\nAGENT RESULT:\n{}",
            task_title,
            if task_context.is_empty() { "(none)" } else { task_context },
            result_preview
        );

        match self.call_llm(system, &prompt, 128).await {
            Ok(resp) => {
                let trimmed = resp.trim();
                info!(verify_result = %trimmed, task = %task_title, "Task verification result");
                if trimmed.starts_with("VERIFIED") {
                    (true, String::new())
                } else if trimmed.starts_with("FAILED:") {
                    let reason = trimmed.strip_prefix("FAILED:").unwrap_or("").trim().to_string();
                    (false, reason)
                } else {
                    warn!(response = %trimmed, "Task verification returned unexpected value — treating as verified");
                    (true, String::new())
                }
            }
            Err(e) => {
                warn!(error = %e, "Task verification LLM call failed — treating as verified");
                (true, String::new())
            }
        }
    }
}

/// One-shot code reviewer and test generator using a dedicated code model.
pub struct CodeReviewer {
    keys: ApiKeySet,
    model: String,
    provider: Provider,
    http: Arc<reqwest::Client>,
}

impl CodeReviewer {
    pub fn new(
        keys: &ApiKeySet,
        model: &str,
        explicit_provider: &str,
        http: Arc<reqwest::Client>,
    ) -> Self {
        let provider = resolve_provider(model, explicit_provider);
        Self {
            keys: keys.clone(),
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
            Provider::Anthropic => self.call_anthropic(system, prompt, max_tokens).await,
            // OpenAI and DeepSeek share the chat-completions shape.
            _ => self.call_openai_compatible(system, prompt, max_tokens).await,
        }
    }

    async fn call_openai_compatible(&self, system: &str, prompt: &str, max_tokens: usize) -> Result<String> {
        let base = self.provider.base_url()
            .ok_or_else(|| anyhow::anyhow!("provider has no OpenAI-compatible endpoint"))?;
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": prompt },
            ],
            "max_tokens": max_tokens,
        });
        let resp = self.http
            .post(format!("{}/chat/completions", base))
            .header("Authorization", format!("Bearer {}", self.keys.for_provider(self.provider)))
            .header("Content-Type", "application/json")
            .json(&body)
            .send().await.with_context(|| format!("{} code review request failed", self.provider.as_str()))?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("{} code review error ({}): {}", self.provider.as_str(), status, if text.len() > 300 { let mut b = 300; while b > 0 && !text.is_char_boundary(b) { b -= 1; } &text[..b] } else { &text });
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
            .header("x-api-key", &self.keys.anthropic)
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

/// Weighted multi-dimension prompt scorer inspired by ClawRouter.
/// Evaluates 10 dimensions to produce a complexity score and confidence level.
/// Handles 60-70% of requests locally with zero LLM cost and <1ms latency.
fn score_prompt(msg: &str) -> ScoreResult {
    let trimmed = msg.trim();
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    let lower = trimmed.to_lowercase();
    let mut score: f64 = 0.0;

    // --- Quality-escalation overrides (hard rules) ---
    // Very long prompts are almost always multi-step complex tasks
    if words.len() > 500 {
        return ScoreResult { score: 0.5, confidence: 0.95 };
    }

    // 1. Reasoning markers (weight 0.18)
    const REASONING: &[&str] = &[
        "prove", "theorem", "derive", "step by step", "chain of thought",
        "formal", "logical", "contradiction", "induction", "deduction",
    ];
    let r_count = REASONING.iter().filter(|k| lower.contains(*k)).count();
    // Hard override: 3+ reasoning keywords = forced Complex
    if r_count >= 3 {
        return ScoreResult { score: 0.5, confidence: 0.90 };
    }
    score += 0.18 * (r_count as f64 / 2.0).min(1.0);

    // 2. Code presence (weight 0.15)
    const CODE: &[&str] = &[
        "```", "function ", "class ", "import ", "async ", "fn ", "struct ", "impl ",
        "def ", "const ", "let ", "var ",
    ];
    let c_count = CODE.iter().filter(|k| trimmed.contains(*k)).count();
    score += 0.15 * (c_count as f64 / 2.0).min(1.0);

    // 3. Multi-step patterns (weight 0.12)
    const MULTISTEP: &[&str] = &[
        "first", "then", "step 1", "step 2", "finally", "next",
        "after that", "followed by",
    ];
    let m_count = MULTISTEP.iter().filter(|k| lower.contains(*k)).count();
    score += 0.12 * (m_count as f64 / 2.0).min(1.0);

    // 4. Technical terms (weight 0.10)
    const TECHNICAL: &[&str] = &[
        "algorithm", "kubernetes", "distributed", "architecture",
        "database", "microservice", "encryption", "protocol",
        "infrastructure", "backend", "frontend", "pipeline",
    ];
    let t_count = TECHNICAL.iter().filter(|k| lower.contains(*k)).count();
    score += 0.10 * (t_count as f64 / 2.0).min(1.0);

    // 5. Token count proxy (weight 0.08)
    score += 0.08 * if words.len() < 10 { -0.5 }
                     else if words.len() > 100 { 0.8 }
                     else { (words.len() as f64 - 30.0) / 100.0 };

    // 6. Simple indicators (weight -0.08, negative — pulls score down)
    const SIMPLE_IND: &[&str] = &[
        "what is", "define", "translate", "hello", "thanks", "hi ",
        "who is", "how are", "good morning", "what time",
    ];
    let s_count = SIMPLE_IND.iter().filter(|k| lower.contains(*k)).count();
    score -= 0.08 * (s_count as f64).min(1.0);

    // 7. Agentic task markers (weight 0.06)
    const AGENTIC: &[&str] = &[
        "read file", "edit file", "deploy", "fix the", "debug",
        "refactor", "implement", "set up", "configure",
    ];
    let a_count = AGENTIC.iter().filter(|k| lower.contains(*k)).count();
    score += 0.06 * (a_count as f64 / 2.0).min(1.0);

    // 8. Creative markers (weight 0.05)
    const CREATIVE: &[&str] = &[
        "story", "poem", "brainstorm", "imagine", "creative", "design",
    ];
    let cr_count = CREATIVE.iter().filter(|k| lower.contains(*k)).count();
    score += 0.05 * (cr_count as f64).min(1.0);

    // 9. Constraint indicators (weight 0.04)
    const CONSTRAINTS: &[&str] = &[
        "at most", "at least", "within", "budget", "maximum", "O(",
        "performance", "optimize",
    ];
    let cn_count = CONSTRAINTS.iter().filter(|k| lower.contains(*k)).count();
    score += 0.04 * (cn_count as f64).min(1.0);

    // 10. Structured output request (weight 0.03)
    const OUTPUT: &[&str] = &["json", "yaml", "csv", "schema", "table format"];
    let o_count = OUTPUT.iter().filter(|k| lower.contains(*k)).count();
    score += 0.03 * (o_count as f64).min(1.0);

    // Hard override: structured output + schema = at least Medium
    if lower.contains("json") && lower.contains("schema") {
        score = score.max(0.1);
    }

    // --- Confidence via distance from nearest tier boundary ---
    // Boundaries: Simple/Medium at -0.02, Medium/Complex at 0.25
    let boundaries: &[f64] = &[-0.02, 0.25];
    let min_dist = boundaries.iter()
        .map(|b| (score - b).abs())
        .fold(f64::MAX, f64::min);
    let confidence = 1.0 / (1.0 + (-10.0 * min_dist).exp());

    ScoreResult { score, confidence }
}

/// Fast-path classifier: handles trivial patterns (greetings, acks, identity) that
/// the weighted scorer can't assess well. Returns None to fall through to scorer.
fn quick_classify_trivial(msg: &str) -> Option<PromptClass> {
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

    // Identity / capability questions — always Simple (main model answers these)
    const IDENTITY_STARTS: &[&str] = &[
        "who are you", "what are you", "what can you do",
        "do you have", "can you remember", "what do you know",
        "are you", "how do you", "what's your name", "what is your name",
    ];
    if IDENTITY_STARTS.iter().any(|p| lower.starts_with(p)) {
        return Some(PromptClass::Simple);
    }

    // Explicit remember/save instructions — only the primary model has memory tools
    if lower.starts_with("remember ") || lower.starts_with("save to memory")
        || lower.starts_with("store ") || lower.starts_with("forget ")
    {
        return Some(PromptClass::Simple);
    }

    None
}

/// Hybrid classifier entry point: trivial patterns → weighted scorer → LLM fallback.
/// The scorer handles 60-70% of requests locally; the LLM handles ambiguous cases.
fn quick_classify(msg: &str) -> Option<PromptClass> {
    // Stage 1: trivial patterns (greetings, acks, identity)
    if let Some(class) = quick_classify_trivial(msg) {
        return Some(class);
    }

    // Stage 2: weighted multi-dimension scorer
    let result = score_prompt(msg);
    if result.confidence >= 0.75 {
        info!(
            score = format!("{:.3}", result.score),
            confidence = format!("{:.3}", result.confidence),
            "Scorer: high confidence, skipping LLM"
        );
        return match result.score {
            s if s < -0.02 => Some(PromptClass::Simple),
            s if s < 0.25 => Some(PromptClass::Medium),
            _ => None,  // Complex territory — let LLM enhance the prompt
        };
    }

    // Stage 3: low confidence — fall through to LLM classifier
    info!(
        score = format!("{:.3}", result.score),
        confidence = format!("{:.3}", result.confidence),
        "Scorer: low confidence, falling through to LLM"
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // The durable-context passes themselves are network calls; their parsing is
    // tested where it lives (memory::facts, agent::planner, agent::trajectory).
    // What is worth testing here is the truncation both they and the older
    // review passes depend on, because the failure mode is a panic on real user
    // input rather than a wrong answer.

    #[test]
    fn short_input_is_returned_whole() {
        assert_eq!(truncate_head("hello", 100), "hello");
        assert_eq!(truncate_tail("hello", 100), "hello");
        assert_eq!(truncate_head("", 10), "");
        assert_eq!(truncate_tail("", 10), "");
    }

    #[test]
    fn head_keeps_the_front_and_tail_keeps_the_back() {
        assert_eq!(truncate_head("abcdef", 3), "abc");
        assert_eq!(truncate_tail("abcdef", 3), "def");
    }

    #[test]
    fn zero_max_is_not_a_panic() {
        assert_eq!(truncate_head("abc", 0), "");
        assert_eq!(truncate_tail("abc", 0), "");
    }

    #[test]
    fn multibyte_input_does_not_panic_or_split_a_char() {
        // The whole reason these helpers exist: byte-slicing this panics.
        let greek = "καταστημα ".repeat(300);
        let head = truncate_head(&greek, 600);
        let tail = truncate_tail(&greek, 600);
        assert_eq!(head.chars().count(), 600);
        assert_eq!(tail.chars().count(), 600);
        assert!(head.starts_with('κ'));
        assert!(greek.ends_with(&tail));
    }

    #[test]
    fn emoji_survive_truncation_intact() {
        let s = "🙂🙃".repeat(200);
        let out = truncate_head(&s, 5);
        assert_eq!(out.chars().count(), 5);
        assert!(out.starts_with('🙂'));
    }

    #[test]
    fn a_boundary_length_input_is_unchanged() {
        let s = "abcde";
        assert_eq!(truncate_head(s, 5), s);
        assert_eq!(truncate_tail(s, 5), s);
    }
}
