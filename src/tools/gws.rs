use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Tool, files};

/// Denied method verbs — these are destructive or send outbound messages.
const DENIED_METHODS: &[&str] = &["delete", "trash", "send", "empty", "remove"];

/// Denied top-level subcommands.
const DENIED_SUBCOMMANDS: &[&str] = &["auth"];
const GWS_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;
const MAX_BASE64_BYTES: u64 = 1 * 1024 * 1024;
const SUPPORTED_IMAGE_MIME_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/gif",
    "image/bmp",
    "image/tiff",
];

pub struct GwsTool;
pub struct GwsDriveDownloadFileTool;
pub struct GwsDriveDownloadFileToPathTool {
    allowed_dirs: Vec<PathBuf>,
}

impl GwsDriveDownloadFileToPathTool {
    pub fn new(allowed_dirs: Vec<PathBuf>) -> Self {
        Self { allowed_dirs }
    }
}

#[derive(Debug, Deserialize)]
struct DriveFileMetadata {
    id: String,
    name: String,
    #[serde(rename = "mimeType")]
    mime_type: String,
    size: Option<serde_json::Value>,
}

/// Split a command string respecting single and double quotes.
fn shell_split(input: &str) -> Result<Vec<String>> {
    let mut args = vec![];
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }

    if in_single || in_double {
        bail!("Unterminated quote in command");
    }

    if !current.is_empty() {
        args.push(current);
    }

    Ok(args)
}

fn is_allowed(args: &[&str]) -> Result<()> {
    if args.is_empty() {
        bail!("No command provided");
    }

    let first = args[0].to_lowercase();
    if DENIED_SUBCOMMANDS.contains(&first.as_str()) {
        bail!("The '{}' subcommand is not allowed", first);
    }

    // Check if any argument matches a denied method verb.
    // gws commands follow `gws <service> <resource> <method>` so the method
    // is typically the 3rd positional arg, but checking all non-flag args is safer.
    for arg in args {
        if arg.starts_with('-') {
            continue;
        }
        let lower = arg.to_lowercase();
        if DENIED_METHODS.contains(&lower.as_str()) {
            bail!(
                "The '{}' method is not allowed — destructive and outbound operations are blocked",
                arg
            );
        }
    }

    Ok(())
}

fn parse_drive_size(size: Option<&serde_json::Value>) -> Result<Option<u64>> {
    match size {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => s
            .parse::<u64>()
            .map(Some)
            .with_context(|| format!("Invalid Drive file size: {}", s)),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("Invalid Drive file size: {}", n)),
        Some(other) => bail!("Invalid Drive file size value: {}", other),
    }
}

fn requested_max_bytes(params: &serde_json::Value) -> Result<u64> {
    requested_max_bytes_with_limit(params, MAX_DOWNLOAD_BYTES)
}

fn requested_base64_max_bytes(params: &serde_json::Value) -> Result<u64> {
    requested_max_bytes_with_limit(params, MAX_BASE64_BYTES)
}

fn requested_max_bytes_with_limit(params: &serde_json::Value, hard_limit: u64) -> Result<u64> {
    match params.get("max_bytes") {
        None | Some(serde_json::Value::Null) => Ok(hard_limit),
        Some(value) => {
            let requested = value
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("max_bytes must be a positive integer"))?;
            if requested == 0 {
                bail!("max_bytes must be greater than zero");
            }
            if requested > hard_limit {
                bail!(
                    "max_bytes may not exceed the hard limit of {} bytes",
                    hard_limit
                );
            }
            Ok(requested)
        }
    }
}

fn requested_mime_allowlist(params: &serde_json::Value) -> Result<Vec<String>> {
    let Some(value) = params.get("mime_type_allowlist") else {
        return Ok(SUPPORTED_IMAGE_MIME_TYPES
            .iter()
            .map(|mime| mime.to_string())
            .collect());
    };

    let values = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("mime_type_allowlist must be an array of strings"))?;
    if values.is_empty() {
        bail!("mime_type_allowlist must not be empty");
    }

    let mut allowlist = Vec::with_capacity(values.len());
    for value in values {
        let mime = value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("mime_type_allowlist must contain only strings"))?;
        if !SUPPORTED_IMAGE_MIME_TYPES.contains(&mime) {
            bail!("Unsupported MIME type in allowlist: {}", mime);
        }
        allowlist.push(mime.to_string());
    }

    Ok(allowlist)
}

async fn run_gws(args: &[&str]) -> Result<std::process::Output> {
    let output = tokio::time::timeout(
        GWS_TIMEOUT,
        tokio::process::Command::new("gws")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("gws timed out after {:?}", GWS_TIMEOUT))?
    .map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "gws is not installed. Install with: npm install -g @googleworkspace/cli"
            )
        } else {
            anyhow::anyhow!("Failed to run gws: {}", e)
        }
    })?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut error_msg = String::new();
        if !stdout.is_empty() {
            error_msg.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !error_msg.is_empty() {
                error_msg.push('\n');
            }
            error_msg.push_str(&stderr);
        }
        bail!("gws exited with {}: {}", output.status, error_msg.trim());
    }

    Ok(output)
}

async fn fetch_drive_file_metadata(file_id: &str) -> Result<DriveFileMetadata> {
    let metadata_params = json!({
        "fileId": file_id,
        "fields": "id,name,mimeType,size",
        "supportsAllDrives": true
    })
    .to_string();
    let metadata_output = run_gws(&["drive", "files", "get", "--params", &metadata_params])
        .await
        .context("Failed to fetch Drive file metadata")?;

    serde_json::from_slice(&metadata_output.stdout).context("Failed to parse Drive file metadata")
}

fn validate_drive_file(
    metadata: &DriveFileMetadata,
    mime_allowlist: &[String],
    max_bytes: u64,
) -> Result<()> {
    if !mime_allowlist.contains(&metadata.mime_type) {
        bail!(
            "Unsupported Drive file MIME type '{}'. Supported image MIME types: {}",
            metadata.mime_type,
            mime_allowlist.join(", ")
        );
    }

    if let Some(size) = parse_drive_size(metadata.size.as_ref())? {
        if size > max_bytes {
            bail!(
                "Drive file is too large: {} bytes exceeds max_bytes {}",
                size,
                max_bytes
            );
        }
    }

    Ok(())
}

async fn download_drive_file_to_path(file_id: &str, output_path: &Path) -> Result<()> {
    let output_path_str = output_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Output path is not valid UTF-8"))?;

    let media_params = json!({
        "fileId": file_id,
        "alt": "media",
        "supportsAllDrives": true
    })
    .to_string();

    run_gws(&[
        "drive",
        "files",
        "get",
        "--params",
        &media_params,
        "--output",
        output_path_str,
    ])
    .await
    .context("Failed to download Drive file contents")?;

    Ok(())
}

#[async_trait]
impl Tool for GwsTool {
    fn name(&self) -> &str {
        "gws"
    }

    fn description_for_llm(&self) -> &str {
        "Run a Google Workspace CLI (gws) command. \
         Parameters: {\"command\": \"<args after gws>\"}. \
         API parameters are passed as a JSON string via --params. \
         Examples: {\"command\": \"gmail users messages list --params '{\\\"userId\\\": \\\"me\\\", \\\"maxResults\\\": 5}'\"}, \
         {\"command\": \"drive files list --params '{\\\"q\\\": \\\"name contains report\\\"}'\"}, \
         {\"command\": \"calendar events list --params '{\\\"calendarId\\\": \\\"primary\\\"}'\"}. \
         Supports Drive, Gmail, Calendar, Sheets, Docs, Chat, Admin, and other Workspace APIs. \
         Returns JSON output. Use 'schema <method>' to inspect request/response schemas. \
         Note: destructive operations (delete, trash, send, empty, remove) are blocked. Use the send_email tool to send emails."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: command"))?;

        let args = shell_split(command)?;
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        is_allowed(&arg_refs)?;

        let mut cmd = tokio::process::Command::new("gws");
        cmd.args(&args).kill_on_drop(true);

        let output = tokio::time::timeout(GWS_TIMEOUT, cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("gws timed out after {:?}", GWS_TIMEOUT))?
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow::anyhow!(
                        "gws is not installed. Install with: npm install -g @googleworkspace/cli"
                    )
                } else {
                    anyhow::anyhow!("Failed to run gws: {}", e)
                }
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            // gws prints structured errors to stdout and info lines (like keyring backend) to stderr.
            // Include both so we always surface the real error.
            let mut error_msg = String::new();
            if !stdout.is_empty() {
                error_msg.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !error_msg.is_empty() {
                    error_msg.push('\n');
                }
                error_msg.push_str(&stderr);
            }
            bail!("gws exited with {}: {}", output.status, error_msg.trim());
        }

        let result = stdout.to_string();

        // Truncate very large output to avoid blowing up context
        const MAX_LEN: usize = 50_000;
        if result.len() > MAX_LEN {
            let mut end = MAX_LEN;
            while !result.is_char_boundary(end) {
                end -= 1;
            }
            Ok(format!(
                "{}\n\n[Truncated — output was {} bytes]",
                &result[..end],
                result.len()
            ))
        } else {
            Ok(result)
        }
    }
}

#[async_trait]
impl Tool for GwsDriveDownloadFileTool {
    fn name(&self) -> &str {
        "gws.drive.download_file"
    }

    fn description_for_llm(&self) -> &str {
        "Download a small uploaded image file from Google Drive and return its bytes as base64. \
         Prefer gws.drive.download_file_to_path for normal or high-resolution images because base64 content is injected into context. \
         Parameters: {\"file_id\": \"<Drive file ID>\", \"max_bytes\": <optional integer up to 1048576>, \
         \"mime_type_allowlist\": <optional array of supported image MIME types>}. \
         Supported MIME types: image/jpeg, image/png, image/webp, image/gif, image/bmp, image/tiff. \
         Returns JSON: {\"file_id\", \"name\", \"mime_type\", \"size_bytes\", \"content_base64\"}. \
         This tool only supports ordinary uploaded image files; Google Docs/Sheets/Slides/Drawings and PDFs are not exported."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let file_id = params
            .get("file_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: file_id"))?;
        if file_id.trim().is_empty() {
            bail!("file_id must not be empty");
        }

        let max_bytes = requested_base64_max_bytes(&params)?;
        let mime_allowlist = requested_mime_allowlist(&params)?;

        let metadata = fetch_drive_file_metadata(file_id).await?;
        validate_drive_file(&metadata, &mime_allowlist, max_bytes)?;

        let output_path = std::env::temp_dir().join(format!(
            "openferris-gws-drive-download-{}",
            uuid::Uuid::new_v4()
        ));

        let download_result = download_drive_file_to_path(file_id, &output_path).await;

        if let Err(err) = download_result {
            let _ = tokio::fs::remove_file(&output_path).await;
            return Err(err);
        }

        let bytes = match tokio::fs::read(&output_path).await {
            Ok(bytes) => bytes,
            Err(err) => {
                let _ = tokio::fs::remove_file(&output_path).await;
                return Err(err).context("Failed to read downloaded Drive file");
            }
        };
        let _ = tokio::fs::remove_file(&output_path).await;

        if bytes.len() as u64 > max_bytes {
            bail!(
                "Drive file is too large: {} bytes exceeds max_bytes {}",
                bytes.len(),
                max_bytes
            );
        }

        let content_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let result = json!({
            "file_id": metadata.id,
            "name": metadata.name,
            "mime_type": metadata.mime_type,
            "size_bytes": bytes.len(),
            "content_base64": content_base64
        });

        Ok(result.to_string())
    }
}

#[async_trait]
impl Tool for GwsDriveDownloadFileToPathTool {
    fn name(&self) -> &str {
        "gws.drive.download_file_to_path"
    }

    fn description_for_llm(&self) -> &str {
        "Download an uploaded image file from Google Drive to a local workspace path without returning file bytes in the tool result. \
         Use this for normal or high-resolution images, OCR, resizing, compression, or asking Codex to inspect/process the file. \
         Parameters: {\"file_id\": \"<Drive file ID>\", \"destination_path\": \"<workspace path>\", \
         \"max_bytes\": <optional integer up to 20971520>, \"mime_type_allowlist\": <optional array of supported image MIME types>}. \
         Supported MIME types: image/jpeg, image/png, image/webp, image/gif, image/bmp, image/tiff. \
         Returns compact JSON: {\"status\", \"file_id\", \"name\", \"mime_type\", \"size_bytes\", \"path\"}. \
         This tool only supports ordinary uploaded image files; Google Docs/Sheets/Slides/Drawings and PDFs are not exported."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let file_id = params
            .get("file_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: file_id"))?;
        if file_id.trim().is_empty() {
            bail!("file_id must not be empty");
        }

        let destination_path = params
            .get("destination_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: destination_path"))?;
        if destination_path.trim().is_empty() {
            bail!("destination_path must not be empty");
        }

        let max_bytes = requested_max_bytes(&params)?;
        let mime_allowlist = requested_mime_allowlist(&params)?;

        let metadata = fetch_drive_file_metadata(file_id).await?;
        validate_drive_file(&metadata, &mime_allowlist, max_bytes)?;

        let destination = files::validate_path(destination_path, &self.allowed_dirs)?;
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("Failed to create parent directory: {}", parent.display())
            })?;
        }

        let temp_path = destination.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));

        let download_result = download_drive_file_to_path(file_id, &temp_path).await;
        if let Err(err) = download_result {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(err);
        }

        let actual_size = match tokio::fs::metadata(&temp_path).await {
            Ok(metadata) => metadata.len(),
            Err(err) => {
                let _ = tokio::fs::remove_file(&temp_path).await;
                return Err(err).context("Failed to stat downloaded Drive file");
            }
        };

        if actual_size > max_bytes {
            let _ = tokio::fs::remove_file(&temp_path).await;
            bail!(
                "Drive file is too large: {} bytes exceeds max_bytes {}",
                actual_size,
                max_bytes
            );
        }

        if destination.exists() {
            tokio::fs::remove_file(&destination)
                .await
                .with_context(|| {
                    format!("Failed to replace existing file: {}", destination.display())
                })?;
        }
        tokio::fs::rename(&temp_path, &destination)
            .await
            .with_context(|| {
                format!(
                    "Failed to move downloaded file into place: {}",
                    destination.display()
                )
            })?;

        let result = json!({
            "status": "success",
            "file_id": metadata.id,
            "name": metadata.name,
            "mime_type": metadata.mime_type,
            "size_bytes": actual_size,
            "path": destination.display().to_string()
        });

        Ok(result.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allowed_read_commands() {
        assert!(is_allowed(&["drive", "files", "list"]).is_ok());
        assert!(is_allowed(&["gmail", "users", "messages", "get", "--id=abc"]).is_ok());
        assert!(is_allowed(&["calendar", "events", "list"]).is_ok());
        assert!(is_allowed(&["schema", "drive.files.list"]).is_ok());
    }

    #[test]
    fn test_denied_destructive() {
        assert!(is_allowed(&["drive", "files", "delete", "--file-id=abc"]).is_err());
        assert!(is_allowed(&["gmail", "users", "messages", "trash", "--id=abc"]).is_err());
        assert!(is_allowed(&["gmail", "users", "messages", "send"]).is_err());
    }

    #[test]
    fn test_denied_auth() {
        assert!(is_allowed(&["auth", "login"]).is_err());
        assert!(is_allowed(&["auth", "setup"]).is_err());
    }

    #[test]
    fn test_empty_command() {
        assert!(is_allowed(&[]).is_err());
    }

    #[test]
    fn test_shell_split_simple() {
        let args = shell_split("drive files list").unwrap();
        assert_eq!(args, vec!["drive", "files", "list"]);
    }

    #[test]
    fn test_shell_split_single_quotes() {
        let args = shell_split(
            r#"gmail users messages list --params '{"userId": "me", "maxResults": 5}'"#,
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "gmail",
                "users",
                "messages",
                "list",
                "--params",
                r#"{"userId": "me", "maxResults": 5}"#,
            ]
        );
    }

    #[test]
    fn test_shell_split_double_quotes() {
        let args = shell_split(r#"drive files list --params "some value with spaces""#).unwrap();
        assert_eq!(
            args,
            vec![
                "drive",
                "files",
                "list",
                "--params",
                "some value with spaces",
            ]
        );
    }

    #[test]
    fn test_parse_drive_size() {
        assert_eq!(
            parse_drive_size(Some(&serde_json::Value::String("123".to_string()))).unwrap(),
            Some(123)
        );
        assert_eq!(parse_drive_size(Some(&json!(456))).unwrap(), Some(456));
        assert_eq!(parse_drive_size(None).unwrap(), None);
        assert!(parse_drive_size(Some(&json!("not-a-number"))).is_err());
    }

    #[test]
    fn test_requested_max_bytes() {
        assert_eq!(requested_max_bytes(&json!({})).unwrap(), MAX_DOWNLOAD_BYTES);
        assert_eq!(
            requested_max_bytes(&json!({"max_bytes": 1024})).unwrap(),
            1024
        );
        assert!(requested_max_bytes(&json!({"max_bytes": 0})).is_err());
        assert!(requested_max_bytes(&json!({"max_bytes": MAX_DOWNLOAD_BYTES + 1})).is_err());
    }

    #[test]
    fn test_requested_base64_max_bytes() {
        assert_eq!(
            requested_base64_max_bytes(&json!({})).unwrap(),
            MAX_BASE64_BYTES
        );
        assert_eq!(
            requested_base64_max_bytes(&json!({"max_bytes": 1024})).unwrap(),
            1024
        );
        assert!(requested_base64_max_bytes(&json!({"max_bytes": MAX_BASE64_BYTES + 1})).is_err());
    }

    #[test]
    fn test_requested_mime_allowlist() {
        assert_eq!(
            requested_mime_allowlist(&json!({"mime_type_allowlist": ["image/png"]})).unwrap(),
            vec!["image/png".to_string()]
        );
        assert!(
            requested_mime_allowlist(&json!({"mime_type_allowlist": ["application/pdf"]})).is_err()
        );
    }
}
