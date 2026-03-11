use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::protocol::{DaemonRequest, DaemonResponse, RequestKind, ResponseKind};

pub async fn send_skill(address: &str, skill_name: &str) -> Result<String> {
    let request = DaemonRequest {
        id: uuid::Uuid::new_v4().to_string(),
        kind: RequestKind::RunSkill {
            skill_name: skill_name.to_string(),
        },
        source: Some("cli".to_string()),
    };
    send_request(address, &request).await
}

pub async fn send_request(address: &str, request: &DaemonRequest) -> Result<String> {
    let stream = TcpStream::connect(address)
        .await
        .context("Failed to connect to daemon. Is it running? Start with: openferris daemon")?;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let mut data = serde_json::to_string(request)?;
    data.push('\n');
    writer.write_all(data.as_bytes()).await?;

    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let response: DaemonResponse =
        serde_json::from_str(line.trim()).context("Failed to parse daemon response")?;

    match response.kind {
        ResponseKind::Done { text } => Ok(text),
        ResponseKind::Error { message } => anyhow::bail!("{}", message),
    }
}
