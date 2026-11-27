//! verity-gui — GPUI client. Reads and writes your 1:1 bot chats as
//! *you* (MTProto), so history comes straight from Telegram: no daemon
//! required to read, and a typed message lands in the bot's DM like one
//! sent from any Telegram client.
//!
//! The sidebar lists your DM with each registered bot — resolved out of
//! your own dialogs, so anything `verity serve` or you ever said there
//! shows up too.
//!
//! A worker thread owns a tokio runtime + the grammers client; the view
//! swaps commands and snapshots with it over std mpsc channels.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::*;
use grammers_session::defs::PeerRef;
use verity_core::botapi::BotApi;
use verity_core::config::Config;
use verity_core::mtproto::{
    split_math, Blk, FmtKind, FmtSpan, LoginCodeResult, Tg, TgEvent, TopicMessage,
};
use verity_core::store::{Bot, Store};
use verity_core::{Error, Result};

/// What the transcript pane is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sel {
    /// Your 1:1 chat with a registered bot, by bot user id
    /// (negative = bot unresolvable, shown greyed).
    Dm(i64),
}

enum Cmd {
    /// Re-list DMs and re-fetch the selected view's messages.
    Refresh,
    /// Re-fetch only the selected view's messages (the live tick).
    Poll,
    /// Fetch one view's messages (also marks it selected worker-side).
    Select(Sel),
    /// Post `text` into the selected view as the user, then re-fetch it.
    Send(String),
    /// Toggle your `emoji` reaction on `msg_id` in the selected view.
    React {
        msg_id: i32,
        emoji: String,
        chosen: bool,
    },
    /// Save Telegram API credentials entered in the onboarding form.
    Configure { api_id: String, api_hash: String },
    /// Start Telegram login for a phone number.
    LoginPhone(String),
    /// Submit a login code; the literal string `resend` re-delivers it.
    LoginCode(String),
    /// Submit a two-factor password after the login code is accepted.
    LoginPassword(String),
}

/// Which onboarding field currently owns the composer-style input.
#[derive(Clone, PartialEq)]
enum LoginStep {
    ApiId,
    ApiHash,
    Phone,
    Code {
        delivery: String,
        resend: Option<String>,
    },
    Password,
    Busy,
}

/// One row of the sidebar.
#[derive(Clone)]
struct DmRow {
    user_id: i64,
    label: String,
    /// The bot's DM peer resolved — false greys the row out.
    available: bool,
}

enum Ev {
    /// Full refresh: DM list, current selection, its messages.
    Snapshot {
        dms: Vec<DmRow>,
        selected: Option<Sel>,
        messages: Vec<TopicMessage>,
        /// Your own user id — messages carrying it render as "mine".
        me: Option<i64>,
    },
    /// New messages for one view (ignored if not selected anymore).
    Messages {
        sel: Sel,
        messages: Vec<TopicMessage>,
    },
    /// One-line status / error for the footer.
    Status(String),
    /// Telegram API credentials are missing or invalid.
    NeedApiKeys,
    /// The session exists but no user is logged in.
    NeedPhone,
    /// Telegram sent a login code; `resend` means another channel exists.
    NeedCode {
        delivery: String,
        resend: Option<String>,
    },
    /// The account's two-factor password is required.
    NeedPassword,
    /// Login finished; ordinary chat mode can begin.
    LoggedIn,
}

const HISTORY: i32 = 100;

/// Registered bots from the local roster — sync sqlite read, so it
/// runs before any await and never holds the store across one.
fn roster() -> Vec<Bot> {
    Config::db_path()
        .and_then(|p| Store::open(&p))
        .and_then(|s| s.list_bots())
        .unwrap_or_default()
}

/// A registered bot's DM target: the label for the sidebar plus the
/// resolved MTProto peer of your 1:1 chat, when one exists.
struct Dm {
    user_id: i64,
    label: String,
    peer: Option<PeerRef>,
}

/// Build the sidebar: one row per registered bot. The bot's user id
/// comes from `getMe` on its token — the DM peer itself is your dialog
/// with that user id, so only chats that actually exist resolve.
async fn build_dms(tg: &Tg) -> Vec<Dm> {
    let mut out = Vec::new();
    for b in roster() {
        let mut username = b.username.clone();
        // Ask the Bot API — a registered bot's token always knows its
        // own id.
        let user_id = match BotApi::new(&b.token).get_me().await {
            Ok(me) => {
                username = username.or(me.username);
                Some(me.id)
            }
            Err(_) => None,
        };
        let label = match username {
            Some(u) => format!("@{u}"),
            None => b.name.clone(),
        };
        let dm = match user_id {
            // The 1:1 chat is one of your dialogs — resolve by bare id.
            Some(id) => {
                let peer = tg.peer_for_user(id).await.ok();
                Dm {
                    user_id: id,
                    label,
                    peer,
                }
            }
            // Can't even identify the account — keep a greyed row so the
            // roster isn't silently missing a bot.
            None => Dm {
                user_id: -(out.len() as i64) - 1,
                label,
                peer: None,
            },
        };
        out.push(dm);
    }
    out
}

/// Messages for `sel`; re-tries DM peer resolution on the way so a chat
/// you opened since the last refresh shows up without a restart.
async fn fetch_view(tg: &Tg, dms: &mut [Dm], sel: Sel) -> Result<Vec<TopicMessage>> {
    match sel {
        Sel::Dm(uid) => {
            let dm = dms
                .iter_mut()
                .find(|d| d.user_id == uid)
                .ok_or_else(|| Error::Mtproto("bot is gone".into()))?;
            if dm.peer.is_none() && uid > 0 {
                dm.peer = tg.peer_for_user(uid).await.ok();
            }
            match dm.peer {
                Some(p) => tg.dm_history(p, HISTORY).await,
                None => Err(Error::Mtproto(format!("no 1:1 chat with {} yet", dm.label))),
            }
        }
    }
}

/// Re-list DMs and re-fetch the selected view, emitting a Snapshot.
async fn refresh_all(
    tg: &Tg,
    dms: &mut Vec<Dm>,
    selected: &mut Option<Sel>,
    tx: &mpsc::Sender<Ev>,
) {
    let me = tg.self_id().await.ok();
    *dms = build_dms(tg).await;
    // Drop a selection whose target vanished.
    let still_there = selected.is_some_and(|s| match s {
        Sel::Dm(uid) => dms.iter().any(|d| d.user_id == uid),
    });
    if !still_there {
        *selected = dms.first().map(|d| Sel::Dm(d.user_id));
    }
    let messages = match *selected {
        Some(s) => fetch_view(tg, dms, s).await.unwrap_or_default(),
        None => vec![],
    };
    let _ = tx.send(Ev::Snapshot {
        me,
        dms: dms
            .iter()
            .map(|d| DmRow {
                user_id: d.user_id,
                label: d.label.clone(),
                available: d.peer.is_some(),
            })
            .collect(),
        selected: *selected,
        messages,
    });
}

/// Receive one command without blocking the tokio runtime. The command
/// channel is std-mpsc because the GPUI view is sync, so poll it gently.
async fn next_cmd(rx: &mpsc::Receiver<Cmd>) -> Option<Cmd> {
    loop {
        match rx.try_recv() {
            Ok(cmd) => return Some(cmd),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => return None,
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Load API credentials, asking through the GUI and saving `config.toml`
/// when they are missing. Environment-provided credentials still win via
/// `Config::api_credentials`.
async fn ensure_api_credentials(
    rx: &mpsc::Receiver<Cmd>,
    tx: &mpsc::Sender<Ev>,
) -> Option<(i32, String)> {
    let mut cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Ev::Status(format!("config: {e}")));
            return None;
        }
    };
    loop {
        if let Ok(creds) = cfg.api_credentials() {
            return Some(creds);
        }
        let _ = tx.send(Ev::NeedApiKeys);
        let _ = tx.send(Ev::Status("Telegram API credentials needed".into()));
        loop {
            let cmd = next_cmd(rx).await?;
            if let Cmd::Configure { api_id, api_hash } = cmd {
                let api_id = match api_id.trim().parse::<i32>() {
                    Ok(v) if v > 0 => v,
                    _ => {
                        let _ = tx.send(Ev::Status("api_id must be a positive number".into()));
                        continue;
                    }
                };
                let api_hash = api_hash.trim().to_string();
                if api_hash.is_empty() {
                    let _ = tx.send(Ev::Status("api_hash cannot be empty".into()));
                    continue;
                }
                cfg.api_id = Some(api_id);
                cfg.api_hash = Some(api_hash);
                match cfg.save() {
                    Ok(()) => {
                        let _ = tx.send(Ev::Status("API credentials saved".into()));
                        break;
                    }
                    Err(e) => {
                        let _ = tx.send(Ev::Status(format!("save config: {e}")));
                    }
                }
            }
        }
    }
}

/// Interactive Telegram login driven by GUI prompts rather than stdin.
/// Returns true once the session is authorized.
async fn telegram_login(tg: &Tg, rx: &mpsc::Receiver<Cmd>, tx: &mpsc::Sender<Ev>) -> bool {
    macro_rules! status {
        ($($arg:tt)*) => {{
            let _ = tx.send(Ev::Status(format!($($arg)*)));
        }};
    }

    'phone: loop {
        let _ = tx.send(Ev::NeedPhone);
        let phone = loop {
            match next_cmd(rx).await {
                Some(Cmd::LoginPhone(phone)) if !phone.trim().is_empty() => {
                    break phone.trim().to_string()
                }
                Some(_) => {}
                None => return false,
            }
        };

        status!("sending login code…");
        let mut challenge = match tg.login_send_code(&phone).await {
            Ok(c) => c,
            Err(e) => {
                status!("send code: {e}");
                continue 'phone;
            }
        };

        'code: loop {
            let _ = tx.send(Ev::NeedCode {
                delivery: challenge.delivery.clone(),
                resend: challenge.next_delivery.clone(),
            });
            let input = loop {
                match next_cmd(rx).await {
                    Some(Cmd::LoginCode(input)) if !input.trim().is_empty() => {
                        break input.trim().to_string()
                    }
                    Some(Cmd::LoginPhone(_)) => continue 'phone,
                    Some(_) => {}
                    None => return false,
                }
            };

            if input.eq_ignore_ascii_case("resend") {
                status!("resending login code…");
                match tg.login_resend_code(&phone, &challenge).await {
                    Ok(c) => {
                        challenge = c;
                        status!("code resent");
                    }
                    Err(e) => status!("resend: {e}"),
                }
                continue 'code;
            }

            status!("checking login code…");
            match tg.login_submit_code(&phone, &challenge, &input).await {
                Ok(LoginCodeResult::Done) => {
                    let _ = tx.send(Ev::LoggedIn);
                    return true;
                }
                Ok(LoginCodeResult::PasswordNeeded) => loop {
                    let _ = tx.send(Ev::NeedPassword);
                    let password = loop {
                        match next_cmd(rx).await {
                            Some(Cmd::LoginPassword(pw)) if !pw.is_empty() => break pw,
                            Some(_) => {}
                            None => return false,
                        }
                    };
                    status!("checking 2FA password…");
                    match tg.login_submit_password(&password).await {
                        Ok(()) => {
                            let _ = tx.send(Ev::LoggedIn);
                            return true;
                        }
                        Err(e) => status!("2FA: {e}"),
                    }
                },
                Err(e) => {
                    status!("{e}");
                    continue 'code;
                }
            }
        }
    }
}

async fn worker(rx: mpsc::Receiver<Cmd>, tx: mpsc::Sender<Ev>) {
    macro_rules! status {
        ($($arg:tt)*) => {{
            let _ = tx.send(Ev::Status(format!($($arg)*)));
        }};
    }

    status!("connecting…");
    let Some((api_id, api_hash)) = ensure_api_credentials(&rx, &tx).await else {
        return;
    };
    let path = match Config::session_path() {
        Ok(p) => p,
        Err(_) => return status!("no home dir"),
    };
    let tg = match Tg::connect(api_id, api_hash, &path).await {
        Ok(t) => t,
        Err(e) => return status!("connect: {e}"),
    };
    match tg.is_authorized().await {
        Ok(true) => {}
        Ok(false) => {
            if !telegram_login(&tg, &rx, &tx).await {
                return;
            }
            match Config::session_path() {
                Ok(path) => {
                    if let Err(e) = tg.save_session(&path) {
                        status!("save session: {e}");
                    }
                }
                Err(e) => status!("save session: {e}"),
            }
        }
        Err(e) => return status!("auth: {e}"),
    }
    // Whether the session was already authorized or the onboarding flow
    // just finished, the GUI can leave login mode now.
    let _ = tx.send(Ev::LoggedIn);
    status!("");

    let mut dms: Vec<Dm> = Vec::new();
    let mut selected: Option<Sel> = None;
    refresh_all(&tg, &mut dms, &mut selected, &tx).await;

    loop {
        match rx.try_recv() {
            Ok(cmd) => {
                match cmd {
                    Cmd::Refresh => refresh_all(&tg, &mut dms, &mut selected, &tx).await,
                    Cmd::Poll => {
                        if let Some(s) = selected {
                            match fetch_view(&tg, &mut dms, s).await {
                                Ok(messages) => {
                                    let _ = tx.send(Ev::Messages { sel: s, messages });
                                }
                                Err(e) => status!("poll: {e}"),
                            }
                        }
                    }
                    Cmd::Select(s) => {
                        selected = Some(s);
                        match fetch_view(&tg, &mut dms, s).await {
                            Ok(messages) => {
                                let _ = tx.send(Ev::Messages { sel: s, messages });
                            }
                            Err(e) => status!("history: {e}"),
                        }
                    }
                    Cmd::Send(text) => {
                        let Some(s) = selected else {
                            status!("nothing selected");
                            continue;
                        };
                        let sent = match s {
                            Sel::Dm(uid) => {
                                match dms.iter().find(|d| d.user_id == uid).and_then(|d| d.peer) {
                                    Some(p) => tg.send_message(p, &text).await,
                                    None => Err(Error::Mtproto("no 1:1 chat to send into".into())),
                                }
                            }
                        };
                        if let Err(e) = sent {
                            status!("send: {e}");
                            continue;
                        }
                        match fetch_view(&tg, &mut dms, s).await {
                            Ok(messages) => {
                                let _ = tx.send(Ev::Messages { sel: s, messages });
                            }
                            Err(e) => status!("history: {e}"),
                        }
                    }
                    Cmd::React {
                        msg_id,
                        emoji,
                        chosen,
                    } => {
                        let Some(s) = selected else {
                            status!("nothing selected");
                            continue;
                        };
                        let target = match s {
                            Sel::Dm(uid) => {
                                dms.iter().find(|d| d.user_id == uid).and_then(|d| d.peer)
                            }
                        };
                        match target {
                            Some(p) => {
                                let r = tg
                                    .send_reaction(
                                        p,
                                        msg_id,
                                        if chosen { None } else { Some(&emoji) },
                                    )
                                    .await;
                                if let Err(e) = r {
                                    status!("reaction: {e}");
                                    continue;
                                }
                                match fetch_view(&tg, &mut dms, s).await {
                                    Ok(messages) => {
                                        let _ = tx.send(Ev::Messages { sel: s, messages });
                                    }
                                    Err(e) => status!("history: {e}"),
                                }
                            }
                            None => status!("no 1:1 chat to react in"),
                        }
                    }
                    Cmd::Configure { .. }
                    | Cmd::LoginPhone(_)
                    | Cmd::LoginCode(_)
                    | Cmd::LoginPassword(_) => {
                        status!("already logged in");
                    }
                }
                // Keep draining queued commands before the next tick.
                continue;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => return,
        }

        // Live push: grammers buffers socket updates — drain them and
        // refetch when one lands in the open view (a message arriving
        // anywhere else doesn't need a refetch — the 3s Poll covers
        // reaction edits, which Telegram doesn't push as messages).
        match tg.updates().await {
            Ok(events) => {
                let mut hit = false;
                for ev in events {
                    if let TgEvent::Message { chat_hint, .. } = ev {
                        hit |= match selected {
                            Some(Sel::Dm(u)) => chat_hint == u,
                            None => false,
                        };
                    }
                }
                if hit {
                    if let Some(s) = selected {
                        match fetch_view(&tg, &mut dms, s).await {
                            Ok(messages) => {
                                let _ = tx.send(Ev::Messages { sel: s, messages });
                            }
                            Err(e) => status!("history: {e}"),
                        }
                    }
                }
            }
            Err(e) => status!("updates: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

/// `HH:MM` UTC — enough to order a transcript at a glance without
/// dragging a date library in.
fn hhmm(unix: i64) -> String {
    let t = unix.rem_euclid(86_400);
    format!("{:02}:{:02}", t / 3600, (t % 3600) / 60)
}

/// The row of reaction pills under a message — `None` when there are
/// none. Clicking a pill toggles your own copy of that reaction.
fn reaction_chips(
    m: &TopicMessage,
    align_end: bool,
    idle_bg: Rgba,
    chosen_bg: Rgba,
    dim: Rgba,
    cx: &mut Context<Root>,
) -> Option<Div> {
    if m.reactions.is_empty() {
        return None;
    }
    let msg_id = m.id;
    let chips = m.reactions.iter().map(|r| {
        let emoji = r.emoji.clone();
        let chosen = r.chosen;
        div()
            .id(ElementId::Name(format!("rx-{msg_id}-{emoji}").into()))
            .px_2()
            .py(px(2.))
            .rounded_md()
            .cursor_pointer()
            .bg(if chosen { chosen_bg } else { idle_bg })
            .border_1()
            .border_color(if chosen { chosen_bg } else { rgb(0x2f3336) })
            .flex()
            .items_center()
            .gap_1()
            .text_xs()
            .child(emoji.clone())
            .when(r.count > 1, |d| {
                d.child(div().text_color(dim).child(r.count.to_string()))
            })
            .on_click(cx.listener(move |this, _ev: &ClickEvent, _w, cx| {
                let _ = this.cmd_tx.send(Cmd::React {
                    msg_id,
                    emoji: emoji.clone(),
                    chosen,
                });
                cx.notify();
            }))
    });
    Some(
        div()
            .flex()
            .gap_1()
            .when(align_end, |d| d.justify_end())
            .children(chips),
    )
}

struct Root {
    cmd_tx: mpsc::Sender<Cmd>,
    ev_rx: mpsc::Receiver<Ev>,
    dms: Vec<DmRow>,
    selected: Option<Sel>,
    messages: Vec<TopicMessage>,
    /// Your user id — `sender_id == me` renders as your bubble.
    me: Option<i64>,
    input: String,
    status: String,
    /// Active onboarding/login field; `None` means ordinary chat input.
    login_step: Option<LoginStep>,
    /// API id carried from the first onboarding field to the second.
    login_api_id: Option<String>,
    /// Id of the newest message shown — auto-scroll only on arrivals.
    last_msg: Option<i32>,
    /// A Poll is in flight — don't stack another on top of it.
    awaiting: bool,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    /// A real monospace family resolved at startup, when one exists.
    mono_font: Option<SharedString>,
    /// `(message id, span's original start)` of spoilers the user
    /// tapped open; cleared when the view changes.
    revealed: HashSet<(i32, usize)>,
    /// Rendered formula bitmaps, keyed `(tex, display)` — `None`
    /// caches failures too, so broken input isn't recompiled per frame.
    math_cache: HashMap<(String, bool), Option<Arc<Image>>>,
}

impl Root {
    fn new(cmd_tx: mpsc::Sender<Cmd>, ev_rx: mpsc::Receiver<Ev>, cx: &mut Context<Self>) -> Self {
        let this = Self {
            cmd_tx,
            ev_rx,
            dms: vec![],
            selected: None,
            messages: vec![],
            me: None,
            input: String::new(),
            status: "connecting…".into(),
            login_step: None,
            login_api_id: None,
            last_msg: None,
            awaiting: false,
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            // gpui matches family names literally — pick the first
            // well-known mono face the system actually has.
            mono_font: {
                let names = cx.text_system().all_font_names();
                [
                    "JetBrainsMono Nerd Font",
                    "JetBrains Mono",
                    "FiraCode Nerd Font",
                    "Fira Code",
                    "DejaVu Sans Mono",
                    "Lilex",
                    "Courier New",
                ]
                .iter()
                .find(|c| names.iter().any(|n| n.as_str() == **c))
                .map(|s| SharedString::from(*s))
                .or_else(|| {
                    names
                        .iter()
                        .find(|n| n.contains("Mono") || n.contains("mono"))
                        .map(|n| SharedString::from(n.clone()))
                })
            },
            revealed: HashSet::new(),
            math_cache: HashMap::new(),
        };
        // Pump worker events into view state; live-tick the selected
        // view every ~3s, re-list DMs every ~30s.
        cx.spawn(async move |this: WeakEntity<Root>, cx: &mut AsyncApp| {
            let mut ticks = 0u32;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                ticks += 1;
                let poll = ticks.is_multiple_of(12);
                let refresh = ticks.is_multiple_of(120);
                let alive = this
                    .update(cx, |this, cx| {
                        this.drain_events();
                        if refresh {
                            let _ = this.cmd_tx.send(Cmd::Refresh);
                        } else if poll
                            && !this.awaiting
                            && this.selected.is_some()
                            && this.cmd_tx.send(Cmd::Poll).is_ok()
                        {
                            this.awaiting = true;
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
        this
    }

    /// Apply every queued worker event. Any reply clears `awaiting`,
    /// letting the next live tick fire.
    fn drain_events(&mut self) {
        let mut any = false;
        while let Ok(ev) = self.ev_rx.try_recv() {
            any = true;
            match ev {
                Ev::Snapshot {
                    dms,
                    selected,
                    messages,
                    me,
                } => {
                    self.dms = dms;
                    self.me = me.or(self.me);
                    self.selected = selected;
                    self.last_msg = messages.last().map(|m| m.id);
                    self.messages = messages;
                    self.scroll.scroll_to_bottom();
                }
                Ev::Messages { sel, messages } => {
                    if self.selected == Some(sel) {
                        let last = messages.last().map(|m| m.id);
                        if last != self.last_msg {
                            self.scroll.scroll_to_bottom();
                        }
                        self.last_msg = last;
                        self.messages = messages;
                    }
                }
                Ev::Status(s) => self.status = s,
                Ev::NeedApiKeys => {
                    self.login_step = Some(LoginStep::ApiId);
                    self.login_api_id = None;
                    self.input.clear();
                }
                Ev::NeedPhone => {
                    self.login_step = Some(LoginStep::Phone);
                    self.input.clear();
                }
                Ev::NeedCode { delivery, resend } => {
                    self.login_step = Some(LoginStep::Code { delivery, resend });
                    self.input.clear();
                }
                Ev::NeedPassword => {
                    self.login_step = Some(LoginStep::Password);
                    self.input.clear();
                }
                Ev::LoggedIn => {
                    self.login_step = None;
                    self.login_api_id = None;
                    self.input.clear();
                    self.status = "logged in".into();
                }
            }
        }
        if any {
            self.awaiting = false;
        }
    }

    /// One text block → styled text: formatting entities merged into
    /// disjoint runs, links open in the browser, spoilers conceal
    /// until tapped, code runs get the resolved mono face. `spans`
    /// are rebased to `text` and paired with their original start —
    /// the spoiler reveal key.
    #[allow(clippy::too_many_arguments)]
    fn rich_text(
        &self,
        msg_id: i32,
        elkey: String,
        text: &str,
        spans: &[(usize, FmtSpan)],
        base: &TextStyle,
        fg: Rgba,
        accent: Rgba,
        code_bg: Rgba,
        dim: Rgba,
        spoiler_bg: Rgba,
        view: Entity<Root>,
    ) -> InteractiveText {
        enum Act {
            Url(String),
            Spoiler(i32, usize),
        }

        let len = text.len();
        let mut cuts = BTreeSet::from([0, len]);
        for (_, s) in spans {
            cuts.insert(s.start.min(len));
            cuts.insert(s.start.saturating_add(s.len).min(len));
        }
        let cuts: Vec<usize> = cuts.into_iter().collect();

        let underline = |color: Hsla| UnderlineStyle {
            thickness: px(1.0),
            color: Some(color),
            wavy: false,
        };
        // Runs are baked eagerly — the ambient style's default color
        // is black, so the caller's body color goes in explicitly.
        let mut base = base.clone();
        base.color = fg.into();

        // Entities nest/cross — merge into disjoint runs carrying the
        // union of every covering span's style.
        let mut runs = Vec::new();
        for w in cuts.windows(2) {
            let (a, b) = (w[0], w[1]);
            if a >= b {
                continue;
            }
            let mut style = base.clone();
            let mut code = false;
            let mut hidden = false;
            for &(orig, ref s) in spans {
                if s.start > a || s.start + s.len < b {
                    continue;
                }
                match s.kind {
                    FmtKind::Bold => style.font_weight = FontWeight::BOLD,
                    FmtKind::Italic => style.font_style = FontStyle::Italic,
                    FmtKind::Underline => style.underline = Some(underline(fg.into())),
                    FmtKind::Strike => {
                        style.strikethrough = Some(StrikethroughStyle {
                            thickness: px(1.0),
                            color: None,
                        })
                    }
                    FmtKind::Code | FmtKind::Pre => {
                        code = true;
                        style.background_color = Some(code_bg.into());
                    }
                    FmtKind::Spoiler => hidden = !self.revealed.contains(&(msg_id, orig)),
                    FmtKind::Blockquote => style.color = dim.into(),
                    FmtKind::Link => {
                        style.color = accent.into();
                        style.underline = Some(underline(accent.into()));
                    }
                    FmtKind::Accent | FmtKind::CustomEmoji => style.color = accent.into(),
                }
            }
            if code {
                if let Some(mono) = &self.mono_font {
                    style.font_family = mono.clone();
                }
            }
            if hidden {
                style.color = spoiler_bg.into();
                style.background_color = Some(spoiler_bg.into());
            }
            runs.push(style.to_run(b - a));
        }

        let mut clickables = Vec::new();
        let mut acts = Vec::new();
        for &(orig, ref s) in spans {
            let range = s.start..(s.start + s.len).min(len);
            match s.kind {
                FmtKind::Link => {
                    if let Some(url) = &s.url {
                        clickables.push(range);
                        acts.push(Act::Url(url.clone()));
                    }
                }
                FmtKind::Spoiler => {
                    clickables.push(range);
                    acts.push(Act::Spoiler(msg_id, orig));
                }
                _ => {}
            }
        }

        let text = InteractiveText::new(
            ElementId::Name(elkey.into()),
            StyledText::new(SharedString::from(text.to_string())).with_runs(runs),
        );
        if clickables.is_empty() {
            return text;
        }
        text.on_click(clickables, move |ix, _window, cx| match &acts[ix] {
            Act::Url(url) => cx.open_url(url),
            Act::Spoiler(msg, span) => {
                view.update(cx, |this, cx| {
                    this.revealed.insert((*msg, *span));
                    cx.notify();
                });
            }
        })
    }

    /// A math block → the rasterized formula (pre-warmed into
    /// `math_cache` before render), or the raw source dimmed when
    /// mitex/typst couldn't compile it.
    fn math_el(&self, tex: &str, display: bool, dim: Rgba) -> AnyElement {
        match self.math_cache.get(&(tex.to_string(), display)) {
            Some(Some(image)) => div()
                .when(display, |d| d.flex().justify_center().py_1())
                .child(img(image.clone()))
                .into_any_element(),
            _ => div()
                .text_color(dim)
                .child(tex.to_string())
                .into_any_element(),
        }
    }

    /// `split_math` output → renderable elements for one message.
    #[allow(clippy::too_many_arguments)]
    fn msg_blocks(
        &self,
        m: &TopicMessage,
        base: &TextStyle,
        fg: Rgba,
        accent: Rgba,
        code_bg: Rgba,
        dim: Rgba,
        spoiler_bg: Rgba,
        view: &Entity<Root>,
    ) -> Vec<AnyElement> {
        split_math(m)
            .iter()
            .enumerate()
            .map(|(bi, b)| match b {
                Blk::Text { text, spans } => self
                    .rich_text(
                        m.id,
                        format!("fmt-{}-{}", m.id, bi),
                        text,
                        spans,
                        base,
                        fg,
                        accent,
                        code_bg,
                        dim,
                        spoiler_bg,
                        view.clone(),
                    )
                    .into_any_element(),
                Blk::Math { tex, display } => self.math_el(tex, *display, dim),
            })
            .collect()
    }

    fn select(&mut self, sel: Sel, cx: &mut Context<Self>) {
        if self.selected != Some(sel) {
            self.selected = Some(sel);
            self.messages.clear();
            self.last_msg = None;
            self.revealed.clear();
            let _ = self.cmd_tx.send(Cmd::Select(sel));
            cx.notify();
        }
    }

    /// Sidebar order for ↑/↓.
    fn items(&self) -> Vec<Sel> {
        self.dms.iter().map(|d| Sel::Dm(d.user_id)).collect()
    }

    fn move_selection(&mut self, dir: i64, cx: &mut Context<Self>) {
        let items = self.items();
        if items.is_empty() {
            return;
        }
        let cur = self
            .selected
            .and_then(|s| items.iter().position(|&x| x == s))
            .unwrap_or(0) as i64;
        let n = items.len() as i64;
        let next = (cur + dir).rem_euclid(n);
        self.select(items[next as usize], cx);
    }

    fn send_input(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() || self.selected.is_none() {
            return;
        }
        let _ = self.cmd_tx.send(Cmd::Send(text));
        self.input.clear();
    }

    /// Advance the onboarding form. The worker validates again; local
    /// validation here only prevents obviously incomplete submissions.
    fn submit_login(&mut self) {
        let value = self.input.trim().to_string();
        let Some(step) = self.login_step.clone() else {
            return;
        };
        match step {
            LoginStep::ApiId => {
                if value.parse::<i32>().ok().is_some_and(|v| v > 0) {
                    self.login_api_id = Some(value);
                    self.input.clear();
                    self.login_step = Some(LoginStep::ApiHash);
                    self.status = "now paste the API hash".into();
                } else {
                    self.status = "api_id must be a positive number".into();
                }
            }
            LoginStep::ApiHash => {
                let Some(api_id) = self.login_api_id.clone() else {
                    self.login_step = Some(LoginStep::ApiId);
                    return;
                };
                if value.is_empty() {
                    self.status = "api_hash cannot be empty".into();
                    return;
                }
                let _ = self.cmd_tx.send(Cmd::Configure {
                    api_id,
                    api_hash: value,
                });
                self.input.clear();
                self.login_step = Some(LoginStep::Busy);
                self.status = "saving API credentials…".into();
            }
            LoginStep::Phone => {
                if value.is_empty() {
                    return;
                }
                let _ = self.cmd_tx.send(Cmd::LoginPhone(value));
                self.input.clear();
                self.login_step = Some(LoginStep::Busy);
                self.status = "sending login code…".into();
            }
            LoginStep::Code { .. } => {
                if value.is_empty() {
                    return;
                }
                let _ = self.cmd_tx.send(Cmd::LoginCode(value));
                self.input.clear();
                self.login_step = Some(LoginStep::Busy);
                self.status = "checking login code…".into();
            }
            LoginStep::Password => {
                if value.is_empty() {
                    return;
                }
                let _ = self.cmd_tx.send(Cmd::LoginPassword(value));
                self.input.clear();
                self.login_step = Some(LoginStep::Busy);
                self.status = "checking 2FA password…".into();
            }
            LoginStep::Busy => {}
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let onboarding = self.login_step.is_some();
        match ks.key.as_str() {
            "backspace" => {
                self.input.pop();
            }
            "enter" => {
                if onboarding {
                    self.submit_login();
                } else {
                    self.send_input();
                }
            }
            "escape" => self.input.clear(),
            "up" if !onboarding => self.move_selection(-1, cx),
            "down" if !onboarding => self.move_selection(1, cx),
            "r" if !onboarding && (ks.modifiers.control || ks.modifiers.platform) => {
                let _ = self.cmd_tx.send(Cmd::Refresh);
            }
            "v" if (ks.modifiers.control || ks.modifiers.platform)
                && !matches!(self.login_step, Some(LoginStep::Busy)) =>
            {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.input.push_str(&text);
                }
            }
            _ => {
                let m = &ks.modifiers;
                if !(m.control || m.platform || m.alt)
                    && !matches!(self.login_step, Some(LoginStep::Busy))
                {
                    if let Some(c) = &ks.key_char {
                        self.input.push_str(c);
                    }
                }
            }
        }
        cx.notify();
    }

    /// Title for the transcript header — the bot's @username.
    fn view_title(&self) -> String {
        match self.selected {
            Some(Sel::Dm(uid)) => self
                .dms
                .iter()
                .find(|d| d.user_id == uid)
                .map(|d| d.label.clone())
                .unwrap_or_else(|| "dm".into()),
            None => "Verity".into(),
        }
    }
}

impl Focusable for Root {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected;
        let me = self.me;
        // Grok's palette: pure-black canvas, near-invisible hairlines,
        // X-grey dim text, dark pill bubbles for your own messages.
        let dim = rgb(0x71767b);
        let text = rgb(0xe7e9ea);
        let hairline = rgb(0x2f3336);
        let hover_bg = rgb(0x16181c);
        let selected_bg = rgb(0x1e1f22);
        let bubble = rgb(0x2f3336);
        let composer_bg = rgb(0x16181c);
        // Rich text accents: Grok's link blue, code chips a step above
        // the canvas, concealed spoilers a hair darker.
        let accent = rgb(0x1d9bf0);
        let code_bg = rgb(0x1e1f22);
        let spoiler_bg = rgb(0x21262b);
        let text_style = window.text_style();
        let view = cx.entity();
        let chip_chosen = rgb(0x20304a);

        if let Some(step) = self.login_step.clone() {
            let (title, description, placeholder, secret): (&str, String, &str, bool) = match &step {
                LoginStep::ApiId => (
                    "Connect Verity to Telegram",
                    "Paste the API ID from my.telegram.org → API development tools.".to_string(),
                    "API ID",
                    false,
                ),
                LoginStep::ApiHash => (
                    "Connect Verity to Telegram",
                    "Now paste the API hash from the same Telegram page. It is stored locally in ~/verity/config.toml.".to_string(),
                    "API hash",
                    true,
                ),
                LoginStep::Phone => (
                    "Log in to Telegram",
                    "Enter your phone number in international format, including the leading +.".to_string(),
                    "+15551234567",
                    false,
                ),
                LoginStep::Code { delivery, resend } => (
                    "Check Telegram",
                    match resend {
                        Some(next) => format!(
                            "Telegram sent the login code via {delivery}. Type 'resend' to send it again via {next}."
                        ),
                        None => format!("Telegram sent the login code via {delivery}."),
                    },
                    "Login code",
                    false,
                ),
                LoginStep::Password => (
                    "Two-factor authentication",
                    "Enter your Telegram 2FA password. It is used only for this login.".to_string(),
                    "2FA password",
                    true,
                ),
                LoginStep::Busy => (
                    "Working",
                    "Verity is talking to Telegram.".to_string(),
                    "working…",
                    false,
                ),
            };
            let busy = matches!(step, LoginStep::Busy);
            let typed = if secret {
                "•".repeat(self.input.chars().count())
            } else {
                self.input.clone()
            };
            let input_line = if busy {
                self.status.clone()
            } else if typed.is_empty() {
                placeholder.to_string()
            } else {
                format!("{typed}▌")
            };

            return div()
                .id("root")
                .track_focus(&self.focus_handle)
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                    this.on_key(ev, cx);
                }))
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(black())
                .text_color(text)
                .child(
                    div()
                        .w(px(440.0))
                        .bg(composer_bg)
                        .border_1()
                        .border_color(hairline)
                        .rounded_xl()
                        .p_6()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(
                            div()
                                .text_xl()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .child(div().text_sm().text_color(dim).child(description))
                        .child(
                            div()
                                .h(px(48.0))
                                .px_4()
                                .rounded_lg()
                                .bg(black())
                                .border_1()
                                .border_color(if busy { hairline } else { accent })
                                .flex()
                                .items_center()
                                .text_color(if typed.is_empty() || busy { dim } else { text })
                                .child(input_line),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(dim)
                                .child("Enter continues · Ctrl/Cmd+V pastes · Esc clears"),
                        )
                        .child(div().text_xs().text_color(dim).child(self.status.clone())),
                );
        }

        // Compile any new formulas once — render() runs on every
        // notify, so typst work must not happen inside the tree walk.
        for m in &self.messages {
            for b in split_math(m) {
                if let Blk::Math { tex, display } = b {
                    self.math_cache
                        .entry((tex.clone(), display))
                        .or_insert_with(|| {
                            verity_math::render_latex(&tex, display, 1.0, true)
                                .ok()
                                .map(|png| Arc::new(Image::from_bytes(ImageFormat::Png, png)))
                        });
                }
            }
        }

        let section = |label: &'static str| {
            div()
                .px_4()
                .pt_5()
                .pb_2()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(dim)
                .child(label)
        };

        let sidebar = div()
            .id("sidebar")
            .w(px(264.0))
            .h_full()
            .border_r_1()
            .border_color(hairline)
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_4()
                    .py_4()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Verity"),
            )
            .child(section("Bots"))
            .children(self.dms.iter().map(|d| {
                let uid = d.user_id;
                div()
                    .id(ElementId::Name(format!("dm-{uid}").into()))
                    .mx_2()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .cursor_pointer()
                    .when(selected == Some(Sel::Dm(uid)), |d| d.bg(selected_bg))
                    .hover(|d| d.bg(hover_bg))
                    .text_color(if d.available { text } else { dim })
                    .child(d.label.clone())
                    .on_click(cx.listener(move |this, _ev: &ClickEvent, _w, cx| {
                        this.select(Sel::Dm(uid), cx);
                    }))
            }));

        let header = div()
            .h(px(52.0))
            .px_5()
            .border_b_1()
            .border_color(hairline)
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.view_title()),
            )
            .child(div().text_xs().text_color(dim).child(self.status.clone()));

        // One centered reading column — Grok keeps the transcript
        // narrow on wide windows instead of stretching edge to edge.
        let messages = div()
            .id("messages")
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                div()
                    .w_full()
                    .max_w(px(720.0))
                    .mx_auto()
                    .px_5()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .children(self.messages.iter().map(|m| {
                        let mine = me.is_some() && m.sender_id == me;
                        let chips = reaction_chips(m, mine, composer_bg, chip_chosen, dim, cx);
                        if mine {
                            // Your message: dark pill, right edge, no
                            // name tag — Grok trusts you know you.
                            div()
                                .flex()
                                .flex_col()
                                .items_end()
                                .gap_1()
                                .child(
                                    div().flex().justify_end().child(
                                        div()
                                            .max_w(px(520.0))
                                            .bg(bubble)
                                            .rounded_2xl()
                                            .px_4()
                                            .py_2()
                                            .text_color(text)
                                            .children(self.msg_blocks(
                                                m,
                                                &text_style,
                                                text,
                                                accent,
                                                code_bg,
                                                dim,
                                                spoiler_bg,
                                                &view,
                                            )),
                                    ),
                                )
                                .children(chips)
                        } else {
                            // Theirs: bare text with a whispered name,
                            // like Grok's own replies.
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .child(
                                            div().text_xs().text_color(dim).child(m.sender.clone()),
                                        )
                                        .child(div().text_xs().text_color(dim).child(hhmm(m.date))),
                                )
                                .children(
                                    self.msg_blocks(
                                        m,
                                        &text_style,
                                        text,
                                        accent,
                                        code_bg,
                                        dim,
                                        spoiler_bg,
                                        &view,
                                    )
                                    .into_iter()
                                    .map(|el| div().text_color(text).child(el).into_any_element())
                                    .collect::<Vec<_>>(),
                                )
                                .children(chips)
                        }
                    }))
                    .when(self.messages.is_empty(), |d| {
                        d.child(div().text_color(dim).child(if self.status.is_empty() {
                            "no messages".to_string()
                        } else {
                            self.status.clone()
                        }))
                    }),
            );

        let hint = match self.selected {
            Some(Sel::Dm(_)) => "Message the bot",
            None => "Select a conversation",
        };
        // Grok's floating pill composer — detached from the edges,
        // rounded, faint border, centred under the reading column.
        let composer = div().px_5().pb_5().pt_2().child(
            div()
                .w_full()
                .max_w(px(720.0))
                .mx_auto()
                .h(px(48.0))
                .bg(composer_bg)
                .border_1()
                .border_color(hairline)
                .rounded_full()
                .px_5()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_color(if self.input.is_empty() { dim } else { text })
                        .child(if self.input.is_empty() {
                            format!("{hint}…")
                        } else {
                            format!("{}▌", self.input)
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(dim)
                        .child(if self.input.is_empty() { "" } else { "↵" }),
                ),
        );

        let chat = div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .child(header)
            .child(messages)
            .child(composer);

        div()
            .id("root")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                this.on_key(ev, cx);
            }))
            .size_full()
            .flex()
            .bg(black())
            .text_color(text)
            .child(sidebar)
            .child(chat)
    }
}

fn main() {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (ev_tx, ev_rx) = mpsc::channel::<Ev>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(worker(cmd_rx, ev_tx));
    });

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.0), px(640.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Verity".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                let view = cx.new(|cx| Root::new(cmd_tx, ev_rx, cx));
                let fh = view.read(cx).focus_handle.clone();
                window.focus(&fh);
                view
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
