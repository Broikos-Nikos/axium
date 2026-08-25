//! bench-rust, drive the real `axium` binary through the shared scenarios.
//!
//! `bench-python` measures the Python implementation by importing it. This
//! measures the Rust implementation by *running it*: one `axium --once` process
//! per scenario, against a freshly generated copy of the same seed project,
//! graded by the same Python graders through `bench-python/bridge.py`, written
//! as the same JSONL row. A row from either project can be compared with a row
//! from the other, which is the entire point.
//!
//! ```text
//! bench-rust --sanity                     validate the graders first (free)
//! bench-rust --list                       the scenarios
//! bench-rust --only X2                    the cheapest real run
//! bench-rust --kind fix --reps 3
//! bench-rust --compare deepseek-v4-pro,deepseek-v4-flash
//! bench-rust --no-facts --no-checkpoints  ablations: each flag removes ONE thing
//! ```
//!
//! What it does not do: reimplement anything. Fixtures, graders and the
//! definition of "correct" live in `bench-python` and are reached over a process
//! boundary. Two graders that could disagree would make the comparison
//! meaningless, so there is one.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const DEFAULT_ITERATIONS: u64 = 20;
const DEFAULT_CONTINUATION: &str = "deepseek-v4-flash";
/// A turn that has not finished in ten minutes is not going to.
const TURN_TIMEOUT: Duration = Duration::from_secs(600);
const BRIDGE_TIMEOUT: Duration = Duration::from_secs(300);

// ── paths ───────────────────────────────────────────────────────────────────
fn repo_root() -> PathBuf {
    // <repo>/bench-rust/src/main.rs → <repo>
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent().map(|p| p.to_path_buf()).unwrap_or(here)
}

fn logs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("logs")
}

fn builds_dir() -> PathBuf {
    std::env::temp_dir().join("axium-bench-builds")
}

fn slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

// ── args ────────────────────────────────────────────────────────────────────
struct Args {
    raw: Vec<String>,
}

impl Args {
    fn parse() -> Self {
        Self { raw: std::env::args().skip(1).collect() }
    }
    fn has(&self, name: &str) -> bool {
        self.raw.iter().any(|a| a == name)
    }
    fn get(&self, name: &str) -> Option<String> {
        self.raw.iter().position(|a| a == name).and_then(|i| self.raw.get(i + 1)).cloned()
    }
    fn get_or(&self, name: &str, default: &str) -> String {
        self.get(name).unwrap_or_else(|| default.to_string())
    }
}

const HELP: &str = "\
bench-rust, benchmark the Rust axium binary through the shared scenarios

  --sanity              validate the graders and exit (free, no API calls)
  --list                list scenarios and exit
  --only B1,B4,X2       comma-separated scenario ids
  --kind fix            one family: fix|refactor|feature|aware|behaviour
  --reps N              repeat the suite N times (default 1)
  --model M             primary model (default: from config)
  --continuation M      cheap continuation model ('' to disable routing)
  --classifier M
  --mode M              simple|supercharge|skills (default supercharge)
  --max-iterations N    tool-loop cap (default 20)
  --effort E            primary reasoning effort: off|low|medium|high|max (default max)
  --compare A,B         run the whole suite once per model
  --no-facts --no-brain --no-planner --no-checkpoints
                        ablations: each removes exactly one mechanism
  --keep                keep build dirs for inspection
  --config PATH         source axium config.json (default: ../python/config.json)
  --axium-bin PATH      the binary (default: ../target/release/axium[.exe])
  --bridge PATH         grader bridge (default: ../bench-python/bridge.py)
  --python EXE          interpreter for the bridge (default: python)
  -v                    show tool calls as they happen
";

// ── subprocess with timeout ─────────────────────────────────────────────────
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Run {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Run { code: None, stdout: String::new(), stderr: format!("spawn failed: {e}"), timed_out: false }
        }
    };
    // Drain both pipes on threads. Reading them sequentially after wait() can
    // deadlock once a pipe buffer fills, and a chatty agent fills stderr fast.
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || { let mut s = String::new(); let _ = out_pipe.read_to_string(&mut s); s });
    let err_t = std::thread::spawn(move || { let mut s = String::new(); let _ = err_pipe.read_to_string(&mut s); s });

    let started = Instant::now();
    let mut timed_out = false;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                timed_out = true;
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break None,
        }
    };
    Run {
        code,
        stdout: out_t.join().unwrap_or_default(),
        stderr: err_t.join().unwrap_or_default(),
        timed_out,
    }
}

// ── the bridge ──────────────────────────────────────────────────────────────
struct Bridge {
    python: String,
    script: PathBuf,
}

impl Bridge {
    fn call(&self, args: &[&str]) -> Result<Value, String> {
        let mut cmd = Command::new(&self.python);
        cmd.arg(&self.script).args(args);
        let r = run_with_timeout(cmd, BRIDGE_TIMEOUT);
        if r.timed_out {
            return Err(format!("bridge {} timed out", args.join(" ")));
        }
        if r.code != Some(0) {
            return Err(format!("bridge {} failed (exit {:?}): {}", args.join(" "), r.code, r.stderr.trim()));
        }
        // Exactly one JSON object on stdout; the bridge promises that.
        serde_json::from_str(r.stdout.trim())
            .map_err(|e| format!("bridge {} printed non-JSON: {e}\n{}", args.join(" "), r.stdout))
    }
}

// ── config for one build ────────────────────────────────────────────────────
struct Knobs {
    model: Option<String>,
    continuation: Option<String>,
    classifier: Option<String>,
    mode: String,
    max_iterations: u64,
    effort: String,
    no_facts: bool,
    no_brain: bool,
    no_planner: bool,
    no_checkpoints: bool,
}

impl Knobs {
    fn from(a: &Args) -> Self {
        Self {
            model: a.get("--model"),
            continuation: a.get("--continuation"),
            classifier: a.get("--classifier"),
            mode: a.get_or("--mode", "supercharge"),
            max_iterations: a.get("--max-iterations").and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_ITERATIONS),
            effort: a.get_or("--effort", "max"),
            no_facts: a.has("--no-facts"),
            no_brain: a.has("--no-brain"),
            no_planner: a.has("--no-planner"),
            no_checkpoints: a.has("--no-checkpoints"),
        }
    }
}

/// The per-build config.json: the source config with every knob applied and
/// every path pointed INSIDE the build, so nothing a scenario does can reach
/// the real memory, facts or history.
fn build_config(source: &Value, knobs: &Knobs, build: &Path) -> Value {
    let mut c = source.clone();
    let models = c["models"].as_object_mut().map(|m| m.clone()).unwrap_or_default();
    let mut models = Value::Object(models);
    if let Some(m) = &knobs.model {
        models["primary"] = json!(m);
        models["primary_provider"] = json!("");
    }
    if let Some(m) = &knobs.continuation {
        models["continuation"] = json!(m);
        models["continuation_provider"] = json!("");
    }
    if let Some(m) = &knobs.classifier {
        models["classifier"] = json!(m);
        models["classifier_provider"] = json!("");
    }
    // The Rust loader requires these; a Python-flavoured config may lack them.
    if models["compactor"].as_str().unwrap_or("").is_empty() {
        let fallback = models["classifier"].as_str().or(models["primary"].as_str()).unwrap_or("").to_string();
        models["compactor"] = json!(fallback);
    }
    c["models"] = models;

    let mut s = c["settings"].clone();
    if s.is_null() {
        s = json!({});
    }
    let defaults: [(&str, Value); 5] = [
        ("token_limit", json!(80000)),
        ("max_tokens", json!(8192)),
        ("max_history_messages", json!(200)),
        ("terminal_timeout_secs", json!(120)),
        ("max_output_chars", json!(15000)),
    ];
    for (k, v) in defaults {
        if s[k].is_null() {
            s[k] = v;
        }
    }
    s["working_directory"] = json!(slash(build));
    // Relative data paths resolve against the config's directory, which is
    // <build>/.axium/, so memory, facts and history all land in the build.
    s["memory_file"] = json!("memory.md");
    s["facts_file"] = json!("facts.db");
    s["max_tool_iterations"] = json!(knobs.max_iterations);
    s["thinking_effort"] = json!(knobs.effort);
    s["mode"] = json!(knobs.mode);
    s["conversation_logging"] = json!(false);
    s["telegram_enabled"] = json!(false);
    s["facts_enabled"] = json!(!knobs.no_facts);
    s["brain_enabled"] = json!(!knobs.no_brain);
    s["planner_enabled"] = json!(!knobs.no_planner);
    s["checkpoints_enabled"] = json!(!knobs.no_checkpoints);
    s["distill_skills"] = json!(false);
    c["settings"] = s;

    if c["agent"].is_null() {
        c["agent"] = json!({"name": "Axium", "soul": ""});
    }
    if c["agent"]["soul"].is_null() {
        c["agent"]["soul"] = json!("");
    }
    c
}

/// Same tag rule as `bench.runner.config_tag`, so a Rust log and a Python log
/// for the same knobs sit under the same name in their respective directories.
fn config_tag(cfg: &Value, knobs: &Knobs) -> String {
    let primary = cfg["models"]["primary"].as_str().unwrap_or("?");
    let cont = cfg["models"]["continuation"].as_str().unwrap_or("");
    let mut parts = vec![primary.to_string(), knobs.mode.clone()];
    if cont.is_empty() {
        parts.push("noroute".into());
    } else if cont != DEFAULT_CONTINUATION {
        parts.push(format!("cont-{cont}"));
    }
    if knobs.max_iterations != DEFAULT_ITERATIONS {
        parts.push(format!("it{}", knobs.max_iterations));
    }
    parts.push(format!("eff-{}", knobs.effort));
    let mut ablations: Vec<&str> = Vec::new();
    if knobs.no_facts { ablations.push("nofacts") }
    if knobs.no_brain { ablations.push("nobrain") }
    if knobs.no_planner { ablations.push("noplanner") }
    if knobs.no_checkpoints { ablations.push("nockpt") }
    if !ablations.is_empty() {
        parts.push(ablations.join("-"));
    }
    parts.join("__")
}

// ── one scenario ────────────────────────────────────────────────────────────
struct Ctx<'a> {
    bridge: &'a Bridge,
    /// Read-only turns between a setup turn and the graded followup, so the setup
    /// is pushed out of the window and compaction has to deal with it.
    filler: &'a [String],
    axium: &'a Path,
    source_cfg: &'a Value,
    knobs: &'a Knobs,
    keep: bool,
    verbose: bool,
}

fn run_scenario(ctx: &Ctx, sc: &Value) -> Value {
    let filler = ctx.filler;
    let id = sc["id"].as_str().unwrap_or("?");
    let build = builds_dir().join(format!("{id}_{}", chrono::Local::now().format("%H%M%S%f")));
    let _ = fs::create_dir_all(&build);

    let mut error: Option<String> = None;
    if let Err(e) = ctx.bridge.call(&["generate", &slash(&build)]) {
        error = Some(e);
    }

    // M2 compares the restored tree against the untouched seed byte for byte, so
    // the comparison copy has to be taken BEFORE the agent runs.
    if sc["pristine_copy"].as_bool() == Some(true) {
        let pristine = build.join(".axium").join("_pristine");
        let _ = fs::create_dir_all(&pristine);
        copy_tree(&build, &pristine);
    }

    let axium_dir = build.join(".axium");
    let _ = fs::create_dir_all(&axium_dir);
    let cfg = build_config(ctx.source_cfg, ctx.knobs, &build);
    let cfg_path = axium_dir.join("config.json");
    let _ = fs::write(&cfg_path, serde_json::to_string_pretty(&cfg).unwrap_or_default());

    // The exact prompt the Python runner sends, prefix included.
    let request = match ctx.bridge.call(&["request", id]) {
        Ok(v) => v["request"].as_str().unwrap_or("").to_string(),
        Err(e) => {
            error.get_or_insert(e);
            String::new()
        }
    };

    // ── the turns ──
    // A scenario may need more than one. `--session` carries history, memory and
    // facts across the separate processes, so several `--once` invocations behave
    // as one conversation, which is what M1 is actually testing.
    let session = format!("bench-{id}-{}", chrono::Local::now().format("%H%M%S%f"));
    let run_turn = |text: &str, err: &mut Option<String>| -> Value {
        if err.is_some() || text.is_empty() {
            return json!({"ok": false, "text": "", "changed": [], "class": "", "asked": [], "metrics": {}});
        }
        let mut cmd = Command::new(ctx.axium);
        cmd.arg("--once").arg(text)
            .arg("--workdir").arg(&build)
            .arg("--config").arg(&cfg_path)
            .arg("--session").arg(&session)
            .current_dir(&build);
        let r = run_with_timeout(cmd, TURN_TIMEOUT);
        if ctx.verbose {
            for line in r.stderr.lines().filter(|l| l.contains("tool") || l.contains("Classifier")) {
                eprintln!("      {line}");
            }
        }
        // `--once` promises exactly one JSON object on stdout; anything else on
        // stdout is a bug in the binary and is reported as such.
        match serde_json::from_str::<Value>(r.stdout.trim()) {
            Ok(v) => {
                if let Some(e) = v["error"].as_str() {
                    *err = Some(e.to_string());
                }
                v
            }
            Err(e) => {
                *err = Some(if r.timed_out {
                    format!("turn timed out after {}s", TURN_TIMEOUT.as_secs())
                } else {
                    format!("axium --once produced no JSON (exit {:?}): {e}; stderr tail: {}",
                            r.code, tail(&r.stderr, 400))
                });
                json!({"ok": false, "text": "", "changed": [], "class": "", "asked": [], "metrics": {}})
            }
        }
    };

    let t0 = Instant::now();
    // A warmup sets the scenario up (M3 builds the Brain) and is NOT graded.
    let warmup = sc["warmup"].as_str().unwrap_or("");
    if !warmup.is_empty() {
        let _ = run_turn(warmup, &mut error);
    }
    let mut once = run_turn(&request, &mut error);
    // A followup is the turn that gets GRADED, after filler has pushed the setup
    // turn out of the window. Grading the setup turn instead is a false pass: it
    // repeats the fact because the user just said it. That is exactly what this
    // harness did before this was wired.
    let followup = sc["followup"].as_str().unwrap_or("");
    if !followup.is_empty() {
        for f in filler {
            let _ = run_turn(f, &mut error);
        }
        once = run_turn(followup, &mut error);
    }
    let wall = t0.elapsed().as_secs_f64();

    // ── grade, through the same graders bench-python uses ──
    let turn_path = axium_dir.join("turn.json");
    let _ = fs::write(&turn_path, serde_json::to_string(&once).unwrap_or_default());
    let memory_path = axium_dir.join("memory.md");
    let graded = ctx.bridge.call(&[
        "grade", id, "--build", &slash(&build), "--turn", &slash(&turn_path),
        "--memory", &slash(&memory_path),
    ]);
    let graded = match graded {
        Ok(v) => v,
        Err(e) => {
            // A grader that crashed is a benchmark bug, not a zero. Say so and
            // score nothing rather than something misleading.
            error.get_or_insert(format!("GRADER FAILED: {e}"));
            json!({"change": 0.0, "regress": null, "change_detail": [], "regress_misses": []})
        }
    };

    let metrics = once["metrics"].clone();
    let rec = json!({
        "impl": "rust",
        "id": id,
        "name": sc["name"],
        "kind": sc["kind"],
        "difficulty": sc["difficulty"],
        "model": cfg["models"]["primary"],
        "continuation": cfg["models"]["continuation"],
        "mode": ctx.knobs.mode,
        "class": once["class"],
        "config": {
            "primary": cfg["models"]["primary"],
            "continuation": cfg["models"]["continuation"],
            "effort": ctx.knobs.effort,
            // The Rust build has no separate cheap-effort knob. Recorded as
            // null rather than copied from a flag it does not honour.
            "cheap_effort": null,
            "max_iterations": ctx.knobs.max_iterations,
            "mode": ctx.knobs.mode,
            "facts": !ctx.knobs.no_facts,
            "brain": !ctx.knobs.no_brain,
            "planner": !ctx.knobs.no_planner,
            "checkpoints": !ctx.knobs.no_checkpoints,
            // Which build produced the row. Found the hard way: `cargo test`
            // builds a test executable, not axium.exe, and a suite happily ran
            // against a binary from before the change it was measuring.
            "binary": slash(ctx.axium),
            "binary_mtime": binary_mtime(ctx.axium),
        },
        "change": graded["change"],
        "regress": graded["regress"],
        "change_detail": graded["change_detail"],
        "regress_misses": graded["regress_misses"],
        "changed_files": once["changed"],
        "answer": truncate(once["text"].as_str().unwrap_or(""), 3000),
        "asked": once["asked"],
        "error": error,
        "wall_s": (wall * 10.0).round() / 10.0,
        "metrics": metrics,
        "stamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string(),
        "build": if ctx.keep { json!(slash(&build)) } else { Value::Null },
    });

    // ── the same console line the Python runner prints ──
    let detail = rec["change_detail"].as_array().cloned().unwrap_or_default();
    let passed = detail.iter().filter(|r| r[1].as_bool() == Some(true)).count();
    let misses: Vec<String> = rec["regress_misses"].as_array().cloned().unwrap_or_default()
        .iter().filter_map(|v| v.as_str().map(String::from)).collect();
    let mut reg_txt = String::new();
    if !rec["regress"].is_null() {
        reg_txt = format!("  regress {}", if misses.is_empty() { "ok".to_string() } else { format!("BROKE: {}", truncate(&misses[0], 50)) });
    }
    println!("  [{id}] {:38} change {passed}/{}{reg_txt}", truncate(sc["name"].as_str().unwrap_or(""), 38), detail.len());
    let m = &rec["metrics"];
    println!("       {wall:5.0}s  {:2} calls  {:2} tools  {:>7}in/{:>6}out  {:>4.0}% cached  ${:.4}{}",
             m["llm_calls"].as_u64().unwrap_or(0), m["tool_calls"].as_u64().unwrap_or(0),
             m["input_tokens"].as_u64().unwrap_or(0), m["output_tokens"].as_u64().unwrap_or(0),
             m["cache_hit_rate"].as_f64().unwrap_or(0.0) * 100.0, m["cost_usd"].as_f64().unwrap_or(0.0),
             rec["error"].as_str().map(|e| format!("  ERROR {}", truncate(e, 60))).unwrap_or_default());
    for r in &detail {
        if r[1].as_bool() != Some(true) {
            println!("         MISS  {}", r[0].as_str().unwrap_or(""));
        }
    }

    if !ctx.keep {
        let _ = fs::remove_dir_all(&build);
    }
    rec
}

/// The binary's modification time, ISO-8601 UTC, so a row says which build
/// produced it. A benchmark that cannot answer "which binary?" cannot answer
/// "did the change help?".
fn binary_mtime(p: &Path) -> String {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// Recursive copy, skipping the agent's own state directory.
fn copy_tree(src: &Path, dst: &Path) {
    let Ok(rd) = fs::read_dir(src) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        if matches!(name.to_string_lossy().as_ref(), ".axium" | "__pycache__" | ".git") {
            continue;
        }
        let (from, to) = (e.path(), dst.join(&name));
        if from.is_dir() {
            let _ = fs::create_dir_all(&to);
            copy_tree(&from, &to);
        } else {
            let _ = fs::copy(&from, &to);
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect() }
}

fn tail(s: &str, n: usize) -> String {
    let c = s.chars().count();
    if c <= n { s.to_string() } else { s.chars().skip(c - n).collect() }
}

// ── suite ───────────────────────────────────────────────────────────────────
fn summarise(recs: &[Value], label: &str) {
    println!("\n{}", "=".repeat(78));
    println!("{label}");
    let mut by_kind: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for r in recs {
        by_kind.entry(r["kind"].as_str().unwrap_or("?").to_string()).or_default().push(r);
    }
    let mut total_cost = 0.0;
    for kind in ["fix", "refactor", "feature", "aware", "behaviour"] {
        let Some(rows) = by_kind.get(kind) else { continue };
        let ch: f64 = rows.iter().map(|r| r["change"].as_f64().unwrap_or(0.0)).sum::<f64>() / rows.len() as f64;
        let rg: Vec<f64> = rows.iter().filter_map(|r| r["regress"].as_f64()).collect();
        let cost: f64 = rows.iter().map(|r| r["metrics"]["cost_usd"].as_f64().unwrap_or(0.0)).sum();
        total_cost += cost;
        let wall: f64 = rows.iter().map(|r| r["wall_s"].as_f64().unwrap_or(0.0)).sum();
        let mut line = format!("{kind:10} change {:6.0}%", ch * 100.0);
        if !rg.is_empty() {
            line += &format!("   regress {:6.0}%", rg.iter().sum::<f64>() / rg.len() as f64 * 100.0);
        }
        line += &format!("   ${cost:7.4}  {:5.1}min  ({} runs)", wall / 60.0, rows.len());
        println!("{line}");
    }
    let overall: f64 = recs.iter().map(|r| r["change"].as_f64().unwrap_or(0.0)).sum::<f64>() / recs.len().max(1) as f64;
    println!("{}", "-".repeat(78));
    println!("{:10} change {:6.0}%   ${total_cost:7.4}", "OVERALL", overall * 100.0);
    let errs: Vec<&str> = recs.iter().filter(|r| !r["error"].is_null()).filter_map(|r| r["id"].as_str()).collect();
    if !errs.is_empty() {
        println!("{} run(s) errored: {}", errs.len(), errs.join(", "));
    }
}

fn main() {
    let a = Args::parse();
    if a.has("--help") || a.has("-h") {
        print!("{HELP}");
        return;
    }
    let root = repo_root();

    let scenarios_path = a.get("--scenarios").map(PathBuf::from).unwrap_or_else(|| root.join("scenarios.json"));
    let scenarios: Value = match fs::read_to_string(&scenarios_path).ok().and_then(|s| serde_json::from_str(&s).ok()) {
        Some(v) => v,
        None => {
            eprintln!("cannot read {}. Run: cd bench-python && python export_scenarios.py", slash(&scenarios_path));
            std::process::exit(1);
        }
    };
    let all: Vec<Value> = scenarios["bench"].as_array().cloned().unwrap_or_default();
    let filler: Vec<String> = scenarios["filler"].as_array().cloned().unwrap_or_default()
        .iter().filter_map(|v| v.as_str().map(String::from)).collect();

    if a.has("--list") {
        for s in &all {
            println!("{:4} {:10} d{}  {}", s["id"].as_str().unwrap_or(""), s["kind"].as_str().unwrap_or(""),
                     s["difficulty"], s["name"].as_str().unwrap_or(""));
        }
        return;
    }

    let bridge = Bridge {
        python: a.get_or("--python", "python"),
        script: a.get("--bridge").map(PathBuf::from).unwrap_or_else(|| root.join("bench-python").join("bridge.py")),
    };

    // Sanity is not optional: a green score against a broken grader means nothing.
    let sanity = match bridge.call(&["sanity"]) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("sanity could not run: {e}");
            std::process::exit(1);
        }
    };
    let problems: Vec<String> = sanity["problems"].as_array().cloned().unwrap_or_default()
        .iter().filter_map(|p| p.as_str().map(String::from)).collect();
    println!("sanity: {} scenarios, {} regression checks, {} problem(s)",
             sanity["scenarios"], sanity["regression_checks"], problems.len());
    for p in &problems {
        println!("  !! {p}");
    }
    if problems.is_empty() {
        println!("  graders detect the planted state and the baseline is green.");
    }
    if a.has("--sanity") {
        std::process::exit(if problems.is_empty() { 0 } else { 1 });
    }
    if !problems.is_empty() {
        eprintln!("\nAborting: graders are not trustworthy.");
        std::process::exit(1);
    }

    // ── select ──
    let only: Vec<String> = a.get("--only").unwrap_or_default().split(',')
        .map(|s| s.trim().to_uppercase()).filter(|s| !s.is_empty()).collect();
    let kind = a.get("--kind");
    let selected: Vec<Value> = all.iter().filter(|s| {
        (kind.as_deref().is_none_or(|k| s["kind"].as_str() == Some(k)))
            && (only.is_empty() || only.iter().any(|o| s["id"].as_str() == Some(o)))
    }).cloned().collect();
    if selected.is_empty() {
        eprintln!("No scenarios matched.");
        std::process::exit(1);
    }

    // ── the binary and the source config ──
    let axium = a.get("--axium-bin").map(PathBuf::from).unwrap_or_else(|| {
        let exe = root.join("target").join("release").join("axium.exe");
        if exe.exists() { exe } else { root.join("target").join("release").join("axium") }
    });
    if !axium.exists() {
        eprintln!("axium binary not found at {}. Build it: cargo build --release", slash(&axium));
        std::process::exit(1);
    }
    let source_cfg_path = a.get("--config").map(PathBuf::from)
        .unwrap_or_else(|| root.join("python").join("config.json"));
    let source_cfg: Value = match fs::read_to_string(&source_cfg_path).ok().and_then(|s| serde_json::from_str(&s).ok()) {
        Some(v) => v,
        None => {
            eprintln!("cannot read config {}. Pass --config PATH to an axium config.json with an API key.", slash(&source_cfg_path));
            std::process::exit(1);
        }
    };

    let reps: usize = a.get("--max-reps").or(a.get("--reps")).and_then(|s| s.parse().ok()).unwrap_or(1);
    let keep = a.has("--keep");
    let verbose = a.has("-v") || a.has("--verbose");
    let _ = fs::create_dir_all(builds_dir());
    let _ = fs::create_dir_all(logs_dir());

    let models: Vec<Option<String>> = {
        let list: Vec<String> = a.get("--compare").unwrap_or_default().split(',')
            .map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if list.is_empty() { vec![None] } else { list.into_iter().map(Some).collect() }
    };

    let mut comparison: Vec<(String, Vec<Value>)> = Vec::new();
    for model in models {
        let mut knobs = Knobs::from(&a);
        if model.is_some() {
            knobs.model = model.clone();
        }
        let cfg_preview = build_config(&source_cfg, &knobs, Path::new("."));
        let tag = config_tag(&cfg_preview, &knobs);
        let primary = cfg_preview["models"]["primary"].as_str().unwrap_or("?").to_string();
        let cont = cfg_preview["models"]["continuation"].as_str().unwrap_or("").to_string();
        println!("\n=== {primary} · continuation={} · mode={} · {} scenario(s) x {reps} rep(s) ===\n",
                 if cont.is_empty() { "(none)".to_string() } else { cont.clone() }, knobs.mode, selected.len());
        println!("    binary {} (built {})", slash(&axium), binary_mtime(&axium));

        let ctx = Ctx { bridge: &bridge, filler: &filler, axium: &axium, source_cfg: &source_cfg, knobs: &knobs, keep, verbose };
        let mut recs: Vec<Value> = Vec::new();
        for rep in 0..reps {
            if reps > 1 {
                println!("-- rep {}/{reps} --", rep + 1);
            }
            for sc in &selected {
                recs.push(run_scenario(&ctx, sc));
            }
        }

        // One file per configuration, so an ablation never averages into the
        // default config's numbers.
        let path = logs_dir().join(format!("{tag}.jsonl"));
        let mut body = String::new();
        for r in &recs {
            body.push_str(&serde_json::to_string(r).unwrap_or_default());
            body.push('\n');
        }
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let _ = fs::write(&path, existing + &body);
        println!("\nlogged {} run(s) -> logs/{tag}.jsonl", recs.len());
        summarise(&recs, &format!("{primary} · mode={}", knobs.mode));
        comparison.push((primary, recs));
    }

    if comparison.len() > 1 {
        println!("\n{}", "=".repeat(78));
        println!("MODEL COMPARISON");
        println!("{:26} {:>8} {:>9} {:>9} {:>8}", "model", "change", "regress", "cost", "time");
        for (model, recs) in &comparison {
            let ch: f64 = recs.iter().map(|r| r["change"].as_f64().unwrap_or(0.0)).sum::<f64>() / recs.len().max(1) as f64;
            let rg: Vec<f64> = recs.iter().filter_map(|r| r["regress"].as_f64()).collect();
            let rgv = if rg.is_empty() { 0.0 } else { rg.iter().sum::<f64>() / rg.len() as f64 };
            let cost: f64 = recs.iter().map(|r| r["metrics"]["cost_usd"].as_f64().unwrap_or(0.0)).sum();
            let wall: f64 = recs.iter().map(|r| r["wall_s"].as_f64().unwrap_or(0.0)).sum();
            println!("{model:26} {:7.0}% {:8.0}% ${cost:8.4} {:7.1}m", ch * 100.0, rgv * 100.0, wall / 60.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn knobs() -> Knobs {
        Knobs { model: None, continuation: None, classifier: None, mode: "supercharge".into(),
                max_iterations: DEFAULT_ITERATIONS, effort: "max".into(),
                no_facts: false, no_brain: false, no_planner: false, no_checkpoints: false }
    }

    fn source() -> Value {
        json!({"api_keys": {"deepseek": "k"},
               "models": {"primary": "deepseek-v4-pro", "continuation": DEFAULT_CONTINUATION,
                          "classifier": "deepseek-v4-flash"},
               "settings": {"token_limit": 80000, "memory_file": "data/memory.md"}})
    }

    #[test]
    fn every_path_in_the_build_config_points_inside_the_build() {
        let build = PathBuf::from("/tmp/axium-bench-builds/B1_1");
        let c = build_config(&source(), &knobs(), &build);
        assert_eq!(c["settings"]["working_directory"], "/tmp/axium-bench-builds/B1_1");
        // Relative to <build>/.axium/config.json, so inside the build.
        assert_eq!(c["settings"]["memory_file"], "memory.md");
        assert_eq!(c["settings"]["facts_file"], "facts.db");
    }

    #[test]
    fn required_rust_fields_are_filled_from_a_python_flavoured_config() {
        // python/config.json lacks max_history_messages and compactor may be
        // absent; the Rust loader refuses to start without them.
        let c = build_config(&source(), &knobs(), Path::new("/b"));
        assert_eq!(c["settings"]["max_history_messages"], 200);
        assert_eq!(c["settings"]["terminal_timeout_secs"], 120);
        assert_eq!(c["models"]["compactor"], "deepseek-v4-flash", "falls back to the classifier");
        assert_eq!(c["agent"]["name"], "Axium");
    }

    #[test]
    fn a_source_value_is_not_overridden_unless_a_knob_says_so() {
        let mut src = source();
        src["settings"]["token_limit"] = json!(123456);
        let c = build_config(&src, &knobs(), Path::new("/b"));
        assert_eq!(c["settings"]["token_limit"], 123456);
        assert_eq!(c["models"]["primary"], "deepseek-v4-pro");
    }

    #[test]
    fn ablation_flags_each_flip_exactly_one_setting() {
        let base = build_config(&source(), &knobs(), Path::new("/b"));
        let mut k = knobs();
        k.no_facts = true;
        let c = build_config(&source(), &k, Path::new("/b"));
        assert_eq!(c["settings"]["facts_enabled"], false);
        // Everything else identical.
        for key in ["brain_enabled", "planner_enabled", "checkpoints_enabled"] {
            assert_eq!(c["settings"][key], base["settings"][key], "{key} moved");
        }
    }

    #[test]
    fn the_tag_matches_the_python_rule() {
        let c = build_config(&source(), &knobs(), Path::new("/b"));
        assert_eq!(config_tag(&c, &knobs()), "deepseek-v4-pro__supercharge__eff-max");

        let mut k = knobs();
        k.continuation = Some(String::new());
        let c = build_config(&source(), &k, Path::new("/b"));
        assert_eq!(config_tag(&c, &k), "deepseek-v4-pro__supercharge__noroute__eff-max");

        let mut k = knobs();
        k.max_iterations = 5;
        k.no_checkpoints = true;
        let c = build_config(&source(), &k, Path::new("/b"));
        assert_eq!(config_tag(&c, &k), "deepseek-v4-pro__supercharge__it5__eff-max__nockpt");
    }

    #[test]
    fn disabling_routing_with_an_empty_continuation_is_honoured() {
        let mut k = knobs();
        k.continuation = Some(String::new());
        let c = build_config(&source(), &k, Path::new("/b"));
        assert_eq!(c["models"]["continuation"], "");
    }

    #[test]
    fn a_missing_binary_run_reports_rather_than_panics() {
        let mut cmd = Command::new("definitely-not-a-real-binary-xyz");
        cmd.arg("--once");
        let r = run_with_timeout(cmd, Duration::from_secs(5));
        assert!(r.code.is_none());
        assert!(r.stderr.contains("spawn failed"));
        assert!(!r.timed_out);
    }

    #[test]
    fn a_hung_process_is_killed_at_the_timeout() {
        let mut cmd = Command::new("python");
        cmd.args(["-c", "import time; time.sleep(30)"]);
        let started = Instant::now();
        let r = run_with_timeout(cmd, Duration::from_secs(1));
        assert!(r.timed_out);
        assert!(started.elapsed() < Duration::from_secs(10), "did not kill promptly");
    }
}
