use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

use crate::llm::{ChatMessage, Role};
use crate::text::truncate_bytes;

pub struct Storage {
    conn: Connection,
}

/// `direction` value for a message the user (or an external sender) sent
/// *to* the agent.
pub const DIRECTION_INBOUND: &str = "inbound";
/// `direction` value for a message the agent sent *to* a counterparty.
pub const DIRECTION_OUTBOUND: &str = "outbound";
/// `kind` for a normal thread turn (rendered as history for the LLM).
pub const KIND_CHAT: &str = "chat";
/// `kind` for a scheduled/skill run's final response — kept for audit but
/// excluded from thread rendering, since the run's user-visible output (if
/// any) already reached the thread via a delivery tool's own `KIND_CHAT`
/// append. Storing both as `chat` would duplicate it.
pub const KIND_RUN_NOTE: &str = "run_note";

/// `wakeups.status` for a wakeup that hasn't fired yet.
pub const WAKEUP_PENDING: &str = "pending";
/// `wakeups.status` for a wakeup the daemon tick has already fired.
pub const WAKEUP_FIRED: &str = "fired";

/// Current local time formatted `YYYY-MM-DD HH:MM:SS`, matching this file's
/// `messages`/`interactions` timestamp convention (system-local clock, i.e.
/// `chrono::Local`, not the user's *configured* `[user] timezone` — those
/// coincide for the typical single-user self-hosted deployment this app
/// targets, and every other stored timestamp already assumes system-local).
/// `wakeups.due_ts` is stored in this same format for exactly that reason:
/// it's compared against this function's output with plain string `<=`, which
/// only gives correct chronological ordering because both sides share the
/// same zero-padded `YYYY-MM-DD HH:MM:SS` shape. Callers that need to store a
/// timestamp a caller supplied in a *different* timezone (see
/// `tools/wakeup.rs`) must convert to system-local first.
pub fn now_local() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Build the `[sent via <channel>, <timestamp>] <text>` tag prefixed onto
/// outbound thread turns logged by delivery tools (including legacy Telegram
/// records and current Gmail auto-replies), so the thread makes clear these lines were an actual
/// delivered message rather than an ordinary assistant reply.
pub fn outbound_tag(channel: &str, text: &str) -> String {
    format!("[sent via {}, {}] {}", channel, now_local(), text)
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // WAL + busy_timeout so the daemon worker and CLI fallback path can
        // write concurrently without SQLITE_BUSY.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS interactions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL DEFAULT (datetime('now', 'localtime')),
                source TEXT NOT NULL DEFAULT 'unknown',
                skill TEXT,
                user_message TEXT NOT NULL,
                agent_response TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_interactions_ts ON interactions(timestamp);

            CREATE TABLE IF NOT EXISTS known_contacts (
                email TEXT PRIMARY KEY,
                first_contacted TEXT NOT NULL DEFAULT (datetime('now', 'localtime'))
            );

            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts TEXT NOT NULL DEFAULT (datetime('now', 'localtime')),
                counterparty TEXT NOT NULL,
                channel TEXT NOT NULL,
                direction TEXT NOT NULL,
                kind TEXT NOT NULL DEFAULT 'chat',
                content TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_counterparty_ts ON messages(counterparty, ts, id);

            CREATE TABLE IF NOT EXISTS wakeups (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created TEXT NOT NULL DEFAULT (datetime('now', 'localtime')),
                due_ts TEXT NOT NULL,
                note TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending'
            );
            CREATE INDEX IF NOT EXISTS idx_wakeups_status_due ON wakeups(status, due_ts);",
        )?;
        Ok(Self { conn })
    }

    pub fn log_interaction(
        &self,
        source: &str,
        skill: Option<&str>,
        user_message: &str,
        agent_response: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO interactions (timestamp, source, skill, user_message, agent_response) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![now_local(), source, skill, user_message, agent_response],
        )?;
        Ok(())
    }

    /// Build a context string from recent interactions for the system prompt.
    pub fn build_context(&self) -> Result<String> {
        let mut ctx = String::new();

        // Load recent interactions (last 20, reversed to chronological)
        let mut stmt = self.conn.prepare(
            "SELECT timestamp, source, skill, user_message, agent_response
             FROM interactions ORDER BY timestamp DESC, id DESC LIMIT 20",
        )?;
        let mut interactions: Vec<(String, String, Option<String>, String, String)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        interactions.reverse();

        if !interactions.is_empty() {
            ctx.push_str("# Recent Interactions\n\n");
            for (ts, source, skill, user_msg, agent_msg) in &interactions {
                let label = match skill {
                    Some(s) => format!("{}/{}", source, s),
                    None => source.clone(),
                };
                ctx.push_str(&format!("[{} via {}]\n", ts, label));
                if source == "telegram" && skill.as_deref() == Some("send_telegram") {
                    ctx.push_str(&format!(
                        "Outbound Telegram message: {}\n\n",
                        truncate_bytes(agent_msg, 2000)
                    ));
                } else {
                    ctx.push_str(&format!(
                        "User: {}\nYou: {}\n\n",
                        truncate_bytes(user_msg, 700),
                        truncate_bytes(agent_msg, 2000)
                    ));
                }
            }
        }

        Ok(ctx)
    }

    /// Count interactions within a time window.
    /// `since` is a SQLite datetime string (local time), or None for all.
    pub fn count_interactions(&self, since: Option<&str>) -> Result<usize> {
        let count: usize = match since {
            Some(ts) => self.conn.query_row(
                "SELECT COUNT(*) FROM interactions WHERE timestamp >= ?1",
                [ts],
                |row| row.get(0),
            )?,
            None => self
                .conn
                .query_row("SELECT COUNT(*) FROM interactions", [], |row| row.get(0))?,
        };
        Ok(count)
    }

    /// Check if an email address is a known contact.
    pub fn is_contact(&self, email: &str) -> Result<bool> {
        let count: usize = self.conn.query_row(
            "SELECT COUNT(*) FROM known_contacts WHERE email = ?1",
            [email.to_lowercase()],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Record an email address as a known contact.
    pub fn add_contact(&self, email: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO known_contacts (email, first_contacted) VALUES (?1, ?2)",
            rusqlite::params![email.to_lowercase(), now_local()],
        )?;
        Ok(())
    }

    /// Delete interactions within a time window.
    /// `since` is a SQLite datetime string (local time), or None for all.
    pub fn delete_interactions(&self, since: Option<&str>) -> Result<usize> {
        let deleted = match since {
            Some(ts) => self
                .conn
                .execute("DELETE FROM interactions WHERE timestamp >= ?1", [ts])?,
            None => self.conn.execute("DELETE FROM interactions", [])?,
        };
        Ok(deleted)
    }

    /// Append a user-visible message to a counterparty's thread. `direction`
    /// should be [`DIRECTION_INBOUND`] or [`DIRECTION_OUTBOUND`]; `kind`
    /// should be [`KIND_CHAT`] (rendered as history) or [`KIND_RUN_NOTE`]
    /// (audit-only, excluded from [`Storage::load_thread`]).
    ///
    /// Never call this for tool calls/results or intermediate agent
    /// iterations — only for inbound texts, outbound sends, and final
    /// responses (see the architecture doc for the coherence design).
    pub fn append_message(
        &self,
        counterparty: &str,
        channel: &str,
        direction: &str,
        kind: &str,
        content: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO messages (ts, counterparty, channel, direction, kind, content) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![now_local(), counterparty, channel, direction, kind, content],
        )?;
        Ok(())
    }

    /// Load a counterparty's thread as `ChatMessage`s for the agent, oldest
    /// first, capped to `max_chars_budget` (a byte/char budget, not tokens —
    /// callers size this from the model's context window the same way
    /// `agent::estimate_tokens` reasons about it). Only `kind = "chat"` rows
    /// are returned; `run_note` rows are audit-only and never rendered here.
    ///
    /// Walks newest-first and stops once the accumulated budget would be
    /// exceeded, but always keeps at least the single most recent message
    /// even if it alone is over budget — mirroring the "always keep the most
    /// recent pair" rule the in-run compactor uses, so a follow-up always has
    /// its immediate antecedent.
    ///
    /// Adjacent messages with the same resolved role (e.g. two outbound sends
    /// with no reply in between) are merged into one `ChatMessage`, since
    /// consecutive same-role turns are otherwise just noise in the transcript
    /// (the OpenAI-compat text prompt tolerates them either way).
    pub fn load_thread(
        &self,
        counterparty: &str,
        max_chars_budget: usize,
    ) -> Result<Vec<ChatMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT direction, content FROM messages
             WHERE counterparty = ?1 AND kind = ?2
             ORDER BY ts DESC, id DESC",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map(rusqlite::params![counterparty, KIND_CHAT], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut selected: Vec<(String, String)> = Vec::new();
        let mut used = 0usize;
        for (direction, content) in rows {
            if !selected.is_empty() && used + content.len() > max_chars_budget {
                break;
            }
            used += content.len();
            selected.push((direction, content));
        }
        selected.reverse(); // now oldest-first

        let mut merged: Vec<ChatMessage> = Vec::new();
        for (direction, content) in selected {
            let role = if direction == DIRECTION_OUTBOUND {
                Role::Assistant
            } else {
                Role::User
            };
            match merged.last_mut() {
                Some(last) if last.role == role => {
                    last.content.push_str("\n\n");
                    last.content.push_str(&content);
                }
                _ => merged.push(ChatMessage { role, content }),
            }
        }

        Ok(merged)
    }

    /// Record a one-shot wakeup. `due_ts` must already be in this file's
    /// system-local `YYYY-MM-DD HH:MM:SS` convention (see [`now_local`]) —
    /// callers accepting a user-facing due time in some other timezone must
    /// convert it first. Returns the new row's id, so the caller (the
    /// `set_wakeup` tool) can report it back for later `cancel`.
    pub fn add_wakeup(&self, due_ts: &str, note: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO wakeups (created, due_ts, note, status) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![now_local(), due_ts, note, WAKEUP_PENDING],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Pending wakeups due at or before `now` (a `YYYY-MM-DD HH:MM:SS` string
    /// in the same convention as [`now_local`]), oldest-due first. This is
    /// what the daemon's tick task polls.
    pub fn due_wakeups(&self, now: &str) -> Result<Vec<(i64, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, due_ts, note FROM wakeups
             WHERE status = ?1 AND due_ts <= ?2
             ORDER BY due_ts ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![WAKEUP_PENDING, now], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark a wakeup as fired. At-most-once: the daemon tick calls this
    /// *before* enqueuing the wakeup's run, so a crash between the mark and
    /// the run landing only ever loses a wakeup — it can never double-fire
    /// one on restart, since a restarted tick would see `status = 'fired'`
    /// and skip it.
    pub fn mark_wakeup_fired(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE wakeups SET status = ?1 WHERE id = ?2",
            rusqlite::params![WAKEUP_FIRED, id],
        )?;
        Ok(())
    }

    /// All pending (not yet fired or cancelled) wakeups, oldest-due first —
    /// for the `set_wakeup` tool's `list` action.
    pub fn pending_wakeups(&self) -> Result<Vec<(i64, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, due_ts, note FROM wakeups
             WHERE status = ?1
             ORDER BY due_ts ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![WAKEUP_PENDING], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Cancel a pending wakeup. Returns `true` if a pending row with this id
    /// existed and was removed, `false` if it didn't exist, already fired, or
    /// was already cancelled — so the tool can report which happened.
    pub fn cancel_wakeup(&self, id: i64) -> Result<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM wakeups WHERE id = ?1 AND status = ?2",
            rusqlite::params![id, WAKEUP_PENDING],
        )?;
        Ok(deleted > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_storage() -> Storage {
        Storage::open(&PathBuf::from(":memory:")).unwrap()
    }

    #[test]
    fn test_interaction_roundtrip() {
        let s = temp_storage();
        s.log_interaction("tui", None, "Hello", "Hi there!")
            .unwrap();
        s.log_interaction("telegram", None, "What's up?", "Not much!")
            .unwrap();

        let ctx = s.build_context().unwrap();
        assert!(ctx.contains("Hello"));
        assert!(ctx.contains("Hi there!"));
        assert!(ctx.contains("tui"));
        assert!(ctx.contains("telegram"));
    }

    #[test]
    fn test_empty_context() {
        let s = temp_storage();
        let ctx = s.build_context().unwrap();
        assert!(ctx.is_empty());
    }

    #[test]
    fn test_outbound_telegram_message_is_rendered_as_context() {
        let s = temp_storage();
        s.log_interaction(
            "telegram",
            Some("send_telegram"),
            "Outbound Telegram message sent to chat 123",
            "Daily briefing:\n1. Finish the report\n2. Confirm the schedule",
        )
        .unwrap();

        let ctx = s.build_context().unwrap();
        assert!(ctx.contains("telegram/send_telegram"));
        assert!(ctx.contains("Outbound Telegram message: Daily briefing"));
        assert!(ctx.contains("Finish the report"));
        assert!(!ctx.contains("User: Outbound Telegram message sent"));
    }

    // Regression for P2-B: two Storage handles on the same file (simulating
    // daemon + gmail-listener processes) must be able to write concurrently
    // without SQLITE_BUSY. Requires WAL + busy_timeout in Storage::open.
    #[test]
    fn test_concurrent_writers_do_not_busy_out() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("concurrent.db");

        // Initial open creates the schema and enables WAL on the file.
        let _seed = Storage::open(&db_path).unwrap();

        const THREADS: usize = 4;
        const WRITES_PER_THREAD: usize = 50;

        let mut handles = Vec::new();
        for t in 0..THREADS {
            let path = db_path.clone();
            handles.push(std::thread::spawn(move || {
                let store = Storage::open(&path).unwrap();
                for i in 0..WRITES_PER_THREAD {
                    store
                        .log_interaction(
                            "test",
                            Some("concurrent"),
                            &format!("thread {} msg {}", t, i),
                            "ok",
                        )
                        .expect("write should not SQLITE_BUSY under WAL");
                    store
                        .add_contact(&format!("t{}_{}@example.com", t, i))
                        .expect("add_contact should not SQLITE_BUSY");
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let reader = Storage::open(&db_path).unwrap();
        let total: usize = reader
            .conn
            .query_row("SELECT COUNT(*) FROM interactions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, THREADS * WRITES_PER_THREAD);

        let contacts: usize = reader
            .conn
            .query_row("SELECT COUNT(*) FROM known_contacts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(contacts, THREADS * WRITES_PER_THREAD);
    }

    #[test]
    fn test_append_and_load_thread_roundtrip() {
        let s = temp_storage();
        s.append_message("owner", "tui", DIRECTION_INBOUND, KIND_CHAT, "Hi there")
            .unwrap();
        s.append_message(
            "owner",
            "tui",
            DIRECTION_OUTBOUND,
            KIND_CHAT,
            "Hello! How can I help?",
        )
        .unwrap();

        let thread = s.load_thread("owner", 100_000).unwrap();
        assert_eq!(thread.len(), 2);
        assert_eq!(thread[0].role, Role::User);
        assert_eq!(thread[0].content, "Hi there");
        assert_eq!(thread[1].role, Role::Assistant);
        assert_eq!(thread[1].content, "Hello! How can I help?");
    }

    #[test]
    fn test_load_thread_is_scoped_to_counterparty() {
        let s = temp_storage();
        s.append_message("owner", "tui", DIRECTION_INBOUND, KIND_CHAT, "For owner")
            .unwrap();
        s.append_message(
            "email:stranger@example.com",
            "email",
            DIRECTION_INBOUND,
            KIND_CHAT,
            "For stranger",
        )
        .unwrap();

        let owner_thread = s.load_thread("owner", 100_000).unwrap();
        assert_eq!(owner_thread.len(), 1);
        assert_eq!(owner_thread[0].content, "For owner");
    }

    #[test]
    fn test_load_thread_excludes_run_notes() {
        let s = temp_storage();
        s.append_message("owner", "tui", DIRECTION_INBOUND, KIND_CHAT, "Chat turn")
            .unwrap();
        s.append_message(
            "owner",
            "daemon",
            DIRECTION_OUTBOUND,
            KIND_RUN_NOTE,
            "Final response of a scheduled skill run",
        )
        .unwrap();

        let thread = s.load_thread("owner", 100_000).unwrap();
        assert_eq!(thread.len(), 1);
        assert_eq!(thread[0].content, "Chat turn");
    }

    #[test]
    fn test_load_thread_merges_adjacent_same_role() {
        let s = temp_storage();
        s.append_message("owner", "tui", DIRECTION_INBOUND, KIND_CHAT, "Question")
            .unwrap();
        // Two legacy outbound sends with no reply in between
        // calls in the same run, or a run's send followed by a later notify).
        s.append_message(
            "owner",
            "telegram",
            DIRECTION_OUTBOUND,
            KIND_CHAT,
            "First send",
        )
        .unwrap();
        s.append_message(
            "owner",
            "telegram",
            DIRECTION_OUTBOUND,
            KIND_CHAT,
            "Second send",
        )
        .unwrap();

        let thread = s.load_thread("owner", 100_000).unwrap();
        assert_eq!(
            thread.len(),
            2,
            "the two outbound turns should merge into one"
        );
        assert_eq!(thread[0].role, Role::User);
        assert_eq!(thread[1].role, Role::Assistant);
        assert!(thread[1].content.contains("First send"));
        assert!(thread[1].content.contains("Second send"));
    }

    #[test]
    fn test_load_thread_trims_to_budget_but_keeps_newest() {
        let s = temp_storage();
        for i in 0..10 {
            s.append_message(
                "owner",
                "tui",
                if i % 2 == 0 {
                    DIRECTION_INBOUND
                } else {
                    DIRECTION_OUTBOUND
                },
                KIND_CHAT,
                &format!("turn {} {}", i, "x".repeat(50)),
            )
            .unwrap();
        }

        // Budget only large enough for ~2 messages.
        let thread = s.load_thread("owner", 130).unwrap();
        assert!(
            thread.len() < 10,
            "expected older turns to be trimmed, got {} messages",
            thread.len()
        );
        // The single newest turn is always kept even alone.
        let newest = thread.last().unwrap();
        assert!(newest.content.contains("turn 9"));
    }

    #[test]
    fn test_load_thread_keeps_single_oversized_newest_message() {
        let s = temp_storage();
        s.append_message(
            "owner",
            "tui",
            DIRECTION_INBOUND,
            KIND_CHAT,
            &"x".repeat(1000),
        )
        .unwrap();

        // Budget smaller than the one message that exists.
        let thread = s.load_thread("owner", 10).unwrap();
        assert_eq!(
            thread.len(),
            1,
            "the most recent message must survive even if it alone exceeds budget"
        );
    }

    #[test]
    fn test_outbound_tag_format() {
        let tag = outbound_tag("telegram", "hello");
        assert!(tag.starts_with("[sent via telegram, "));
        assert!(tag.ends_with("] hello"));
    }

    #[test]
    fn test_add_and_list_pending_wakeup() {
        let s = temp_storage();
        let id = s
            .add_wakeup("2026-07-09 09:00:00", "Check if they replied")
            .unwrap();
        assert!(id > 0);

        let pending = s.pending_wakeups().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, id);
        assert_eq!(pending[0].1, "2026-07-09 09:00:00");
        assert_eq!(pending[0].2, "Check if they replied");
    }

    #[test]
    fn test_due_wakeups_only_returns_due_and_pending() {
        let s = temp_storage();
        let past = s.add_wakeup("2020-01-01 00:00:00", "past, due").unwrap();
        let future = s
            .add_wakeup("2099-01-01 00:00:00", "future, not due")
            .unwrap();

        let due = s.due_wakeups("2026-07-08 12:00:00").unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, past);
        assert!(due.iter().all(|(id, _, _)| *id != future));
    }

    #[test]
    fn test_due_wakeups_boundary_is_inclusive() {
        let s = temp_storage();
        let id = s
            .add_wakeup("2026-07-08 09:00:00", "right on time")
            .unwrap();
        let due = s.due_wakeups("2026-07-08 09:00:00").unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, id);
    }

    #[test]
    fn test_mark_wakeup_fired_excludes_it_from_due_and_pending() {
        let s = temp_storage();
        let id = s.add_wakeup("2020-01-01 00:00:00", "fire me").unwrap();

        s.mark_wakeup_fired(id).unwrap();

        assert!(s.due_wakeups("2026-07-08 00:00:00").unwrap().is_empty());
        assert!(s.pending_wakeups().unwrap().is_empty());
    }

    #[test]
    fn test_cancel_pending_wakeup_removes_it() {
        let s = temp_storage();
        let id = s.add_wakeup("2099-01-01 00:00:00", "cancel me").unwrap();

        assert!(s.cancel_wakeup(id).unwrap());
        assert!(s.pending_wakeups().unwrap().is_empty());
    }

    #[test]
    fn test_cancel_nonexistent_or_fired_wakeup_returns_false() {
        let s = temp_storage();
        assert!(!s.cancel_wakeup(9999).unwrap());

        let id = s
            .add_wakeup("2020-01-01 00:00:00", "already fired")
            .unwrap();
        s.mark_wakeup_fired(id).unwrap();
        assert!(!s.cancel_wakeup(id).unwrap());
    }

    #[test]
    fn test_due_wakeups_ordered_oldest_due_first() {
        let s = temp_storage();
        let later = s.add_wakeup("2020-06-01 00:00:00", "later").unwrap();
        let earlier = s.add_wakeup("2020-01-01 00:00:00", "earlier").unwrap();

        let due = s.due_wakeups("2026-01-01 00:00:00").unwrap();
        assert_eq!(due.len(), 2);
        assert_eq!(due[0].0, earlier);
        assert_eq!(due[1].0, later);
    }
}
