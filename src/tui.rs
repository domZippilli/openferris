use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;

use crate::protocol::{DaemonRequest, DaemonResponse, RequestKind, ResponseKind};

pub async fn run(address: &str) -> Result<()> {
    let stream = TcpStream::connect(address)
        .await
        .context("Failed to connect to daemon. Is it running? Start with: openferris daemon")?;

    let (tcp_reader, tcp_writer) = stream.into_split();
    let mut tcp_reader = BufReader::new(tcp_reader);
    let mut tcp_writer = BufWriter::new(tcp_writer);

    let stdin = tokio::io::stdin();
    let mut stdin_reader = BufReader::new(stdin);

    println!("OpenFerris TUI — connected to daemon at {}", address);
    println!("Type your message and press Enter. Ctrl+C to quit.\n");

    loop {
        eprint!("> ");

        let mut input = String::new();
        match stdin_reader.read_line(&mut input).await {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                eprintln!("Input error: {}", e);
                break;
            }
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        let request = DaemonRequest {
            id: uuid::Uuid::new_v4().to_string(),
            kind: RequestKind::FreeformMessage {
                text: input.to_string(),
            },
            source: Some("tui".to_string()),
        };

        let mut data = serde_json::to_string(&request)?;
        data.push('\n');
        tcp_writer.write_all(data.as_bytes()).await?;
        tcp_writer.flush().await?;

        let mut response_line = String::new();
        tcp_reader.read_line(&mut response_line).await?;

        if response_line.is_empty() {
            eprintln!("Daemon disconnected.");
            break;
        }

        let response: DaemonResponse = serde_json::from_str(response_line.trim())
            .context("Failed to parse daemon response")?;

        match response.kind {
            ResponseKind::Done { text } => {
                println!("\n{}\n", text);
            }
            ResponseKind::Error { message } => {
                eprintln!("\nError: {}\n", message);
            }
        }
    }

    Ok(())
}
