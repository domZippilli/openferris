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
                timestamp TEXT NOT NULL DEFAULT (datetime('now')),
                source TEXT NOT NULL DEFAULT 'unknown',
                skill TEXT,
                user_message TEXT NOT NULL,
                agent_response TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS memories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL DEFAULT (datetime('now')),
                content TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_interactions_ts ON interactions(timestamp);
            CREATE INDEX IF NOT EXISTS idx_memories_ts ON memories(timestamp);",
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
            "INSERT INTO interactions (source, skill, user_message, agent_response) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![source, skill, user_message, agent_response],
        )?;
        Ok(())
    }

    pub fn store_memory(&self, content: &str) -> Result<()> {
        self.conn
            .execute("INSERT INTO memories (content) VALUES (?1)", [content])?;
        Ok(())
    }

    /// Build a context string from memories and recent interactions for the system prompt.
    pub fn build_context(&self) -> Result<String> {
        let mut ctx = String::new();

        // Load all memories
        let mut stmt = self
            .conn
            .prepare("SELECT timestamp, content FROM memories ORDER BY timestamp")?;
        let memories: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        if !memories.is_empty() {
            ctx.push_str("# Things I Remember\n\n");
            for (ts, content) in &memories {
                let date = truncate(ts, 10);
                ctx.push_str(&format!("- {} ({})\n", content, date));
            }
            ctx.push('\n');
        }

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_storage() -> Storage {
        Storage::open(&PathBuf::from(":memory:")).unwrap()
    }

    #[test]
    fn test_memory_roundtrip() {
        let s = temp_storage();
        s.store_memory("Safety word is banana").unwrap();
        s.store_memory("User prefers dark mode").unwrap();

        let ctx = s.build_context().unwrap();
        assert!(ctx.contains("Safety word is banana"));
        assert!(ctx.contains("User prefers dark mode"));
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
