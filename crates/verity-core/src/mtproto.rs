//! User-side MTProto client (grammers). Logged in as *you*, it can do what
//! bots can't: read full forum history, list topics, post into topics,
//! act as the GUI's eyes and hands.

use std::path::Path;
use std::sync::Arc;

use grammers_client::types::{Media, Message};
use grammers_client::{Client, InputMessage, SignInError};
use grammers_mtsender::SenderPool;
use grammers_session::defs::PeerRef;
use grammers_session::storages::SqliteSession;
use grammers_tl_types as tl;

use crate::{Error, Result};

pub struct Tg {
    // held so the pool keeps a live session for its lifetime
    #[allow(dead_code)]
    session: Arc<SqliteSession>,
    client: Client,
    api_hash: String,
}

#[derive(Debug, Clone)]
pub struct TopicInfo {
    pub id: i64,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct GroupInfo {
    /// Bot-API style chat id (`-100…` for supergroups, `-…` for basic).
    pub chat_id: i64,
    pub title: String,
    /// `forum` (topics enabled), `group` (megagroup), `chat` (basic group),
    /// `channel` (broadcast).
    pub kind: &'static str,
}

#[derive(Debug, Clone)]
pub struct TopicMessage {
    pub id: i32,
    pub date: i64,
    pub sender: String,
    pub text: String,
}

/// A bot sitting in the group — member or admin. `input` is what
/// `channels.editBanned` needs to kick it.
#[derive(Debug, Clone)]
pub struct BotMember {
    pub user_id: i64,
    pub username: Option<String>,
    pub input: tl::enums::InputPeer,
}

/// What a full-group wipe did.
#[derive(Debug, Default)]
pub struct WipeReport {
    pub bots_kicked: usize,
    pub topics_deleted: usize,
    pub general_messages: usize,
}

/// Normalize a Bot-API-style chat id to the bare peer id grammers uses.
/// Supergroups are `-100<channel_id>`; basic groups are `-<chat_id>`;
/// anything else (usernames, positive ids) passes through unchanged.
pub fn chat_id_to_bare(id: i64) -> i64 {
    if id >= 0 {
        return id;
    }
    let s = id.to_string();
    if let Some(rest) = s.strip_prefix("-100") {
        return rest.parse().unwrap_or(id);
    }
    -id
}

impl Tg {
    /// Connect with a persisted session; may be unauthorized on first run.
    /// Spawns the sender-pool runner in the background — needs a tokio runtime.
    pub async fn connect(api_id: i32, api_hash: String, session_path: &Path) -> Result<Self> {
        let session = Arc::new(
            SqliteSession::open(session_path)
                .map_err(|e| Error::Mtproto(format!("session: {e}")))?,
        );
        crate::config::chmod_private(session_path); // user session = full account access
        let pool = SenderPool::new(session.clone(), api_id);
        let client = Client::new(&pool);
        tokio::spawn(pool.runner.run());
        Ok(Self {
            session,
            client,
            api_hash,
        })
    }

    pub async fn is_authorized(&self) -> Result<bool> {
        self.client
            .is_authorized()
            .await
            .map_err(|e| Error::Mtproto(format!("auth check: {e}")))
    }

    pub fn save_session(&self, _path: &Path) -> Result<()> {
        // SqliteSession persists as it goes; nothing to flush.
        Ok(())
    }

    /// Full login flow. `ask` supplies the code and, if needed, the 2FA password.
    pub async fn sign_in<F>(&self, phone: &str, mut ask: F) -> Result<()>
    where
        F: FnMut(&str) -> Result<String>,
    {
        let token = self
            .client
            .request_login_code(phone, &self.api_hash)
            .await
            .map_err(|e| Error::Mtproto(format!("request code: {e}")))?;
        let code = ask("Login code (from Telegram)")?;
        match self.client.sign_in(&token, &code).await {
            Ok(_) => Ok(()),
            Err(SignInError::PasswordRequired(pw_token)) => {
                let pw = ask("2FA password")?;
                self.client
                    .check_password(pw_token, pw)
                    .await
                    .map_err(|e| Error::Mtproto(format!("2fa: {e}")))?;
                Ok(())
            }
            Err(e) => Err(Error::Mtproto(format!("sign in: {e}"))),
        }
    }

    /// Find a peer among your dialogs by Bot-API chat id or title substring.
    /// Chat ids are normalized (see [`chat_id_to_bare`]) so a `-100…`
    /// `group_id` from `verity config` resolves like the group title does.
    pub async fn peer(&self, id_or_title: &str) -> Result<PeerRef> {
        let want_id = id_or_title.parse::<i64>().ok().map(chat_id_to_bare);
        let mut dialogs = self.client.iter_dialogs();
        while let Some(dialog) = dialogs
            .next()
            .await
            .map_err(|e| Error::Mtproto(format!("dialogs: {e}")))?
        {
            let peer = dialog.peer();
            let id_match = want_id
                .map(|want| peer.id().bare_id() == want)
                .unwrap_or(false);
            if id_match
                || peer
                    .name()
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&id_or_title.to_lowercase())
            {
                return Ok(PeerRef::from(peer));
            }
        }
        Err(Error::Mtproto(format!("no group matches '{id_or_title}'")))
    }

    /// List every group/channel in your dialogs with Bot-API chat ids —
    /// answers "what's my group_id" without external tools.
    pub async fn list_groups(&self) -> Result<Vec<GroupInfo>> {
        use grammers_client::types::Peer;
        let mut out = Vec::new();
        let mut dialogs = self.client.iter_dialogs();
        while let Some(dialog) = dialogs
            .next()
            .await
            .map_err(|e| Error::Mtproto(format!("dialogs: {e}")))?
        {
            let peer = dialog.peer();
            let bare = peer.id().bare_id();
            let info = match peer {
                Peer::Group(g) if g.is_megagroup() => {
                    let is_forum =
                        matches!(&g.raw, tl::enums::Chat::Channel(c) if c.forum);
                    GroupInfo {
                        // supergroup chat_id = -100 concat bare channel id
                        chat_id: -(1_000_000_000_000 + bare),
                        title: peer.name().unwrap_or_default().to_string(),
                        kind: if is_forum { "forum" } else { "group" },
                    }
                }
                Peer::Group(_) => GroupInfo {
                    chat_id: -bare,
                    title: peer.name().unwrap_or_default().to_string(),
                    kind: "chat",
                },
                Peer::Channel(_) => GroupInfo {
                    chat_id: -(1_000_000_000_000 + bare),
                    title: peer.name().unwrap_or_default().to_string(),
                    kind: "channel",
                },
                Peer::User(_) => continue,
            };
            out.push(info);
        }
        Ok(out)
    }

    /// Rename a supergroup/channel via `channels.editTitle`.
    /// (Basic groups would need `messages.editChatTitle` — not needed here.)
    pub async fn rename_chat(&self, peer: PeerRef, title: &str) -> Result<()> {
        self.client
            .invoke(&tl::functions::channels::EditTitle {
                channel: peer.into(),
                title: title.to_string(),
            })
            .await
            .map_err(|e| Error::Mtproto(format!("editTitle: {e}")))?;
        Ok(())
    }

    /// List forum topics of a group via `messages.getForumTopics`.
    pub async fn list_topics(&self, peer: PeerRef) -> Result<Vec<TopicInfo>> {
        let res = self
            .client
            .invoke(&tl::functions::messages::GetForumTopics {
                peer: peer.into(),
                q: None,
                offset_date: 0,
                offset_id: 0,
                offset_topic: 0,
                limit: 100,
            })
            .await
            .map_err(|e| Error::Mtproto(format!("getForumTopics: {e}")))?;

        let tl::enums::messages::ForumTopics::Topics(t) = res;
        Ok(t.topics
            .into_iter()
            .filter_map(|t| match t {
                tl::enums::ForumTopic::Topic(ft) => Some(TopicInfo {
                    id: ft.id as i64,
                    title: ft.title,
                }),
                tl::enums::ForumTopic::Deleted(_) => None,
            })
            .collect())
    }

    /// Read recent messages of one topic via `messages.getReplies`.
    /// Returned oldest-first.
    pub async fn topic_history(
        &self,
        peer: PeerRef,
        topic_id: i64,
        limit: i32,
    ) -> Result<Vec<TopicMessage>> {
        let res = self
            .client
            .invoke(&tl::functions::messages::GetReplies {
                peer: peer.into(),
                msg_id: topic_id as i32,
                offset_id: 0,
                offset_date: 0,
                add_offset: 0,
                limit,
                max_id: 0,
                min_id: 0,
                hash: 0,
            })
            .await
            .map_err(|e| Error::Mtproto(format!("getReplies: {e}")))?;

        use tl::enums::messages::Messages as M;
        let (messages, users) = match res {
            M::Messages(m) => (m.messages, m.users),
            M::Slice(m) => (m.messages, m.users),
            M::ChannelMessages(m) => (m.messages, m.users),
            M::NotModified(_) => (vec![], vec![]),
        };

        let name_of = |id: i64| -> String {
            users
                .iter()
                .find_map(|u| match u {
                    tl::enums::User::User(u) if u.id == id => Some(
                        u.first_name
                            .clone()
                            .filter(|n| !n.is_empty())
                            .or_else(|| u.username.clone())
                            .unwrap_or_default(),
                    ),
                    _ => None,
                })
                .unwrap_or_else(|| id.to_string())
        };

        let mut out: Vec<TopicMessage> = messages
            .into_iter()
            .filter_map(|m| match m {
                tl::enums::Message::Message(m) => {
                    let sender = match m.from_id {
                        Some(tl::enums::Peer::User(p)) => name_of(p.user_id),
                        _ => "group".to_string(),
                    };
                    // Media-only messages carry no text — mark them so
                    // "what do you see in this image" still has context.
                    let text = if m.message.is_empty() {
                        match &m.media {
                            Some(tl::enums::MessageMedia::Photo(_)) => "[image]".into(),
                            Some(tl::enums::MessageMedia::Document(_)) => "[file]".into(),
                            Some(_) => "[media]".into(),
                            None => String::new(),
                        }
                    } else {
                        m.message
                    };
                    Some(TopicMessage {
                        id: m.id,
                        date: m.date as i64,
                        sender,
                        text,
                    })
                }
                _ => None,
            })
            .collect();
        out.reverse(); // API returns newest-first
        Ok(out)
    }

    /// Newest message per file name among documents posted to the
    /// group's General topic. Scans the `scan` most recent channel
    /// messages (newest first); stops early once every name matched.
    pub async fn find_general_documents(
        &self,
        peer: PeerRef,
        want: &[&str],
        scan: usize,
    ) -> Result<Vec<(String, Message)>> {
        let mut found: Vec<(String, Message)> = Vec::new();
        let mut it = self.client.iter_messages(peer);
        let mut scanned = 0;
        while let Some(m) = it
            .next()
            .await
            .map_err(|e| Error::Mtproto(format!("messages: {e}")))?
        {
            scanned += 1;
            if scanned > scan || found.len() == want.len() {
                break;
            }
            // General is forum topic 1; pre-forum messages carry no
            // reply_to at all, which is the same stream.
            let in_general = match &m.raw {
                tl::enums::Message::Message(tl::types::Message { reply_to, .. }) => match reply_to {
                    None => true,
                    Some(tl::enums::MessageReplyHeader::Header(h)) => {
                        h.reply_to_top_id.is_none_or(|t| t == 1)
                    }
                    _ => false,
                },
                _ => false,
            };
            if !in_general {
                continue;
            }
            let Some(Media::Document(doc)) = m.media() else {
                continue;
            };
            let name = doc.name().to_string();
            if want.contains(&name.as_str()) && !found.iter().any(|(n, _)| n == &name) {
                found.push((name, m));
            }
        }
        Ok(found)
    }

    /// Wait for the next message from `peer` with id > `after_id`.
    /// Polls `iter_messages` (newest-first) up to ~30s.
    async fn wait_reply(&self, peer: PeerRef, after_id: i32) -> Result<String> {
        Ok(self
            .wait_reply_msg(peer, after_id)
            .await?
            .text()
            .to_string())
    }

    /// [`wait_reply`] but keeps the [`Message`] — needed when the
    /// reply's inline keyboard (not just its text) carries the answer.
    async fn wait_reply_msg(&self, peer: PeerRef, after_id: i32) -> Result<Message> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let mut it = self.client.iter_messages(peer);
            while let Some(m) = it
                .next()
                .await
                .map_err(|e| Error::Mtproto(format!("messages: {e}")))?
            {
                if m.id() <= after_id {
                    break;
                }
                // Skip our own outgoing messages — we want *their* reply.
                if !m.outgoing() && !m.text().is_empty() {
                    return Ok(m);
                }
            }
            if std::time::Instant::now() > deadline {
                return Err(Error::Mtproto("timed out waiting for reply".into()));
            }
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        }
    }

    /// Wait for BotFather's post-pick prompt: a new incoming message
    /// after `picker_id`, or the picker itself edited in place (button
    /// clicks sometimes update the same message). Polls up to ~30s.
    async fn wait_confirm(&self, peer: PeerRef, picker_id: i32) -> Result<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let mut it = self.client.iter_messages(peer);
            while let Some(m) = it
                .next()
                .await
                .map_err(|e| Error::Mtproto(format!("messages: {e}")))?
            {
                if m.id() > picker_id && !m.outgoing() && !m.text().is_empty() {
                    return Ok(m.text().to_string());
                }
                if m.id() == picker_id && m.text().to_lowercase().contains("sure") {
                    return Ok(m.text().to_string());
                }
                if m.id() < picker_id {
                    break;
                }
            }
            if std::time::Instant::now() > deadline {
                return Err(Error::Mtproto("timed out waiting for reply".into()));
            }
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        }
    }

    /// Every label on a message's inline keyboard, row by row.
    fn button_labels(msg: &Message) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        if let Some(tl::enums::ReplyMarkup::ReplyInlineMarkup(mk)) = msg.reply_markup() {
            for row in mk.rows {
                let tl::enums::KeyboardButtonRow::Row(row) = row;
                for b in row.buttons {
                    match b {
                        tl::enums::KeyboardButton::Callback(b) => {
                            out.push((b.text, b.data));
                        }
                        tl::enums::KeyboardButton::Url(b) => out.push((b.text, Vec::new())),
                        tl::enums::KeyboardButton::Button(b) => out.push((b.text, Vec::new())),
                        _ => {}
                    }
                }
            }
        }
        out
    }

    async fn last_msg_id(&self, peer: PeerRef) -> i32 {
        self.client
            .iter_messages(peer)
            .next()
            .await
            .ok()
            .flatten()
            .map(|m| m.id())
            .unwrap_or(0)
    }

    /// Automate @BotFather: `/newbot` → name → username → token.
    /// Returns the issued HTTP API token.
    pub async fn create_bot(&self, name: &str, username: &str) -> Result<String> {
        let bf = self
            .client
            .resolve_username("botfather")
            .await
            .map_err(|e| Error::Mtproto(format!("resolve: {e}")))?
            .ok_or_else(|| Error::Mtproto("can't resolve @BotFather".into()))?;
        let bf_ref = PeerRef::from(&bf);

        let mut last = self.last_msg_id(bf_ref).await;
        self.client
            .send_message(&bf, InputMessage::new().text("/newbot"))
            .await
            .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
        self.wait_reply(bf_ref, last).await?; // "choose a name"

        last = self.last_msg_id(bf_ref).await;
        self.client
            .send_message(&bf, InputMessage::new().text(name))
            .await
            .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
        let ask = self.wait_reply(bf_ref, last).await?; // "choose a username" or error

        last = self.last_msg_id(bf_ref).await;
        self.client
            .send_message(&bf, InputMessage::new().text(username))
            .await
            .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
        let done = self.wait_reply(bf_ref, last).await?;

        // Token looks like `123456789:AAA…` (~35 url-safe chars after ':').
        let token = done
            .split_whitespace()
            .find(|w| {
                w.split_once(':')
                    .map(|(a, b)| {
                        a.chars().all(|c| c.is_ascii_digit())
                            && b.len() >= 30
                            && b.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    })
                    .unwrap_or(false)
            })
            .map(|w| w.trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-'));
        match token {
            Some(t) => Ok(t.to_string()),
            // Username taken etc. — surface BotFather's own words.
            None => Err(Error::Mtproto(format!("botfather: {ask} / {done}"))),
        }
    }

    /// Labels of the bot-picker keyboard @BotFather shows for `/mybots`
    /// — the whole fleet you own, not just registered teammates.
    pub async fn my_bots(&self) -> Result<Vec<String>> {
        let bf = self.botfather().await?;
        let bf_ref = PeerRef::from(&bf);
        let last = self.last_msg_id(bf_ref).await;
        self.client
            .send_message(&bf, InputMessage::new().text("/mybots"))
            .await
            .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
        let picker = self.wait_reply_msg(bf_ref, last).await?;
        Ok(Self::button_labels(&picker)
            .into_iter()
            .map(|(label, _)| label)
            .collect())
    }

    /// Automate @BotFather: `/deletebot` → pick the bot → the quoted
    /// confirmation phrase ("Yes, I am totally sure."). Irreversible —
    /// the bot dies and its username is freed. `name` matches a picker
    /// button label; if none matches it's sent as `@name` text, which
    /// BotFather also accepts.
    pub async fn delete_bot(&self, name: &str) -> Result<()> {
        let bf = self.botfather().await?;
        let bf_ref = PeerRef::from(&bf);

        let mut last = self.last_msg_id(bf_ref).await;
        self.client
            .send_message(&bf, InputMessage::new().text("/deletebot"))
            .await
            .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
        let picker = self.wait_reply_msg(bf_ref, last).await?;
        let picker_id = picker.id();

        // Click the picker button whose label mentions the bot; fall
        // back to typing @name when there is no keyboard or no match.
        let want = name.trim_start_matches('@').to_lowercase();
        let data = Self::button_labels(&picker)
            .into_iter()
            .find(|(label, _)| label.to_lowercase().contains(&want))
            .map(|(_, data)| data);
        match data {
            Some(data) if !data.is_empty() => {
                self.client
                    .invoke(&tl::functions::messages::GetBotCallbackAnswer {
                        game: false,
                        peer: bf_ref.into(),
                        msg_id: picker.id(),
                        data: Some(data),
                        password: None,
                    })
                    .await
                    .map_err(|e| Error::Mtproto(format!("callback: {e}")))?;
            }
            _ => {
                self.client
                    .send_message(&bf, InputMessage::new().text(format!("@{want}")))
                    .await
                    .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
            }
        }

        // "OK, you selected @x_bot. Are you sure? Send 'Yes, I am
        // totally sure.' to confirm…" — echo the quoted phrase back.
        // BotFather may send it fresh *or* edit the picker in place.
        let ask = self
            .wait_confirm(bf_ref, picker_id)
            .await?;
        let phrase = ask
            .split('\'')
            .nth(1)
            .map(str::to_string)
            .unwrap_or_else(|| "Yes, I am totally sure.".into());
        last = self.last_msg_id(bf_ref).await;
        self.client
            .send_message(&bf, InputMessage::new().text(phrase))
            .await
            .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
        let done = self.wait_reply(bf_ref, last).await?;
        if done.to_lowercase().contains("done") {
            Ok(())
        } else {
            Err(Error::Mtproto(format!("botfather: {ask} / {done}")))
        }
    }

    async fn botfather(&self) -> Result<grammers_client::types::Peer> {
        self.client
            .resolve_username("botfather")
            .await
            .map_err(|e| Error::Mtproto(format!("resolve: {e}")))?
            .ok_or_else(|| Error::Mtproto("can't resolve @BotFather".into()))
    }

    /// Invite a bot into the group and grant it admin rights
    /// (manage topics etc. — needed to own a forum topic).
    pub async fn invite_bot_admin(&self, group: PeerRef, bot_username: &str) -> Result<()> {
        let bot = self
            .client
            .resolve_username(bot_username.trim_start_matches('@'))
            .await
            .map_err(|e| Error::Mtproto(format!("resolve: {e}")))?
            .ok_or_else(|| Error::Mtproto(format!("can't resolve @{bot_username}")))?;
        let bot_ref = PeerRef::from(&bot);
        self.client
            .invoke(&tl::functions::channels::InviteToChannel {
                channel: group.into(),
                users: vec![bot_ref.into()],
            })
            .await
            .map_err(|e| Error::Mtproto(format!("invite: {e}")))?;
        self.client
            .invoke(&tl::functions::channels::EditAdmin {
                channel: group.into(),
                user_id: bot_ref.into(),
                admin_rights: tl::enums::ChatAdminRights::Rights(tl::types::ChatAdminRights {
                    change_info: false,
                    post_messages: true,
                    edit_messages: true,
                    delete_messages: true,
                    ban_users: false,
                    invite_users: false,
                    pin_messages: true,
                    add_admins: false,
                    anonymous: false,
                    manage_call: false,
                    other: true,
                    manage_topics: true,
                    post_stories: false,
                    edit_stories: false,
                    delete_stories: false,
                    manage_direct_messages: false,
                }),
                rank: String::new(),
            })
            .await
            .map_err(|e| Error::Mtproto(format!("editAdmin: {e}")))?;
        Ok(())
    }

    /// Every bot member of the group — `channels.getParticipants` with
    /// the bots filter sees admins too, which is exactly what a reset
    /// needs (teammates hold admin rights to own topics).
    pub async fn list_bot_members(&self, peer: PeerRef) -> Result<Vec<BotMember>> {
        let res = self
            .client
            .invoke(&tl::functions::channels::GetParticipants {
                channel: peer.into(),
                filter: tl::enums::ChannelParticipantsFilter::ChannelParticipantsBots,
                offset: 0,
                limit: 200,
                hash: 0,
            })
            .await
            .map_err(|e| Error::Mtproto(format!("getParticipants: {e}")))?;
        let tl::enums::channels::ChannelParticipants::Participants(p) = res else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for u in p.users {
            let tl::enums::User::User(u) = u else { continue };
            if !u.bot {
                continue;
            }
            let Some(access_hash) = u.access_hash else { continue };
            out.push(BotMember {
                user_id: u.id,
                username: u.username.clone(),
                input: tl::enums::InputPeer::User(tl::types::InputPeerUser {
                    user_id: u.id,
                    access_hash,
                }),
            });
        }
        Ok(out)
    }

    /// Kick a member via `channels.editBanned` — works on admins too
    /// (the Bot API's unban can't touch them). The ban carries a 60s
    /// expiry: kicked now, but re-invitable later.
    pub async fn kick_member(&self, peer: PeerRef, participant: tl::enums::InputPeer) -> Result<()> {
        let until_date = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i32 + 60)
            .unwrap_or(0);
        self.client
            .invoke(&tl::functions::channels::EditBanned {
                channel: peer.into(),
                participant,
                banned_rights: tl::enums::ChatBannedRights::Rights(
                    tl::types::ChatBannedRights {
                        view_messages: true,
                        send_messages: false,
                        send_media: false,
                        send_stickers: false,
                        send_gifs: false,
                        send_games: false,
                        send_inline: false,
                        embed_links: false,
                        send_polls: false,
                        change_info: false,
                        invite_users: false,
                        pin_messages: false,
                        manage_topics: false,
                        send_photos: false,
                        send_videos: false,
                        send_roundvideos: false,
                        send_audios: false,
                        send_voices: false,
                        send_docs: false,
                        send_plain: false,
                        until_date,
                    },
                ),
            })
            .await
            .map_err(|e| Error::Mtproto(format!("editBanned: {e}")))?;
        Ok(())
    }

    /// Delete a forum topic and its whole history (user-side
    /// `messages.deleteTopicHistory` — the Bot API's deleteForumTopic
    /// equivalent for when no bot token is handy).
    pub async fn delete_topic(&self, peer: PeerRef, topic_id: i64) -> Result<()> {
        self.client
            .invoke(&tl::functions::messages::DeleteTopicHistory {
                peer: peer.into(),
                top_msg_id: topic_id as i32,
            })
            .await
            .map_err(|e| Error::Mtproto(format!("deleteTopicHistory: {e}")))?;
        Ok(())
    }

    /// Ids of every message in the General stream — the one forum
    /// "topic" that can't be deleted wholesale. Service posts (joins,
    /// topic actions) count; they're part of the same stream.
    pub async fn general_message_ids(&self, peer: PeerRef) -> Result<Vec<i32>> {
        let mut ids = Vec::new();
        let mut it = self.client.iter_messages(peer);
        while let Some(m) = it
            .next()
            .await
            .map_err(|e| Error::Mtproto(format!("messages: {e}")))?
        {
            let reply_to = match &m.raw {
                tl::enums::Message::Message(m) => &m.reply_to,
                tl::enums::Message::Service(m) => &m.reply_to,
                _ => continue,
            };
            // Pre-forum posts carry no reply_to; General posts point at
            // topic 1. Everything else lives in a deletable topic.
            let in_general = match reply_to {
                None => true,
                Some(tl::enums::MessageReplyHeader::Header(h)) => {
                    h.reply_to_top_id.is_none_or(|t| t == 1)
                }
                _ => false,
            };
            if in_general {
                ids.push(m.id());
            }
        }
        Ok(ids)
    }

    /// Delete channel messages by id, 100 per call — user-side, so any
    /// message goes, not just ones we sent.
    pub async fn delete_messages(&self, peer: PeerRef, ids: &[i32]) -> Result<usize> {
        let mut n = 0;
        for chunk in ids.chunks(100) {
            self.client
                .invoke(&tl::functions::channels::DeleteMessages {
                    channel: peer.into(),
                    id: chunk.to_vec(),
                })
                .await
                .map_err(|e| Error::Mtproto(format!("deleteMessages: {e}")))?;
            n += chunk.len();
        }
        Ok(n)
    }

    /// Nuke the group back to empty: kick every bot member except
    /// `keep_user_id`, delete every forum topic except General, and
    /// purge General's stream. Everything a "wipe all messages + all
    /// bots" reset needs, done user-side because bots can't see enough.
    pub async fn wipe_group(&self, peer: PeerRef, keep_user_id: Option<i64>) -> WipeReport {
        let mut rep = WipeReport::default();
        if let Ok(members) = self.list_bot_members(peer).await {
            for m in members {
                if Some(m.user_id) == keep_user_id {
                    continue;
                }
                if self.kick_member(peer, m.input.clone()).await.is_ok() {
                    rep.bots_kicked += 1;
                }
            }
        }
        if let Ok(topics) = self.list_topics(peer).await {
            for t in topics {
                if t.id != 1 && self.delete_topic(peer, t.id).await.is_ok() {
                    rep.topics_deleted += 1;
                }
            }
        }
        if let Ok(ids) = self.general_message_ids(peer).await {
            rep.general_messages = self.delete_messages(peer, &ids).await.unwrap_or(0);
        }
        rep
    }

    /// Post a text message into a forum topic *as the logged-in user*.
    /// The topic id doubles as the thread's root `reply_to` message id.
    /// Pass `None` to post to the group's general stream.
    pub async fn send_message(
        &self,
        peer: PeerRef,
        topic_id: Option<i64>,
        text: &str,
    ) -> Result<()> {
        self.client
            .send_message(
                peer,
                InputMessage::new()
                    .text(text)
                    .reply_to(topic_id.map(|t| t as i32)),
            )
            .await
            .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_supergroup_prefix() {
        assert_eq!(chat_id_to_bare(-1001234567890), 1234567890);
    }

    #[test]
    fn strips_basic_group_sign() {
        assert_eq!(chat_id_to_bare(-987654321), 987654321);
    }

    #[test]
    fn passes_positive_ids() {
        assert_eq!(chat_id_to_bare(1234567890), 1234567890);
    }
}
