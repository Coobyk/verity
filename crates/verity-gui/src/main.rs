//! verity-gui — GPUI client. Reads and writes the bot supergroup as *you*
//! (MTProto), so chat history comes straight from Telegram: no daemon
//! required to read, and a typed message lands in the topic like one sent
//! from any Telegram client.
//!
//! The DIRECT section lists your 1:1 chats with each registered bot —
//! resolved out of your own dialogs, so anything `verity serve` or you
//! ever said there shows up too.
//!
//! A worker thread owns a tokio runtime + the grammers client; the view
//! swaps commands and snapshots with it over std mpsc channels.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::*;
use grammers_session::defs::PeerRef;
use verity_core::botapi::BotApi;
use verity_core::config::Config;
use verity_core::mtproto::{Tg, TopicInfo, TopicMessage};
use verity_core::store::{Bot, Store};
use verity_core::{Error, Result};

/// What the transcript pane is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sel {
    /// A forum topic in the host group, by topic id.
    Topic(i64),
    /// Your 1:1 chat with a registered bot, by bot user id
    /// (negative = bot unresolvable, shown greyed).
    Dm(i64),
}

enum Cmd {
    /// Re-list topics + DMs and re-fetch the selected view's messages.
    Refresh,
    /// Re-fetch only the selected view's messages (the live tick).
    Poll,
    /// Fetch one view's messages (also marks it selected worker-side).
    Select(Sel),
    /// Post `text` into the selected view as the user, then re-fetch it.
    Send(String),
}

/// One row of the DIRECT sidebar section.
#[derive(Clone)]
struct DmRow {
    user_id: i64,
    label: String,
    /// The bot's DM peer resolved — false greys the row out.
    available: bool,
}

enum Ev {
    /// Full refresh: topic list, DM list, current selection, its messages.
    Snapshot {
        topics: Vec<TopicInfo>,
        dms: Vec<DmRow>,
        selected: Option<Sel>,
        messages: Vec<TopicMessage>,
    },
    /// New messages for one view (ignored if not selected anymore).
    Messages {
        sel: Sel,
        messages: Vec<TopicMessage>,
    },
    /// One-line status / error for the footer.
    Status(String),
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

/// Build the DIRECT list: one row per registered bot. The bot's user id
/// comes from group membership where possible, else from `getMe` on its
/// token — the DM peer itself is your dialog with that user id, so only
/// chats that actually exist resolve.
async fn build_dms(tg: &Tg, group: PeerRef) -> Vec<Dm> {
    let members: HashMap<String, i64> = tg
        .list_bot_members(group)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| m.username.map(|u| (u.to_lowercase(), m.user_id)))
        .collect();

    let mut out = Vec::new();
    for b in roster() {
        let mut username = b.username.clone();
        let mut user_id = username
            .as_ref()
            .and_then(|u| members.get(&u.to_lowercase()).copied());
        // Not a group member (or no username on file): ask the Bot API —
        // a registered bot's token always knows its own id.
        if user_id.is_none() {
            if let Ok(me) = BotApi::new(&b.token).get_me().await {
                user_id = Some(me.id);
                username = username.or(me.username);
            }
        }
        let label = match username {
            Some(u) => format!("@{u}"),
            None => b.name.clone(),
        };
        let dm = match user_id {
            // The 1:1 chat is one of your dialogs — resolve by bare id.
            Some(id) => {
                let peer = tg.peer(&id.to_string()).await.ok();
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
async fn fetch_view(
    tg: &Tg,
    group: PeerRef,
    dms: &mut [Dm],
    sel: Sel,
) -> Result<Vec<TopicMessage>> {
    match sel {
        Sel::Topic(id) => tg.topic_history(group, id, HISTORY).await,
        Sel::Dm(uid) => {
            let dm = dms
                .iter_mut()
                .find(|d| d.user_id == uid)
                .ok_or_else(|| Error::Mtproto("bot is gone".into()))?;
            if dm.peer.is_none() && uid > 0 {
                dm.peer = tg.peer(&uid.to_string()).await.ok();
            }
            match dm.peer {
                Some(p) => tg.dm_history(p, HISTORY).await,
                None => Err(Error::Mtproto(format!("no 1:1 chat with {} yet", dm.label))),
            }
        }
    }
}

/// Re-list topics + DMs and re-fetch the selected view, emitting a Snapshot.
async fn refresh_all(
    tg: &Tg,
    group: PeerRef,
    dms: &mut Vec<Dm>,
    selected: &mut Option<Sel>,
    tx: &mpsc::Sender<Ev>,
) {
    match tg.list_topics(group).await {
        Ok(topics) => {
            *dms = build_dms(tg, group).await;
            // Drop a selection whose target vanished.
            let still_there = selected.is_some_and(|s| match s {
                Sel::Topic(id) => topics.iter().any(|t| t.id == id),
                Sel::Dm(uid) => dms.iter().any(|d| d.user_id == uid),
            });
            if !still_there {
                *selected = topics.first().map(|t| Sel::Topic(t.id));
            }
            let messages = match *selected {
                Some(s) => fetch_view(tg, group, dms, s).await.unwrap_or_default(),
                None => vec![],
            };
            let _ = tx.send(Ev::Snapshot {
                topics,
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
        Err(e) => {
            let _ = tx.send(Ev::Status(format!("topics: {e}")));
        }
    }
}

async fn worker(group: String, rx: mpsc::Receiver<Cmd>, tx: mpsc::Sender<Ev>) {
    macro_rules! status {
        ($($arg:tt)*) => {{
            let _ = tx.send(Ev::Status(format!($($arg)*)));
        }};
    }

    status!("connecting…");
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => return status!("config: {e}"),
    };
    let (api_id, api_hash) = match cfg.api_credentials() {
        Ok(v) => v,
        Err(_) => return status!("no api credentials — run `verity config`"),
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
        Ok(false) => return status!("not logged in — run `verity login`"),
        Err(e) => return status!("auth: {e}"),
    }
    let peer = match tg.peer(&group).await {
        Ok(p) => p,
        Err(e) => return status!("group: {e}"),
    };
    status!("");

    let mut dms: Vec<Dm> = Vec::new();
    let mut selected: Option<Sel> = None;
    refresh_all(&tg, peer, &mut dms, &mut selected, &tx).await;

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Refresh => refresh_all(&tg, peer, &mut dms, &mut selected, &tx).await,
            Cmd::Poll => {
                if let Some(s) = selected {
                    match fetch_view(&tg, peer, &mut dms, s).await {
                        Ok(messages) => {
                            let _ = tx.send(Ev::Messages { sel: s, messages });
                        }
                        Err(e) => status!("poll: {e}"),
                    }
                }
            }
            Cmd::Select(s) => {
                selected = Some(s);
                match fetch_view(&tg, peer, &mut dms, s).await {
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
                // Topics get a threaded reply; DMs go straight to the bot.
                let sent = match s {
                    Sel::Topic(t) => tg.send_message(peer, Some(t), &text).await,
                    Sel::Dm(uid) => {
                        match dms.iter().find(|d| d.user_id == uid).and_then(|d| d.peer) {
                            Some(p) => tg.send_message(p, None, &text).await,
                            None => Err(Error::Mtproto("no 1:1 chat to send into".into())),
                        }
                    }
                };
                if let Err(e) = sent {
                    status!("send: {e}");
                    continue;
                }
                match fetch_view(&tg, peer, &mut dms, s).await {
                    Ok(messages) => {
                        let _ = tx.send(Ev::Messages { sel: s, messages });
                    }
                    Err(e) => status!("history: {e}"),
                }
            }
        }
    }
}

/// `HH:MM` UTC — enough to order a transcript at a glance without
/// dragging a date library in.
fn hhmm(unix: i64) -> String {
    let t = unix.rem_euclid(86_400);
    format!("{:02}:{:02}", t / 3600, (t % 3600) / 60)
}

struct Root {
    cmd_tx: mpsc::Sender<Cmd>,
    ev_rx: mpsc::Receiver<Ev>,
    topics: Vec<TopicInfo>,
    dms: Vec<DmRow>,
    selected: Option<Sel>,
    messages: Vec<TopicMessage>,
    input: String,
    status: String,
    /// Id of the newest message shown — auto-scroll only on arrivals.
    last_msg: Option<i32>,
    /// A Poll is in flight — don't stack another on top of it.
    awaiting: bool,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
}

impl Root {
    fn new(cmd_tx: mpsc::Sender<Cmd>, ev_rx: mpsc::Receiver<Ev>, cx: &mut Context<Self>) -> Self {
        let this = Self {
            cmd_tx,
            ev_rx,
            topics: vec![],
            dms: vec![],
            selected: None,
            messages: vec![],
            input: String::new(),
            status: "connecting…".into(),
            last_msg: None,
            awaiting: false,
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
        };
        // Pump worker events into view state; live-tick the selected
        // view every ~3s, re-list topics + DMs every ~30s.
        cx.spawn(async move |this: WeakEntity<Root>, cx: &mut AsyncApp| {
            let mut ticks = 0u32;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                ticks += 1;
                let poll = ticks % 12 == 0;
                let refresh = ticks % 120 == 0;
                let alive = this
                    .update(cx, |this, cx| {
                        this.drain_events();
                        if refresh {
                            let _ = this.cmd_tx.send(Cmd::Refresh);
                        } else if poll && !this.awaiting && this.selected.is_some() {
                            if this.cmd_tx.send(Cmd::Poll).is_ok() {
                                this.awaiting = true;
                            }
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
                    topics,
                    dms,
                    selected,
                    messages,
                } => {
                    self.topics = topics;
                    self.dms = dms;
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
            }
        }
        if any {
            self.awaiting = false;
        }
    }

    fn select(&mut self, sel: Sel, cx: &mut Context<Self>) {
        if self.selected != Some(sel) {
            self.selected = Some(sel);
            self.messages.clear();
            self.last_msg = None;
            let _ = self.cmd_tx.send(Cmd::Select(sel));
            cx.notify();
        }
    }

    /// Combined sidebar order — topics first, then DMs — for ↑/↓.
    fn items(&self) -> Vec<Sel> {
        self.topics
            .iter()
            .map(|t| Sel::Topic(t.id))
            .chain(self.dms.iter().map(|d| Sel::Dm(d.user_id)))
            .collect()
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

    fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "backspace" => {
                self.input.pop();
            }
            "enter" => self.send_input(),
            "escape" => self.input.clear(),
            "up" => self.move_selection(-1, cx),
            "down" => self.move_selection(1, cx),
            "r" if ks.modifiers.control || ks.modifiers.platform => {
                let _ = self.cmd_tx.send(Cmd::Refresh);
            }
            _ => {
                let m = &ks.modifiers;
                if !(m.control || m.platform || m.alt) {
                    if let Some(c) = &ks.key_char {
                        self.input.push_str(c);
                    }
                }
            }
        }
        cx.notify();
    }

    /// Title for the transcript header — topic title or @bot.
    fn view_title(&self) -> String {
        match self.selected {
            Some(Sel::Topic(id)) => self
                .topics
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.title.clone())
                .unwrap_or_else(|| "…".into()),
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected;
        let dim = rgb(0x8b949e);
        let text = rgb(0xc9d1d9);
        let accent = rgb(0x58a6ff);
        let selected_bg = rgb(0x1f6feb);
        let hover_bg = rgb(0x21262d);

        let section = |label: &'static str| {
            div()
                .px_2()
                .pt_3()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(dim)
                .child(label)
        };

        let sidebar = div()
            .id("sidebar")
            .w(px(230.0))
            .h_full()
            .bg(rgb(0x161b22))
            .border_r_1()
            .border_color(rgb(0x30363d))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .child(
                div()
                    .p_3()
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .child("Verity"),
            )
            .child(section("TOPICS"))
            .children(self.topics.iter().map(|t| {
                let id = t.id;
                div()
                    .id(ElementId::Name(format!("topic-{id}").into()))
                    .mx_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected == Some(Sel::Topic(id)), |d| d.bg(selected_bg))
                    .hover(|d| d.bg(hover_bg))
                    .child(t.title.clone())
                    .on_click(cx.listener(move |this, _ev: &ClickEvent, _w, cx| {
                        this.select(Sel::Topic(id), cx);
                    }))
            }))
            .child(section("DIRECT"))
            .children(self.dms.iter().map(|d| {
                let uid = d.user_id;
                div()
                    .id(ElementId::Name(format!("dm-{uid}").into()))
                    .mx_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
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
            .h(px(44.0))
            .px_4()
            .border_b_1()
            .border_color(rgb(0x30363d))
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.view_title()),
            )
            .child(div().text_xs().text_color(dim).child(self.status.clone()));

        let messages = div()
            .id("messages")
            .flex_1()
            .h_full()
            .p_4()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_col()
            .gap_2()
            .children(self.messages.iter().map(|m| {
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(div().text_xs().text_color(accent).child(m.sender.clone()))
                            .child(div().text_xs().text_color(dim).child(hhmm(m.date))),
                    )
                    .child(div().text_color(text).child(m.text.clone()))
            }))
            .when(self.messages.is_empty(), |d| {
                d.child(div().text_color(dim).child(if self.status.is_empty() {
                    "no messages".to_string()
                } else {
                    self.status.clone()
                }))
            });

        let hint = match self.selected {
            Some(Sel::Topic(_)) => {
                "Message this topic…  (Enter sends, Esc clears, ↑↓ switch, ⌃R refresh)"
            }
            Some(Sel::Dm(_)) => {
                "Message the bot…  (Enter sends, Esc clears, ↑↓ switch, ⌃R refresh)"
            }
            None => "Select a topic or a bot…",
        };
        let composer = div()
            .h(px(44.0))
            .bg(rgb(0x161b22))
            .border_t_1()
            .border_color(rgb(0x30363d))
            .px_4()
            .flex()
            .items_center()
            .child(
                div()
                    .text_color(if self.input.is_empty() { dim } else { text })
                    .child(if self.input.is_empty() {
                        hint.to_string()
                    } else {
                        format!("{}▌", self.input)
                    }),
            );

        let chat = div()
            .flex_1()
            .h_full()
            .bg(rgb(0x0d1117))
            .flex()
            .flex_col()
            .child(header)
            .child(div().flex_1().h_full().flex().child(messages))
            .child(composer);

        div()
            .id("root")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                this.on_key(ev, cx);
            }))
            .size_full()
            .flex()
            .text_color(text)
            .child(sidebar)
            .child(chat)
    }
}

fn main() {
    let group = std::env::args().nth(1).unwrap_or_else(|| {
        Config::load()
            .ok()
            .and_then(|c| c.group_id)
            .map(|g| g.to_string())
            .unwrap_or_default()
    });

    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (ev_tx, ev_rx) = mpsc::channel::<Ev>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(worker(group, cmd_rx, ev_tx));
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
