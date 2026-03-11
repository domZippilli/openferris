use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

pub struct Storage {
    conn: Connection,
}

pub fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    let mut end = max_len;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS interactions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL DEFAULT (datetime('now', 'localtime')),
                source TEXT NOT NULL DEFAULT 'unknown',
                skill TEXT,
                user_message TEXT NOT NULL,
                agent_response TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_interactions_ts ON interactions(timestamp);",
        )?;
        Ok(Self { conn })
    }

    fn now_local() -> String {
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
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
            rusqlite::params![Self::now_local(), source, skill, user_message, agent_response],
        )?;
        Ok(())
    }

    /// Build a context string from recent interactions for the system prompt.
    pub fn build_context(&self) -> Result<String> {
        let mut ctx = String::new();

        // Load recent interactions (last 20, reversed to chronological)
        let mut stmt = self.conn.prepare(
            "SELECT timestamp, source, skill, user_message, agent_response
             FROM interactions ORDER BY timestamp DESC LIMIT 20",
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
                ctx.push_str(&format!(
                    "User: {}\nYou: {}\n\n",
                    truncate(user_msg, 300),
                    truncate(agent_msg, 500)
                ));
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
}
