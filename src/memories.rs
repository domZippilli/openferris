use anyhow::{Context, Result};
use std::path::PathBuf;

/// Manages long-term memories as a human-readable markdown file.
/// Each line is: `- <fact> (<date>)`
pub struct Memories {
    path: PathBuf,
}

impl Memories {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Default location: ~/.local/share/openferris/MEMORIES.md
    pub fn default_path() -> PathBuf {
        crate::config::data_dir().join("MEMORIES.md")
    }

    /// Append a memory with today's date.
    pub fn add(&self, content: &str) -> Result<()> {
        use std::io::Write;

        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let line = format!("- {} ({})\n", content.trim(), date);

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("Failed to open {}", self.path.display()))?;

        file.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Load all memories as a formatted string for the system prompt.
    /// Returns empty string if the file doesn't exist or is empty.
    pub fn load_for_prompt(&self) -> Result<String> {
        if !self.path.exists() {
            return Ok(String::new());
        }

        let content = std::fs::read_to_string(&self.path)
            .with_context(|| format!("Failed to read {}", self.path.display()))?;

        let content = content.trim();
        if content.is_empty() {
            return Ok(String::new());
        }

        Ok(format!("# Things I Remember\n\n{}\n\n", content))
    }

    /// Count memory lines.
    pub fn count(&self) -> Result<usize> {
        if !self.path.exists() {
            return Ok(0);
        }
        let content = std::fs::read_to_string(&self.path)?;
        Ok(content.lines().filter(|l| l.starts_with("- ")).count())
    }

    /// Count memories within a time window (by date prefix in the line).
    pub fn count_since(&self, since: &str) -> Result<usize> {
        if !self.path.exists() {
            return Ok(0);
        }
        let since_date = &since[..10]; // extract YYYY-MM-DD
        let content = std::fs::read_to_string(&self.path)?;
        Ok(content
            .lines()
            .filter(|l| {
                if let Some(date) = extract_date(l) {
                    date >= since_date
                } else {
                    false
                }
            })
            .count())
    }

    /// Delete all memories.
    pub fn delete_all(&self) -> Result<usize> {
        if !self.path.exists() {
            return Ok(0);
        }
        let count = self.count()?;
        std::fs::write(&self.path, "")?;
        Ok(count)
    }

    /// Delete memories within a time window (by date prefix in the line).
    pub fn delete_since(&self, since: &str) -> Result<usize> {
        if !self.path.exists() {
            return Ok(0);
        }
        let since_date = &since[..10];
        let content = std::fs::read_to_string(&self.path)?;
        let mut deleted = 0;
        let kept: Vec<&str> = content
            .lines()
            .filter(|l| {
                if let Some(date) = extract_date(l)
                    && date >= since_date
                {
                    deleted += 1;
                    return false;
                }
                true
            })
            .collect();

        let mut output = kept.join("\n");
        if !output.is_empty() {
            output.push('\n');
        }
        std::fs::write(&self.path, output)?;
        Ok(deleted)
    }
}

/// Extract the date from a memory line like `- Some fact (2026-03-11)`
fn extract_date(line: &str) -> Option<&str> {
    let line = line.trim();
    if !line.starts_with("- ") {
        return None;
    }
    // Look for (YYYY-MM-DD) at the end
    if line.ends_with(')') {
        let open = line.rfind('(')?;
        let date = &line[open + 1..line.len() - 1];
        if date.len() == 10 && date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-' {
            return Some(date);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_memories() -> (Memories, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("MEMORIES.md");
        (Memories::new(path), dir)
    }

    #[test]
    fn test_add_and_load() {
        let (m, _dir) = temp_memories();
        m.add("Safety word is banana").unwrap();
        m.add("User prefers dark mode").unwrap();

        let prompt = m.load_for_prompt().unwrap();
        assert!(prompt.contains("Safety word is banana"));
        assert!(prompt.contains("User prefers dark mode"));
        assert!(prompt.starts_with("# Things I Remember"));
    }

    #[test]
    fn test_empty_file() {
        let (m, _dir) = temp_memories();
        assert_eq!(m.load_for_prompt().unwrap(), "");
        assert_eq!(m.count().unwrap(), 0);
    }

    #[test]
    fn test_delete_all() {
        let (m, _dir) = temp_memories();
        m.add("Fact one").unwrap();
        m.add("Fact two").unwrap();
        assert_eq!(m.count().unwrap(), 2);

        let deleted = m.delete_all().unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(m.count().unwrap(), 0);
    }

    #[test]
    fn test_extract_date() {
        assert_eq!(
            extract_date("- Safety word is banana (2026-03-11)"),
            Some("2026-03-11")
        );
        assert_eq!(extract_date("not a memory line"), None);
        assert_eq!(extract_date("- no date here"), None);
    }
}
