use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use openferris::config;
use openferris::protocol::{DaemonRequest, DaemonResponse, RequestKind, ResponseKind};

/// Read the daemon-published socket pointer file, if it exists and is non-empty.
/// The daemon writes this on startup so clients with a different env-derived
/// default path (notably cron without `$XDG_RUNTIME_DIR`) can still find it.
pub fn read_socket_pointer() -> Option<String> {
    let path = config::socket_pointer_path();
    let content = std::fs::read_to_string(&path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub async fn send_skill(socket_path: &str, skill_name: &str) -> Result<String> {
    let request = DaemonRequest {
        id: uuid::Uuid::new_v4().to_string(),
        kind: RequestKind::RunSkill {
            skill_name: skill_name.to_string(),
            context: None,
        },
        source: Some("cli".to_string()),
    };
    send_request(socket_path, &request).await
}

pub async fn send_request(socket_path: &str, request: &DaemonRequest) -> Result<String> {
    let stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running? Start with: openferris daemon")?;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let mut data = serde_json::to_string(request)?;
    data.push('\n');
    writer.write_all(data.as_bytes()).await?;

    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line.is_empty() {
            anyhow::bail!("Daemon disconnected");
        }

        let response: DaemonResponse =
            serde_json::from_str(line.trim()).context("Failed to parse daemon response")?;

        match response.kind {
            ResponseKind::Done { text } => return Ok(text),
            ResponseKind::Error { message } => anyhow::bail!("{}", message),
            ResponseKind::Progress { .. } => continue,
            ResponseKind::AssistantChunk { .. } => continue,
        }
    }
}
