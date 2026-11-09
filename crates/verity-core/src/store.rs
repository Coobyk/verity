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
        Ok(Self { conn })
    }

    pub fn add_bot(&self, bot: &Bot) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO bots (name, token, username, topic_id, instructions, provider, model)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;
        stmt.bind((1, bot.name.as_str()))?;
        stmt.bind((2, bot.token.as_str()))?;
        stmt.bind((3, bot.username.as_deref()))?;
        stmt.bind((4, bot.topic_id))?;
        stmt.bind((5, bot.instructions.as_str()))?;
        stmt.bind((6, bot.provider.as_str()))?;
        stmt.bind((7, bot.model.as_deref()))?;
        stmt.next()?;
        Ok(())
    }

    pub fn remove_bot(&self, name: &str) -> Result<bool> {
        let mut stmt = self
            .conn
            .prepare("DELETE FROM bots WHERE name = ?")?;
        stmt.bind((1, name))?;
        stmt.next()?;
        Ok(self.conn.change_count() > 0)
    }

    pub fn list_bots(&self) -> Result<Vec<Bot>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, token, username, topic_id, instructions, provider, model
             FROM bots ORDER BY name",
        )?;
        let mut out = Vec::new();
        while let State::Row = stmt.next()? {
            out.push(Bot {
                name: stmt.read(0)?,
                token: stmt.read(1)?,
                username: stmt.read::<Option<String>, _>(2)?,
                topic_id: stmt.read::<Option<i64>, _>(3)?,
                instructions: stmt.read(4)?,
                provider: stmt.read(5)?,
                model: stmt.read::<Option<String>, _>(6)?,
            });
        }
        Ok(out)
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
