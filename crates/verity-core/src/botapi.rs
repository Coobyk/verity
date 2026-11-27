//! Thin Telegram Bot API client (Bot API 10.x).
//!
//! Only the methods Verity needs; everything is `POST /bot<token>/<method>`
//! with a JSON body. Rich messages (10.1+) carry block-structured content
//! up to 32,768 chars with LaTeX, tables, and collapsible thinking blocks.

use std::path::Path;

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
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub from: Option<User>,
    pub chat: Chat,
    pub date: i64,
    pub text: Option<String>,
    /// Media caption — photos/docs/etc. carry their text here.
    pub caption: Option<String>,
    /// Attached photo, smallest→largest sizes (pick the biggest).
    pub photo: Option<Vec<PhotoSize>>,
    /// Attached file — includes images sent "uncompressed".
    pub document: Option<Document>,
    pub reply_to_message: Option<Box<Message>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PhotoSize {
    pub file_id: String,
    pub width: i64,
    pub height: i64,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Document {
    pub file_id: String,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<i64>,
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
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    error_code: Option<i64>,
    description: Option<String>,
}

fn unwrap<T>(resp: ApiResponse<T>) -> Result<T> {
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
            // 75s > the 30s getUpdates long-poll; a stalled connection
            // must error out instead of hanging the bot loop forever.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(75))
                .build()
                .expect("reqwest client"),
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
        unwrap(resp)
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
        reply_to: Option<i64>,
    ) -> Result<Message> {
        self.send_message_ex(chat_id, text, reply_to, false).await
    }

    /// Same send, but `disable_notification` — arrives with no sound
    /// or badge. For chatter that shouldn't ping (progress notes).
    pub async fn send_message_silent(
        &self,
        chat_id: i64,
        text: &str,
        reply_to: Option<i64>,
    ) -> Result<Message> {
        self.send_message_ex(chat_id, text, reply_to, true).await
    }

    async fn send_message_ex(
        &self,
        chat_id: i64,
        text: &str,
        reply_to: Option<i64>,
        silent: bool,
    ) -> Result<Message> {
        let mut body = json!({ "chat_id": chat_id, "text": text });
        if silent {
            body["disable_notification"] = true.into();
        }
        if let Some(m) = reply_to {
            body["reply_parameters"] =
                json!({"message_id": m, "allow_sending_without_reply": true});
        }
        self.call("sendMessage", &body).await
    }

    /// Plain sendMessage with legacy `parse_mode: Markdown` — good
    /// fallback render for GFM-ish text when sendRichMessage fails.
    pub async fn send_markdown_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_to: Option<i64>,
    ) -> Result<Message> {
        let mut body = json!({ "chat_id": chat_id, "text": text, "parse_mode": "Markdown" });
        if let Some(m) = reply_to {
            body["reply_parameters"] =
                json!({"message_id": m, "allow_sending_without_reply": true});
        }
        self.call("sendMessage", &body).await
    }

    /// Send a rich (block-structured, up to 32k chars, LaTeX/tables) message.
    pub async fn send_rich_message(
        &self,
        chat_id: i64,
        rich: InputRichMessage,
        reply_to: Option<i64>,
    ) -> Result<Message> {
        let mut body = json!({ "chat_id": chat_id, "rich_message": rich });
        if let Some(m) = reply_to {
            body["reply_parameters"] =
                json!({"message_id": m, "allow_sending_without_reply": true});
        }
        self.call("sendRichMessage", &body).await
    }

    /// Upload a file as a document (multipart).
    pub async fn send_document(
        &self,
        chat_id: i64,
        path: &Path,
        caption: Option<&str>,
    ) -> Result<Message> {
        let bytes = std::fs::read(path)?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                "document",
                reqwest::multipart::Part::bytes(bytes).file_name(name),
            );
        if let Some(c) = caption {
            form = form.text("caption", c.to_string());
        }
        let url = format!("{}/bot{}/sendDocument", self.base, self.token);
        let resp: ApiResponse<Message> = self
            .http
            .post(&url)
            .multipart(form)
            .send()
            .await?
            .json()
            .await?;
        unwrap(resp)
    }

    /// Upload an image as a photo (multipart) — displays inline, unlike
    /// [`Self::send_document`].
    pub async fn send_photo(
        &self,
        chat_id: i64,
        path: &Path,
        caption: Option<&str>,
    ) -> Result<Message> {
        let bytes = std::fs::read(path)?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("photo.png")
            .to_string();
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                "photo",
                reqwest::multipart::Part::bytes(bytes).file_name(name),
            );
        if let Some(c) = caption {
            form = form.text("caption", c.to_string());
        }
        let url = format!("{}/bot{}/sendPhoto", self.base, self.token);
        let resp: ApiResponse<Message> = self
            .http
            .post(&url)
            .multipart(form)
            .send()
            .await?
            .json()
            .await?;
        unwrap(resp)
    }

    /// `setMyProfilePhoto` — swap the bot's avatar. The photo ships as a
    /// multipart attachment referenced from the InputProfilePhoto JSON.
    pub async fn set_profile_photo(&self, png: Vec<u8>) -> Result<()> {
        let form = reqwest::multipart::Form::new()
            .text("photo", r#"{"type":"static","photo":"attach://p"}"#)
            .part(
                "p",
                reqwest::multipart::Part::bytes(png).file_name("pfp.png"),
            );
        let url = format!("{}/bot{}/setMyProfilePhoto", self.base, self.token);
        let resp: ApiResponse<serde_json::Value> = self
            .http
            .post(&url)
            .multipart(form)
            .send()
            .await?
            .json()
            .await?;
        unwrap(resp)?;
        Ok(())
    }

    /// `getFile` — resolve a `file_id` to the server-side `file_path`
    /// used by [`Self::download_file`].
    pub async fn file_path(&self, file_id: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct F {
            file_path: Option<String>,
        }
        let f: F = self.call("getFile", &json!({ "file_id": file_id })).await?;
        f.file_path.ok_or_else(|| Error::BotApi {
            code: -1,
            description: "getFile: no file_path".into(),
        })
    }

    /// Download a `file_path` (from `getFile`) to a local `dest` —
    /// `GET {base}/file/bot<token>/<file_path>`.
    pub async fn download_file(&self, file_path: &str, dest: &Path) -> Result<()> {
        let url = format!("{}/file/bot{}/{}", self.base, self.token, file_path);
        let bytes = self.http.get(&url).send().await?.bytes().await?;
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(dest, &bytes)?;
        Ok(())
    }

    /// Stream a partial message while it is being generated.
    /// `draft_id` identifies the draft; same id = animated update.
    /// `can_stop` renders a stop button; `keep_on_stop` keeps the text.
    pub async fn send_message_draft(
        &self,
        chat_id: i64,
        draft_id: i64,
        text: &str,
        can_stop: bool,
    ) -> Result<()> {
        self.call::<bool>(
            "sendMessageDraft",
            &json!({
                "chat_id": chat_id,
                "draft_id": draft_id,
                "text": text,
                "can_stop": can_stop,
            }),
        )
        .await?;
        Ok(())
    }

    /// Stream a rich partial message (thinking blocks, markdown) while generating.
    pub async fn send_rich_message_draft(
        &self,
        chat_id: i64,
        draft_id: i64,
        rich: InputRichMessage,
        can_stop: bool,
    ) -> Result<()> {
        self.call::<bool>(
            "sendRichMessageDraft",
            &json!({
                "chat_id": chat_id,
                "draft_id": draft_id,
                "rich_message": rich,
                "can_stop": can_stop,
            }),
        )
        .await?;
        Ok(())
    }

    /// Edit a previously sent message's text. Used for the single
    /// progress message a long turn keeps updated before deleting it.
    /// Returns `Message`, not `true` (that's only for inline edits).
    pub async fn edit_message(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        self.call::<Message>(
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
    pub async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<()> {
        self.call::<bool>(
            "sendChatAction",
            &json!({ "chat_id": chat_id, "action": action }),
        )
        .await?;
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

    /// Send a prompt with rows of inline buttons — `(label, callback_data)`
    /// per button, one Vec per row. Returns the sent message's id.
    pub async fn send_buttons(
        &self,
        chat_id: i64,
        text: &str,
        rows: &[Vec<(String, String)>],
    ) -> Result<i64> {
        let keyboard: Vec<Vec<serde_json::Value>> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|(label, data)| json!({"text": label, "callback_data": data}))
                    .collect()
            })
            .collect();
        let body = json!({
            "chat_id": chat_id,
            "text": text,
            "reply_markup": {"inline_keyboard": keyboard},
        });
        let m: Message = self.call("sendMessage", &body).await?;
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
}
