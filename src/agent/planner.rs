//! Grounded planner — a cheap-model plan before the expensive model starts.
//!
//! The classifier already rewrites a vague COMPLEX request into an explicit
//! brief. That fixes the *wording* of the task and knows nothing about the
//! codebase, so the primary model still opens with several orientation calls
//! before it does any work.
//!
//! The planner closes that loop: it is handed the Project Brain (profile, recent
//! journal, overview) plus the standing facts and the enhanced brief, and returns
//! a short ordered plan naming the actual files. It runs on the continuation
//! model at low effort, so it costs a fraction of a cent and saves the primary
//! model the orientation round-trips it would otherwise pay for at full price.
//!
//! It is advisory. The plan is injected as context, never as a contract: a plan
//! that turns out wrong must cost the agent one paragraph of prompt, not a
//! locked-in sequence of edits it cannot leave.
//!
//! Prompt text and thresholds match `python/axium/planner.py`, so a scenario
//! plans the same way in both benchmarks.

pub const PLAN_SYSTEM: &str = r#"You plan a coding task for an autonomous agent that will execute it with tools.

You are given what is already known about the project and the task. Produce a
SHORT ordered plan:

1. <step> - name the concrete files or symbols involved
2. ...

Rules:
- At most 5 steps. Fewer is better.
- Name real files from the project context. If the context does not name a file,
  say which file to FIND first, do not invent a path.
- The last step is always the check that proves the task is done.
- Do NOT write code. Do NOT explain. Output only the numbered steps.
- If the task is a question rather than a change, plan how to ANSWER it and say
  explicitly that no files are to be modified."#;

pub const MAX_PLAN_TOKENS: usize = 400;
pub const MAX_CONTEXT_CHARS: usize = 4000;
const MAX_FACTS_CHARS: usize = 1500;
const MIN_USEFUL_CHARS: usize = 30;
const MIN_NUMBERED_STEPS: usize = 2;

/// Openings that mean the model declined. A refusal is not a plan, and shipping
/// one costs tokens on every call of the loop while steering nothing.
const REFUSAL_PREFIXES: [&str; 5] = [
    "i cannot",
    "i can't",
    "i'm sorry",
    "sorry,",
    "unable to",
];

/// Truncate on a char boundary — byte-slicing a Greek or emoji value panics.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Assemble the planner's user message.
///
/// Brain first, then facts, then the task. The order is deliberate: the model
/// reads the ground truth about the project before it reads what it is being
/// asked to do, which is what stops it inventing file paths.
pub fn build_prompt(task: &str, brain_context: &str, facts: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !brain_context.trim().is_empty() {
        parts.push(format!(
            "[WHAT IS ALREADY KNOWN ABOUT THIS PROJECT]\n{}",
            truncate_chars(brain_context, MAX_CONTEXT_CHARS)
        ));
    }
    if !facts.trim().is_empty() {
        parts.push(format!(
            "[STANDING FACTS AND RULES]\n{}",
            truncate_chars(facts, MAX_FACTS_CHARS)
        ));
    }
    parts.push(format!("[TASK]\n{}", task.trim()));
    parts.join("\n\n")
}

/// Whether a plan is worth injecting.
///
/// A plan that is empty, apologetic, or a single vague line is worse than none.
/// The bar is two numbered steps: one step is a restatement of the task.
pub fn is_useful(plan: &str) -> bool {
    let p = plan.trim();
    if p.chars().count() < MIN_USEFUL_CHARS {
        return false;
    }
    let lowered = p.to_lowercase();
    if REFUSAL_PREFIXES.iter().any(|r| lowered.starts_with(r)) {
        return false;
    }
    count_numbered_steps(p) >= MIN_NUMBERED_STEPS
}

/// Lines that open with a step number: "1.", "2)", "3 - ", "4".
fn count_numbered_steps(plan: &str) -> usize {
    plan.lines()
        .filter(|line| {
            let t = line.trim_start();
            let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
            !digits.is_empty() && digits.len() <= 2
        })
        .count()
}

/// The `[PLAN]` block.
///
/// The wording matters as much as the plan: an agent told to follow a plan will
/// follow a wrong one off a cliff, so the block says explicitly that deviating
/// is allowed and that a wrong plan should be called out.
pub fn render(plan: &str) -> String {
    format!(
        "[PLAN]\nA cheap pre-pass produced this plan. Follow it where it is right; \
         deviate where it is wrong, and say so.\n\n{}",
        plan.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "1. Read app.py and list its symbols\n\
                        2. Split total() into pricing/total.py\n\
                        3. Run the tests to prove nothing broke";

    #[test]
    fn a_real_plan_is_useful() {
        assert!(is_useful(GOOD));
    }

    #[test]
    fn empty_and_tiny_plans_are_rejected() {
        assert!(!is_useful(""));
        assert!(!is_useful("   \n  "));
        assert!(!is_useful("ok"));
        assert!(!is_useful("1. do it"));
    }

    #[test]
    fn a_refusal_is_not_a_plan() {
        assert!(!is_useful("I cannot help with that request at all, sorry."));
        assert!(!is_useful("I'm sorry, but I do not have enough information here."));
        assert!(!is_useful("Unable to produce a plan for this particular task."));
    }

    #[test]
    fn one_step_is_a_restatement_not_a_plan() {
        assert!(!is_useful(
            "1. Refactor the pricing module into a package as requested by the user."
        ));
    }

    #[test]
    fn prose_of_sufficient_length_is_still_rejected() {
        let prose = "You should probably look at the pricing module and then change \
                     the discount threshold, after which the tests ought to pass fine.";
        assert!(!is_useful(prose), "unnumbered prose is not a plan");
    }

    #[test]
    fn alternative_step_markers_count() {
        assert!(is_useful("1) Read app.py\n2) Patch total()\n3) Run tests"));
        assert!(is_useful("1 - Read app.py\n2 - Patch total()"));
    }

    #[test]
    fn a_line_number_prefix_is_not_mistaken_for_a_step() {
        // A pasted file listing has 3+ digit line numbers; a plan does not.
        let listing = "1024 def total(x):\n1025     return x\n1026 # end of file";
        assert!(!is_useful(listing));
    }

    #[test]
    fn build_prompt_orders_brain_then_facts_then_task() {
        let out = build_prompt("Fix the discount", "Stack: Python.", "- (rule) Free over 50.");
        let brain = out.find("ALREADY KNOWN").unwrap();
        let facts = out.find("STANDING FACTS").unwrap();
        let task = out.find("[TASK]").unwrap();
        assert!(brain < facts && facts < task, "wrong order:\n{out}");
        assert!(out.contains("Fix the discount"));
    }

    #[test]
    fn build_prompt_omits_empty_sections() {
        let out = build_prompt("Fix the discount", "", "   ");
        assert!(!out.contains("ALREADY KNOWN"));
        assert!(!out.contains("STANDING FACTS"));
        assert!(out.starts_with("[TASK]"));
    }

    #[test]
    fn build_prompt_truncates_a_huge_brain() {
        let huge = "x".repeat(20_000);
        let out = build_prompt("task", &huge, "");
        assert!(out.len() < MAX_CONTEXT_CHARS + 500, "context not capped: {}", out.len());
        assert!(out.contains("[TASK]"), "the task must survive truncation");
    }

    #[test]
    fn build_prompt_truncates_huge_facts_but_keeps_the_task() {
        let huge = "- (rule) something\n".repeat(2000);
        let out = build_prompt("do the thing", "", &huge);
        assert!(out.contains("do the thing"));
        assert!(out.len() < MAX_FACTS_CHARS + 500);
    }

    #[test]
    fn multibyte_context_does_not_panic_on_truncation() {
        let greek = "καταστημα ".repeat(2000);
        let out = build_prompt("task", &greek, &greek);
        assert!(out.contains("[TASK]"));
    }

    #[test]
    fn render_says_the_plan_may_be_deviated_from() {
        let block = render(GOOD);
        assert!(block.starts_with("[PLAN]"));
        assert!(block.contains("deviate where it is wrong"));
        assert!(block.contains("Split total()"));
    }

    #[test]
    fn render_trims_surrounding_whitespace() {
        assert!(!render("\n\n  1. a\n2. b  \n\n").contains("\n\n\n"));
    }
}
