use anyhow::{bail, Result};
use async_trait::async_trait;
use std::path::PathBuf;

use super::Tool;

/// Validates that a path resolves to somewhere within the allowed directories.
/// Returns the canonicalized path if valid.
fn validate_path(path: &str, allowed_dirs: &[PathBuf]) -> Result<PathBuf> {
    let requested = PathBuf::from(path);

    // Expand ~ to home directory
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(rest)
    } else {
        requested
    };

    // For writes, the file may not exist yet — canonicalize the parent
    let check_path = if expanded.exists() {
        expanded.canonicalize()?
    } else {
        let parent = expanded
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Invalid path: {}", path))?;
        if !parent.exists() {
            // Parent doesn't exist yet either — use the absolute path for checking
            if expanded.is_absolute() {
                expanded.clone()
            } else {
                std::env::current_dir()?.join(&expanded)
            }
        } else {
            parent.canonicalize()?.join(expanded.file_name().unwrap_or_default())
        }
    };

    for allowed in allowed_dirs {
        // Ensure the allowed dir exists (create workspace on first use)
        if !allowed.exists() {
            std::fs::create_dir_all(allowed)?;
        }
        let allowed_canonical = allowed.canonicalize()?;
        if check_path.starts_with(&allowed_canonical) {
            return Ok(check_path);
        }
    }

    bail!(
        "Path '{}' is outside allowed directories. Allowed: {}",
        path,
        allowed_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

// --- ReadFile ---

pub struct ReadFileTool {
    allowed_dirs: Vec<PathBuf>,
}

impl ReadFileTool {
    pub fn new(allowed_dirs: Vec<PathBuf>) -> Self {
        Self { allowed_dirs }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description_for_llm(&self) -> &str {
        "Read the contents of a file. Parameters: {\"path\": \"<file path>\"}. \
         Only files within allowed directories can be read."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: path"))?;

        let validated = validate_path(path, &self.allowed_dirs)?;

        if !validated.exists() {
            bail!("File not found: {}", path);
        }

        if !validated.is_file() {
            bail!("Not a file: {}", path);
        }

        let content = std::fs::read_to_string(&validated)?;
        Ok(content)
    }
}

// --- WriteFile ---

pub struct WriteFileTool {
    allowed_dirs: Vec<PathBuf>,
}

impl WriteFileTool {
    pub fn new(allowed_dirs: Vec<PathBuf>) -> Self {
        Self { allowed_dirs }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description_for_llm(&self) -> &str {
        "Write content to a file, creating it and any parent directories if needed. \
         Parameters: {\"path\": \"<file path>\", \"content\": \"<text content>\"}. \
         Only files within allowed directories can be written."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: path"))?;

        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: content"))?;

        let validated = validate_path(path, &self.allowed_dirs)?;

        // Create parent directories if needed
        if let Some(parent) = validated.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(&validated, content)?;
        Ok(format!("Written {} bytes to {}", content.len(), validated.display()))
    }
}

// --- ListDirectory ---

pub struct ListDirTool {
    allowed_dirs: Vec<PathBuf>,
}

impl ListDirTool {
    pub fn new(allowed_dirs: Vec<PathBuf>) -> Self {
        Self { allowed_dirs }
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description_for_llm(&self) -> &str {
        "List files and directories at a given path. Parameters: {\"path\": \"<directory path>\"}. \
         Only directories within allowed paths can be listed."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: path"))?;

        let validated = validate_path(path, &self.allowed_dirs)?;

        if !validated.is_dir() {
            bail!("Not a directory: {}", path);
        }

        let mut entries: Vec<String> = vec![];
        for entry in std::fs::read_dir(&validated)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let suffix = if entry.file_type()?.is_dir() { "/" } else { "" };
            entries.push(format!("{}{}", name, suffix));
        }
        entries.sort();

        if entries.is_empty() {
            Ok("(empty directory)".to_string())
        } else {
            Ok(entries.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_path_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().to_path_buf()];
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();

        let result = validate_path(file.to_str().unwrap(), &allowed);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().to_path_buf()];

        let result = validate_path("/etc/passwd", &allowed);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("outside allowed"));
    }

    #[test]
    fn test_validate_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().to_path_buf()];

        // Create a file inside so the parent exists
        std::fs::write(dir.path().join("legit.txt"), "ok").unwrap();

        let sneaky = format!("{}/../../../etc/passwd", dir.path().display());
        let result = validate_path(&sneaky, &allowed);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().to_path_buf()];

        let write_tool = WriteFileTool::new(allowed.clone());
        let read_tool = ReadFileTool::new(allowed);

        let file_path = dir.path().join("notes.txt");
        let path_str = file_path.to_str().unwrap();

        let result = write_tool
            .execute(serde_json::json!({"path": path_str, "content": "hello world"}))
            .await
            .unwrap();
        assert!(result.contains("11 bytes"));

        let content = read_tool
            .execute(serde_json::json!({"path": path_str}))
            .await
            .unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_write_creates_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().to_path_buf()];

        let write_tool = WriteFileTool::new(allowed);
        let file_path = dir.path().join("sub/dir/file.txt");
        let path_str = file_path.to_str().unwrap();

        let result = write_tool
            .execute(serde_json::json!({"path": path_str, "content": "nested"}))
            .await;
        assert!(result.is_ok());
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "nested");
    }
}
