use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Build a file-level dependency graph for a Rust project and answer a
/// "who uses this file" / "what does this file use" query.
pub fn get_dependency_graph(path: &str, direction: &str, working_dir: &str) -> String {
    // Resolve the target file to an absolute path
    let target = resolve_path(path, working_dir);
    if !target.exists() {
        return format!("File not found: {}", path);
    }

    // Find Cargo.toml root
    let cargo_root = match find_cargo_root(&target) {
        Some(r) => r,
        None => return "No Cargo.toml found — not a Rust project.".to_string(),
    };

    let src_dir = cargo_root.join("src");
    if !src_dir.is_dir() {
        return "No src/ directory found.".to_string();
    }

    // Build the full import map: file → list of files it imports via `use crate::`
    let import_map = build_import_map(&src_dir);

    // Build reverse map: file → list of files that import it
    let mut reverse_map: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for (file, imports) in &import_map {
        for imp in imports {
            reverse_map.entry(imp.clone()).or_default().push(file.clone());
        }
    }

    let target_canonical = target.canonicalize().unwrap_or(target.clone());
    let mut out = String::new();

    let show_deps = matches!(direction, "dependencies" | "both");
    let show_dependents = matches!(direction, "dependents" | "both");

    if show_deps {
        let empty = Vec::new();
        let deps = import_map.get(&target_canonical).unwrap_or(&empty);
        if deps.is_empty() {
            out.push_str("Dependencies (imports): none\n");
        } else {
            out.push_str("Dependencies (this file imports):\n");
            for dep in deps {
                out.push_str(&format!("  {}\n", rel(&dep, &src_dir)));
            }
        }
    }

    if show_dependents {
        if show_deps && !out.is_empty() { out.push('\n'); }
        let empty = Vec::new();
        let users = reverse_map.get(&target_canonical).unwrap_or(&empty);
        if users.is_empty() {
            out.push_str("Dependents (imported by): none\n");
        } else {
            out.push_str("Dependents (files that import this):\n");
            for user in users {
                out.push_str(&format!("  {}\n", rel(user, &src_dir)));
            }
        }
    }

    if out.is_empty() {
        format!("No dependency information found for: {}", path)
    } else {
        out
    }
}

/// Walk src/ and for every .rs file extract the set of other .rs files it
/// imports via `use crate::` statements.
fn build_import_map(src_dir: &Path) -> HashMap<PathBuf, Vec<PathBuf>> {
    let mut map: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

    let rs_files = collect_rs_files(src_dir);

    for file in &rs_files {
        let content = match std::fs::read_to_string(file) {
            Ok(c) => c, Err(_) => continue,
        };

        let mut imports: Vec<PathBuf> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();

        for line in content.lines() {
            let t = line.trim();
            // Match: use crate::X, pub use crate::X, use crate::X as Y, use crate::X::{..}
            let crate_path = if let Some(rest) = t.strip_prefix("use crate::").or_else(|| {
                t.strip_prefix("pub use crate::")
            }) {
                // Take the module path segment before `::{`, ` as `, `;`, or whitespace
                let end = rest.find(|c: char| c == ';' || c == ' ' || c == '\n')
                    .unwrap_or(rest.len());
                let seg = &rest[..end];
                // Strip trailing ::{...} group imports — take up to the last :: before {
                seg.split("::").filter(|s| !s.starts_with('{') && s.chars().all(|c| c.is_alphanumeric() || c == '_'))
                    .collect::<Vec<_>>()
                    .join("/")
            } else {
                continue;
            };

            if crate_path.is_empty() { continue; }

            // Try src/X/Y.rs then src/X/Y/mod.rs
            let candidate1 = src_dir.join(format!("{}.rs", crate_path));
            let candidate2 = src_dir.join(format!("{}/mod.rs", crate_path));

            let resolved = if candidate1.exists() {
                candidate1.canonicalize().ok()
            } else if candidate2.exists() {
                candidate2.canonicalize().ok()
            } else {
                None
            };

            if let Some(r) = resolved {
                if !seen.contains(&r) {
                    seen.insert(r.clone());
                    imports.push(r);
                }
            }
        }

        if let Ok(canonical) = file.canonicalize() {
            map.insert(canonical, imports);
        }
    }

    map
}

fn collect_rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let skip = ["target", "node_modules", ".git"];
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || skip.contains(&name.as_str()) { continue; }
            if path.is_dir() {
                out.extend(collect_rs_files(&path));
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    out
}

fn find_cargo_root(path: &Path) -> Option<PathBuf> {
    let start = if path.is_file() { path.parent()? } else { path };
    let mut current = start;
    loop {
        if current.join("Cargo.toml").exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn resolve_path(path: &str, working_dir: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() { p.to_path_buf() } else { Path::new(working_dir).join(p) }
}

fn rel(path: &Path, src_dir: &Path) -> String {
    path.strip_prefix(src_dir)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}
