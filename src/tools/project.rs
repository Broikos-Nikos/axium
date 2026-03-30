use std::path::Path;
use std::process::{Command, Stdio};
use std::io::Write;
use serde::{Serialize, Deserialize};

// ── Architecture cache ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default)]
struct ArchCache {
    entries: std::collections::HashMap<String, ArchCacheEntry>,
}

#[derive(Serialize, Deserialize)]
struct ArchCacheEntry {
    mtime: u64,
    line_count: usize,
    symbols: String,
}

/// Build a compact project context string for the system prompt.
/// Keeps it small to avoid burning tokens — just the essentials.
pub fn build_project_context(working_dir: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Git status (if in a git repo)
    if Path::new(working_dir).join(".git").exists() {
        if let Ok(out) = Command::new("git")
            .args(["status", "--short", "--branch"])
            .current_dir(working_dir)
            .output()
        {
            let status = String::from_utf8_lossy(&out.stdout);
            if !status.is_empty() {
                parts.push(format!("[GIT]\n{}", status.trim()));
            }
        }
        if let Ok(out) = Command::new("git")
            .args(["log", "--oneline", "-5"])
            .current_dir(working_dir)
            .output()
        {
            let log = String::from_utf8_lossy(&out.stdout);
            if !log.is_empty() {
                parts.push(format!("[RECENT COMMITS]\n{}", log.trim()));
            }
        }
    }

    // Detect project type and key files
    let mut key_files = Vec::new();
    for name in &[
        "Cargo.toml", "package.json", "pyproject.toml", "requirements.txt",
        "Makefile", "CMakeLists.txt", "go.mod", "Dockerfile", ".env",
        "README.md", "setup.py", "build.gradle",
    ] {
        if Path::new(working_dir).join(name).exists() {
            key_files.push(*name);
        }
    }
    if !key_files.is_empty() {
        parts.push(format!("[PROJECT FILES] {}", key_files.join(", ")));
    }

    // Shallow directory listing (depth 1, max 40 entries)
    if let Ok(entries) = std::fs::read_dir(working_dir) {
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                !name.starts_with('.') && name != "target" && name != "node_modules"
            })
            .take(40)
            .map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if e.path().is_dir() { format!("{}/", name) } else { name }
            })
            .collect();
        names.sort();
        if !names.is_empty() {
            parts.push(format!("[DIRECTORY]\n{}", names.join("  ")));
        }
    }

    // Architecture map for Rust projects (cached, uses rust-analyzer)
    if Path::new(working_dir).join("Cargo.toml").exists() {
        let arch = build_architecture_map(working_dir);
        if !arch.is_empty() {
            parts.push(format!("[ARCHITECTURE]\n{}", arch.trim_end()));
        }
    }

    if parts.is_empty() {
        return String::new();
    }

    // Project-specific knowledge saved by the agent (.axium/knowledge.md)
    let knowledge_path = Path::new(working_dir).join(".axium/knowledge.md");
    if knowledge_path.exists() {
        if let Ok(knowledge) = std::fs::read_to_string(&knowledge_path) {
            let trimmed = knowledge.trim();
            if !trimmed.is_empty() {
                parts.push(format!("[PROJECT KNOWLEDGE]\n{}", trimmed));
            }
        }
    }

    parts.join("\n\n")
}

// ── scan_project ─────────────────────────────────────────────────────────────

const SKIP_DIRS: &[&str] = &[
    ".git", "target", "node_modules", "__pycache__", ".next", "dist",
    "build", "vendor", ".venv", "venv", ".mypy_cache", ".ruff_cache",
    "coverage", ".turbo", ".cache", "out",
];

const CODE_EXTS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "py", "go",
    "java", "kt", "swift", "c", "cpp", "h", "hpp", "cs", "rb",
];

/// Produce an annotated file-tree of `root_path` (max `max_depth` levels).
/// Code files get a short symbol list extracted from their top-level declarations.
/// Output is capped at ~8 KB.
pub fn scan_project(root_path: &str, max_depth: usize) -> String {
    let root = Path::new(root_path);
    if !root.exists() {
        return format!("Path not found: {}", root_path);
    }

    let mut out = String::new();
    out.push_str(root_path);
    out.push('\n');

    let mut buf: Vec<u8> = Vec::new();
    walk_tree(root, 0, max_depth, &mut vec![], &mut buf);

    let tree = String::from_utf8_lossy(&buf);
    out.push_str(&tree);

    // Cap output
    const CAP: usize = 16 * 1024;
    if out.len() > CAP {
        let mut b = CAP;
        while b > 0 && !out.is_char_boundary(b) { b -= 1; }
        out.truncate(b);
        out.push_str("\n... (output truncated)");
    }
    out
}

fn walk_tree(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    prefix_stack: &mut Vec<bool>, // true = more siblings follow
    buf: &mut Vec<u8>,
) {
    if depth >= max_depth { return; }

    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(e) => e.filter_map(|x| x.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| {
        let name = e.file_name().to_string_lossy().to_lowercase();
        // dirs first, then files; alphabetically within each group
        let is_dir = e.path().is_dir();
        (!is_dir, name)
    });

    // Filter noise
    let entries: Vec<_> = entries.into_iter().filter(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.')
    }).collect();

    let count = entries.len();
    for (i, entry) in entries.iter().enumerate() {
        let last = i + 1 == count;
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();

        // Build the line prefix from the stack
        let mut line = String::new();
        for &has_more in prefix_stack.iter() {
            line.push_str(if has_more { "│   " } else { "    " });
        }
        line.push_str(if last { "└── " } else { "├── " });
        line.push_str(&name);

        if path.is_dir() {
            line.push('/');
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if CODE_EXTS.contains(&ext) {
                let symbols = if ext == "rs" {
                    extract_symbols_ra(&path).or_else(|| extract_symbols(&path))
                } else {
                    extract_symbols(&path)
                };
                if let Some(syms) = symbols {
                    if !syms.is_empty() {
                        line.push_str("  [");
                        line.push_str(&syms);
                        line.push(']');
                    }
                }
            }
        }

        line.push('\n');
        buf.extend_from_slice(line.as_bytes());

        if path.is_dir() && depth + 1 < max_depth {
            prefix_stack.push(!last);
            walk_tree(&path, depth + 1, max_depth, prefix_stack, buf);
            prefix_stack.pop();
        }

        if buf.len() > 16 * 1024 { return; }
    }
}

/// Extract top-level symbol names from a source file using pattern matching.
fn extract_symbols(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?;
    let content = std::fs::read_to_string(path).ok()?;

    let mut symbols: Vec<String> = Vec::new();

    for line in content.lines() {
        let t = line.trim();
        let sym = match ext {
            "rs" => {
                // pub fn foo, pub struct Foo, pub enum Foo, pub trait Foo, impl Foo
                if let Some(rest) = t.strip_prefix("pub fn ").or_else(|| t.strip_prefix("pub async fn ")) {
                    rest.split('(').next().map(|s| s.trim().to_string())
                } else if let Some(rest) = t.strip_prefix("pub struct ")
                    .or_else(|| t.strip_prefix("pub enum "))
                    .or_else(|| t.strip_prefix("pub trait "))
                    .or_else(|| t.strip_prefix("pub type "))
                {
                    rest.split(|c: char| !c.is_alphanumeric() && c != '_').next()
                        .map(|s| s.to_string())
                } else if let Some(rest) = t.strip_prefix("impl ") {
                    let name = rest.split_whitespace().last().unwrap_or("").trim_end_matches('{').trim();
                    if !name.is_empty() { Some(format!("impl {}", name)) } else { None }
                } else {
                    None
                }
            }
            "py" => {
                if let Some(rest) = t.strip_prefix("def ")
                    .or_else(|| t.strip_prefix("async def "))
                    .or_else(|| t.strip_prefix("class "))
                {
                    rest.split(|c: char| c == '(' || c == ':').next()
                        .map(|s| s.trim().to_string())
                } else {
                    None
                }
            }
            "go" => {
                if let Some(rest) = t.strip_prefix("func ") {
                    rest.split('(').next().map(|s| s.trim().to_string())
                } else {
                    None
                }
            }
            "ts" | "tsx" | "js" | "jsx" | "mjs" => {
                if let Some(rest) = t.strip_prefix("export function ")
                    .or_else(|| t.strip_prefix("export async function "))
                    .or_else(|| t.strip_prefix("export class "))
                    .or_else(|| t.strip_prefix("export default function "))
                {
                    rest.split(|c: char| c == '(' || c == ' ' || c == '{').next()
                        .map(|s| s.trim().to_string())
                } else if let Some(rest) = t.strip_prefix("export const ")
                    .or_else(|| t.strip_prefix("export let "))
                {
                    rest.split(|c: char| c == ':' || c == '=' || c == ' ').next()
                        .map(|s| s.trim().to_string())
                } else if !t.starts_with("//") && !t.starts_with("*") {
                    if let Some(rest) = t.strip_prefix("function ")
                        .or_else(|| t.strip_prefix("class "))
                    {
                        rest.split(|c: char| c == '(' || c == ' ').next()
                            .map(|s| s.trim().to_string())
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some(s) = sym {
            let s = s.trim().to_string();
            if !s.is_empty() && s.len() <= 40 {
                symbols.push(s);
            }
        }

        if symbols.len() >= 8 { break; }
    }

    if symbols.is_empty() {
        None
    } else {
        Some(symbols.join(", "))
    }
}

// ── Architecture map ─────────────────────────────────────────────────────────

/// Build a compact architecture summary of `src/` for Rust projects.
/// Per-file entries are cached in `.axium/architecture_cache.json` by mtime.
fn build_architecture_map(working_dir: &str) -> String {
    let src_dir = Path::new(working_dir).join("src");
    if !src_dir.is_dir() { return String::new(); }

    let axium_dir = Path::new(working_dir).join(".axium");
    let cache_path = axium_dir.join("architecture_cache.json");

    let mut cache: ArchCache = cache_path.exists()
        .then(|| std::fs::read_to_string(&cache_path).ok())
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut dirty = false;
    let mut entries: Vec<(String, usize, String)> = Vec::new();
    collect_arch_entries(&src_dir, &src_dir, &mut cache, &mut dirty, &mut entries);

    if dirty {
        let _ = std::fs::create_dir_all(&axium_dir);
        if let Ok(json) = serde_json::to_string(&cache) {
            let _ = std::fs::write(&cache_path, json);
        }
    }

    if entries.is_empty() { return String::new(); }

    // Format flat lines: "  rel/path.rs [NL] — symbols"
    let mut out = String::new();
    for (rel, line_count, symbols) in &entries {
        let indent = "  ".repeat(rel.matches('/').count() + 1);
        let fname = rel.rsplit('/').next().unwrap_or(rel);
        if symbols.is_empty() {
            out.push_str(&format!("{}{}  [{}L]\n", indent, fname, line_count));
        } else {
            let syms = if symbols.len() > 100 { &symbols[..100] } else { symbols.as_str() };
            out.push_str(&format!("{}{}  [{}L] — {}\n", indent, fname, line_count, syms));
        }
        if out.len() > 2000 {
            out.push_str("  ...\n");
            break;
        }
    }
    out
}

fn collect_arch_entries(
    dir: &Path,
    src_root: &Path,
    cache: &mut ArchCache,
    dirty: &mut bool,
    entries: &mut Vec<(String, usize, String)>,
) {
    let mut items: Vec<_> = match std::fs::read_dir(dir) {
        Ok(e) => e.filter_map(|x| x.ok()).collect(),
        Err(_) => return,
    };
    items.sort_by_key(|e| {
        let n = e.file_name().to_string_lossy().to_lowercase();
        (!e.path().is_dir(), n)
    });

    for item in &items {
        let path = item.path();
        let name = item.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) { continue; }

        if path.is_dir() {
            collect_arch_entries(&path, src_root, cache, dirty, entries);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let rel = path.strip_prefix(src_root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| name.clone());

            let mtime = path.metadata().ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            if let Some(cached) = cache.entries.get(&rel) {
                if cached.mtime == mtime {
                    entries.push((rel, cached.line_count, cached.symbols.clone()));
                    continue;
                }
            }

            // Cache miss — read file and extract symbols
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c, Err(_) => continue,
            };
            let line_count = content.lines().count();
            let symbols = extract_symbols_ra(&path).unwrap_or_default();

            cache.entries.insert(rel.clone(), ArchCacheEntry { mtime, line_count, symbols: symbols.clone() });
            *dirty = true;
            entries.push((rel, line_count, symbols));
        }
    }
}

/// Extract symbols from a Rust file using `rust-analyzer symbols` via stdin.
/// Returns a compact hierarchical string: enums with variants, structs with fields,
/// impl blocks with method→ReturnType. Falls back to None if rust-analyzer unavailable.
fn extract_symbols_ra(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;

    let mut child = Command::new("rust-analyzer")
        .arg("symbols")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    {
        let stdin = child.stdin.as_mut()?;
        let _ = stdin.write_all(content.as_bytes());
    }

    let output = child.wait_with_output().ok()?;
    if output.stdout.is_empty() { return None; }

    let text = String::from_utf8_lossy(&output.stdout);

    struct Node {
        parent: Option<usize>,
        label: String,
        kind: String,
        ret_type: Option<String>,
    }

    // Parse each StructureNode line. We keep ALL nodes (including Local) to
    // preserve the original 0-based indices, which parent: Some(N) refers to.
    let parse_line = |line: &str| -> Option<Node> {
        let parent = if line.contains("parent: None") {
            None
        } else if let Some(pos) = line.find("parent: Some(") {
            let s = &line[pos + 13..];
            Some(s[..s.find(')')?].parse::<usize>().ok()?)
        } else {
            return None;
        };
        let label = {
            let pos = line.find("label: \"")?;
            let s = &line[pos + 8..];
            s[..s.find('"')?].to_string()
        };
        let kind = {
            let pos = line.find("SymbolKind(")?;
            let s = &line[pos + 11..];
            s[..s.find(')')?].to_string()
        };
        // Return type from detail "fn(...) -> X" → "X"
        let ret_type = if kind != "Local" {
            line.find("detail: Some(\"").and_then(|pos| {
                let s = &line[pos + 14..];
                let end = s.find('"')?;
                let detail = &s[..end];
                detail.rfind(" -> ").map(|a| detail[a + 4..].trim().to_string())
            })
        } else {
            None
        };
        Some(Node { parent, label, kind, ret_type })
    };

    let nodes: Vec<Node> = text.lines().filter_map(parse_line).collect();
    if nodes.is_empty() { return None; }

    let mut parts: Vec<String> = Vec::new();

    for (i, node) in nodes.iter().enumerate() {
        // Only process top-level non-local items
        if node.parent.is_some() || node.kind == "Local" { continue; }

        let formatted = match node.kind.as_str() {
            "Enum" => {
                let variants: Vec<&str> = nodes.iter()
                    .filter(|n| n.parent == Some(i) && n.kind == "Variant")
                    .map(|n| n.label.as_str())
                    .collect();
                if variants.is_empty() { format!("enum {}", node.label) }
                else { format!("enum {}{{{}}}", node.label, variants.join(",")) }
            }
            "Struct" => {
                let fields: Vec<&str> = nodes.iter()
                    .filter(|n| n.parent == Some(i) && n.kind == "Field")
                    .take(6)
                    .map(|n| n.label.as_str())
                    .collect();
                if fields.is_empty() { format!("struct {}", node.label) }
                else { format!("struct {}{{{}}}", node.label, fields.join(",")) }
            }
            "Trait" => format!("trait {}", node.label),
            "Module" => format!("mod {}", node.label),
            "Impl" => {
                let methods: Vec<String> = nodes.iter()
                    .filter(|n| n.parent == Some(i) && matches!(n.kind.as_str(), "Function" | "Method"))
                    .take(15)
                    .map(|n| match &n.ret_type {
                        Some(r) => format!("{}→{}", n.label, r),
                        None => n.label.clone(),
                    })
                    .collect();
                if methods.is_empty() { format!("impl {}", node.label) }
                else { format!("impl {}: {}", node.label, methods.join(", ")) }
            }
            "Function" | "Method" => match &node.ret_type {
                Some(r) => format!("fn {}→{}", node.label, r),
                None => format!("fn {}", node.label),
            },
            "Const" | "Static" => match &node.ret_type {
                Some(r) => format!("const {}: {}", node.label, r),
                None => format!("const {}", node.label),
            },
            "TypeAlias" => format!("type {}", node.label),
            _ => node.label.clone(),
        };

        parts.push(formatted);
        if parts.len() >= 25 { break; }
    }

    if parts.is_empty() { None } else { Some(parts.join("; ")) }
}
