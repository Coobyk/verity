//! The quiet-bot loop. Each registered bot long-polls its own Bot API
//! updates; a message in its topic gets: 👀 reaction → typing → work →
//! one rich reply. No tool-call spam.

use std::path::PathBuf;
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::botapi::{BotApi, InputRichMessage};
use crate::harness::{Harness, TurnContext};
use crate::store::Store;
use crate::{Error, Result};

pub struct Bridge {
    pub group_id: i64,
    /// sqlite connections aren't Send, so the bridge takes the db path and
    /// touches the store inside `spawn_blocking` only.
    pub db_path: PathBuf,
    pub harness: Arc<dyn Harness>,
}

impl Bridge {
    /// Run every bot in the store forever. Individual bot loops log and
    /// keep polling on transient failures.
    pub async fn run(&self) -> Result<()> {
        let db = self.db_path.clone();
        let bots = tokio::task::spawn_blocking(move || -> Result<Vec<crate::store::Bot>> {
            Store::open(&db)?.list_bots()
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))??;
        if bots.is_empty() {
            return Err(Error::Config("no bots registered — `verity bot new`".into()));
        }
        let mut handles = Vec::new();
        for bot in bots {
            let db = self.db_path.clone();
            let harness = self.harness.clone();
            let group_id = self.group_id;
            handles.push(tokio::spawn(async move {
                if let Err(e) = run_bot(bot, group_id, db, harness).await {
                    warn!(error = %e, "bot loop exited");
                }
            }));
        }
        for h in handles {
            let _ = h.await;
        }
        Ok(())
    }
}

async fn run_bot(
    bot: crate::store::Bot,
    group_id: i64,
    db: PathBuf,
    harness: Arc<dyn Harness>,
) -> Result<()> {
    let api = BotApi::new(&bot.token);
    let me = api.get_me().await?;
    info!(bot = %bot.name, username = ?me.username, "polling");

    // Ensure the bot owns a forum topic named after it.
    let topic_id = match bot.topic_id {
        Some(t) => t,
        None => {
            let t = api.create_forum_topic(group_id, &bot.name).await?;
            let (db2, name, tid) = (db.clone(), bot.name.clone(), t.message_thread_id);
            tokio::task::spawn_blocking(move || -> Result<()> {
                Store::open(&db2)?.set_bot_topic(&name, tid)
            })
            .await
            .map_err(|e| Error::Other(e.to_string()))??;
            info!(bot = %bot.name, topic = tid, "created topic");
            tid
        }
    };

    let mut offset = 0i64;
    loop {
        let updates = match api.get_updates(offset, 30).await {
            Ok(u) => u,
            Err(e) => {
                warn!(bot = %bot.name, error = %e, "getUpdates failed, retrying");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };
        for update in updates {
            offset = update.update_id + 1;
            let Some(msg) = update.message else { continue };
            if msg.chat.id != group_id || msg.message_thread_id != Some(topic_id) {
                continue;
            }
            if msg.from.as_ref().map(|u| u.is_bot).unwrap_or(true) {
                continue; // never react to bots (incl. ourselves)
            }
            let Some(text) = msg.text.clone() else { continue };
            debug!(bot = %bot.name, %text, "turn request");
            if let Err(e) =
                handle_turn(&api, &bot, group_id, topic_id, msg.message_id, &text, &db, &*harness).await
            {
                warn!(bot = %bot.name, error = %e, "turn failed");
                let _ = api
                    .send_message(group_id, "Something went wrong on my side.", Some(topic_id))
                    .await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_turn(
    api: &BotApi,
    bot: &crate::store::Bot,
    group_id: i64,
    topic_id: i64,
    message_id: i64,
    text: &str,
    db: &std::path::Path,
    harness: &dyn Harness,
) -> Result<()> {
    // 1. Acknowledge silently.
    let _ = api.set_reaction(group_id, message_id, "👀").await;
    let _ = api.send_chat_action(group_id, "typing", Some(topic_id)).await;

    // 2. Do the work.
    let (db2, bot_name) = (db.to_path_buf(), bot.name.clone());
    let memory = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String)>> {
        Store::open(&db2)?.recall(&bot_name)
    })
    .await
    .map_err(|e| Error::Other(e.to_string()))?
    .unwrap_or_default();
    let ctx = TurnContext {
        bot_name: bot.name.clone(),
        instructions: bot.instructions.clone(),
        memory,
        cwd: std::env::current_dir().unwrap_or_else(|_| "/".into()),
        model: bot.model.clone(),
    };
    let out = harness.run_turn(&ctx, text).await?;

    // 3. One rich reply. Fall back to plain text if rich parsing fails.
    let reply = if out.reply.is_empty() { "Done.".into() } else { out.reply };
    match api
        .send_rich_message(group_id, InputRichMessage::markdown(&reply), Some(topic_id))
        .await
    {
        Ok(_) => Ok(()),
        Err(e) => {
            debug!(error = %e, "rich send failed, falling back to text");
            let plain = reply.chars().take(4000).collect::<String>();
            api.send_message(group_id, &plain, Some(topic_id)).await?;
            Ok(())
        }
    }
}
