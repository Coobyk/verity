//! Thin Telegram Bot API client (Bot API 10.x).
//!
//! Only the methods Verity needs; everything is `POST /bot<token>/<method>`
//! with a JSON body. Rich messages (10.1+) carry block-structured content
//! up to 32,768 chars with LaTeX, tables, and collapsible thinking blocks.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{Error, Result};

const API_BASE: &str = "https://api.telegram.org";

#[derive(Debug, Clone)]
pub struct BotApi {
    token: String,
    http: reqwest::Client,
    base: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    pub is_bot: bool,
    pub first_name: String,
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: Option<String>,
    pub is_forum: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub message_thread_id: Option<i64>,
    pub is_topic_message: Option<bool>,
    pub from: Option<User>,
    pub chat: Chat,
    pub date: i64,
    pub text: Option<String>,
    /// Media caption — photos/docs/etc. carry their text here.
    pub caption: Option<String>,
    pub reply_to_message: Option<Box<Message>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub data: Option<String>,
    pub message: Option<Message>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub edited_message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForumTopic {
    pub message_thread_id: i64,
    pub name: String,
    pub icon_color: i64,
    pub icon_custom_emoji_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    error_code: Option<i64>,
    description: Option<String>,
}

/// A rich message payload for `sendRichMessage`/`sendRichMessageDraft`.
/// Prefer `markdown` (GFM-compatible); `blocks` is the explicit form.
#[derive(Debug, Clone, Serialize)]
pub struct InputRichMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Value>,
}

impl InputRichMessage {
    pub fn markdown(text: impl Into<String>) -> Self {
        Self {
            markdown: Some(text.into()),
            html: None,
            blocks: None,
        }
    }
}

impl BotApi {
    /// `VERITY_BOT_API_BASE` overrides the api.telegram.org base —
    /// for a local Bot API server (or a test mock).
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            http: reqwest::Client::new(),
            base: std::env::var("VERITY_BOT_API_BASE").unwrap_or_else(|_| API_BASE.into()),
        }
    }

    /// Point at a local Bot API server (e.g. http://localhost:8081).
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    async fn call<T: for<'de> Deserialize<'de>>(&self, method: &str, body: &Value) -> Result<T> {
        let url = format!("{}/bot{}/{}", self.base, self.token, method);
        let resp: ApiResponse<T> = self.http.post(&url).json(body).send().await?.json().await?;
        if resp.ok {
            resp.result.ok_or_else(|| Error::BotApi {
                code: -1,
                description: "empty result".into(),
            })
        } else {
            Err(Error::BotApi {
                code: resp.error_code.unwrap_or(-1),
                description: resp.description.unwrap_or_else(|| "unknown".into()),
            })
        }
    }

    pub async fn get_me(&self) -> Result<User> {
        self.call("getMe", &json!({})).await
    }

    /// Long-poll for updates. `timeout` is server-side seconds.
    pub async fn get_updates(&self, offset: i64, timeout: u32) -> Result<Vec<Update>> {
        self.call(
            "getUpdates",
            &json!({
                "offset": offset,
                "timeout": timeout,
                "allowed_updates": ["message", "edited_message", "callback_query"],
            }),
        )
        .await
    }

    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        thread_id: Option<i64>,
    ) -> Result<Message> {
        let mut body = json!({ "chat_id": chat_id, "text": text });
        if let Some(t) = thread_id {
            body["message_thread_id"] = t.into();
        }
        self.call("sendMessage", &body).await
    }

    /// Plain sendMessage with legacy `parse_mode: Markdown` — good
    /// fallback render for GFM-ish text when sendRichMessage fails.
    pub async fn send_markdown_message(
        &self,
        chat_id: i64,
        text: &str,
        thread_id: Option<i64>,
    ) -> Result<Message> {
        let mut body =
            json!({ "chat_id": chat_id, "text": text, "parse_mode": "Markdown" });
        if let Some(t) = thread_id {
            body["message_thread_id"] = t.into();
        }
        self.call("sendMessage", &body).await
    }

    /// Send a rich (block-structured, up to 32k chars, LaTeX/tables) message.
    pub async fn send_rich_message(
        &self,
        chat_id: i64,
        rich: InputRichMessage,
        thread_id: Option<i64>,
    ) -> Result<Message> {
        let mut body = json!({ "chat_id": chat_id, "rich_message": rich });
        if let Some(t) = thread_id {
            body["message_thread_id"] = t.into();
        }
        self.call("sendRichMessage", &body).await
    }

    /// Stream a partial message while it is being generated.
    /// `draft_id` identifies the draft; same id = animated update.
    /// `can_stop` renders a stop button; `keep_on_stop` keeps the text.
    pub async fn send_message_draft(
        &self,
        chat_id: i64,
        draft_id: i64,
        text: &str,
        thread_id: Option<i64>,
        can_stop: bool,
    ) -> Result<()> {
        let mut body = json!({
            "chat_id": chat_id,
            "draft_id": draft_id,
            "text": text,
            "can_stop": can_stop,
        });
        if let Some(t) = thread_id {
            body["message_thread_id"] = t.into();
        }
        self.call::<bool>("sendMessageDraft", &body).await?;
        Ok(())
    }

    /// Stream a rich partial message (thinking blocks, markdown) while generating.
    pub async fn send_rich_message_draft(
        &self,
        chat_id: i64,
        draft_id: i64,
        rich: InputRichMessage,
        thread_id: Option<i64>,
        can_stop: bool,
    ) -> Result<()> {
        let mut body = json!({
            "chat_id": chat_id,
            "draft_id": draft_id,
            "rich_message": rich,
            "can_stop": can_stop,
        });
        if let Some(t) = thread_id {
            body["message_thread_id"] = t.into();
        }
        self.call::<bool>("sendRichMessageDraft", &body).await?;
        Ok(())
    }

    /// Edit a previously sent message's text. Used for the single
    /// progress message a long turn keeps updated before deleting it.
    pub async fn edit_message(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        self.call::<bool>(
            "editMessageText",
            &json!({ "chat_id": chat_id, "message_id": message_id, "text": text }),
        )
        .await?;
        Ok(())
    }

    /// Delete a message (e.g. the progress message once the real reply lands).
    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<()> {
        self.call::<bool>(
            "deleteMessage",
            &json!({ "chat_id": chat_id, "message_id": message_id }),
        )
        .await?;
        Ok(())
    }

    /// "typing" / "upload_document" / etc. Cheap presence, no message.
    pub async fn send_chat_action(
        &self,
        chat_id: i64,
        action: &str,
        thread_id: Option<i64>,
    ) -> Result<()> {
        let mut body = json!({ "chat_id": chat_id, "action": action });
        if let Some(t) = thread_id {
            body["message_thread_id"] = t.into();
        }
        self.call::<bool>("sendChatAction", &body).await?;
        Ok(())
    }

    /// React to a message — the "acknowledged, working" signal with no noise.
    /// Remove all our reactions from a message.
    pub async fn clear_reaction(&self, chat_id: i64, message_id: i64) -> Result<()> {
        self.call::<bool>(
            "setMessageReaction",
            &json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "reaction": [],
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn set_reaction(&self, chat_id: i64, message_id: i64, emoji: &str) -> Result<()> {
        self.call::<bool>(
            "setMessageReaction",
            &json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "reaction": [{ "type": "emoji", "emoji": emoji }],
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn create_forum_topic(&self, chat_id: i64, name: &str) -> Result<ForumTopic> {
        self.call(
            "createForumTopic",
            &json!({ "chat_id": chat_id, "name": name }),
        )
        .await
    }

    pub async fn edit_forum_topic(&self, chat_id: i64, thread_id: i64, name: &str) -> Result<()> {
        self.call::<bool>(
            "editForumTopic",
            &json!({ "chat_id": chat_id, "message_thread_id": thread_id, "name": name }),
        )
        .await?;
        Ok(())
    }

    /// Send a prompt with rows of inline buttons — `(label, callback_data)`
    /// per button, one Vec per row. Returns the sent message's id.
    pub async fn send_buttons(
        &self,
        chat_id: i64,
        text: &str,
        topic_id: Option<i64>,
        rows: &[Vec<(String, String)>],
    ) -> Result<i64> {
        let keyboard: Vec<Vec<serde_json::Value>> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|(label, data)| {
                        json!({"text": label, "callback_data": data})
                    })
                    .collect()
            })
            .collect();
        let m: Message = self
            .call(
                "sendMessage",
                &json!({
                    "chat_id": chat_id,
                    "text": text,
                    "message_thread_id": topic_id,
                    "reply_markup": {"inline_keyboard": keyboard},
                }),
            )
            .await?;
        Ok(m.message_id)
    }

    /// Dismiss the spinner on a clicked inline button; `text` shows a toast.
    pub async fn answer_callback_query(&self, id: &str, text: Option<&str>) -> Result<()> {
        self.call::<bool>(
            "answerCallbackQuery",
            &json!({"callback_query_id": id, "text": text, "show_alert": text.is_some()}),
        )
        .await?;
        Ok(())
    }

    /// Kick a member from the group (unbanChatMember with only_if_banned=false).
    pub async fn kick_member(&self, chat_id: i64, user_id: i64) -> Result<()> {
        self.call::<bool>(
            "unbanChatMember",
            &json!({ "chat_id": chat_id, "user_id": user_id, "only_if_banned": false }),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_forum_topic(&self, chat_id: i64, thread_id: i64) -> Result<()> {
        self.call::<bool>(
            "deleteForumTopic",
            &json!({ "chat_id": chat_id, "message_thread_id": thread_id }),
        )
        .await?;
        Ok(())
    }
}
