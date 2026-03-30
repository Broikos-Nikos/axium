use regex::Regex;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Search files matching a regex pattern, optionally filtering by glob.
pub async fn search_files(pattern: &str, search_path: &str, include: &str) -> String {
    let re = match Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return format!("Invalid regex: {}", e),
    };

    let glob_pattern = if !include.is_empty() {
        glob::Pattern::new(include).ok()
    } else {
        None
    };

    let mut results = Vec::new();
    let mut files_searched = 0;
    collect_matches(Path::new(search_path), &re, &glob_pattern, &mut results, &mut files_searched, 0);

    if results.is_empty() {
        format!("No matches for '{}' in {} ({} files searched)", pattern, search_path, files_searched)
    } else {
        let total = results.len();
        // Cap at 50 results to avoid huge output
        results.truncate(50);
        let mut out = results.join("\n");
        if total > 50 {
            out.push_str(&format!("\n... and {} more matches", total - 50));
        }
        out
    }
}

fn collect_matches(
    dir: &Path,
    re: &Regex,
    glob_filter: &Option<glob::Pattern>,
    results: &mut Vec<String>,
    files_searched: &mut usize,
    depth: usize,
) {
    if depth > 8 || results.len() > 200 {
        return;
    }

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden dirs, build artifacts, node_modules
        if name.starts_with('.') || name == "target" || name == "node_modules" || name == "__pycache__" {
            continue;
        }

        if path.is_dir() {
            collect_matches(&path, re, glob_filter, results, files_searched, depth + 1);
        } else if path.is_file() {
            if let Some(ref pat) = glob_filter {
                if !pat.matches(&name) {
                    continue;
                }
            }

            // Skip binary files (check extension)
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "png" | "jpg" | "jpeg" | "gif" | "bmp" | "ico" | "svg" |
                "woff" | "woff2" | "ttf" | "eot" | "zip" | "tar" | "gz" | "bz2" |
                "pdf" | "exe" | "dll" | "so" | "dylib" | "o" | "a" | "class" | "pyc") {
                continue;
            }

            *files_searched += 1;

            // Use BufReader for line-by-line reading (avoids loading full file)
            if let Ok(file) = fs::File::open(&path) {
                let reader = BufReader::new(file);
                for (i, line_result) in reader.lines().enumerate() {
                    let line = match line_result {
                        Ok(l) => l,
                        Err(_) => break, // binary file or encoding error
                    };
                    if re.is_match(&line) {
                        results.push(format!("{}:{}:{}", path.display(), i + 1, line.trim()));
                        if results.len() > 200 {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// List directory contents.
pub fn list_directory(path: &str) -> String {
    let dir = Path::new(path);
    if !dir.exists() {
        return format!("Path not found: {}", path);
    }
    if !dir.is_dir() {
        return format!("{} is not a directory", path);
    }

    match fs::read_dir(dir) {
        Ok(entries) => {
            let mut items: Vec<String> = entries
                .flatten()
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    if e.path().is_dir() {
                        format!("{}/", name)
                    } else {
                        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                        format!("{} ({})", name, human_size(size))
                    }
                })
                .collect();
            items.sort();
            items.join("\n")
        }
        Err(e) => format!("Error listing {}: {}", path, e),
    }
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
