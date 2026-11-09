//! User-side MTProto client (grammers). Logged in as *you*, it can do what
//! bots can't: read full forum history, list topics, act as the GUI's eyes.

use std::path::Path;
use std::sync::Arc;

use grammers_client::{Client, SignInError};
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
pub struct TopicMessage {
    pub id: i32,
    pub date: i64,
    pub sender: String,
    pub text: String,
}

impl Tg {
    /// Connect with a persisted session; may be unauthorized on first run.
    /// Spawns the sender-pool runner in the background — needs a tokio runtime.
    pub async fn connect(api_id: i32, api_hash: String, session_path: &Path) -> Result<Self> {
        let session = Arc::new(
            SqliteSession::open(session_path)
                .map_err(|e| Error::Mtproto(format!("session: {e}")))?,
        );
        let pool = SenderPool::new(session.clone(), api_id);
        let client = Client::new(&pool);
        tokio::spawn(pool.runner.run());
        Ok(Self { session, client, api_hash })
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

    /// Find a peer among your dialogs by bot-api-style id or title substring.
    async fn find_peer(&self, id_or_title: &str) -> Result<PeerRef> {
        let mut dialogs = self.client.iter_dialogs();
        while let Some(dialog) = dialogs
            .next()
            .await
            .map_err(|e| Error::Mtproto(format!("dialogs: {e}")))?
        {
            let peer = dialog.peer();
            let title = peer.name().unwrap_or_default();
            let id_match = id_or_title
                .parse::<i64>()
                .map(|want| peer.id().bare_id() == want)
                .unwrap_or(false);
            if id_match || title.to_lowercase().contains(&id_or_title.to_lowercase()) {
                return Ok(PeerRef::from(peer));
            }
        }
        Err(Error::Mtproto(format!("no group matches '{id_or_title}'")))
    }

    /// List forum topics of a group via `messages.getForumTopics`.
    pub async fn list_topics(&self, group: &str) -> Result<Vec<TopicInfo>> {
        let peer = self.find_peer(group).await?;
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
    pub async fn topic_history(
        &self,
        group: &str,
        topic_id: i64,
        limit: i32,
    ) -> Result<Vec<TopicMessage>> {
        let peer = self.find_peer(group).await?;
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
}
