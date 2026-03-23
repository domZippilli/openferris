use anyhow::{bail, Result};
use async_trait::async_trait;
use std::path::{Component, Path, PathBuf};

use super::Tool;

/// Normalize a path by resolving `.` and `..` components lexically (without
/// touching the filesystem). This prevents traversal attacks that rely on
/// `canonicalize` failing when intermediate directories do not exist.
fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => { /* skip `.` */ }
            Component::ParentDir => {
                // Pop the last component; if there is nothing to pop, keep the
                // `..` only for relative paths (absolute paths just stay at root).
                if !normalized.pop() && !path.is_absolute() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other),
        }
    }
    normalized
}

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

    // Make the path absolute before normalizing
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(&expanded)
    };

    // First pass: normalize the path lexically to resolve `.` and `..` without
    // requiring the path to exist on disk. This catches traversal attempts even
    // when intermediate directories are missing.
    let normalized = normalize_path(&absolute);

    for allowed in allowed_dirs {
        if !allowed.exists() {
            std::fs::create_dir_all(allowed)?;
        }
        let allowed_canonical = allowed.canonicalize()?;
        let allowed_normalized = normalize_path(&allowed_canonical);

        if !normalized.starts_with(&allowed_normalized) {
            continue;
        }

        // Second pass: if the path (or its parent) exists on disk, verify with
        // canonicalize to resolve any symlinks.
        if normalized.exists() {
            let canonical = normalized.canonicalize()?;
            if canonical.starts_with(&allowed_canonical) {
                return Ok(canonical);
            }
            bail!(
                "Path '{}' resolves via symlink to outside allowed directories",
                path
            );
        }

        // Path does not exist yet (write case) — canonicalize the longest
        // existing ancestor and re-check.
        let mut ancestor = normalized.clone();
        let mut suffix_parts: Vec<std::ffi::OsString> = Vec::new();
        loop {
            if ancestor.exists() {
                let canonical_ancestor = ancestor.canonicalize()?;
                if canonical_ancestor.starts_with(&allowed_canonical) {
                    let mut result = canonical_ancestor;
                    for part in suffix_parts.into_iter().rev() {
                        result.push(part);
                    }
                    return Ok(result);
                }
                bail!(
                    "Path '{}' resolves via symlink to outside allowed directories",
                    path
                );
            }
            if let Some(name) = ancestor.file_name() {
                suffix_parts.push(name.to_os_string());
            }
            if !ancestor.pop() {
                break;
            }
        }

        // No ancestor exists (shouldn't happen for absolute paths on a real FS,
        // but handle gracefully) — the normalized check already passed.
        return Ok(normalized);
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

    #[test]
    fn test_validate_path_traversal_nonexistent_parent() {
        // This is the key bypass case: the intermediate dirs don't exist so
        // canonicalize can't resolve them, but the `..` components escape the
        // allowed directory.
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().to_path_buf()];

        let sneaky = format!("{}/nonexistent/../../etc/passwd", dir.path().display());
        let result = validate_path(&sneaky, &allowed);
        assert!(result.is_err(), "should reject traversal via nonexistent parent");
        assert!(
            result.unwrap_err().to_string().contains("outside allowed"),
            "error message should mention outside allowed directories"
        );
    }

    #[test]
    fn test_validate_path_deep_traversal_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().to_path_buf()];

        // Multiple levels of nonexistent dirs with enough `..` to escape
        let sneaky = format!(
            "{}/a/b/c/../../../../etc/shadow",
            dir.path().display()
        );
        let result = validate_path(&sneaky, &allowed);
        assert!(result.is_err(), "should reject deep traversal");
    }

    #[test]
    fn test_validate_path_traversal_with_dot_segments() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().to_path_buf()];

        // Mix `.` and `..` to try to confuse the normalizer
        let sneaky = format!("{}/./nonexistent/./../../../etc/passwd", dir.path().display());
        let result = validate_path(&sneaky, &allowed);
        assert!(result.is_err(), "should reject traversal with mixed . and ..");
    }

    #[test]
    fn test_validate_path_legitimate_dotdot_within_allowed() {
        // A `..` that stays within the allowed directory should still work
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().to_path_buf()];

        // Create a subdir so the path makes sense
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let file = dir.path().join("legit.txt");
        std::fs::write(&file, "ok").unwrap();

        let path = format!("{}/sub/../legit.txt", dir.path().display());
        let result = validate_path(&path, &allowed);
        assert!(result.is_ok(), ".. that stays within allowed dir should be fine");
    }

    #[test]
    fn test_normalize_path_basic() {
        let p = PathBuf::from("/a/b/../c/./d");
        assert_eq!(normalize_path(&p), PathBuf::from("/a/c/d"));
    }

    #[test]
    fn test_normalize_path_at_root() {
        // `..` at root should stay at root
        let p = PathBuf::from("/a/../../../b");
        assert_eq!(normalize_path(&p), PathBuf::from("/b"));
    }
}
