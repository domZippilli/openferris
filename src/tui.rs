use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixStream;

use openferris::protocol::{DaemonRequest, DaemonResponse, RequestKind, ResponseKind};

pub async fn run(socket_path: &str) -> Result<()> {
    let stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running? Start with: openferris daemon")?;

    let (tcp_reader, tcp_writer) = stream.into_split();
    let mut tcp_reader = BufReader::new(tcp_reader);
    let mut tcp_writer = BufWriter::new(tcp_writer);

    let stdin = tokio::io::stdin();
    let mut stdin_reader = BufReader::new(stdin);

    println!("OpenFerris TUI — connected to daemon at {}", socket_path);
    println!("Type your message and press Enter. Ctrl+C to quit.");
    println!("  /remember <fact> — save a memory directly");
    println!("  /goal [--max-turns N] <exit criteria> — pursue a bounded goal\n");

    // One conversation per TUI process: all turns share this key so the daemon
    // threads them together (history now lives in the daemon, not the socket).
    let session_id = format!("tui:{}", uuid::Uuid::new_v4());

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

        let request = if let Some(fact) = input.strip_prefix("/remember ") {
            let fact = fact.trim();
            if fact.is_empty() {
                eprintln!("Usage: /remember <fact to remember>");
                continue;
            }
            DaemonRequest {
                id: uuid::Uuid::new_v4().to_string(),
                kind: RequestKind::StoreMemory {
                    content: fact.to_string(),
                },
                source: Some("tui".to_string()),
                session_id: None,
            }
        } else if let Some(args) = input.strip_prefix("/goal ") {
            let (max_turns, exit_criteria) = match parse_goal_args(args) {
                Ok(parsed) => parsed,
                Err(e) => {
                    eprintln!("{}", e);
                    continue;
                }
            };
            DaemonRequest {
                id: uuid::Uuid::new_v4().to_string(),
                kind: RequestKind::PursueGoal {
                    exit_criteria,
                    max_turns,
                },
                source: Some("tui".to_string()),
                session_id: None,
            }
        } else {
            DaemonRequest {
                id: uuid::Uuid::new_v4().to_string(),
                kind: RequestKind::FreeformMessage {
                    text: input.to_string(),
                },
                source: Some("tui".to_string()),
                session_id: Some(session_id.clone()),
            }
        };

        let mut data = serde_json::to_string(&request)?;
        data.push('\n');
        tcp_writer.write_all(data.as_bytes()).await?;
        tcp_writer.flush().await?;

        let mut rendered_assistant_chunks = false;
        loop {
            let mut response_line = String::new();
            tcp_reader.read_line(&mut response_line).await?;

            if response_line.is_empty() {
                eprintln!("Daemon disconnected.");
                return Ok(());
            }

            let response: DaemonResponse = serde_json::from_str(response_line.trim())
                .context("Failed to parse daemon response")?;

            match response.kind {
                ResponseKind::Done { text } => {
                    eprint!("\r\x1b[K");
                    if rendered_assistant_chunks {
                        println!("\n");
                    } else {
                        println!("\n{}\n", text);
                    }
                    break;
                }
                ResponseKind::Error { message } => {
                    eprint!("\r\x1b[K");
                    eprintln!("\nError: {}\n", message);
                    break;
                }
                ResponseKind::Progress { text } => {
                    eprint!("\r\x1b[K{}", text);
                }
                ResponseKind::AssistantChunk { text } => {
                    // Phase 1C/D: render incrementally. For now, append to
                    // stdout so it's visible if Phase 1A/C ship before TUI
                    // gets a proper renderer.
                    rendered_assistant_chunks = true;
                    print!("{}", text);
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }

    Ok(())
}

fn parse_goal_args(input: &str) -> Result<(usize, String), String> {
    let mut parts = input.split_whitespace().peekable();
    let mut max_turns = 5usize;
    let mut criteria = Vec::new();

    while let Some(part) = parts.next() {
        if part == "--max-turns" {
            let Some(raw) = parts.next() else {
                return Err("Usage: /goal [--max-turns N] <exit criteria>".to_string());
            };
            max_turns = raw
                .parse::<usize>()
                .map_err(|_| "max turns must be a positive integer".to_string())?;
        } else if let Some(raw) = part.strip_prefix("--max-turns=") {
            max_turns = raw
                .parse::<usize>()
                .map_err(|_| "max turns must be a positive integer".to_string())?;
        } else {
            criteria.push(part);
            criteria.extend(parts);
            break;
        }
    }

    let exit_criteria = criteria.join(" ").trim().to_string();
    if exit_criteria.is_empty() {
        return Err("Usage: /goal [--max-turns N] <exit criteria>".to_string());
    }

    Ok((max_turns, exit_criteria))
}
