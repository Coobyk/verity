//! User-side MTProto client (grammers). Logged in as *you*, it can do what
//! bots can't: read full DM history, clear chats, automate BotFather,
//! act as the GUI's eyes and hands.

use std::path::Path;
use std::sync::Arc;

use grammers_client::client::updates::UpdateStream;
use grammers_client::types::{Media, Message, PasswordToken};
use grammers_client::{Client, InputMessage, UpdatesConfiguration};
use grammers_mtsender::{InvocationError, SenderPool};
use grammers_session::defs::{PeerAuth, PeerInfo, PeerRef, UpdateState, UpdatesState};
use grammers_session::storages::SqliteSession;
use grammers_session::Session;
use grammers_tl_types as tl;

use crate::{Error, Result};

/// How many recent chat messages a document search scans.
const RESTORE_SCAN_LIMIT: usize = 500;

pub struct Tg {
    // held so the pool keeps a live session for its lifetime
    session: Arc<SqliteSession>,
    client: Client,
    // the pool's update feed — claimed at connect so socket events
    // aren't dropped on the floor; `updates()` drains it
    updates: tokio::sync::Mutex<UpdateStream>,
    api_id: i32,
    api_hash: String,
}

/// A login code Telegram dispatched — where it went plus the hash
/// `auth.signIn` needs to verify it.
struct PendingCode {
    phone_code_hash: String,
    code_type: tl::enums::auth::SentCodeType,
    next_type: Option<tl::enums::auth::CodeType>,
}

/// Public, stepwise login challenge for GUIs. The code hash stays
/// opaque inside the core; the display strings tell the user where Telegram
/// delivered the code and what channel a resend would use.
pub struct LoginChallenge {
    pending: PendingCode,
    pub delivery: String,
    pub next_delivery: Option<String>,
}

impl From<PendingCode> for LoginChallenge {
    fn from(pending: PendingCode) -> Self {
        Self {
            delivery: describe_code_type(&pending.code_type),
            next_delivery: pending
                .next_type
                .as_ref()
                .map(describe_next_type)
                .map(str::to_string),
            pending,
        }
    }
}

/// Result of submitting a Telegram login code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginCodeResult {
    Done,
    PasswordNeeded,
}

impl From<tl::types::auth::SentCode> for PendingCode {
    fn from(c: tl::types::auth::SentCode) -> Self {
        Self {
            phone_code_hash: c.phone_code_hash,
            code_type: c.r#type,
            next_type: c.next_type,
        }
    }
}

fn describe_code_type(t: &tl::enums::auth::SentCodeType) -> String {
    use tl::enums::auth::SentCodeType as T;
    match t {
        T::App(_) => "the Telegram app (service chat named 'Telegram')".into(),
        T::Sms(_) | T::FragmentSms(_) | T::FirebaseSms(_) => "SMS".into(),
        T::SmsWord(_) | T::SmsPhrase(_) => "SMS".into(),
        T::Call(_) => "a phone call".into(),
        T::FlashCall(_) => "a flash call".into(),
        T::MissedCall(_) => "a missed call".into(),
        T::EmailCode(e) => format!("email ({})", e.email_pattern),
        T::SetUpEmailRequired(_) => "email (setup required)".into(),
    }
}

fn describe_next_type(t: &tl::enums::auth::CodeType) -> &'static str {
    use tl::enums::auth::CodeType as T;
    match t {
        T::Sms | T::FragmentSms => "SMS",
        T::Call => "a phone call",
        T::FlashCall => "a flash call",
        T::MissedCall => "a missed call",
    }
}

/// One reaction pill on a message — the emoji, how many people set
/// it, and whether the logged-in user is one of them.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReactionInfo {
    pub emoji: String,
    pub count: i32,
    pub chosen: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TopicMessage {
    pub id: i32,
    pub date: i64,
    pub sender: String,
    /// Bare user id of the author, when the message names one — the
    /// GUI uses it to tell "my" messages from everyone else's.
    pub sender_id: Option<i64>,
    pub text: String,
    /// Formatting spans over `text` — Telegram message entities with
    /// UTF-16 offsets already converted to byte offsets.
    pub spans: Vec<FmtSpan>,
    pub reactions: Vec<ReactionInfo>,
}

/// The styling one entity applies to a slice of message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FmtKind {
    Bold,
    Italic,
    Underline,
    Strike,
    /// Inline monospace.
    Code,
    /// Multiline code block.
    Pre,
    Spoiler,
    Blockquote,
    /// Clickable link — `url` carries the target.
    Link,
    /// Colored but unactionable: mentions, hashtags, commands.
    Accent,
    CustomEmoji,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FmtSpan {
    pub kind: FmtKind,
    /// Byte offset into `TopicMessage.text`.
    pub start: usize,
    /// Byte length of the span.
    pub len: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `Pre` fence language ("math", "rust", …) when Telegram sent one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

/// One row of the account's chat list — the GUI's sidebar. Unlike
/// [`GroupInfo`] this covers user DMs too, so bot conversations
/// show up alongside groups.
#[derive(Debug, Clone)]
pub struct DialogInfo {
    /// Bot-API style chat id — same shape [`GroupInfo::chat_id`].
    pub chat_id: i64,
    pub title: String,
    pub peer: PeerRef,
    /// True when this dialog is a bot account.
    pub is_bot: bool,
    /// `private` for user DMs; `forum`/`group`/`chat`/`channel`
    /// otherwise.
    pub kind: &'static str,
}

/// A normalized nudge for the GUI — enough to know which chat
/// stream to refresh.
#[derive(Debug, Clone)]
pub enum TgEvent {
    /// A new or edited message landed in `chat_hint` (Bot-API style
    /// id — the DM partner's user id for private chats).
    Message { chat_hint: i64, msg: TopicMessage },
    /// Anything else Telegram pushed (reads, typing, pins…).
    Other,
}

/// One sticker of a fetched set, downloaded to `path`.
#[derive(Debug)]
pub struct PackSticker {
    /// Associated emoji (the `alt` of the sticker attribute).
    pub emoji: String,
    pub mime: String,
    pub path: std::path::PathBuf,
}

/// A sticker set fetched via `messages.getStickerSet`, with every
/// sticker downloaded to disk.
#[derive(Debug)]
pub struct StickerPack {
    pub title: String,
    pub short_name: String,
    pub count: i32,
    /// `emojis` flag — a custom-emoji set rather than a sticker pack.
    pub is_emoji_set: bool,
    pub stickers: Vec<PackSticker>,
}

/// `Downloadable` adapter for a bare `tl::types::Document` — grammers'
/// own impl sits on its `media::Document` wrapper, which a
/// `messages.StickerSet` result doesn't come in.
struct DocFile(tl::types::Document);

impl grammers_client::types::Downloadable for DocFile {
    fn to_raw_input_location(&self) -> Option<tl::enums::InputFileLocation> {
        Some(
            tl::types::InputDocumentFileLocation {
                id: self.0.id,
                access_hash: self.0.access_hash,
                file_reference: self.0.file_reference.clone(),
                thumb_size: String::new(),
            }
            .into(),
        )
    }

    fn size(&self) -> Option<usize> {
        Some(self.0.size as usize)
    }
}

/// A message's renderable chunks — styled text or a formula image.
pub enum Blk {
    /// `text` is the message slice; spans are rebased to it and
    /// paired with their original start (the spoiler reveal key).
    Text {
        text: String,
        spans: Vec<(usize, FmtSpan)>,
    },
    Math {
        tex: String,
        display: bool,
    },
}

/// Split message text into text/latex blocks. Delimiters: `$$…$$` and
/// `\[…\]` (display), `\(…\)` and `$…$` (inline, pandoc-style — the
/// closing `$` needs a non-space left neighbour and no digit right
/// neighbour). Spans marked code/pre are off-limits; a `Pre` fence
/// with `lang` in {math, latex, tex} becomes a formula directly.
pub fn split_math(m: &TopicMessage) -> Vec<Blk> {
    let text = &m.text;
    let b = text.as_bytes();
    let n = b.len();

    // (block_start, block_end, tex_start, tex_end, display)
    let mut maths: Vec<(usize, usize, usize, usize, bool)> = Vec::new();
    let mut code_ranges: Vec<(usize, usize)> = Vec::new();
    for s in &m.spans {
        match s.kind {
            FmtKind::Pre
                if s.lang.as_deref().is_some_and(|l| {
                    matches!(l.to_ascii_lowercase().as_str(), "math" | "latex" | "tex")
                }) =>
            {
                maths.push((s.start, s.start + s.len, s.start, s.start + s.len, true));
            }
            FmtKind::Code | FmtKind::Pre => code_ranges.push((s.start, s.start + s.len)),
            _ => {}
        }
    }
    let covered = |pos: usize, maths: &[(usize, usize, usize, usize, bool)]| -> bool {
        code_ranges.iter().any(|(a, e)| pos >= *a && pos < *e)
            || maths.iter().any(|(a, e, ..)| pos >= *a && pos < *e)
    };

    let is_space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r');
    // Pandoc's inline rule: non-space inside both delimiters, and the
    // char after the close must not be a digit (kills "$5 and $10").
    let tight = |from: usize, to: usize| -> bool {
        from < to && !is_space(b[from]) && !is_space(b[to - 1])
    };
    let close_ok =
        |j: usize, clen: usize| -> bool { j + clen >= n || !b[j + clen].is_ascii_digit() };
    // First occurrence of `close` at/after `from`, skipping
    // backslash-escaped candidates. Byte-wise: the walk position is
    // not necessarily a char boundary.
    let find_close = |close: &[u8], mut from: usize| -> Option<usize> {
        loop {
            let pos = from + b[from..].windows(close.len()).position(|w| w == close)?;
            if pos > 0 && b[pos - 1] == b'\\' {
                from = pos + close.len();
            } else {
                return Some(pos);
            }
        }
    };

    let mut i = 0;
    while i < n {
        if covered(i, &maths) {
            i += 1;
            continue;
        }
        let two = &b[i..(i + 2).min(n)];
        let (clen, disp, close): (usize, bool, &[u8]) = match two {
            b"$$" => (2, true, b"$$"),
            b"\\[" => (2, true, b"\\]"),
            b"\\(" => (2, false, b"\\)"),
            _ if b[i] == b'$' => (1, false, b"$"),
            _ => {
                i += 1;
                continue;
            }
        };
        let body_start = i + clen;
        let Some(j) = find_close(close, body_start) else {
            i += 1;
            continue;
        };
        if !tight(body_start, j) || !close_ok(j, close.len()) {
            i += 1;
            continue;
        }
        maths.push((i, j + close.len(), body_start, j, disp));
        i = j + close.len();
    }

    maths.sort_by_key(|m| m.0);
    let mut out = Vec::new();
    let mut cur = 0;
    let emit_text = |a: usize, e: usize, out: &mut Vec<Blk>| {
        if a >= e || m.text[a..e].is_empty() {
            return;
        }
        let spans: Vec<(usize, FmtSpan)> = m
            .spans
            .iter()
            .filter_map(|s| {
                let start = s.start.max(a);
                let end = (s.start + s.len).min(e);
                (end > start).then(|| {
                    (
                        s.start,
                        FmtSpan {
                            kind: s.kind,
                            start: start - a,
                            len: end - start,
                            url: s.url.clone(),
                            lang: s.lang.clone(),
                        },
                    )
                })
            })
            .collect();
        out.push(Blk::Text {
            text: m.text[a..e].to_string(),
            spans,
        });
    };
    for (bs, be, ts, te, disp) in maths {
        if bs < cur {
            continue; // nested inside an earlier region
        }
        emit_text(cur, bs, &mut out);
        let tex = m.text[ts..te].trim();
        if !tex.is_empty() {
            out.push(Blk::Math {
                tex: tex.to_string(),
                display: disp,
            });
        }
        cur = be;
    }
    emit_text(cur, n, &mut out);
    out
}

/// `MessageEntity` offsets are UTF-16 code units — walk the UTF-8
/// text counting UTF-16 length per char to find the byte position.
fn utf16_to_byte(text: &str, utf16_pos: i32) -> usize {
    let mut acc = 0i32;
    for (byte_ix, ch) in text.char_indices() {
        if acc >= utf16_pos {
            return byte_ix;
        }
        acc += ch.len_utf16() as i32;
    }
    text.len()
}

/// `Message.entities` → byte-offset formatting spans for the GUI.
/// Bare URL/email entities link to their own text; unactionable
/// entities (mentions, hashtags, commands) collapse to `Accent`.
fn fmt_spans(entities: Option<&Vec<tl::enums::MessageEntity>>, text: &str) -> Vec<FmtSpan> {
    use tl::enums::MessageEntity as E;
    let Some(entities) = entities else {
        return Vec::new();
    };
    entities
        .iter()
        .filter_map(|e| {
            let kind = match e {
                E::Bold(_) => FmtKind::Bold,
                E::Italic(_) => FmtKind::Italic,
                E::Underline(_) => FmtKind::Underline,
                E::Strike(_) => FmtKind::Strike,
                E::Code(_) => FmtKind::Code,
                E::Pre(_) => FmtKind::Pre,
                E::Spoiler(_) => FmtKind::Spoiler,
                E::Blockquote(_) => FmtKind::Blockquote,
                E::CustomEmoji(_) => FmtKind::CustomEmoji,
                E::TextUrl(_) | E::Url(_) | E::Email(_) => FmtKind::Link,
                E::Mention(_)
                | E::Hashtag(_)
                | E::Cashtag(_)
                | E::BotCommand(_)
                | E::Phone(_)
                | E::BankCard(_)
                | E::MentionName(_)
                | E::InputMessageEntityMentionName(_) => FmtKind::Accent,
                E::Unknown(_) => return None,
            };
            let start = utf16_to_byte(text, e.offset());
            let end = utf16_to_byte(text, e.offset() + e.length());
            if end <= start {
                return None;
            }
            let url = match e {
                E::TextUrl(t) => Some(t.url.clone()),
                E::Url(_) => Some(text[start..end].to_string()),
                E::Email(_) => Some(format!("mailto:{}", &text[start..end])),
                _ => None,
            };
            let lang = match e {
                E::Pre(p) if !p.language.is_empty() => Some(p.language.clone()),
                _ => None,
            };
            Some(FmtSpan {
                kind,
                start,
                len: end - start,
                url,
                lang,
            })
        })
        .collect()
}

/// `grammers` Message → the `reactions` field of the underlying TL
/// object; service/empty variants carry none.
fn raw_reactions(m: &tl::enums::Message) -> Option<&tl::enums::MessageReactions> {
    match m {
        tl::enums::Message::Message(m) => m.reactions.as_ref(),
        _ => None,
    }
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
        // Claim the pool's update feed — with no receiver holding it,
        // socket events would be dropped silently. `catch_up` replays
        // messages that arrived while we were offline.
        let updates = client.stream_updates(
            pool.updates,
            UpdatesConfiguration {
                catch_up: true,
                ..Default::default()
            },
        );
        tokio::spawn(pool.runner.run());
        Ok(Self {
            session,
            client,
            updates: tokio::sync::Mutex::new(updates),
            api_id,
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

    /// `auth.sendCode`, with the 303 DC-migration retry grammers does.
    async fn send_code(&self, phone: &str) -> Result<PendingCode> {
        let request = tl::functions::auth::SendCode {
            phone_number: phone.to_string(),
            api_id: self.api_id,
            api_hash: self.api_hash.clone(),
            settings: tl::types::CodeSettings {
                allow_flashcall: false,
                current_number: false,
                allow_app_hash: false,
                allow_missed_call: false,
                allow_firebase: false,
                logout_tokens: None,
                token: None,
                app_sandbox: None,
                unknown_number: false,
            }
            .into(),
        };
        match self.client.invoke(&request).await {
            Ok(tl::enums::auth::SentCode::Code(c)) => Ok(c.into()),
            Ok(_) => Err(Error::Mtproto("sendCode: unexpected response".into())),
            Err(InvocationError::Rpc(e)) if e.code == 303 => {
                // Account lives on another DC — move home and retry.
                self.session
                    .set_home_dc_id(e.value.unwrap_or_default() as i32);
                match self.client.invoke(&request).await {
                    Ok(tl::enums::auth::SentCode::Code(c)) => Ok(c.into()),
                    Ok(_) => Err(Error::Mtproto("sendCode: unexpected response".into())),
                    Err(e) => Err(Error::Mtproto(format!("request code: {e}"))),
                }
            }
            Err(e) => Err(Error::Mtproto(format!("request code: {e}"))),
        }
    }

    /// `auth.resendCode` — Telegram re-delivers through the next
    /// channel (typically SMS when the first code went in-app).
    async fn resend_code(&self, phone: &str, hash: &str) -> Result<PendingCode> {
        match self
            .client
            .invoke(&tl::functions::auth::ResendCode {
                phone_number: phone.to_string(),
                phone_code_hash: hash.to_string(),
                reason: None,
            })
            .await
        {
            Ok(tl::enums::auth::SentCode::Code(c)) => Ok(c.into()),
            Ok(_) => Err(Error::Mtproto("resendCode: unexpected response".into())),
            Err(e) => Err(Error::Mtproto(format!("resend code: {e}"))),
        }
    }

    /// Replicates grammers' private `complete_login`: cache our own
    /// peer (that flag is what `is_authorized` checks) and seed the
    /// update state.
    async fn complete_login(&self, auth: tl::types::auth::Authorization) -> Result<()> {
        let update_state = self
            .client
            .invoke(&tl::functions::updates::GetState {})
            .await
            .ok();
        let tl::enums::User::User(u) = auth.user else {
            return Err(Error::Mtproto("sign in: no user in authorization".into()));
        };
        self.session.cache_peer(&PeerInfo::User {
            id: u.id,
            auth: u.access_hash.map(PeerAuth::from_hash),
            bot: Some(u.bot),
            is_self: Some(true),
        });
        if let Some(tl::enums::updates::State::State(s)) = update_state {
            self.session
                .set_update_state(UpdateState::All(UpdatesState {
                    pts: s.pts,
                    qts: s.qts,
                    date: s.date,
                    seq: s.seq,
                    channels: Vec::new(),
                }));
        }
        Ok(())
    }

    /// Start a stepwise login flow: send the login code and return
    /// the challenge a UI needs for `login_submit_code`.
    pub async fn login_send_code(&self, phone: &str) -> Result<LoginChallenge> {
        self.send_code(phone).await.map(LoginChallenge::from)
    }

    /// Re-deliver the login code through Telegram's next available channel.
    pub async fn login_resend_code(
        &self,
        phone: &str,
        challenge: &LoginChallenge,
    ) -> Result<LoginChallenge> {
        self.resend_code(phone, &challenge.pending.phone_code_hash)
            .await
            .map(LoginChallenge::from)
    }

    /// Submit the code Telegram sent. `PasswordNeeded` means the account
    /// has two-factor authentication; finish with `login_submit_password`.
    pub async fn login_submit_code(
        &self,
        phone: &str,
        challenge: &LoginChallenge,
        code: &str,
    ) -> Result<LoginCodeResult> {
        match self
            .client
            .invoke(&tl::functions::auth::SignIn {
                phone_number: phone.to_string(),
                phone_code_hash: challenge.pending.phone_code_hash.clone(),
                phone_code: Some(code.trim().to_string()),
                email_verification: None,
            })
            .await
        {
            Ok(tl::enums::auth::Authorization::Authorization(a)) => {
                self.complete_login(a).await?;
                Ok(LoginCodeResult::Done)
            }
            Ok(tl::enums::auth::Authorization::SignUpRequired(_)) => Err(Error::Mtproto(
                "this number isn't registered on Telegram".into(),
            )),
            Err(e) if e.is("SESSION_PASSWORD_NEEDED") => Ok(LoginCodeResult::PasswordNeeded),
            Err(e) if e.is("PHONE_CODE_*") => Err(Error::Mtproto("wrong code".into())),
            Err(e) => Err(Error::Mtproto(format!("sign in: {e}"))),
        }
    }

    /// Finish a login after Telegram reports `SESSION_PASSWORD_NEEDED`.
    pub async fn login_submit_password(&self, password: &str) -> Result<()> {
        let pw_info: tl::types::account::Password = self
            .client
            .invoke(&tl::functions::account::GetPassword {})
            .await
            .map_err(|e| Error::Mtproto(format!("2fa: {e}")))?
            .into();
        self.client
            .check_password(PasswordToken::new(pw_info), password)
            .await
            .map_err(|e| Error::Mtproto(format!("2fa: {e}")))?;
        Ok(())
    }

    /// Full login flow. `ask` supplies the code and, if needed, the 2FA
    /// password. Reports where Telegram delivered the code; answering
    /// `resend` at the code prompt re-delivers via the next channel
    /// (usually SMS).
    pub async fn sign_in<F>(&self, phone: &str, mut ask: F) -> Result<()>
    where
        F: FnMut(&str) -> Result<String>,
    {
        let mut sent = self.send_code(phone).await?;
        println!("code sent via {}", describe_code_type(&sent.code_type));
        if let Some(t) = &sent.next_type {
            println!(
                "no code? answer 'resend' to send it again via {}",
                describe_next_type(t)
            );
        }
        let code = loop {
            let input = ask("Login code (from Telegram)")?;
            let input = input.trim().to_string();
            if input.eq_ignore_ascii_case("resend") {
                match self.resend_code(phone, &sent.phone_code_hash).await {
                    Ok(s) => {
                        sent = s;
                        println!("code sent via {}", describe_code_type(&sent.code_type));
                    }
                    Err(e) => eprintln!(
                        "resend failed: {e}\n\
                         (Telegram is likely rate-limiting this number — wait a while,\n\
                         and the code already sent may still be valid)"
                    ),
                }
                continue;
            }
            break input;
        };
        match self
            .client
            .invoke(&tl::functions::auth::SignIn {
                phone_number: phone.to_string(),
                phone_code_hash: sent.phone_code_hash,
                phone_code: Some(code),
                email_verification: None,
            })
            .await
        {
            Ok(tl::enums::auth::Authorization::Authorization(a)) => self.complete_login(a).await,
            Ok(tl::enums::auth::Authorization::SignUpRequired(_)) => Err(Error::Mtproto(
                "this number isn't registered on Telegram".into(),
            )),
            Err(e) if e.is("SESSION_PASSWORD_NEEDED") => {
                let pw = ask("2FA password")?;
                let pw_info: tl::types::account::Password = self
                    .client
                    .invoke(&tl::functions::account::GetPassword {})
                    .await
                    .map_err(|e| Error::Mtproto(format!("2fa: {e}")))?
                    .into();
                self.client
                    .check_password(PasswordToken::new(pw_info), pw)
                    .await
                    .map_err(|e| Error::Mtproto(format!("2fa: {e}")))?;
                Ok(())
            }
            Err(e) if e.is("PHONE_CODE_*") => Err(Error::Mtproto("wrong code".into())),
            Err(e) => Err(Error::Mtproto(format!("sign in: {e}"))),
        }
    }

    /// Find a peer among your dialogs by Bot-API chat id or title
    /// substring. Chat ids are normalized (see [`chat_id_to_bare`]) so
    /// a `-100…` id resolves like a title does.
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
        Err(Error::Mtproto(format!("no dialog matches '{id_or_title}'")))
    }

    /// Resolve a bare user id to a [`PeerRef`] by scanning the dialog
    /// list — how the GUI opens a DM with a bot knowing only its id
    /// (the dialogs fetch supplies the access hash).
    pub async fn peer_for_user(&self, user_id: i64) -> Result<PeerRef> {
        use grammers_client::types::Peer;
        let mut dialogs = self.client.iter_dialogs();
        while let Some(dialog) = dialogs
            .next()
            .await
            .map_err(|e| Error::Mtproto(format!("dialogs: {e}")))?
        {
            let peer = dialog.peer();
            if matches!(peer, Peer::User(_)) && peer.id().bare_id() == user_id {
                return Ok(PeerRef::from(peer));
            }
        }
        Err(Error::Mtproto(format!("no dialog for user {user_id}")))
    }

    /// Every dialog in the account's chat list — the GUI's sidebar:
    /// user DMs (bots included), groups and channels alike.
    pub async fn list_dialogs(&self) -> Result<Vec<DialogInfo>> {
        use grammers_client::types::Peer;
        let mut out = Vec::new();
        let mut dialogs = self.client.iter_dialogs();
        while let Some(dialog) = dialogs
            .next()
            .await
            .map_err(|e| Error::Mtproto(format!("dialogs: {e}")))?
        {
            let peer = dialog.peer();
            let (is_bot, kind) = match peer {
                Peer::User(u) => (u.is_bot(), "private"),
                Peer::Group(g) if g.is_megagroup() => {
                    let is_forum = matches!(&g.raw, tl::enums::Chat::Channel(c) if c.forum);
                    (false, if is_forum { "forum" } else { "group" })
                }
                Peer::Group(_) => (false, "chat"),
                Peer::Channel(_) => (false, "channel"),
            };
            out.push(DialogInfo {
                chat_id: peer.id().bot_api_dialog_id(),
                title: peer.name().unwrap_or_default().to_string(),
                peer: PeerRef::from(peer),
                is_bot,
                kind,
            });
        }
        Ok(out)
    }

    /// Our own user id — used to gate bot DMs to the owner.
    pub async fn self_id(&self) -> Result<i64> {
        let me = self
            .client
            .get_me()
            .await
            .map_err(|e| Error::Mtproto(format!("get_me: {e}")))?;
        match me.raw {
            tl::enums::User::User(u) => Ok(u.id),
            _ => Err(Error::Mtproto("get_me: not a user".into())),
        }
    }

    /// Recent history of a private chat with `peer`, oldest-first —
    /// the transcript a DM turn carries as context.
    pub async fn dm_history(&self, peer: PeerRef, limit: i32) -> Result<Vec<TopicMessage>> {
        use grammers_client::types::Media;
        let mut it = self.client.iter_messages(peer);
        let mut out: Vec<TopicMessage> = Vec::new();
        while let Some(m) = it
            .next()
            .await
            .map_err(|e| Error::Mtproto(format!("messages: {e}")))?
        {
            if out.len() >= limit as usize {
                break;
            }
            let from = m.sender();
            let sender_id = from.as_ref().map(|s| s.id().bare_id());
            let sender = from
                .and_then(|s| {
                    s.name()
                        .or_else(|| s.username())
                        .map(str::to_string)
                        .or_else(|| Some(s.id().bare_id().to_string()))
                })
                .unwrap_or_else(|| "dm".to_string());
            let text = if m.text().is_empty() {
                match m.media() {
                    Some(Media::Photo(_)) => "[image]".into(),
                    Some(Media::Document(_)) => "[file]".into(),
                    Some(_) => "[media]".into(),
                    None => continue,
                }
            } else {
                m.text().to_string()
            };
            let spans = fmt_spans(m.fmt_entities(), &text);
            out.push(TopicMessage {
                id: m.id(),
                date: m.date().timestamp(),
                sender,
                sender_id,
                text,
                spans,
                reactions: Self::reaction_list(raw_reactions(&m.raw)),
            });
        }
        out.reverse(); // iter_messages is newest-first
        Ok(out)
    }

    /// Delete every message in a private chat — user-side with revoke,
    /// so the bot's messages go too. `/clearchat` for a DM.
    pub async fn clear_chat(&self, peer: PeerRef) -> Result<usize> {
        let mut ids = Vec::new();
        let mut it = self.client.iter_messages(peer);
        while let Some(m) = it
            .next()
            .await
            .map_err(|e| Error::Mtproto(format!("messages: {e}")))?
        {
            ids.push(m.id());
        }
        self.delete_any_messages(peer, &ids).await
    }

    /// Delete arbitrary message ids from a chat (user-side, with
    /// revoke — works on our own messages and, in a private DM, the
    /// other side's too).
    pub async fn delete_any_messages(&self, peer: PeerRef, ids: &[i32]) -> Result<usize> {
        let mut n = 0;
        for chunk in ids.chunks(100) {
            n += self
                .client
                .delete_messages(peer, chunk)
                .await
                .map_err(|e| Error::Mtproto(format!("deleteMessages: {e}")))?;
        }
        Ok(n)
    }

    /// Newest document in a chat whose file name passes `pred`.
    pub async fn find_document(
        &self,
        peer: PeerRef,
        pred: impl Fn(&str) -> bool,
    ) -> Result<Option<Message>> {
        let mut it = self.client.iter_messages(peer);
        let mut scanned = 0;
        while let Some(m) = it
            .next()
            .await
            .map_err(|e| Error::Mtproto(format!("messages: {e}")))?
        {
            scanned += 1;
            if scanned > RESTORE_SCAN_LIMIT {
                break;
            }
            let Some(Media::Document(doc)) = m.media() else {
                continue;
            };
            if pred(doc.name()) {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }

    /// Newest message per file name among documents in a chat. Scans
    /// the `scan` most recent messages (newest first); stops early
    /// once every name matched.
    pub async fn find_documents(
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
                            && b.chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    })
                    .unwrap_or(false)
            })
            .map(|w| {
                w.trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            });
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
        let ask = self.wait_confirm(bf_ref, picker_id).await?;
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

    /// Automate @BotFather: `/newapp` → pick the bot → title →
    /// description → cover photo → host URL → short name. Returns
    /// BotFather's final reply (contains the t.me/<bot>/<short> link).
    /// `bot` matches a picker button label, like in [`Self::delete_bot`].
    pub async fn create_mini_app(
        &self,
        bot: &str,
        title: &str,
        description: &str,
        photo: &Path,
        url: &str,
        short_name: &str,
    ) -> Result<String> {
        let bf = self.botfather().await?;
        let bf_ref = PeerRef::from(&bf);

        let mut last = self.last_msg_id(bf_ref).await;
        self.client
            .send_message(&bf, InputMessage::new().text("/newapp"))
            .await
            .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
        let picker = self.wait_reply_msg(bf_ref, last).await?;
        let picker_id = picker.id();

        let want = bot.trim_start_matches('@').to_lowercase();
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

        // Post-pick prompt — fresh message or edited picker. BotFather's
        // /newapp order isn't fixed (photo can come last), so each reply
        // decides the next input instead of walking a rigid sequence.
        let mut prompt = self.wait_confirm(bf_ref, picker_id).await?;
        let mut photo_sent = false;
        for _ in 0..10 {
            if prompt.contains("t.me/") && prompt.contains(&format!("/{short_name}")) {
                return Ok(prompt);
            }
            let l = prompt.to_lowercase();
            if l.contains("sorry")
                || l.contains("taken")
                || l.contains("occupied")
                || l.contains("invalid")
            {
                return Err(Error::Mtproto(format!(
                    "botfather rejected a step: {prompt}"
                )));
            }
            last = self.last_msg_id(bf_ref).await;
            if l.contains("gif") || l.contains("skip") {
                // Optional demo-GIF step — decline it.
                self.send_line(&bf, "/empty").await?;
            } else if l.contains("photo") {
                // BotFather wants the cover as a real photo, not a document.
                let uploaded = self
                    .client
                    .upload_file(photo)
                    .await
                    .map_err(|e| Error::Mtproto(format!("upload: {e}")))?;
                self.client
                    .send_message(&bf, InputMessage::new().photo(uploaded))
                    .await
                    .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
                photo_sent = true;
            } else if l.contains("title") {
                self.send_line(&bf, title).await?;
            } else if l.contains("description") {
                let d = if description.is_empty() {
                    "/empty"
                } else {
                    description
                };
                self.send_line(&bf, d).await?;
            } else if l.contains("url") || l.contains("link") || l.contains("address") {
                self.send_line(&bf, url).await?;
            } else if l.contains("name") {
                self.send_line(&bf, short_name).await?;
            } else {
                return Err(Error::Mtproto(format!("botfather, stuck: {prompt}")));
            }
            prompt = self.wait_reply(bf_ref, last).await?;
        }
        Err(Error::Mtproto(format!(
            "botfather: never finished (photo_sent={photo_sent}, last: {prompt})"
        )))
    }

    /// Wait until a BotFather view is replaced: a new incoming message
    /// after `msg_id`, or `msg_id` itself edited (text or keyboard —
    /// button clicks usually update the same message). Polls ~30s.
    async fn wait_change(
        &self,
        peer: PeerRef,
        msg_id: i32,
        old_text: &str,
        old_labels: &[String],
    ) -> Result<Message> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let mut it = self.client.iter_messages(peer);
            while let Some(m) = it
                .next()
                .await
                .map_err(|e| Error::Mtproto(format!("messages: {e}")))?
            {
                if m.id() > msg_id && !m.outgoing() && !m.text().is_empty() {
                    return Ok(m);
                }
                if m.id() == msg_id {
                    let labels: Vec<String> = Self::button_labels(&m)
                        .into_iter()
                        .map(|(l, _)| l)
                        .collect();
                    if m.text() != old_text || labels != old_labels {
                        return Ok(m);
                    }
                }
                if m.id() < msg_id {
                    break;
                }
            }
            if std::time::Instant::now() > deadline {
                return Err(Error::Mtproto("timed out waiting for view change".into()));
            }
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        }
    }

    /// Click the first inline-callback button on `msg` whose label
    /// contains `needle` (case-insensitive) and wait for the view it
    /// opens. Errs listing the available labels when nothing matches.
    async fn click_and_wait(&self, peer: PeerRef, msg: &Message, needle: &str) -> Result<Message> {
        let want = needle.to_lowercase();
        let labels = Self::button_labels(msg);
        let hit = labels
            .iter()
            .find(|(label, data)| !data.is_empty() && label.to_lowercase().contains(&want));
        let Some((_, data)) = hit else {
            let all: Vec<String> = labels.into_iter().map(|(l, _)| l).collect();
            return Err(Error::Mtproto(format!(
                "no '{needle}' button (have: {})\nview: {}",
                all.join(", "),
                msg.text()
            )));
        };
        let mid = msg.id();
        let old_text = msg.text().to_string();
        let old_labels: Vec<String> = labels.iter().map(|(l, _)| l.clone()).collect();
        self.client
            .invoke(&tl::functions::messages::GetBotCallbackAnswer {
                game: false,
                peer: peer.into(),
                msg_id: mid,
                data: Some(data.clone()),
                password: None,
            })
            .await
            .map_err(|e| Error::Mtproto(format!("callback: {e}")))?;
        self.wait_change(peer, mid, &old_text, &old_labels).await
    }

    /// Navigate @BotFather's `/myapps` to one app's card: pick the bot
    /// (skipped when the first view is already the app list), then the
    /// app. Both match picker buttons by substring, case-insensitive.
    async fn mini_app_card_msg(&self, bot: &str, app: &str) -> Result<Message> {
        let bf = self.botfather().await?;
        let bf_ref = PeerRef::from(&bf);
        let last = self.last_msg_id(bf_ref).await;
        self.client
            .send_message(&bf, InputMessage::new().text("/myapps"))
            .await
            .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
        let picker = self.wait_reply_msg(bf_ref, last).await?;
        let want_bot = bot.trim_start_matches('@').to_lowercase();
        let first = Self::button_labels(&picker);
        let mut view = if first
            .iter()
            .any(|(l, _)| l.to_lowercase().contains(&want_bot))
        {
            self.click_and_wait(bf_ref, &picker, bot).await?
        } else {
            picker
        };
        // Picking the bot may land straight on an app card — step back
        // to the app list before choosing when `app` isn't here.
        let has = |m: &Message, n: &str| {
            Self::button_labels(m)
                .iter()
                .any(|(l, _)| l.to_lowercase().contains(&n.to_lowercase()))
        };
        if !has(&view, app) && has(&view, "back") {
            view = self.click_and_wait(bf_ref, &view, "back").await?;
        }
        self.click_and_wait(bf_ref, &view, app).await
    }

    /// The `/myapps` card for one mini app — text plus button labels,
    /// so the registered URL is visible without changing anything.
    pub async fn mini_app_card(&self, bot: &str, app: &str) -> Result<String> {
        let card = self.mini_app_card_msg(bot, app).await?;
        let labels: Vec<String> = Self::button_labels(&card)
            .into_iter()
            .map(|(l, _)| l)
            .collect();
        Ok(format!("{}\n[buttons: {}]", card.text(), labels.join(", ")))
    }

    /// `/myapps` → bot → app → Edit → URL: repoint where the mini app
    /// is served from. Returns BotFather's confirmation.
    pub async fn mini_app_set_url(&self, bot: &str, app: &str, url: &str) -> Result<String> {
        let bf = self.botfather().await?;
        let bf_ref = PeerRef::from(&bf);
        let mut view = self.mini_app_card_msg(bot, app).await?;
        // The URL control sits either on the card itself or one level
        // into an "Edit" menu.
        if !Self::button_labels(&view)
            .iter()
            .any(|(l, _)| l.to_lowercase().contains("url"))
        {
            view = self.click_and_wait(bf_ref, &view, "edit").await?;
        }
        self.click_and_wait(bf_ref, &view, "url").await?;
        let last = self.last_msg_id(bf_ref).await;
        self.send_line(&bf, url).await?;
        self.wait_reply(bf_ref, last).await
    }

    /// Send a raw text line to @BotFather (mid-flow repairs);
    /// returns his reply.
    pub async fn bf_send(&self, text: &str) -> Result<String> {
        let bf = self.botfather().await?;
        let bf_ref = PeerRef::from(&bf);
        let last = self.last_msg_id(bf_ref).await;
        self.send_line(&bf, text).await?;
        self.wait_reply(bf_ref, last).await
    }

    /// One text message to a peer — shorthand for the BotFather flows.
    async fn send_line(&self, peer: &grammers_client::types::Peer, text: &str) -> Result<()> {
        self.client
            .send_message(peer, InputMessage::new().text(text))
            .await
            .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
        Ok(())
    }

    async fn botfather(&self) -> Result<grammers_client::types::Peer> {
        self.client
            .resolve_username("botfather")
            .await
            .map_err(|e| Error::Mtproto(format!("resolve: {e}")))?
            .ok_or_else(|| Error::Mtproto("can't resolve @BotFather".into()))
    }

    /// Post a text message into a chat *as the logged-in user*.
    pub async fn send_message(&self, peer: PeerRef, text: &str) -> Result<()> {
        self.client
            .send_message(peer, InputMessage::new().text(text))
            .await
            .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
        Ok(())
    }

    /// Upload `path` and post it into a chat *as the logged-in user* —
    /// MTProto, so it sails past the Bot API's 50 MB document cap.
    pub async fn send_file(&self, peer: PeerRef, path: &Path, caption: &str) -> Result<()> {
        let uploaded = self
            .client
            .upload_file(path)
            .await
            .map_err(|e| Error::Mtproto(format!("upload: {e}")))?;
        self.client
            .send_message(peer, InputMessage::new().text(caption).file(uploaded))
            .await
            .map_err(|e| Error::Mtproto(format!("send: {e}")))?;
        Ok(())
    }

    /// Rebuild a `NewMessage`/`MessageEdited` payload into the
    /// `(id, date, sender, text)` shape history calls return, so a
    /// live event slots into the same view as fetched history.
    fn topic_message(m: &Message) -> TopicMessage {
        let sender_id = m.sender().map(|s| s.id().bare_id());
        let sender = m
            .sender()
            .and_then(|s| {
                s.name()
                    .or_else(|| s.username())
                    .map(str::to_string)
                    .or_else(|| Some(s.id().bare_id().to_string()))
            })
            .unwrap_or_else(|| "unknown".to_string());
        let text = if m.text().is_empty() {
            match m.media() {
                Some(Media::Photo(_)) => "[image]".into(),
                Some(Media::Document(_)) => "[file]".into(),
                Some(_) => "[media]".into(),
                None => String::new(),
            }
        } else {
            m.text().to_string()
        };
        let spans = fmt_spans(m.fmt_entities(), &text);
        TopicMessage {
            id: m.id(),
            date: m.date().timestamp(),
            sender,
            sender_id,
            text,
            spans,
            reactions: Self::reaction_list(raw_reactions(&m.raw)),
        }
    }

    /// `Message.reactions` → flat pills. Custom emoji get a neutral
    /// glyph (the sticker itself needs a document fetch we don't do);
    /// paid reactions collapse to a star.
    fn reaction_list(r: Option<&tl::enums::MessageReactions>) -> Vec<ReactionInfo> {
        let Some(tl::enums::MessageReactions::Reactions(r)) = r else {
            return Vec::new();
        };
        r.results
            .iter()
            .filter_map(|rc| {
                let tl::enums::ReactionCount::Count(rc) = rc;
                if rc.count <= 0 {
                    return None;
                }
                let emoji = match &rc.reaction {
                    tl::enums::Reaction::Emoji(e) => e.emoticon.clone(),
                    tl::enums::Reaction::CustomEmoji(_) => "✨".into(),
                    tl::enums::Reaction::Paid => "⭐".into(),
                    _ => return None,
                };
                Some(ReactionInfo {
                    emoji,
                    count: rc.count,
                    chosen: rc.chosen_order.is_some(),
                })
            })
            .collect()
    }

    /// Set (or with `None`, clear) the caller's reaction on a message.
    /// Telegram keeps one reaction set per user — the last send wins.
    pub async fn send_reaction(
        &self,
        peer: PeerRef,
        msg_id: i32,
        emoji: Option<&str>,
    ) -> Result<()> {
        self.client
            .invoke(&tl::functions::messages::SendReaction {
                big: false,
                add_to_recent: true,
                peer: peer.into(),
                msg_id,
                reaction: emoji.map(|e| {
                    vec![tl::enums::Reaction::Emoji(tl::types::ReactionEmoji {
                        emoticon: e.to_string(),
                    })]
                }),
            })
            .await
            .map_err(|e| Error::Mtproto(format!("sendReaction: {e}")))?;
        Ok(())
    }

    /// Fetch the sticker set a message's sticker belongs to: set meta
    /// plus every sticker document downloaded into `dir` as
    /// `NN_<emoji>.<ext>`.
    pub async fn sticker_pack(
        &self,
        peer: PeerRef,
        msg_id: i32,
        dir: &Path,
    ) -> Result<StickerPack> {
        let mut it = self.client.iter_messages(peer);
        let mut input = None;
        while let Some(m) = it
            .next()
            .await
            .map_err(|e| Error::Mtproto(format!("messages: {e}")))?
        {
            if m.id() != msg_id {
                continue;
            }
            input = match m.media() {
                Some(Media::Sticker(s)) => Some(s.raw_attrs.stickerset.clone()),
                Some(Media::Document(d)) => match &d.raw.document {
                    Some(tl::enums::Document::Document(doc)) => {
                        doc.attributes.iter().find_map(|a| match a {
                            tl::enums::DocumentAttribute::Sticker(s) => Some(s.stickerset.clone()),
                            tl::enums::DocumentAttribute::CustomEmoji(s) => {
                                Some(s.stickerset.clone())
                            }
                            _ => None,
                        })
                    }
                    _ => None,
                },
                _ => None,
            };
            break;
        }
        let stickerset =
            input.ok_or_else(|| Error::Mtproto(format!("message {msg_id} isn't a sticker")))?;
        let res = self
            .client
            .invoke(&tl::functions::messages::GetStickerSet {
                stickerset,
                hash: 0,
            })
            .await
            .map_err(|e| Error::Mtproto(format!("getStickerSet: {e}")))?;
        let tl::enums::messages::StickerSet::Set(ss) = res else {
            return Err(Error::Mtproto("sticker set not modified".into()));
        };
        let tl::enums::StickerSet::Set(set) = ss.set;
        std::fs::create_dir_all(dir)?;
        let mut stickers = Vec::new();
        for (i, d) in ss.documents.iter().enumerate() {
            let tl::enums::Document::Document(doc) = d else {
                continue;
            };
            let emoji = doc
                .attributes
                .iter()
                .find_map(|a| match a {
                    tl::enums::DocumentAttribute::Sticker(s) => Some(s.alt.clone()),
                    tl::enums::DocumentAttribute::CustomEmoji(s) => Some(s.alt.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let ext = match doc.mime_type.as_str() {
                "image/webp" => "webp",
                "video/webm" => "webm",
                "application/x-tgsticker" => "tgs",
                _ => "bin",
            };
            let path = dir.join(format!("{i:02}_{emoji}.{ext}"));
            self.client
                .download_media(&DocFile(doc.clone()), &path)
                .await
                .map_err(|e| Error::Mtproto(format!("download: {e}")))?;
            stickers.push(PackSticker {
                emoji,
                mime: doc.mime_type.clone(),
                path,
            });
        }
        Ok(StickerPack {
            title: set.title,
            short_name: set.short_name,
            count: set.count,
            is_emoji_set: set.emojis,
            stickers,
        })
    }

    /// Drain the update queue — every event received since the last
    /// poll, normalized for the GUI ("something new in chat X topic
    /// Y — refresh it"). `UpdateStream::next` blocks until the socket
    /// delivers; the short timeout turns it into a drain: queued
    /// events pop instantly and one empty wait marks the queue dry.
    /// Cancelling `next` mid-wait is safe — processed updates stay
    /// buffered inside the stream.
    pub async fn updates(&self) -> Result<Vec<TgEvent>> {
        use grammers_client::Update;
        let mut stream = self.updates.lock().await;
        let mut out = Vec::new();
        loop {
            let update =
                match tokio::time::timeout(std::time::Duration::from_millis(100), stream.next())
                    .await
                {
                    Ok(Ok(u)) => u,
                    // Once events are in hand a stream error only means
                    // "the rest didn't arrive" — return what we got.
                    Ok(Err(e)) if !out.is_empty() => {
                        eprintln!("updates: {e}");
                        break;
                    }
                    Ok(Err(e)) => return Err(Error::Mtproto(format!("updates: {e}"))),
                    Err(_) => break,
                };
            out.push(match update {
                Update::NewMessage(m) | Update::MessageEdited(m) => TgEvent::Message {
                    chat_hint: m.peer_id().bot_api_dialog_id(),
                    msg: Self::topic_message(&m),
                },
                _ => TgEvent::Other,
            });
        }
        Ok(out)
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

    #[test]
    fn utf16_offsets_convert_to_bytes() {
        // Emoji cost 2 UTF-16 units; ä costs 1. Entities point at
        // UTF-16 positions — byte offsets must count wide chars double.
        let text = "ab👀cd äf";
        assert_eq!(utf16_to_byte(text, 4), 6); // after 👀
        assert_eq!(utf16_to_byte(text, 0), 0);
        assert_eq!(utf16_to_byte(text, 2), 2);
        assert_eq!(utf16_to_byte(text, 7), "ab👀cd ".len());
    }

    #[test]
    fn fmt_spans_maps_entities() {
        use tl::enums::MessageEntity as E;
        use tl::types as tt;
        let text = "bold link rest";
        let entities = vec![
            E::Bold(tt::MessageEntityBold {
                offset: 0,
                length: 4,
            }),
            E::TextUrl(tt::MessageEntityTextUrl {
                offset: 5,
                length: 4,
                url: "https://example.com".into(),
            }),
        ];
        let spans = fmt_spans(Some(&entities), text);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].kind, FmtKind::Bold);
        assert_eq!(&text[spans[0].start..spans[0].start + spans[0].len], "bold");
        assert_eq!(spans[1].kind, FmtKind::Link);
        assert_eq!(spans[1].url.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn fmt_spans_bare_url_links_itself() {
        use tl::enums::MessageEntity as E;
        use tl::types as tt;
        let text = "see https://x.y now";
        let entities = vec![E::Url(tt::MessageEntityUrl {
            offset: 4,
            length: 11,
        })];
        let spans = fmt_spans(Some(&entities), text);
        assert_eq!(spans[0].kind, FmtKind::Link);
        assert_eq!(spans[0].url.as_deref(), Some("https://x.y"));
    }

    fn msg(text: &str, spans: Vec<FmtSpan>) -> TopicMessage {
        TopicMessage {
            id: 1,
            date: 0,
            sender: "t".into(),
            sender_id: None,
            text: text.into(),
            spans,
            reactions: vec![],
        }
    }

    #[test]
    fn splits_display_math() {
        let m = msg("before $$\\frac{a}{b}$$ after", vec![]);
        let blocks = split_math(&m);
        assert_eq!(blocks.len(), 3);
        match &blocks[1] {
            Blk::Math { tex, display } => {
                assert_eq!(tex, "\\frac{a}{b}");
                assert!(display);
            }
            _ => panic!("expected math"),
        }
        match &blocks[0] {
            Blk::Text { text, .. } => assert_eq!(text, "before "),
            _ => panic!(),
        }
        match &blocks[2] {
            Blk::Text { text, .. } => assert_eq!(text, " after"),
            _ => panic!(),
        }
    }

    #[test]
    fn ignores_currency_dollars() {
        let m = msg("it costs $5 and $10 total", vec![]);
        assert_eq!(split_math(&m).len(), 1);
    }

    #[test]
    fn skips_dollars_inside_code() {
        let m = msg(
            "echo $x$ done",
            vec![FmtSpan {
                kind: FmtKind::Code,
                start: 5,
                len: 4,
                url: None,
                lang: None,
            }],
        );
        assert_eq!(split_math(&m).len(), 1);
    }

    #[test]
    fn pre_math_fence_becomes_formula() {
        let m = msg(
            "x^2 + y^2",
            vec![FmtSpan {
                kind: FmtKind::Pre,
                start: 0,
                len: 9,
                url: None,
                lang: Some("latex".into()),
            }],
        );
        let blocks = split_math(&m);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(blocks[0], Blk::Math { display: true, .. }));
    }

    #[test]
    fn inline_math() {
        let m = msg("so $x^2$ works", vec![]);
        let blocks = split_math(&m);
        assert_eq!(blocks.len(), 3);
        assert!(matches!(blocks[1], Blk::Math { display: false, .. }));
    }

    #[test]
    fn emoji_text_does_not_panic() {
        // ⏳ is 3 UTF-8 bytes — the byte-walk must not slice mid-char.
        let m = msg("⏳ $x^2$ done", vec![]);
        let blocks = split_math(&m);
        assert_eq!(blocks.len(), 3);
        assert!(matches!(blocks[1], Blk::Math { .. }));
    }
}
