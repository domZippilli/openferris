use anyhow::Result;
use async_trait::async_trait;
use chrono::TimeZone;
use chrono_tz::Tz;
use std::path::PathBuf;

use super::Tool;
use crate::storage::Storage;

/// `set_wakeup`: a one-shot, precise-time deferred-action primitive (refactor
/// plan 1.3). The goal-runner heartbeat (1.2) already covers coarse
/// every-couple-hours resumption for goal files; this covers "remind me at
/// 9", "check tomorrow morning whether they replied", and any other
/// non-goal follow-up that needs a specific clock time rather than a
/// recurring cron cadence (that's what `schedule` is for).
pub struct SetWakeupTool {
    db_path: PathBuf,
    /// The user's configured `[user] timezone` (see `tools/datetime.rs`),
    /// used to interpret the `due` parameter the same way the agent already
    /// reports "now" to the LLM.
    timezone: String,
}

impl SetWakeupTool {
    pub fn new(db_path: PathBuf, timezone: String) -> Self {
        Self { db_path, timezone }
    }

    fn tz(&self) -> Tz {
        self.timezone.parse().unwrap_or(chrono_tz::UTC)
    }
}

#[async_trait]
impl Tool for SetWakeupTool {
    fn name(&self) -> &str {
        "set_wakeup"
    }

    fn description_for_llm(&self) -> &str {
        "Schedule a one-shot wakeup: at the given time (fires within about a minute of `due`), \
         a fresh agent run starts with your note as its only instruction. That run has no \
         memory of this conversation or any other context — the note must be fully \
         self-contained (what to do, why, any file paths, who to notify, etc). \
         Use this for a single precise-time follow-up (\"remind me at 9\", \"check tomorrow \
         morning whether they replied\") — do NOT use it for recurring schedules (use `schedule` \
         instead) or for per-goal pacing inside goal-pursuit/goal-runner (use the goal file's \
         `next_check` instead). If you tell the owner you'll do something later, either do it \
         now or call this — a promise with no wakeup and no goal file is a bug. \
         Parameters: \
         {\"action\": \"add\", \"due\": \"YYYY-MM-DD HH:MM\", \"note\": \"<self-contained instruction for the future run>\"} \
         to schedule one (`due` is interpreted in the user's configured timezone and must be in \
         the future; returns the new wakeup's id); \
         {\"action\": \"list\"} to see all pending wakeups with their ids, due times, and notes; \
         {\"action\": \"cancel\", \"id\": <number>} to cancel a pending wakeup by id."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: action"))?;

        let storage = Storage::open(&self.db_path)?;

        match action {
            "add" => self.add(&storage, &params),
            "list" => Self::list(&storage),
            "cancel" => Self::cancel(&storage, &params),
            other => anyhow::bail!("Unknown action '{}'. Use: add, list, or cancel", other),
        }
    }
}

impl SetWakeupTool {
    fn add(&self, storage: &Storage, params: &serde_json::Value) -> Result<String> {
        let due = params
            .get("due")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: due"))?;
        let note = params
            .get("note")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: note"))?;

        if note.trim().is_empty() {
            anyhow::bail!(
                "note must not be empty — it is the only context the fired run will have"
            );
        }

        let due_ts = normalize_due(due, &self.tz())?;

        let id = storage.add_wakeup(&due_ts, note)?;
        Ok(format!(
            "Wakeup #{} set for {} ({}).",
            id, due, self.timezone
        ))
    }

    fn list(storage: &Storage) -> Result<String> {
        let pending = storage.pending_wakeups()?;
        if pending.is_empty() {
            return Ok("No pending wakeups.".to_string());
        }
        let mut out = String::new();
        for (id, due_ts, note) in pending {
            out.push_str(&format!("#{} due {}: {}\n", id, due_ts, note));
        }
        Ok(out.trim_end().to_string())
    }

    fn cancel(storage: &Storage, params: &serde_json::Value) -> Result<String> {
        let id = params
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: id"))?;

        if storage.cancel_wakeup(id)? {
            Ok(format!("Wakeup #{} cancelled.", id))
        } else {
            Ok(format!(
                "No pending wakeup with id #{} (already fired, cancelled, or never existed).",
                id
            ))
        }
    }
}

/// Parse `due` (`YYYY-MM-DD HH:MM`, interpreted in `tz`), validate it's in
/// the future, and convert it to this codebase's system-local storage
/// convention (`storage::now_local`'s `YYYY-MM-DD HH:MM:SS` format) so the
/// daemon's tick — which compares against `chrono::Local::now()` — can find
/// it with a plain string comparison. See `storage::now_local`'s doc comment
/// for why storage always uses system-local rather than the configured tz.
fn normalize_due(due: &str, tz: &Tz) -> Result<String> {
    let naive = chrono::NaiveDateTime::parse_from_str(due, "%Y-%m-%d %H:%M").map_err(|_| {
        anyhow::anyhow!(
            "Invalid due '{}': expected format YYYY-MM-DD HH:MM (e.g. \"2026-07-09 09:00\")",
            due
        )
    })?;

    let due_in_tz = tz.from_local_datetime(&naive).single().ok_or_else(|| {
        anyhow::anyhow!(
            "'{}' is ambiguous or invalid in timezone {} (e.g. a DST transition)",
            due,
            tz
        )
    })?;

    let now_in_tz = chrono::Utc::now().with_timezone(tz);
    if due_in_tz <= now_in_tz {
        anyhow::bail!(
            "due '{}' must be in the future (current time in {}: {})",
            due,
            tz,
            now_in_tz.format("%Y-%m-%d %H:%M")
        );
    }

    let due_local = due_in_tz.with_timezone(&chrono::Local);
    Ok(due_local.format("%Y-%m-%d %H:%M:%S").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> SetWakeupTool {
        SetWakeupTool::new(std::path::PathBuf::from(":memory:"), "UTC".to_string())
    }

    #[tokio::test]
    async fn test_add_missing_due_errors() {
        let t = tool();
        let err = t
            .execute(serde_json::json!({"action": "add", "note": "hi"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("due"));
    }

    #[tokio::test]
    async fn test_add_missing_note_errors() {
        let t = tool();
        let err = t
            .execute(serde_json::json!({"action": "add", "due": "2099-01-01 09:00"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("note"));
    }

    #[tokio::test]
    async fn test_add_empty_note_errors() {
        let t = tool();
        let err = t
            .execute(serde_json::json!({
                "action": "add",
                "due": "2099-01-01 09:00",
                "note": "   "
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn test_add_malformed_due_errors() {
        let t = tool();
        let err = t
            .execute(serde_json::json!({
                "action": "add",
                "due": "tomorrow at 9",
                "note": "check in"
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid due"));
    }

    #[tokio::test]
    async fn test_add_past_due_errors() {
        let t = tool();
        let err = t
            .execute(serde_json::json!({
                "action": "add",
                "due": "2020-01-01 09:00",
                "note": "check in"
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("future"));
    }

    #[tokio::test]
    async fn test_add_future_due_succeeds() {
        let t = tool();
        let result = t
            .execute(serde_json::json!({
                "action": "add",
                "due": "2099-01-01 09:00",
                "note": "check in"
            }))
            .await
            .unwrap();
        assert!(result.starts_with("Wakeup #"));
        assert!(result.contains("2099-01-01 09:00"));
    }

    #[tokio::test]
    async fn test_missing_action_errors() {
        let t = tool();
        let err = t.execute(serde_json::json!({})).await.unwrap_err();
        assert!(err.to_string().contains("action"));
    }

    #[tokio::test]
    async fn test_unknown_action_errors() {
        let t = tool();
        let err = t
            .execute(serde_json::json!({"action": "snooze"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_cancel_missing_id_errors() {
        let t = tool();
        let err = t
            .execute(serde_json::json!({"action": "cancel"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("id"));
    }

    #[test]
    fn test_normalize_due_converts_to_system_local_format() {
        let tz: Tz = "UTC".parse().unwrap();
        let due = normalize_due("2099-06-15 09:30", &tz).unwrap();
        // Must match storage::now_local's YYYY-MM-DD HH:MM:SS shape so
        // string comparison against it in the daemon tick works.
        assert_eq!(due.len(), "2099-06-15 09:30:00".len());
        assert!(due.starts_with("2099-"));
        assert!(due.ends_with(":00"));
    }

    #[test]
    fn test_normalize_due_rejects_past() {
        let tz: Tz = "UTC".parse().unwrap();
        assert!(normalize_due("2020-01-01 00:00", &tz).is_err());
    }

    #[test]
    fn test_normalize_due_rejects_bad_format() {
        let tz: Tz = "UTC".parse().unwrap();
        assert!(normalize_due("not-a-date", &tz).is_err());
    }
}
