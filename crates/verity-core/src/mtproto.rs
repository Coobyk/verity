//! User-side MTProto client (grammers). Logged in as *you*, it can do what
//! bots can't: read full forum history, list topics, post into topics,
//! act as the GUI's eyes and hands.

use std::path::Path;
use std::sync::Arc;

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
                        u.username
                            .clone()
                            .unwrap_or_else(|| u.first_name.clone().unwrap_or_default()),
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
                    Some(TopicMessage {
                        id: m.id,
                        date: m.date as i64,
                        sender,
                        text: m.message,
                    })
                }
                _ => None,
            })
            .collect();
        out.reverse(); // API returns newest-first
        Ok(out)
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
