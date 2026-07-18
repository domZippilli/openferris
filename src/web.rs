use std::convert::Infallible;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use futures_util::stream;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use tokio::time::{Duration, Instant, interval};

use openferris::protocol::{DaemonRequest, RequestKind, parse_goal_args};

#[derive(Clone)]
struct WebState {
    daemon_socket: String,
    agent_name: String,
}

#[derive(Deserialize)]
struct ChatRequest {
    message: String,
}

pub async fn run(daemon_socket: String, listen: &str, agent_name: &str) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("Failed to bind web chat to {listen}"))?;
    let app = Router::new()
        .route("/", get(index))
        .route("/app.css", get(css))
        .route("/app.js", get(js))
        .route("/api/health", get(health))
        .route("/api/chat", post(chat))
        .with_state(WebState {
            daemon_socket,
            agent_name: agent_name.to_string(),
        });

    tracing::info!("OpenFerris web chat listening on http://{listen}");
    axum::serve(listener, app)
        .await
        .context("Web server failed")
}

async fn index(State(state): State<WebState>) -> Html<String> {
    Html(include_str!("web/index.html").replace("{{AGENT_NAME}}", &html_escape(&state.agent_name)))
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

async fn css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("web/app.css"),
    )
}

async fn js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("web/app.js"),
    )
}

async fn health(State(state): State<WebState>) -> StatusCode {
    match UnixStream::connect(&state.daemon_socket).await {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

async fn chat(State(state): State<WebState>, Json(input): Json<ChatRequest>) -> Response {
    let message = input.message.trim();
    if message.is_empty() {
        return (StatusCode::BAD_REQUEST, "Message cannot be empty").into_response();
    }

    let kind = if let Some(fact) = message.strip_prefix("/remember ") {
        RequestKind::StoreMemory {
            content: fact.trim().to_string(),
        }
    } else if let Some(args) = message.strip_prefix("/goal ") {
        match parse_goal_args(args) {
            Ok((max_turns, exit_criteria)) => RequestKind::PursueGoal {
                exit_criteria,
                max_turns,
            },
            Err(error) => return (StatusCode::BAD_REQUEST, error).into_response(),
        }
    } else {
        RequestKind::FreeformMessage {
            text: message.to_string(),
        }
    };

    let request = DaemonRequest {
        id: uuid::Uuid::new_v4().to_string(),
        kind,
        source: Some("web".to_string()),
        session_id: Some("web:owner".to_string()),
    };

    let stream = match UnixStream::connect(&state.daemon_socket).await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!("Web chat could not connect to daemon: {error}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Assistant daemon is unavailable",
            )
                .into_response();
        }
    };
    let (reader, mut writer) = stream.into_split();
    let mut encoded = match serde_json::to_vec(&request) {
        Ok(encoded) => encoded,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    encoded.push(b'\n');
    if let Err(error) = writer.write_all(&encoded).await {
        return (StatusCode::BAD_GATEWAY, error.to_string()).into_response();
    }

    let mut heartbeat = interval(Duration::from_secs(5));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let stream_state = (
        BufReader::new(reader),
        heartbeat,
        Instant::now(),
        request.id,
    );
    let lines = stream::unfold(
        stream_state,
        |(mut reader, mut heartbeat, started, request_id)| async move {
            let mut line = String::new();
            tokio::select! {
                result = reader.read_line(&mut line) => match result {
                    Ok(0) => None,
                    Ok(_) => Some((
                        Ok::<Bytes, Infallible>(Bytes::from(line)),
                        (reader, heartbeat, started, request_id),
                    )),
                    Err(error) => {
                        tracing::warn!("Daemon stream ended with an error: {error}");
                        None
                    }
                },
                _ = heartbeat.tick() => {
                    let seconds = started.elapsed().as_secs();
                    let heartbeat_line = serde_json::json!({
                        "request_id": request_id,
                        "kind": { "Progress": { "text": format!("Thinking… {seconds}s") } }
                    }).to_string() + "\n";
                    Some((
                        Ok::<Bytes, Infallible>(Bytes::from(heartbeat_line)),
                        (reader, heartbeat, started, request_id),
                    ))
                }
            }
        },
    );

    Response::builder()
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .body(Body::from_stream(lines))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_name_for_html_attribute() {
        assert_eq!(html_escape("A&B\""), "A&amp;B&quot;");
    }
}
