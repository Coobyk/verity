//! SQLite brain: bot records, per-bot memory, misc kv.
//! Conversation content deliberately lives in Telegram, not here.

use sqlite::{Connection, State};
use std::path::Path;

use crate::Result;

#[derive(Debug, Clone)]
pub struct Bot {
    pub name: String,
    pub token: String,
    pub username: Option<String>,
    /// Forum topic this bot owns; created lazily.
    pub topic_id: Option<i64>,
    pub instructions: String,
    /// Harness spec, e.g. "pi" or "acp:claude".
    pub provider: String,
    pub model: Option<String>,
    /// Posted into the bot's topic right after it's created.
    pub greeting: Option<String>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        crate::config::chmod_private(path); // bot tokens live here
        conn.execute(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS bots (
                 name         TEXT PRIMARY KEY,
                 token        TEXT NOT NULL,
                 username     TEXT,
                 topic_id     INTEGER,
                 instructions TEXT NOT NULL DEFAULT '',
                 provider     TEXT NOT NULL DEFAULT 'pi',
                 model        TEXT,
                 greeting     TEXT,
                 created_at   INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE TABLE IF NOT EXISTS memory (
                 bot     TEXT NOT NULL REFERENCES bots(name),
                 key     TEXT NOT NULL,
                 value   TEXT NOT NULL,
                 updated INTEGER NOT NULL DEFAULT (unixepoch()),
                 PRIMARY KEY (bot, key)
             );
             CREATE TABLE IF NOT EXISTS kv (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        // migration for pre-greeting databases
        let _ = conn.execute("ALTER TABLE bots ADD COLUMN greeting TEXT");
        Ok(Self { conn })
    }

    pub fn add_bot(&self, bot: &Bot) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO bots (name, token, username, topic_id, instructions, provider, model, greeting)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )?;
        stmt.bind((1, bot.name.as_str()))?;
        stmt.bind((2, bot.token.as_str()))?;
        stmt.bind((3, bot.username.as_deref()))?;
        stmt.bind((4, bot.topic_id))?;
        stmt.bind((5, bot.instructions.as_str()))?;
        stmt.bind((6, bot.provider.as_str()))?;
        stmt.bind((7, bot.model.as_deref()))?;
        stmt.bind((8, bot.greeting.as_deref()))?;
        stmt.next()?;
        Ok(())
    }

    /// Rename a bot record and move its memory rows along.
    pub fn rename_bot(&self, name: &str, new_name: &str) -> Result<bool> {
        let mut s = self
            .conn
            .prepare("UPDATE bots SET name = ? WHERE name = ?")?;
        s.bind((1, new_name))?;
        s.bind((2, name))?;
        s.next()?;
        let changed = self.conn.change_count() > 0;
        if changed {
            let mut s = self
                .conn
                .prepare("UPDATE memory SET bot = ? WHERE bot = ?")?;
            s.bind((1, new_name))?;
            s.bind((2, name))?;
            s.next()?;
        }
        Ok(changed)
    }

    pub fn remove_bot(&self, name: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare("DELETE FROM bots WHERE name = ?")?;
        stmt.bind((1, name))?;
        stmt.next()?;
        Ok(self.conn.change_count() > 0)
    }

    pub fn list_bots(&self) -> Result<Vec<Bot>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, token, username, topic_id, instructions, provider, model, greeting
             FROM bots ORDER BY name",
        )?;
        let mut out = Vec::new();
        while let State::Row = stmt.next()? {
            out.push(read_bot(&stmt)?);
        }
        Ok(out)
    }

    /// The lead bot — first one registered. Singleton jobs like the
    /// periodic backup go through it so they always come from the same
    /// sender (not whichever name sorts first).
    pub fn primary_bot(&self) -> Result<Option<Bot>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, token, username, topic_id, instructions, provider, model, greeting
             FROM bots ORDER BY rowid LIMIT 1",
        )?;
        match stmt.next()? {
            State::Row => Ok(Some(read_bot(&stmt)?)),
            State::Done => Ok(None),
        }
    }

    /// Forget everything except registrations — all memory and the
    /// transient kv flags. `/reset` leaves a truly clean brain.
    pub fn wipe_transient(&self) -> Result<()> {
        self.conn.execute("DELETE FROM memory; DELETE FROM kv")?;
        Ok(())
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        let mut s = self
            .conn
            .prepare("INSERT OR REPLACE INTO kv (key, value) VALUES (?, ?)")?;
        s.bind((1, key))?;
        s.bind((2, value))?;
        s.next()?;
        Ok(())
    }

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let mut s = self.conn.prepare("SELECT value FROM kv WHERE key = ?")?;
        s.bind((1, key))?;
        match s.next()? {
            State::Row => Ok(Some(s.read(0)?)),
            State::Done => Ok(None),
        }
    }

    pub fn kv_del(&self, key: &str) -> Result<()> {
        let mut s = self.conn.prepare("DELETE FROM kv WHERE key = ?")?;
        s.bind((1, key))?;
        s.next()?;
        Ok(())
    }

    /// All kv rows whose key starts with `prefix` — e.g. the per-message
    /// `pending:<bot>:` records after a crash.
    pub fn kv_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let mut s = self
            .conn
            .prepare("SELECT key, value FROM kv WHERE substr(key, 1, ?) = ?")?;
        s.bind((1, prefix.len() as i64))?;
        s.bind((2, prefix))?;
        let mut out = Vec::new();
        while let State::Row = s.next()? {
            out.push((s.read(0)?, s.read(1)?));
        }
        Ok(out)
    }

    pub fn kv_del_prefix(&self, prefix: &str) -> Result<()> {
        let mut s = self
            .conn
            .prepare("DELETE FROM kv WHERE substr(key, 1, ?) = ?")?;
        s.bind((1, prefix.len() as i64))?;
        s.bind((2, prefix))?;
        s.next()?;
        Ok(())
    }

    pub fn clear_memory(&self, name: &str) -> Result<()> {
        let mut s = self.conn.prepare("DELETE FROM memory WHERE bot = ?")?;
        s.bind((1, name))?;
        s.next()?;
        Ok(())
    }

    /// Forget every bot's topic except `keep` (theirs were deleted).
    pub fn clear_other_topics(&self, keep: &str) -> Result<()> {
        let mut s = self
            .conn
            .prepare("UPDATE bots SET topic_id = NULL WHERE name != ?")?;
        s.bind((1, keep))?;
        s.next()?;
        Ok(())
    }

    /// Forget a bot's forum topic (next turn recreates it).
    pub fn clear_bot_topic(&self, name: &str) -> Result<()> {
        let mut s = self
            .conn
            .prepare("UPDATE bots SET topic_id = NULL WHERE name = ?")?;
        s.bind((1, name))?;
        s.next()?;
        Ok(())
    }

    pub fn get_bot(&self, name: &str) -> Result<Option<Bot>> {
        Ok(self.list_bots()?.into_iter().find(|b| b.name == name))
    }

    pub fn set_bot_topic(&self, name: &str, topic_id: i64) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("UPDATE bots SET topic_id = ? WHERE name = ?")?;
        stmt.bind((1, topic_id))?;
        stmt.bind((2, name))?;
        stmt.next()?;
        Ok(())
    }

    pub fn remember(&self, bot: &str, key: &str, value: &str) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO memory (bot, key, value, updated) VALUES (?, ?, ?, unixepoch())
             ON CONFLICT (bot, key) DO UPDATE SET value = excluded.value, updated = unixepoch()",
        )?;
        stmt.bind((1, bot))?;
        stmt.bind((2, key))?;
        stmt.bind((3, value))?;
        stmt.next()?;
        Ok(())
    }

    /// Write a consistent copy of the whole db to `path` via
    /// `VACUUM INTO` — safe while `serve` keeps writing to the live db.
    /// Fails if `path` already exists, so it's removed first.
    pub fn vacuum_into(&self, path: &Path) -> Result<()> {
        let _ = std::fs::remove_file(path);
        let esc = path.to_string_lossy().replace('\'', "''");
        self.conn.execute(format!("VACUUM INTO '{esc}'"))?;
        Ok(())
    }

    pub fn recall(&self, bot: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM memory WHERE bot = ? ORDER BY key")?;
        stmt.bind((1, bot))?;
        let mut out = Vec::new();
        while let State::Row = stmt.next()? {
            out.push((stmt.read(0)?, stmt.read(1)?));
        }
        Ok(out)
    }
}

fn read_bot(stmt: &sqlite::Statement) -> Result<Bot> {
    Ok(Bot {
        name: stmt.read(0)?,
        token: stmt.read(1)?,
        username: stmt.read::<Option<String>, _>(2)?,
        topic_id: stmt.read::<Option<i64>, _>(3)?,
        instructions: stmt.read(4)?,
        provider: stmt.read(5)?,
        model: stmt.read::<Option<String>, _>(6)?,
        greeting: stmt.read::<Option<String>, _>(7)?,
    })
}
