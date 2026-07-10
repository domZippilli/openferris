use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::config::GwsConfig;
use crate::gws_cli;

use super::{Tool, files, require_str, truncate_for_context};

/// Denied method verbs — these are destructive or send outbound messages.
const DENIED_METHODS: &[&str] = &["delete", "trash", "send", "empty", "remove"];

/// Denied top-level subcommands.
const DENIED_SUBCOMMANDS: &[&str] = &["auth"];
/// Calendar event listing/retrieval methods blocked in the generic tool —
/// they have no invitee check, so they go through the dedicated
/// invitee-scoped tools instead.
const BLOCKED_CALENDAR_EVENT_METHODS: &[&str] = &["list", "instances", "get"];
const MAX_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;
const MAX_BASE64_BYTES: u64 = 1024 * 1024;
/// Cap on any tool output returned to the model.
const MAX_OUTPUT_LEN: usize = 50_000;
/// Filtered events returned per gws.calendar.list_events call.
const CALENDAR_DEFAULT_RESULTS: u64 = 50;
const CALENDAR_MAX_RESULTS: u64 = 250;
/// Raw pages fetched per list call (250 events each) before reporting
/// `incomplete` — bounds both subprocess spawns and worst-case latency.
const CALENDAR_MAX_PAGES: usize = 4;
/// Attendees included per event in tool output; the rest are counted in
/// `attendees_omitted` so all-hands events don't eat the context budget.
const CALENDAR_MAX_ATTENDEES: usize = 30;
/// Cap on a single event description in gws.calendar.get_event output.
const CALENDAR_MAX_DESCRIPTION_BYTES: usize = 10_000;
const SUPPORTED_IMAGE_MIME_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/gif",
    "image/bmp",
    "image/tiff",
];

pub struct GwsTool {
    config: GwsConfig,
}
pub struct GwsCalendarListEventsTool;
pub struct GwsCalendarGetEventTool;
pub struct GwsDriveDownloadFileTool;
pub struct GwsDriveDownloadFileToPathTool {
    allowed_dirs: Vec<PathBuf>,
}

impl GwsDriveDownloadFileToPathTool {
    pub fn new(allowed_dirs: Vec<PathBuf>) -> Self {
        Self { allowed_dirs }
    }
}

impl GwsTool {
    pub fn new(config: GwsConfig) -> Self {
        Self { config }
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
    let chars = input.chars();
    let mut in_single = false;
    let mut in_double = false;

    for c in chars {
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

fn is_drive_file_delete(args: &[&str], method: &str) -> bool {
    args.len() >= 3
        && args[0].eq_ignore_ascii_case("drive")
        && args[1].eq_ignore_ascii_case("files")
        && args[2].eq_ignore_ascii_case(method)
}

fn is_blocked_calendar_events_read(args: &[&str]) -> bool {
    args.len() >= 3
        && args[0].eq_ignore_ascii_case("calendar")
        && args[1].eq_ignore_ascii_case("events")
        && BLOCKED_CALENDAR_EVENT_METHODS
            .iter()
            .any(|m| args[2].eq_ignore_ascii_case(m))
}

fn is_allowed(args: &[&str], config: &GwsConfig) -> Result<()> {
    if args.is_empty() {
        bail!("No command provided");
    }

    let first = args[0].to_lowercase();
    if DENIED_SUBCOMMANDS.contains(&first.as_str()) {
        bail!("The '{}' subcommand is not allowed", first);
    }

    if is_blocked_calendar_events_read(args) {
        bail!(
            "'calendar events list/instances/get' are blocked through the generic gws tool for privacy. \
             Use gws.calendar.list_events to list a person's events and gws.calendar.get_event for \
             single-event details — both require an 'invitee' email and return only that person's events."
        );
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
            if config.allow_drive_file_deletes
                && (lower == "delete" || lower == "trash")
                && is_drive_file_delete(args, &lower)
            {
                continue;
            }
            bail!(
                "The '{}' method is not allowed — destructive and outbound operations are blocked unless explicitly configured",
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

/// Whether `invitee` is entitled to see an event on `calendar_id`, and with
/// what response status. Pure so the privacy rule is directly unit-testable.
#[derive(Debug, PartialEq)]
enum InviteeStatus {
    NotInvited,
    Invited { response_status: Option<String> },
}

fn invitee_status(event: &serde_json::Value, invitee: &str, calendar_id: &str) -> InviteeStatus {
    match event.get("attendees").and_then(|a| a.as_array()) {
        Some(attendees) if !attendees.is_empty() => {
            for attendee in attendees {
                if attendee
                    .get("email")
                    .and_then(|e| e.as_str())
                    .is_some_and(|email| email.eq_ignore_ascii_case(invitee))
                {
                    return InviteeStatus::Invited {
                        response_status: attendee
                            .get("responseStatus")
                            .and_then(|r| r.as_str())
                            .map(str::to_string),
                    };
                }
            }
            InviteeStatus::NotInvited
        }
        // No guest list: a solo/personal event. Visible only to its owner —
        // the organizer or the calendar being queried.
        _ => {
            let organizer_matches = event
                .pointer("/organizer/email")
                .and_then(|e| e.as_str())
                .is_some_and(|email| email.eq_ignore_ascii_case(invitee));
            if organizer_matches || calendar_id.eq_ignore_ascii_case(invitee) {
                InviteeStatus::Invited {
                    response_status: None,
                }
            } else {
                InviteeStatus::NotInvited
            }
        }
    }
}

/// Project an event down to the fields a briefing needs, dropping bulky
/// noise (description, htmlLink, etag, reminders, ...). `invitee_response`
/// is the matched attendee's responseStatus; `None` means the invitee
/// matched via the organizer fallback.
fn project_event(event: &serde_json::Value, invitee_response: Option<&str>) -> serde_json::Value {
    let mut projected = serde_json::Map::new();

    for key in [
        "id",
        "summary",
        "start",
        "end",
        "location",
        "status",
        "hangoutLink",
    ] {
        if let Some(value) = event.get(key)
            && !value.is_null()
        {
            projected.insert(key.to_string(), value.clone());
        }
    }

    if let Some(organizer) = event.get("organizer") {
        let mut trimmed = serde_json::Map::new();
        for key in ["email", "displayName"] {
            if let Some(value) = organizer.get(key) {
                trimmed.insert(key.to_string(), value.clone());
            }
        }
        if !trimmed.is_empty() {
            projected.insert("organizer".to_string(), serde_json::Value::Object(trimmed));
        }
    }

    if let Some(attendees) = event.get("attendees").and_then(|a| a.as_array()) {
        let kept: Vec<serde_json::Value> = attendees
            .iter()
            .take(CALENDAR_MAX_ATTENDEES)
            .map(|attendee| {
                let mut trimmed = serde_json::Map::new();
                for key in ["email", "displayName", "responseStatus"] {
                    if let Some(value) = attendee.get(key) {
                        trimmed.insert(key.to_string(), value.clone());
                    }
                }
                serde_json::Value::Object(trimmed)
            })
            .collect();
        if attendees.len() > CALENDAR_MAX_ATTENDEES {
            projected.insert(
                "attendees_omitted".to_string(),
                json!(attendees.len() - CALENDAR_MAX_ATTENDEES),
            );
        }
        projected.insert("attendees".to_string(), serde_json::Value::Array(kept));
    }

    projected.insert(
        "invitee_response_status".to_string(),
        json!(invitee_response.unwrap_or("organizer")),
    );

    serde_json::Value::Object(projected)
}

/// Extract one page of a Calendar events-list response.
fn take_page(response: &serde_json::Value) -> (Vec<serde_json::Value>, Option<String>) {
    let items = response
        .get("items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();
    let next_page_token = response
        .get("nextPageToken")
        .and_then(|t| t.as_str())
        .map(str::to_string);
    (items, next_page_token)
}

fn require_nonempty_str(params: &serde_json::Value, key: &str) -> Result<String> {
    let value = require_str(params, key)?.trim().to_string();
    if value.is_empty() {
        bail!("{} must not be empty", key);
    }
    Ok(value)
}

fn require_invitee(params: &serde_json::Value) -> Result<String> {
    let invitee = require_nonempty_str(params, "invitee")?;
    if !invitee.contains('@') {
        bail!("invitee must be an email address, got '{}'", invitee);
    }
    Ok(invitee)
}

fn optional_rfc3339(params: &serde_json::Value, key: &str) -> Result<Option<String>> {
    match params.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => {
            let s = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{} must be an RFC3339 timestamp string", key))?;
            chrono::DateTime::parse_from_rfc3339(s).map_err(|e| {
                anyhow::anyhow!(
                    "{} must be a valid RFC3339 timestamp (e.g. 2026-07-10T00:00:00Z): {}",
                    key,
                    e
                )
            })?;
            Ok(Some(s.to_string()))
        }
    }
}

fn requested_max_events(params: &serde_json::Value) -> Result<u64> {
    match params.get("max_results") {
        None | Some(serde_json::Value::Null) => Ok(CALENDAR_DEFAULT_RESULTS),
        Some(value) => {
            let requested = value
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("max_results must be a positive integer"))?;
            if requested == 0 {
                bail!("max_results must be greater than zero");
            }
            if requested > CALENDAR_MAX_RESULTS {
                bail!("max_results may not exceed {}", CALENDAR_MAX_RESULTS);
            }
            Ok(requested)
        }
    }
}

async fn fetch_drive_file_metadata(file_id: &str) -> Result<DriveFileMetadata> {
    let metadata_params = json!({
        "fileId": file_id,
        "fields": "id,name,mimeType,size",
        "supportsAllDrives": true
    })
    .to_string();
    let metadata_output =
        gws_cli::run_gws(&["drive", "files", "get", "--params", &metadata_params])
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

    if let Some(size) = parse_drive_size(metadata.size.as_ref())?
        && size > max_bytes
    {
        bail!(
            "Drive file is too large: {} bytes exceeds max_bytes {}",
            size,
            max_bytes
        );
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

    gws_cli::run_gws(&[
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
         {\"command\": \"calendar calendars get --params '{\\\"calendarId\\\": \\\"primary\\\"}'\"}. \
         Supports Drive, Gmail, Calendar, Sheets, Docs, Chat, Admin, and other Workspace APIs. \
         Returns JSON output. Use 'schema <method>' to inspect request/response schemas. \
         Note: destructive operations (delete, trash, send, empty, remove) are blocked by default. \
         If [gws].allow_drive_file_deletes is true, Drive file delete/trash commands are allowed. \
         Use the send_email tool to send emails. \
         Calendar event listing and retrieval ('calendar events list/instances/get') are blocked \
         here for privacy — use the gws.calendar.list_events and gws.calendar.get_event tools. \
         Other calendar reads (calendars list/get, freebusy query) are allowed."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let command = require_str(&params, "command")?;

        let args = shell_split(command)?;
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        is_allowed(&arg_refs, &self.config)?;

        // gws prints structured errors to stdout and info lines (like keyring
        // backend) to stderr; gws_cli::run_gws includes both in its error
        // message so we always surface the real error.
        let output = gws_cli::run_gws(&arg_refs).await?;
        let result = String::from_utf8_lossy(&output.stdout).to_string();

        // Truncate very large output to avoid blowing up context
        Ok(truncate_for_context(result, MAX_OUTPUT_LEN, "output"))
    }
}

#[async_trait]
impl Tool for GwsCalendarListEventsTool {
    fn name(&self) -> &str {
        "gws.calendar.list_events"
    }

    fn description_for_llm(&self) -> &str {
        "List Google Calendar events that a specific person is invited to. This is the ONLY way \
         to list calendar events; invitee filtering is enforced in code for privacy. \
         Parameters: {\"calendar_id\": \"<calendar to query>\", \
         \"invitee\": \"<email — only events where this person is an attendee (or the organizer, \
         for events with no guest list) are returned>\", \
         \"time_min\": <optional RFC3339 timestamp>, \"time_max\": <optional RFC3339 timestamp>, \
         \"max_results\": <optional, default 50, max 250>}. \
         Recurring events are expanded to instances, ordered by start time. \
         Each event carries invitee_response_status (accepted/declined/needsAction/tentative, or \
         'organizer' for guest-list-free events) — declined events are included, so filter on it \
         when composing schedules. \
         Returns JSON: {\"calendar_id\", \"invitee\", \"count\", \"events\": [...]}, plus \
         \"incomplete\": true when more events exist than were returned. \
         Use gws.calendar.get_event for full details (e.g. description) of a single event."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let calendar_id = require_nonempty_str(&params, "calendar_id")?;
        let invitee = require_invitee(&params)?;
        let time_min = optional_rfc3339(&params, "time_min")?;
        let time_max = optional_rfc3339(&params, "time_max")?;
        let max_results = requested_max_events(&params)?;

        let mut events: Vec<serde_json::Value> = Vec::new();
        let mut page_token: Option<String> = None;
        let mut incomplete = false;
        let mut pages = 0;

        loop {
            pages += 1;
            let mut api_params = json!({
                "calendarId": calendar_id,
                "singleEvents": true,
                "orderBy": "startTime",
                "maxResults": CALENDAR_MAX_RESULTS,
            });
            if let Some(ref time_min) = time_min {
                api_params["timeMin"] = json!(time_min);
            }
            if let Some(ref time_max) = time_max {
                api_params["timeMax"] = json!(time_max);
            }
            if let Some(ref token) = page_token {
                api_params["pageToken"] = json!(token);
            }
            let params_str = api_params.to_string();

            let output = gws_cli::run_gws(&["calendar", "events", "list", "--params", &params_str])
                .await
                .context("Failed to list calendar events")?;
            let response: serde_json::Value = serde_json::from_slice(&output.stdout)
                .context("Failed to parse calendar events response")?;

            let (items, next_page_token) = take_page(&response);
            for event in &items {
                if let InviteeStatus::Invited { response_status } =
                    invitee_status(event, &invitee, &calendar_id)
                {
                    if events.len() as u64 >= max_results {
                        incomplete = true;
                        break;
                    }
                    events.push(project_event(event, response_status.as_deref()));
                }
            }
            if incomplete {
                break;
            }

            match next_page_token {
                Some(token) => page_token = Some(token),
                None => break,
            }
            if pages >= CALENDAR_MAX_PAGES {
                incomplete = true;
                break;
            }
        }

        let mut result = json!({
            "calendar_id": calendar_id,
            "invitee": invitee,
            "count": events.len(),
            "events": events,
        });
        if incomplete {
            result["incomplete"] = json!(true);
            result["note"] =
                json!("More events may exist; narrow time_min/time_max or raise max_results.");
        }

        Ok(truncate_for_context(
            result.to_string(),
            MAX_OUTPUT_LEN,
            "output",
        ))
    }
}

#[async_trait]
impl Tool for GwsCalendarGetEventTool {
    fn name(&self) -> &str {
        "gws.calendar.get_event"
    }

    fn description_for_llm(&self) -> &str {
        "Fetch full details of a single Google Calendar event, including its description. This is \
         the ONLY way to retrieve a calendar event; it is returned only if the invitee is an \
         attendee or organizer of the event. \
         Parameters: {\"calendar_id\": \"<calendar the event lives on>\", \
         \"event_id\": \"<event id, e.g. from gws.calendar.list_events>\", \
         \"invitee\": \"<email of the person the details are for>\"}. \
         Returns JSON with id, summary, start, end, location, status, organizer, attendees, \
         hangoutLink, description, and invitee_response_status."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let calendar_id = require_nonempty_str(&params, "calendar_id")?;
        let invitee = require_invitee(&params)?;
        let event_id = require_nonempty_str(&params, "event_id")?;

        let api_params = json!({
            "calendarId": calendar_id,
            "eventId": event_id,
        })
        .to_string();
        let output = gws_cli::run_gws(&["calendar", "events", "get", "--params", &api_params])
            .await
            .context("Failed to fetch calendar event")?;
        let event: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("Failed to parse calendar event response")?;

        match invitee_status(&event, &invitee, &calendar_id) {
            // Deliberately content-free: must not confirm anything about the
            // event beyond the id the caller already had.
            InviteeStatus::NotInvited => bail!(
                "Access denied: {} is not an attendee or organizer of event {}",
                invitee,
                event_id
            ),
            InviteeStatus::Invited { response_status } => {
                let mut projected = project_event(&event, response_status.as_deref());
                if let Some(description) = event.get("description").and_then(|d| d.as_str()) {
                    projected["description"] = json!(truncate_for_context(
                        description.to_string(),
                        CALENDAR_MAX_DESCRIPTION_BYTES,
                        "description"
                    ));
                }
                Ok(truncate_for_context(
                    projected.to_string(),
                    MAX_OUTPUT_LEN,
                    "output",
                ))
            }
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
        let file_id = require_str(&params, "file_id")?;
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
        let file_id = require_str(&params, "file_id")?;
        if file_id.trim().is_empty() {
            bail!("file_id must not be empty");
        }

        let destination_path = require_str(&params, "destination_path")?;
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
        let config = GwsConfig::default();
        assert!(is_allowed(&["drive", "files", "list"], &config).is_ok());
        assert!(is_allowed(&["gmail", "users", "messages", "get", "--id=abc"], &config).is_ok());
        assert!(is_allowed(&["calendar", "calendars", "list"], &config).is_ok());
        assert!(is_allowed(&["calendar", "freebusy", "query"], &config).is_ok());
        assert!(is_allowed(&["schema", "drive.files.list"], &config).is_ok());
    }

    #[test]
    fn test_denied_calendar_event_reads() {
        let config = GwsConfig::default();
        for method in ["list", "instances", "get"] {
            let err =
                is_allowed(&["calendar", "events", method, "--params", "{}"], &config).unwrap_err();
            assert!(
                err.to_string().contains("gws.calendar.list_events"),
                "error should point at the dedicated tools: {}",
                err
            );
        }
        // Case-insensitive, like the rest of the denylist.
        assert!(is_allowed(&["Calendar", "Events", "List"], &config).is_err());
    }

    #[test]
    fn test_denied_destructive_by_default() {
        let config = GwsConfig::default();
        assert!(is_allowed(&["drive", "files", "delete", "--file-id=abc"], &config).is_err());
        assert!(
            is_allowed(
                &["gmail", "users", "messages", "trash", "--id=abc"],
                &config
            )
            .is_err()
        );
        assert!(is_allowed(&["gmail", "users", "messages", "send"], &config).is_err());
    }

    #[test]
    fn test_allows_configured_drive_file_deletes_only() {
        let config = GwsConfig {
            allow_drive_file_deletes: true,
        };
        assert!(is_allowed(&["drive", "files", "delete", "--file-id=abc"], &config).is_ok());
        assert!(is_allowed(&["drive", "files", "trash", "--file-id=abc"], &config).is_ok());
        assert!(
            is_allowed(
                &["gmail", "users", "messages", "trash", "--id=abc"],
                &config
            )
            .is_err()
        );
        assert!(is_allowed(&["gmail", "users", "messages", "send"], &config).is_err());
        assert!(is_allowed(&["drive", "comments", "delete", "--file-id=abc"], &config).is_err());
    }

    #[test]
    fn test_denied_auth() {
        let config = GwsConfig::default();
        assert!(is_allowed(&["auth", "login"], &config).is_err());
        assert!(is_allowed(&["auth", "setup"], &config).is_err());
    }

    #[test]
    fn test_empty_command() {
        assert!(is_allowed(&[], &GwsConfig::default()).is_err());
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

    fn invited(status: Option<&str>) -> InviteeStatus {
        InviteeStatus::Invited {
            response_status: status.map(str::to_string),
        }
    }

    #[test]
    fn test_invitee_status_attendee_match() {
        let event = json!({
            "attendees": [
                {"email": "dom@zippilli.xyz", "responseStatus": "accepted"},
                {"email": "colleen@zippilli.xyz", "responseStatus": "needsAction"},
            ]
        });
        assert_eq!(
            invitee_status(&event, "dom@zippilli.xyz", "primary"),
            invited(Some("accepted"))
        );
        assert_eq!(
            invitee_status(&event, "colleen@zippilli.xyz", "primary"),
            invited(Some("needsAction"))
        );
    }

    #[test]
    fn test_invitee_status_case_insensitive() {
        let event =
            json!({"attendees": [{"email": "Dom@Zippilli.XYZ", "responseStatus": "accepted"}]});
        assert_eq!(
            invitee_status(&event, "dom@zippilli.xyz", "primary"),
            invited(Some("accepted"))
        );
        let event = json!({"attendees": [{"email": "dom@zippilli.xyz"}]});
        assert_eq!(
            invitee_status(&event, "DOM@ZIPPILLI.XYZ", "primary"),
            invited(None)
        );
    }

    #[test]
    fn test_invitee_status_not_invited() {
        // The core privacy assertion: attendees present, invitee not among them.
        let event = json!({
            "attendees": [{"email": "dom@zippilli.xyz", "responseStatus": "accepted"}],
            "organizer": {"email": "dom@zippilli.xyz"},
        });
        assert_eq!(
            invitee_status(&event, "colleen@zippilli.xyz", "dom@zippilli.xyz"),
            InviteeStatus::NotInvited
        );
    }

    #[test]
    fn test_invitee_status_organizer_not_in_attendee_list_is_fail_closed() {
        // "Booked on behalf of" shape: guest list exists but omits the
        // organizer. Fallback only applies to attendee-less events.
        let event = json!({
            "attendees": [{"email": "someone@example.com"}],
            "organizer": {"email": "dom@zippilli.xyz"},
        });
        assert_eq!(
            invitee_status(&event, "dom@zippilli.xyz", "primary"),
            InviteeStatus::NotInvited
        );
    }

    #[test]
    fn test_invitee_status_solo_event_organizer_fallback() {
        let event = json!({"organizer": {"email": "dom@zippilli.xyz"}, "summary": "Dentist"});
        assert_eq!(
            invitee_status(&event, "dom@zippilli.xyz", "primary"),
            invited(None)
        );
        assert_eq!(
            invitee_status(&event, "colleen@zippilli.xyz", "primary"),
            InviteeStatus::NotInvited
        );
        // Empty attendees array behaves like no attendees.
        let event = json!({"attendees": [], "organizer": {"email": "dom@zippilli.xyz"}});
        assert_eq!(
            invitee_status(&event, "dom@zippilli.xyz", "primary"),
            invited(None)
        );
    }

    #[test]
    fn test_invitee_status_solo_event_calendar_id_fallback() {
        // Solo event with no organizer field at all: the calendar owner sees it.
        let event = json!({"summary": "Dentist"});
        assert_eq!(
            invitee_status(&event, "dom@zippilli.xyz", "Dom@Zippilli.xyz"),
            invited(None)
        );
        assert_eq!(
            invitee_status(&event, "colleen@zippilli.xyz", "dom@zippilli.xyz"),
            InviteeStatus::NotInvited
        );
    }

    #[test]
    fn test_invitee_status_declined_is_included() {
        let event =
            json!({"attendees": [{"email": "dom@zippilli.xyz", "responseStatus": "declined"}]});
        assert_eq!(
            invitee_status(&event, "dom@zippilli.xyz", "primary"),
            invited(Some("declined"))
        );
    }

    #[test]
    fn test_project_event_keeps_and_drops_fields() {
        let event = json!({
            "id": "evt1",
            "summary": "Standup",
            "start": {"dateTime": "2026-07-10T09:00:00-04:00", "timeZone": "America/New_York"},
            "end": {"dateTime": "2026-07-10T09:15:00-04:00"},
            "location": "Meet",
            "status": "confirmed",
            "hangoutLink": "https://meet.google.com/abc",
            "organizer": {"email": "dom@zippilli.xyz", "displayName": "Dom", "self": true},
            "attendees": [{"email": "dom@zippilli.xyz", "responseStatus": "accepted", "organizer": true}],
            "description": "a very long agenda",
            "htmlLink": "https://calendar.google.com/...",
            "etag": "\"123\"",
            "iCalUID": "evt1@google.com",
            "reminders": {"useDefault": true},
        });
        let projected = project_event(&event, Some("accepted"));

        assert_eq!(projected["id"], "evt1");
        assert_eq!(projected["summary"], "Standup");
        assert_eq!(projected["start"]["timeZone"], "America/New_York");
        assert_eq!(projected["location"], "Meet");
        assert_eq!(projected["hangoutLink"], "https://meet.google.com/abc");
        assert_eq!(projected["organizer"]["email"], "dom@zippilli.xyz");
        assert!(projected["organizer"].get("self").is_none());
        assert_eq!(projected["attendees"][0]["responseStatus"], "accepted");
        assert!(projected["attendees"][0].get("organizer").is_none());
        assert_eq!(projected["invitee_response_status"], "accepted");
        for dropped in ["description", "htmlLink", "etag", "iCalUID", "reminders"] {
            assert!(
                projected.get(dropped).is_none(),
                "{} should be dropped",
                dropped
            );
        }
    }

    #[test]
    fn test_project_event_organizer_fallback_status() {
        let event = json!({"id": "evt1", "organizer": {"email": "dom@zippilli.xyz"}});
        let projected = project_event(&event, None);
        assert_eq!(projected["invitee_response_status"], "organizer");
    }

    #[test]
    fn test_project_event_caps_attendees() {
        let attendees: Vec<serde_json::Value> = (0..40)
            .map(|i| json!({"email": format!("person{}@example.com", i)}))
            .collect();
        let event = json!({"id": "evt1", "attendees": attendees});
        let projected = project_event(&event, Some("accepted"));
        assert_eq!(projected["attendees"].as_array().unwrap().len(), 30);
        assert_eq!(projected["attendees_omitted"], 10);
    }

    #[test]
    fn test_take_page() {
        let response = json!({"items": [{"id": "a"}, {"id": "b"}], "nextPageToken": "tok"});
        let (items, next) = take_page(&response);
        assert_eq!(items.len(), 2);
        assert_eq!(next.as_deref(), Some("tok"));

        let (items, next) = take_page(&json!({}));
        assert!(items.is_empty());
        assert!(next.is_none());
    }

    #[test]
    fn test_require_invitee() {
        assert_eq!(
            require_invitee(&json!({"invitee": " dom@zippilli.xyz "})).unwrap(),
            "dom@zippilli.xyz"
        );
        assert!(require_invitee(&json!({})).is_err());
        assert!(require_invitee(&json!({"invitee": ""})).is_err());
        assert!(require_invitee(&json!({"invitee": "not-an-email"})).is_err());
    }

    #[test]
    fn test_require_nonempty_str() {
        assert!(require_nonempty_str(&json!({"calendar_id": "  "}), "calendar_id").is_err());
        assert!(require_nonempty_str(&json!({}), "calendar_id").is_err());
    }

    #[test]
    fn test_optional_rfc3339() {
        assert_eq!(optional_rfc3339(&json!({}), "time_min").unwrap(), None);
        assert_eq!(
            optional_rfc3339(&json!({"time_min": "2026-07-10T00:00:00Z"}), "time_min").unwrap(),
            Some("2026-07-10T00:00:00Z".to_string())
        );
        assert!(optional_rfc3339(&json!({"time_min": "tomorrow"}), "time_min").is_err());
        assert!(optional_rfc3339(&json!({"time_min": "2026-07-10"}), "time_min").is_err());
    }

    #[test]
    fn test_requested_max_events() {
        assert_eq!(
            requested_max_events(&json!({})).unwrap(),
            CALENDAR_DEFAULT_RESULTS
        );
        assert_eq!(
            requested_max_events(&json!({"max_results": 10})).unwrap(),
            10
        );
        assert!(requested_max_events(&json!({"max_results": 0})).is_err());
        assert!(requested_max_events(&json!({"max_results": 251})).is_err());
    }

    #[tokio::test]
    async fn test_get_event_param_validation() {
        let tool = GwsCalendarGetEventTool;
        // Missing event_id fails before any subprocess is spawned.
        let err = tool
            .execute(json!({"calendar_id": "primary", "invitee": "dom@zippilli.xyz"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("event_id"));

        let err = tool
            .execute(json!({"calendar_id": "primary", "invitee": "nope", "event_id": "e1"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("email address"));
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
