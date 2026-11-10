//! The quiet-bot loop. Each registered bot long-polls its own Bot API
//! updates; a message in its topic gets: 👀 reaction → typing → work →
//! ✅ → rich replies (split at the 32k cap). No tool-call spam.
//!
//! If a user-side MTProto session is attached, each turn also carries the
//! recent topic transcript — bots can't read history themselves, so the
//! user's session reads it for them.
//!
//! Replies can carry `:::` markers the bridge strips and acts on:
//! - `:::remember k = v:::` — persist to the bot's memory table
//! - `:::tell name: msg:::` — run a turn for teammate `name`; it replies
//!   in this topic as itself (bots can't see each other, so verity routes)
//! - `:::spawn label: task:::` — fan out up to 3 short-lived helpers on
//!   the same harness; results fold into the reply as `<details>` blocks
//! - `:::backup:::` — upload `~/verity` (config, session, brain) to
//!   General as documents

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grammers_session::defs::PeerRef;
use tracing::{debug, info, warn};

use crate::botapi::{BotApi, InputRichMessage};
use crate::harness::{Harness, TurnContext};
use crate::mtproto::Tg;
use crate::rich::{split_markdown, RICH_LIMIT, TEXT_LIMIT};
use crate::store::{Bot, Store};
use crate::{Error, Result};

/// How many topic messages are fetched as turn context (when a user
/// session is attached).
const HISTORY_LIMIT: i32 = 30;
/// Per-message cap so one giant paste can't eat the whole transcript.
const HISTORY_MSG_CHARS: usize = 2_000;
/// Max teammates one reply may delegate to.
const MAX_TELLS: usize = 5;
/// How deep `tell` chains may go before being dropped (0 = human turn).
const MAX_TELL_DEPTH: u8 = 2;
/// Helpers folded into one reply (Akeru's "helpers, up to three").
const MAX_HELPERS: usize = 3;
/// A turn this slow gets a single edited progress message; deleted when
/// the real reply lands.
const PROGRESS_AFTER: Duration = Duration::from_secs(12);
const PROGRESS_TICK: Duration = Duration::from_secs(20);

/// Builds the harness for a bot's `provider` string (`pi`, `echo`,
/// `acp:<cmd> [args]`).
pub type MakeHarness = dyn Fn(&str) -> Arc<dyn Harness> + Send + Sync;

pub struct Bridge {
    pub group_id: i64,
    /// sqlite connections aren't Send, so the bridge takes the db path and
    /// touches the store inside `spawn_blocking` only.
    pub db_path: PathBuf,
    pub make_harness: Arc<MakeHarness>,
    /// User-side session + resolved group peer, for topic transcripts.
    /// Absent in `--echo` or when the user isn't logged in.
    pub tg: Option<Arc<Tg>>,
    pub tg_peer: Option<PeerRef>,
}

/// Everything a turn needs besides the triggering message — cloneable so
/// detached `tell` turns can carry it.
#[derive(Clone)]
struct Env {
    group_id: i64,
    db: PathBuf,
    make: Arc<MakeHarness>,
    tg: Option<Arc<Tg>>,
    peer: Option<PeerRef>,
}

/// Supervisor sweep interval: how often the bridge looks for newly
/// registered bots and starts their loops.
const BOT_SWEEP: Duration = Duration::from_secs(10);
/// Default `~/verity` → General backup cadence while `serve` runs;
/// `VERITY_BACKUP_EVERY` (seconds) overrides, `0` disables.
const DEFAULT_BACKUP_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

impl Bridge {
    /// Run every bot in the store forever. Individual bot loops log and
    /// keep polling on transient failures.
    pub async fn run(&self) -> Result<()> {
        let env = Env {
            group_id: self.group_id,
            db: self.db_path.clone(),
            make: self.make_harness.clone(),
            tg: self.tg.clone(),
            peer: self.tg_peer,
        };
        // Periodic backup of ~/verity into General — the off-machine
        // copy that survives a dead laptop or a move to a VM. First one
        // lands a full interval in; `/backup` or `:::backup:::` for now.
        let every = std::env::var("VERITY_BACKUP_EVERY")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_BACKUP_EVERY);
        if !every.is_zero() {
            let env = env.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    let bot = db_call(&env.db, |s| s.primary_bot())
                        .await
                        .ok()
                        .flatten();
                    match bot {
                        Some(b) => {
                            let api = BotApi::new(&b.token);
                            match crate::backup::upload(&api, env.group_id).await {
                                Ok(files) => {
                                    info!(n = files.len(), "backup uploaded to General")
                                }
                                Err(e) => warn!(error = %e, "periodic backup failed"),
                            }
                        }
                        None => warn!("periodic backup skipped — no bots registered"),
                    }
                }
            });
        }
        // Supervisor: run a loop per bot in the store, and pick up bots
        // registered later (`bot new` or a `:::bot_new:::` marker) without
        // a restart.
        let mut running: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            let bots = db_call(&env.db, |s| s.list_bots())
                .await
                .unwrap_or_default();
            if bots.is_empty() && running.is_empty() {
                warn!("no bots registered — `verity bot new`");
            }
            for bot in bots {
                if !running.insert(bot.name.clone()) {
                    continue;
                }
                let env = env.clone();
                let bot_name = bot.name.clone();
                tokio::spawn(async move {
                    loop {
                        match run_bot(&bot, &env).await {
                            Ok(()) => return, // clean exit (only if polling ends gracefully)
                            Err(e) => {
                                warn!(bot = %bot.name, error = %e, "bot loop failed, restarting in 10s");
                                tokio::time::sleep(Duration::from_secs(10)).await;
                            }
                        }
                    }
                });
                info!(bot = %bot_name, "loop started");
            }
            tokio::time::sleep(BOT_SWEEP).await;
        }
    }
}

async fn run_bot(bot: &Bot, env: &Env) -> Result<()> {
    let api = BotApi::new(&bot.token);
    let me = api.get_me().await?;
    info!(bot = %bot.name, username = ?me.username, "polling");

    // Ensure the bot owns a forum topic named after it.
    let mut topic_id = match bot.topic_id {
        Some(t) => t,
        None => {
            let t = api.create_forum_topic(env.group_id, &bot.name).await?;
            let (db, name, tid) = (env.db.clone(), bot.name.clone(), t.message_thread_id);
            tokio::task::spawn_blocking(move || -> Result<()> {
                Store::open(&db)?.set_bot_topic(&name, tid)
            })
            .await
            .map_err(|e| Error::Other(e.to_string()))??;
            info!(bot = %bot.name, topic = tid, "created topic");
            if let Some(g) = &bot.greeting {
                let _ = send_reply(&api, env.group_id, tid, g).await;
            }
            tid
        }
    };

    // Skip the offline backlog: confirm everything pending so a restart
    // doesn't re-answer stale messages. `offset = -1` returns only the
    // most recent update; acking it clears the queue.
    let mut offset = api
        .get_updates(-1, 0)
        .await
        .ok()
        .and_then(|u| u.last().map(|u| u.update_id + 1))
        .unwrap_or(0);

    // Crash recovery: a leftover pending record means the last run died
    // mid-turn — the 👀 is still on the message. Re-run it.
    let pkey = pending_key(&bot.name);
    if let Ok(Some(v)) = db_call(&env.db, move |s| s.kv_get(&pkey)).await {
        if let Some((mid, text)) = v.split_once('\n') {
            if let Ok(mid) = mid.parse::<i64>() {
                warn!(bot = %bot.name, msg = mid, "resuming crashed turn");
                let _ = handle_turn(&api, bot, env, topic_id, mid, text).await;
            }
        }
    }

    loop {
        // Self-terminate if this bot was removed from the store (e.g.
        // by a /reset) — otherwise the supervisor keeps us alive. The
        // fresh record also catches a recreated topic: reset deletes
        // ours and makes a new one we should start listening to.
        let n = bot.name.clone();
        match db_call(&env.db, move |s| s.get_bot(&n)).await {
            Ok(Some(fresh)) => {
                if let Some(t) = fresh.topic_id {
                    if t != topic_id {
                        info!(bot = %bot.name, old = topic_id, new = t, "topic swapped — following");
                        topic_id = t;
                    }
                }
            }
            _ => {
                info!(bot = %bot.name, "unregistered — stopping loop");
                return Ok(());
            }
        }
        let updates = match api.get_updates(offset, 30).await {
            Ok(u) => u,
            Err(e) => {
                warn!(bot = %bot.name, error = %e, "getUpdates failed, retrying");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        for update in updates {
            offset = update.update_id + 1;
            // Confirm-button clicks for the destructive commands.
            if let Some(cb) = update.callback_query {
                if let (Some(data), Some(m)) = (cb.data, cb.message) {
                    if m.message_thread_id == Some(topic_id) {
                        match data.as_str() {
                            "v:cancel" => {
                                let _ = api.answer_callback_query(&cb.id, Some("cancelled")).await;
                                let _ = api.delete_message(env.group_id, m.message_id).await;
                            }
                            d if d.starts_with("v:") => {
                                let mut f = d[2..].rsplitn(2, ':');
                                match (f.next(), f.next()) {
                                    (Some("ok"), Some(cmd)) => {
                                        let _ = api.answer_callback_query(&cb.id, None).await;
                                        run_command(&api, bot, env, topic_id, cmd, m.message_id)
                                            .await;
                                    }
                                    (Some("no"), Some(_)) => {
                                        let _ = api
                                            .answer_callback_query(&cb.id, Some("nope — try again"))
                                            .await;
                                    }
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    }
                }
                continue;
            }
            let Some(msg) = update.message else { continue };
            if msg.chat.id != env.group_id || msg.message_thread_id != Some(topic_id) {
                continue;
            }
            if msg.from.as_ref().map(|u| u.is_bot).unwrap_or(true) {
                continue; // never react to bots (incl. ourselves)
            }
            // Text or caption; media gets downloaded and its local path
            // appended so the agent can actually look at it.
            let mut text = msg
                .text
                .clone()
                .or_else(|| msg.caption.clone())
                .unwrap_or_default();
            if let Some(att) = attachment(&msg) {
                match save_media(&api, &env.db, &bot.name, msg.message_id, &att).await {
                    Ok(path) => {
                        let note = format!(
                            "[{} attached: {} — read this file to view it]",
                            att.desc,
                            path.display()
                        );
                        text = if text.is_empty() {
                            note
                        } else {
                            format!("{text}\n\n{note}")
                        };
                    }
                    Err(e) => {
                        warn!(bot = %bot.name, error = %e, "attachment download failed")
                    }
                }
            }
            if text.trim().is_empty() {
                continue;
            }
            // In-chat commands — destructive, so they confirm via an
            // inline button before anything runs.
            if let Some(cmd) = text
                .strip_prefix('/')
                .map(|c| c.split_whitespace().next().unwrap_or(""))
            {
                let ask = match cmd {
                    "backup" => {
                        // Not destructive — no quiz-confirm needed.
                        let note = match crate::backup::upload(&api, env.group_id).await {
                            Ok(files) => {
                                format!("🗄 backed up {} to General", files.join(", "))
                            }
                            Err(e) => format!("⚠️ backup failed: {e}"),
                        };
                        let _ = api
                            .send_message(env.group_id, &note, Some(topic_id))
                            .await;
                        continue;
                    }
                    "clear" => "Wipe my memory? Can't undo this.",
                    "clearchat" => {
                        "Delete every message in this topic AND wipe my memory? Can't undo."
                    }
                    "reset" => "Nuke EVERYTHING — all topics, all messages, every bot but me, all memory? Can't undo.",
                    _ => "",
                };
                if !ask.is_empty() {
                    // Quiz-confirm: pick the right answer among decoys.
                    let mut rng = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.subsec_nanos() as u64)
                        .unwrap_or(1);
                    let mut next = || {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        rng
                    };
                    let (a, b) = (2 + next() % 8, 2 + next() % 8);
                    let ans = a + b;
                    let mut opts: Vec<u64> = vec![ans];
                    while opts.len() < 4 {
                        let d = (ans as i64 + (next() % 7) as i64 - 3).max(0) as u64;
                        if !opts.contains(&d) {
                            opts.push(d);
                        }
                    }
                    for i in (1..opts.len()).rev() {
                        opts.swap(i, (next() as usize) % (i + 1));
                    }
                    let row: Vec<(String, String)> = opts
                        .iter()
                        .map(|o| {
                            let verdict = if *o == ans { "ok" } else { "no" };
                            (o.to_string(), format!("v:{cmd}:{verdict}"))
                        })
                        .collect();
                    let prompt = format!("{ask}\n\nTo confirm, tap the answer to {a} + {b}:");
                    let _ = api
                        .send_buttons(
                            env.group_id,
                            &prompt,
                            Some(topic_id),
                            &[row, vec![("Cancel".into(), "v:cancel".into())]],
                        )
                        .await;
                    continue;
                }
            }
            debug!(bot = %bot.name, %text, "turn request");
            if let Err(e) = handle_turn(&api, bot, env, topic_id, msg.message_id, &text).await {
                warn!(bot = %bot.name, error = %e, "turn failed");
                let _ = api.set_reaction(env.group_id, msg.message_id, "⚠️").await;
                let detail: String = e.to_string().chars().take(300).collect();
                let _ = api
                    .send_message(
                        env.group_id,
                        &format!("uh — broke on my side: {detail}"),
                        Some(topic_id),
                    )
                    .await;
            }
        }
    }
}

/// Execute a confirmed destructive command (button click landed in our
/// topic). `confirm_mid` is the prompt message to clean up after.
async fn run_command(api: &BotApi, bot: &Bot, env: &Env, topic_id: i64, cmd: &str, confirm_mid: i64) {
    info!(bot = %bot.name, %cmd, "confirmed command executing");
    let n = bot.name.clone();
    let wipe = move |s: &Store| -> Result<()> {
        s.clear_memory(&n)?;
        s.kv_del(&format!("greeted:{n}"))?;
        s.kv_del(&format!("pending:{n}"))
    };
    match cmd {
        "clear" => {
            let _ = db_call(&env.db, wipe).await;
            let _ = api.delete_message(env.group_id, confirm_mid).await;
            let _ = api
                .send_message(env.group_id, "memory wiped", Some(topic_id))
                .await;
        }
        "clearchat" => {
            // Delete every message in the topic (ids via the user session
            // — bots can't list history), wipe memory, re-greet.
            if let (Some(tg), Some(peer)) = (&env.tg, &env.peer) {
                if let Ok(msgs) = tg.topic_history(*peer, topic_id, 200).await {
                    for m in msgs {
                        let _ = api.delete_message(env.group_id, m.id as i64).await;
                    }
                }
            }
            let _ = api.delete_message(env.group_id, confirm_mid).await;
            let _ = db_call(&env.db, wipe).await;
            if let Some(g) = &bot.greeting {
                let _ = send_reply(api, env.group_id, topic_id, g).await;
            }
        }
        "reset" => {
            // 1. Wipe the whole group user-side: every bot member but us
            //    (admins included — the Bot API can't kick those), every
            //    topic, every message in General. Registered teammates
            //    get forgotten below either way; stray bots and topics
            //    the store no longer knows die here too.
            if let (Some(tg), Some(peer)) = (&env.tg, &env.peer) {
                let keep = api.get_me().await.ok().map(|u| u.id);
                let rep = tg.wipe_group(*peer, keep).await;
                info!(
                    kicked = rep.bots_kicked,
                    topics = rep.topics_deleted,
                    general = rep.general_messages,
                    "group wiped"
                );
            }
            // 2. Forget every teammate. Without a user session step 1
            //    never ran, so do a best-effort kick + topic delete via
            //    the Bot API too (works on non-admin members).
            let others = db_call(&env.db, |s| s.list_bots())
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|b| b.name != bot.name)
                .collect::<Vec<_>>();
            for b in others {
                if env.tg.is_none() {
                    if let Ok(me) = BotApi::new(&b.token).get_me().await {
                        let _ = api.kick_member(env.group_id, me.id).await;
                    }
                    if let Some(t) = b.topic_id {
                        let _ = api.delete_forum_topic(env.group_id, t).await;
                    }
                }
                let n = b.name.clone();
                let _ = db_call(&env.db, move |s| s.remove_bot(&n)).await;
            }
            // 3. Our topic goes too — already wiped above (or now) — and
            //    recreated empty. The poll loop follows the new id from
            //    the store, so we stay live without a serve restart.
            let _ = api.delete_forum_topic(env.group_id, topic_id).await;
            let new_tid = match api.create_forum_topic(env.group_id, &bot.name).await {
                Ok(t) => Some(t.message_thread_id),
                Err(e) => {
                    warn!(bot = %bot.name, error = %e, "reset: topic recreate failed");
                    None
                }
            };
            if let Some(t) = new_tid {
                let (db, n) = (env.db.clone(), bot.name.clone());
                let _ = db_call(&db, move |s| s.set_bot_topic(&n, t)).await;
            }
            // 4. Brain wipe: every memory row and transient flag, for
            //    every bot — not just ours.
            let _ = db_call(&env.db, |s| s.wipe_transient()).await;
            let _ = api.delete_message(env.group_id, confirm_mid).await;
            // 5. Leave exactly one thing behind: a fresh backup in
            //    General, so the wipe still has a restore point.
            let note = match crate::backup::upload(api, env.group_id).await {
                Ok(files) => format!("fresh backup in General: {}", files.join(", ")),
                Err(e) => format!("backup failed: {e}"),
            };
            if let Some(t) = new_tid {
                let _ = api
                    .send_message(env.group_id, &format!("🧹 clean slate — {note}"), Some(t))
                    .await;
                if let Some(g) = &bot.greeting {
                    let _ = send_reply(api, env.group_id, t, g).await;
                }
            }
        }
        _ => {}
    }
}

/// A downloadable attachment found on a message.
struct Attachment {
    file_id: String,
    /// Human description for the prompt note — `image`, `file "x.pdf"`.
    desc: String,
    /// Extension for the local filename (no dot).
    ext: String,
}

/// Pick a message's attachment worth downloading: the largest photo
/// size, or a sent-as-file document. Stickers/video/voice don't make
/// the cut yet — they still show as `[media]` in the transcript.
fn attachment(msg: &crate::botapi::Message) -> Option<Attachment> {
    if let Some(photos) = &msg.photo {
        if let Some(p) = photos.iter().max_by_key(|p| p.width * p.height) {
            return Some(Attachment {
                file_id: p.file_id.clone(),
                desc: "image".into(),
                ext: "jpg".into(),
            });
        }
    }
    if let Some(d) = &msg.document {
        // Bot API getFile caps at 20 MB — don't bother past that.
        if d.file_size.map(|s| s > 20 * 1024 * 1024).unwrap_or(false) {
            return None;
        }
        let ext = d
            .file_name
            .as_deref()
            .and_then(|n| n.rsplit_once('.'))
            .map(|(_, e)| e.chars().take(8).collect::<String>())
            .unwrap_or_else(|| "bin".into());
        let desc = match &d.file_name {
            Some(n) => format!("file \"{n}\""),
            None => d
                .mime_type
                .as_deref()
                .and_then(|m| m.strip_prefix("image/").map(|_| "image".into()))
                .unwrap_or_else(|| "file".into()),
        };
        return Some(Attachment {
            file_id: d.file_id.clone(),
            desc,
            ext,
        });
    }
    None
}

/// Download an attachment to `<verity home>/media/<bot>/<msgid>.<ext>`
/// (the db dir is the bot home) and prune files older than a week.
async fn save_media(
    api: &BotApi,
    db: &Path,
    bot: &str,
    msg_id: i64,
    att: &Attachment,
) -> Result<PathBuf> {
    let dir = db
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("media")
        .join(bot);
    std::fs::create_dir_all(&dir)?;
    // Sweep stale downloads — media is only useful during its turn.
    let week = Duration::from_secs(7 * 24 * 3600);
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let stale = e
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t.elapsed().map(|a| a > week).unwrap_or(false))
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let remote = api.file_path(&att.file_id).await?;
    let dest = dir.join(format!("{msg_id}.{}", att.ext));
    api.download_file(&remote, &dest).await?;
    Ok(dest)
}

/// Pull the recent topic transcript via the user session, as
/// `(sender, text)` pairs with oversized messages truncated.
async fn fetch_history(tg: &Tg, peer: &PeerRef, topic_id: i64) -> Vec<(String, String)> {
    match tg.topic_history(*peer, topic_id, HISTORY_LIMIT).await {
        Ok(msgs) => msgs
            .into_iter()
            .filter(|m| !m.text.is_empty())
            .map(|m| {
                let text = if m.text.chars().count() > HISTORY_MSG_CHARS {
                    format!(
                        "{}…",
                        m.text.chars().take(HISTORY_MSG_CHARS).collect::<String>()
                    )
                } else {
                    m.text
                };
                (m.sender, text)
            })
            .collect(),
        Err(e) => {
            debug!(error = %e, "topic history fetch failed; turn runs without it");
            Vec::new()
        }
    }
}

/// Keep the "typing" indicator alive for the duration of a turn —
/// Telegram clears it after ~5s, so re-send every 4.
fn typing_keepalive(api: BotApi, group_id: i64, topic_id: i64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(4)).await;
            if api
                .send_chat_action(group_id, "typing", Some(topic_id))
                .await
                .is_err()
            {
                return;
            }
        }
    })
}

/// After [`PROGRESS_AFTER`], post one progress message and edit it every
/// [`PROGRESS_TICK`] with the elapsed count. Returns the task handle and
/// the shared slot holding the message id (for later deletion).
fn progress_task(
    api: BotApi,
    group_id: i64,
    topic_id: i64,
) -> (tokio::task::JoinHandle<()>, Arc<Mutex<Option<i64>>>) {
    let msg_id = Arc::new(Mutex::new(None));
    let slot = msg_id.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(PROGRESS_AFTER).await;
        let Ok(m) = api
            .send_message(group_id, "⏳ working…", Some(topic_id))
            .await
        else {
            return;
        };
        *slot.lock().unwrap() = Some(m.message_id);
        let start = Instant::now();
        loop {
            tokio::time::sleep(PROGRESS_TICK).await;
            let text = format!("⏳ working… {}s elapsed", start.elapsed().as_secs());
            if api
                .edit_message(group_id, m.message_id, &text)
                .await
                .is_err()
            {
                return;
            }
        }
    });
    (handle, msg_id)
}

/// Everything `:::`-marked in a reply, by verb.
#[derive(Default)]
struct Markers {
    remember: Vec<(String, String)>,
    tell: Vec<(String, String)>,
    spawn: Vec<(String, String)>,
    /// (name, token, instructions, optional provider) from `bot_new`.
    bot_new: Vec<(String, String, String, Option<String>)>,
    /// `:::backup:::` — ship ~/verity to General after the reply.
    backup: bool,
}

/// Pull `:::verb …:::` lines out of a reply. Returns the cleaned reply
/// plus the parsed markers. Malformed markers never reach Telegram.
fn extract_markers(reply: &str) -> (String, Markers) {
    let mut kept = Vec::new();
    let mut markers = Markers::default();
    for line in reply.lines() {
        let t = line.trim();
        if t.starts_with(":::") && t.ends_with(":::") && t.len() > 6 {
            let inner = t[3..t.len() - 3].trim();
            let (verb, rest) = inner
                .split_once(|c: char| c.is_whitespace() || c == ':')
                .map(|(v, r)| (v, r.trim_start_matches(':').trim()))
                .unwrap_or((inner, ""));
            let parsed = match verb {
                "remember" => rest.split_once('=').map(|(k, v)| (k.trim(), v.trim())),
                "tell" | "spawn" => rest.split_once(':').map(|(k, v)| (k.trim(), v.trim())),
                _ => None,
            };
            match verb {
                "remember" => {
                    if let Some((k, v)) = parsed.filter(|(k, v)| !k.is_empty() && !v.is_empty()) {
                        markers.remember.push((k.into(), v.into()));
                    }
                }
                "tell" => {
                    if let Some((n, m)) = parsed.filter(|(n, m)| !n.is_empty() && !m.is_empty()) {
                        markers.tell.push((n.into(), m.into()));
                    }
                }
                "spawn" => {
                    if let Some((l, t)) = parsed.filter(|(l, t)| !l.is_empty() && !t.is_empty()) {
                        markers.spawn.push((l.into(), t.into()));
                    }
                }
                "backup" => markers.backup = true,
                // `bot_new name | token | instructions [| provider]`
                "bot_new" => {
                    let mut f = rest.split('|').map(str::trim);
                    match (f.next(), f.next(), f.next(), f.next()) {
                        (Some(n), Some(t), Some(i), p)
                            if !n.is_empty() && !t.is_empty() && !i.is_empty() =>
                        {
                            markers.bot_new.push((
                                n.into(),
                                t.into(),
                                i.into(),
                                p.filter(|s| !s.is_empty()).map(String::from),
                            ));
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            continue;
        }
        kept.push(line);
    }
    (kept.join("\n").trim().to_string(), markers)
}

/// Send a reply as one or more rich messages, falling back to plain text
/// (again split at the cap) when rich parsing rejects the markdown.
async fn send_reply(api: &BotApi, group_id: i64, topic_id: i64, reply: &str) -> Result<()> {
    for part in split_markdown(reply, RICH_LIMIT) {
        match api
            .send_rich_message(group_id, InputRichMessage::markdown(&part), Some(topic_id))
            .await
        {
            Ok(_) => continue,
            Err(e) => {
                warn!(error = %e, "rich send failed, falling back to text");
                for chunk in split_markdown(&part, TEXT_LIMIT) {
                    // Legacy markdown render first; raw text last resort.
                    if let Err(e) = api
                        .send_markdown_message(group_id, &chunk, Some(topic_id))
                        .await
                    {
                        warn!(error = %e, "legacy markdown send failed, sending raw");
                        api.send_message(group_id, &chunk, Some(topic_id)).await?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn db_call<T: Send + 'static>(
    db: &Path,
    f: impl FnOnce(&Store) -> Result<T> + Send + 'static,
) -> impl std::future::Future<Output = Result<T>> + Send {
    let db = db.to_path_buf();
    async move {
        tokio::task::spawn_blocking(move || f(&Store::open(&db)?))
            .await
            .map_err(|e| Error::Other(e.to_string()))?
    }
}

/// A human-facing turn: reaction, typing keep-alive, progress message on
/// slow work, ✅ when the reply lands.
async fn handle_turn(
    api: &BotApi,
    bot: &Bot,
    env: &Env,
    topic_id: i64,
    message_id: i64,
    text: &str,
) -> Result<()> {
    let _ = api.set_reaction(env.group_id, message_id, "👀").await;
    // Mark the turn pending — if we die mid-turn, the leftover record
    // (and the lingering 👀) lets a restart re-run it.
    let pkey = pending_key(&bot.name);
    let pval = format!("{message_id}\n{text}");
    let _ = db_call(&env.db, move |s| s.kv_set(&pkey, &pval)).await;
    let _ = api
        .send_chat_action(env.group_id, "typing", Some(topic_id))
        .await;
    let keepalive = typing_keepalive(api.clone(), env.group_id, topic_id);
    let (progress, progress_msg) = progress_task(api.clone(), env.group_id, topic_id);

    let result = do_turn(env, bot, topic_id, text, 0).await;

    keepalive.abort();
    progress.abort();
    let progress_id = *progress_msg.lock().unwrap();
    if let Some(id) = progress_id {
        let _ = api.delete_message(env.group_id, id).await;
    }
    // Turn over — clear the pending record, then resolve the 👀:
    // ✅ only when the turn spawned helpers; a plain reply leaves no mark.
    let pkey = pending_key(&bot.name);
    let _ = db_call(&env.db, move |s| s.kv_del(&pkey)).await;
    match result {
        Ok(true) => {
            let _ = api.set_reaction(env.group_id, message_id, "✅").await;
        }
        Ok(false) => {
            let _ = api.clear_reaction(env.group_id, message_id).await;
        }
        Err(_) => {} // upstream marks ⚠️, which also replaces the 👀
    }
    result.map(|_| ())
}

fn pending_key(bot_name: &str) -> String {
    format!("pending:{bot_name}")
}

/// One turn of agent work for `bot` in `topic_id`: context → harness →
/// markers → memory, helpers, tells → rich reply. Shared by human turns
/// (depth 0, via `handle_turn`) and `tell`-routed teammate turns.
/// Returns `true` when the turn fanned out spawn helpers — real work
/// worth a ✅; plain chats get no final reaction.
async fn do_turn(env: &Env, bot: &Bot, topic_id: i64, text: &str, depth: u8) -> Result<bool> {
    let api = BotApi::new(&bot.token);
    let harness = (env.make)(&bot.provider);

    // 1. Context: memory, transcript, teammate roster.
    let bot_name = bot.name.clone();
    let memory = db_call(&env.db, move |s| s.recall(&bot_name))
        .await
        .unwrap_or_default();
    let peers = db_call(&env.db, {
        let me = bot.name.clone();
        move |s| {
            Ok(s.list_bots()?
                .into_iter()
                .filter(|b| b.name != me)
                .map(|b| b.name)
                .collect::<Vec<String>>())
        }
    })
    .await
    .unwrap_or_default();
    let history = match (&env.tg, &env.peer) {
        (Some(tg), Some(peer)) => fetch_history(tg, peer, topic_id).await,
        _ => Vec::new(),
    };
    let gkey = format!("greeted:{}", bot.name);
    let first_contact = depth == 0
        && matches!(db_call(&env.db, move |s| s.kv_get(&gkey)).await, Ok(None));
    let ctx = TurnContext {
        bot_name: bot.name.clone(),
        instructions: bot.instructions.clone(),
        memory,
        history,
        peers,
        first_contact,
        cwd: std::env::current_dir().unwrap_or_else(|_| "/".into()),
        model: bot.model.clone(),
    };

    // 2. Work.
    let out = harness.run_turn(&ctx, text).await?;
    if first_contact {
        let gkey = format!("greeted:{}", bot.name);
        let _ = db_call(&env.db, move |s| s.kv_set(&gkey, "1")).await;
    }
    let (mut reply, markers) = extract_markers(&out.reply);

    // 3. Memory markers persist under the acting bot's name.
    if !markers.remember.is_empty() {
        let bot_name = bot.name.clone();
        let mem = markers.remember;
        db_call(&env.db, move |s| {
            for (k, v) in &mem {
                s.remember(&bot_name, k, v)?;
            }
            Ok(())
        })
        .await?;
    }

    // 4. Helpers: same harness, subtask prompt, results folded back as
    //    collapsible blocks in completion order. Helper output is raw —
    //    no marker recursion.
    let mut helpers = tokio::task::JoinSet::new();
    for (label, task) in markers.spawn.iter().take(MAX_HELPERS) {
        let hctx = TurnContext {
            bot_name: format!("{}·{}", bot.name, label),
            instructions: "You are a short-lived helper. Do the task, report findings tersely."
                .into(),
            memory: vec![],
            history: vec![],
            peers: vec![],
            first_contact: false,
            cwd: ctx.cwd.clone(),
            model: ctx.model.clone(),
        };
        let (harness, label, task) = (harness.clone(), label.clone(), task.clone());
        helpers.spawn(async move { (label, harness.run_turn(&hctx, &task).await) });
    }
    while let Some(res) = helpers.join_next().await {
        match res {
            Ok((label, Ok(o))) => reply.push_str(&format!(
                "\n\n<details><summary>helper · {label}</summary>\n\n{}\n\n</details>",
                o.reply.trim()
            )),
            Ok((label, Err(e))) => {
                warn!(bot = %bot.name, helper = %label, error = %e, "helper failed")
            }
            Err(e) => warn!(bot = %bot.name, error = %e, "helper task panicked"),
        }
    }

    // 5. Reply — as this bot, in the topic the request came from.
    let reply = if reply.trim().is_empty() {
        "Done.".to_string()
    } else {
        reply
    };
    send_reply(&api, env.group_id, topic_id, &reply).await?;

    // `:::backup:::` — the reply asked for a config snapshot; upload
    // ~/verity to General and note it in the topic.
    if markers.backup {
        let note = match crate::backup::upload(&api, env.group_id).await {
            Ok(files) => format!("🗄 backed up {} to General", files.join(", ")),
            Err(e) => format!("⚠️ backup failed: {e}"),
        };
        let _ = api
            .send_message(env.group_id, &note, Some(topic_id))
            .await;
    }

    // 6. Tells: route work to teammates. The teammate answers in ITS OWN
    //    topic — bots never post outside their own thread. Detached so
    //    this turn's reply isn't gated on another bot's runtime;
    //    depth-capped to keep bot-to-bot loops finite.
    if depth < MAX_TELL_DEPTH && !markers.tell.is_empty() {
        for (name, msg) in markers.tell.into_iter().take(MAX_TELLS) {
            let (target, env2) = (find_bot(&env.db, &name).await, env.clone());
            match target {
                Some(target) if target.name != bot.name => {
                    let origin = bot.name.clone();
                    let tname = target.name.clone();
                    tokio::spawn(async move {
                        // Ensure the teammate has its own topic to answer in.
                        let tapi = BotApi::new(&target.token);
                        let ttopic = match target.topic_id {
                            Some(t) => t,
                            None => match tapi.create_forum_topic(env2.group_id, &target.name).await {
                                Ok(t) => {
                                    let (db, n, tid) =
                                        (env2.db.clone(), target.name.clone(), t.message_thread_id);
                                    let _ = db_call(&db, move |s| s.set_bot_topic(&n, tid)).await;
                                    if let Some(g) = &target.greeting {
                                        let _ = send_reply(&tapi, env2.group_id, tid, g).await;
                                    }
                                    tid
                                }
                                Err(e) => {
                                    warn!(bot = %target.name, error = %e, "tell: topic create failed");
                                    return;
                                }
                            },
                        };
                        let prompt = format!("Message from teammate {origin}: {msg}");
                        if let Err(e) = turn(&env2, &target, ttopic, &prompt, depth + 1).await {
                            warn!(bot = %target.name, error = %e, "tell turn failed");
                        }
                    });
                    let _ = api
                        .send_markdown_message(
                            env.group_id,
                            &format!("📨 handed off to *{tname}* — they'll answer in their own topic"),
                            Some(topic_id),
                        )
                        .await;
                }
                _ => warn!(bot = %bot.name, teammate = %name, "tell: unknown teammate"),
            }
        }
    }

    // 7. bot_new: register a new teammate. The supervisor sweep picks it
    //    up within ~10s — it creates its topic and starts working.
    for (name, token, instructions, provider) in markers.bot_new {
        let result = async {
            let me = BotApi::new(&token).get_me().await?;
            let username = me.username.unwrap_or_else(|| "?".into());
            let bot2 = Bot {
                name: name.clone(),
                token,
                username: Some(username.clone()),
                topic_id: None,
                instructions,
                provider: provider.unwrap_or_else(|| bot.provider.clone()),
                model: None,
                greeting: None,
            };
            db_call(&env.db, move |s| s.add_bot(&bot2)).await?;
            Ok::<_, Error>(username)
        }
        .await;
        let note = match result {
            Ok(u) => format!("🤖 *{name}* registered (@{u}) — starting up in its own topic"),
            Err(e) => format!("⚠️ couldn't register *{name}*: {e}"),
        };
        let _ = api
            .send_markdown_message(env.group_id, &note, Some(topic_id))
            .await;
    }
    Ok(!markers.spawn.is_empty())
}

/// `do_turn` recurses through `tell` → `tokio::spawn` → `do_turn`, and
/// rustc can't prove the opaque `impl Future` Send through that cycle.
/// Spelling the recursion as a boxed `Send` future breaks it.
fn turn<'a>(
    env: &'a Env,
    bot: &'a Bot,
    topic_id: i64,
    text: &'a str,
    depth: u8,
) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + 'a>> {
    Box::pin(do_turn(env, bot, topic_id, text, depth))
}

async fn find_bot(db: &Path, name: &str) -> Option<Bot> {
    let want = name.to_lowercase();
    db_call(db, move |s| {
        Ok(s.list_bots()?
            .into_iter()
            .find(|b| b.name.to_lowercase() == want))
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_remember_markers() {
        let reply = "Here you go.\n:::remember color = blue:::\nDone!";
        let (clean, m) = extract_markers(reply);
        assert_eq!(clean, "Here you go.\nDone!");
        assert_eq!(m.remember, vec![("color".to_string(), "blue".to_string())]);
    }

    #[test]
    fn parses_tell_and_spawn() {
        let reply = "On it.\n:::tell scout: price check LIS:::\n:::spawn draft: write a haiku:::";
        let (clean, m) = extract_markers(reply);
        assert_eq!(clean, "On it.");
        assert_eq!(
            m.tell,
            vec![("scout".to_string(), "price check LIS".to_string())]
        );
        assert_eq!(
            m.spawn,
            vec![("draft".to_string(), "write a haiku".to_string())]
        );
    }

    #[test]
    fn drops_malformed_markers_quietly() {
        let reply = "ok\n:::remember novalue:::\n:::tell nobody:::\n:::unknown x:::";
        let (clean, m) = extract_markers(reply);
        assert_eq!(clean, "ok");
        assert!(m.remember.is_empty() && m.tell.is_empty() && m.spawn.is_empty());
    }

    /// A bare-bones mock of the Bot API HTTP surface: records each method
    /// + body and answers with canned results — enough to drive a whole
    /// turn without Telegram.
    struct MockApi {
        base: String,
        calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        _server: tokio::task::JoinHandle<()>,
    }

    async fn mock_api() -> MockApi {
        use serde_json::json;
        let calls: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let calls2 = calls.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut sock, _)) = listener.accept().await {
                let calls = calls2.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    let header_end = loop {
                        let n = match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break p + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let path = head
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    let len = head
                        .to_lowercase()
                        .split("content-length:")
                        .nth(1)
                        .and_then(|s| s.trim_start().split_whitespace().next())
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    while buf.len() < header_end + len {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let body: serde_json::Value =
                        serde_json::from_slice(&buf[header_end..buf.len().min(header_end + len)])
                            .unwrap_or(json!({}));
                    let method = path.rsplit('/').next().unwrap_or("").to_string();
                    calls.lock().unwrap().push((method.clone(), body));
                    let result = match method.as_str() {
                        "getMe" => {
                            json!({"id":1,"is_bot":true,"first_name":"Mock","username":"mockbot"})
                        }
                        "createForumTopic" => {
                            json!({"message_thread_id":7,"name":"x","icon_color":0})
                        }
                        "getUpdates" => json!([]),
                        "sendMessage" | "sendRichMessage" => json!({
                            "message_id": 1,
                            "chat": {"id": -100123, "type": "supergroup"},
                            "date": 0
                        }),
                        _ => json!(true),
                    };
                    let payload =
                        serde_json::to_string(&json!({"ok": true, "result": result})).unwrap();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        MockApi {
            base: format!("http://127.0.0.1:{port}"),
            calls,
            _server: server,
        }
    }

    /// Full quiet-bot turn against the mock: 👀 → typing → work → rich
    /// reply → ✅. Exercises `handle_turn` end to end, DB included.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn turn_flow_against_mock_api() {
        let mock = mock_api().await;
        std::env::set_var("VERITY_BOT_API_BASE", &mock.base);

        let dir = std::env::temp_dir().join(format!("verity-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let env = Env {
            group_id: -100123,
            db: dir.join("state.sqlite"),
            make: Arc::new(|_: &str| -> Arc<dyn Harness> { Arc::new(crate::harness::EchoHarness) }),
            tg: None,
            peer: None,
        };
        let bot = Bot {
            name: "scout".into(),
            token: "TESTTOKEN".into(),
            username: Some("scoutbot".into()),
            topic_id: Some(7),
            instructions: String::new(),
            provider: "echo".into(),
            model: None,
            greeting: None,
        };
        let api = BotApi::new("TESTTOKEN").with_base(&mock.base);

        handle_turn(&api, &bot, &env, 7, 42, "hello verity")
            .await
            .unwrap();

        let calls = mock.calls.lock().unwrap();
        let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
        assert!(
            methods.contains(&"sendChatAction"),
            "typing action: {methods:?}"
        );
        assert!(
            methods.contains(&"sendRichMessage"),
            "rich reply: {methods:?}"
        );

        let rich = calls
            .iter()
            .find(|(m, _)| m == "sendRichMessage")
            .expect("rich reply");
        let md = rich.1["rich_message"]["markdown"].as_str().unwrap_or("");
        assert!(md.contains("heard: hello verity"), "reply body: {md}");

        let reactions: Vec<&str> = calls
            .iter()
            .filter(|(m, _)| m == "setMessageReaction")
            .filter_map(|(_, b)| b["reaction"][0]["emoji"].as_str())
            .collect();
        assert_eq!(reactions, ["👀"], "👀 set, then cleared on plain reply");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
