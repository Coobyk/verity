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
        let db = self.db_path.clone();
        let bots = tokio::task::spawn_blocking(move || -> Result<Vec<Bot>> {
            Store::open(&db)?.list_bots()
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))??;
        if bots.is_empty() {
            return Err(Error::Config(
                "no bots registered — `verity bot new`".into(),
            ));
        }
        let mut handles = Vec::new();
        for bot in bots {
            let env = env.clone();
            handles.push(tokio::spawn(async move {
                loop {
                    match run_bot(&bot, &env).await {
                        Ok(()) => return, // clean exit (only if polling ends gracefully)
                        Err(e) => {
                            warn!(bot = %bot.name, error = %e, "bot loop failed, restarting in 10s");
                            tokio::time::sleep(Duration::from_secs(10)).await;
                        }
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.await;
        }
        Ok(())
    }
}

async fn run_bot(bot: &Bot, env: &Env) -> Result<()> {
    let api = BotApi::new(&bot.token);
    let me = api.get_me().await?;
    info!(bot = %bot.name, username = ?me.username, "polling");

    // Ensure the bot owns a forum topic named after it.
    let topic_id = match bot.topic_id {
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

    loop {
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
            let Some(msg) = update.message else { continue };
            if msg.chat.id != env.group_id || msg.message_thread_id != Some(topic_id) {
                continue;
            }
            if msg.from.as_ref().map(|u| u.is_bot).unwrap_or(true) {
                continue; // never react to bots (incl. ourselves)
            }
            let Some(text) = msg.text.clone().or_else(|| msg.caption.clone()) else {
                continue;
            };
            debug!(bot = %bot.name, %text, "turn request");
            if let Err(e) = handle_turn(&api, bot, env, topic_id, msg.message_id, &text).await {
                warn!(bot = %bot.name, error = %e, "turn failed");
                let _ = api.set_reaction(env.group_id, msg.message_id, "⚠️").await;
                let _ = api
                    .send_message(
                        env.group_id,
                        "Something went wrong on my side.",
                        Some(topic_id),
                    )
                    .await;
            }
        }
    }
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
            match (verb, parsed) {
                ("remember", Some((k, v))) if !k.is_empty() && !v.is_empty() => {
                    markers.remember.push((k.into(), v.into()));
                }
                ("tell", Some((n, m))) if !n.is_empty() && !m.is_empty() => {
                    markers.tell.push((n.into(), m.into()));
                }
                ("spawn", Some((l, t))) if !l.is_empty() && !t.is_empty() => {
                    markers.spawn.push((l.into(), t.into()));
                }
                _ => {} // malformed or unknown marker: strip it anyway
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
                debug!(error = %e, "rich send failed, falling back to text");
                for chunk in split_markdown(&part, TEXT_LIMIT) {
                    api.send_message(group_id, &chunk, Some(topic_id)).await?;
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
    if result.is_ok() {
        let _ = api.set_reaction(env.group_id, message_id, "✅").await;
    }
    result
}

/// One turn of agent work for `bot` in `topic_id`: context → harness →
/// markers → memory, helpers, tells → rich reply. Shared by human turns
/// (depth 0, via `handle_turn`) and `tell`-routed teammate turns.
async fn do_turn(env: &Env, bot: &Bot, topic_id: i64, text: &str, depth: u8) -> Result<()> {
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
    let ctx = TurnContext {
        bot_name: bot.name.clone(),
        instructions: bot.instructions.clone(),
        memory,
        history,
        peers,
        cwd: std::env::current_dir().unwrap_or_else(|_| "/".into()),
        model: bot.model.clone(),
    };

    // 2. Work.
    let out = harness.run_turn(&ctx, text).await?;
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

    // 6. Tells: route work to teammates. Detached so this turn's reply
    //    isn't gated on another bot's runtime; depth-capped to keep
    //    bot-to-bot loops finite.
    if depth < MAX_TELL_DEPTH && !markers.tell.is_empty() {
        for (name, msg) in markers.tell.into_iter().take(MAX_TELLS) {
            let (target, env2) = (find_bot(&env.db, &name).await, env.clone());
            match target {
                Some(target) if target.name != bot.name => {
                    let origin = bot.name.clone();
                    tokio::spawn(async move {
                        let prompt = format!("Message from teammate {origin}: {msg}");
                        if let Err(e) = turn(&env2, &target, topic_id, &prompt, depth + 1).await {
                            warn!(bot = %target.name, error = %e, "tell turn failed");
                        }
                    });
                }
                _ => warn!(bot = %bot.name, teammate = %name, "tell: unknown teammate"),
            }
        }
    }
    Ok(())
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
) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
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
        assert_eq!(reactions, ["👀", "✅"], "reaction sequence");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
