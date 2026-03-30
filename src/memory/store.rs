use anyhow::Result;
use std::fs;
use std::path::Path;

const DEFAULT_MEMORY: &str = "\
# Agent Memory

## User Preferences

## Active Projects

## Key Facts

## Session Notes
";

pub struct Memory {
    pub path: String,
    pub content: String,
}

pub fn load_memory(path: &str) -> Result<Memory> {
    let content = if Path::new(path).exists() {
        fs::read_to_string(path)?
    } else {
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, DEFAULT_MEMORY)?;
        DEFAULT_MEMORY.to_string()
    };
    Ok(Memory {
        path: path.to_string(),
        content,
    })
}

impl Memory {
    pub fn save(&self) -> Result<()> {
        let tmp = format!("{}.tmp", self.path);
        fs::write(&tmp, &self.content)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Find the byte offset of the next `## ` section header after `from`.
    fn next_section_start(&self, from: usize) -> usize {
        let slice = &self.content[from..];
        let mut byte_offset = 0;
        for (i, line) in slice.lines().enumerate() {
            if i > 0 && line.starts_with("## ") {
                return from + byte_offset;
            }
            // +1 for the '\n' that .lines() strips
            byte_offset += line.len() + 1;
        }
        self.content.len()
    }

    pub fn append_to_section(&mut self, section: &str, text: &str) -> Result<()> {
        let header = format!("## {}", section);
        if let Some(header_pos) = self.content.find(&header) {
            let after_header = header_pos + header.len();
            let insert_pos = self.next_section_start(after_header);
            // Ensure trailing newline before next section
            self.content.insert_str(insert_pos, &format!("\n- {}\n", text));
        } else {
            self.content
                .push_str(&format!("\n## {}\n- {}\n", section, text));
        }
        self.save()
    }

    pub fn replace_section(&mut self, section: &str, new_content: &str) -> Result<()> {
        let header = format!("## {}", section);
        if let Some(header_pos) = self.content.find(&header) {
            let content_start = header_pos + header.len();
            let section_end = self.next_section_start(content_start);
            // Ensure trailing newline before next section
            self.content
                .replace_range(content_start..section_end, &format!("\n{}\n", new_content));
        } else {
            self.content
                .push_str(&format!("\n## {}\n{}\n", section, new_content));
        }
        self.save()
    }
}

