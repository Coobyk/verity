//! verity-gui — GPUI client. Reads and writes the bot supergroup as *you*
//! (MTProto), so chat history comes straight from Telegram: no daemon
//! required to read, and a typed message lands in the topic like one sent
//! from any Telegram client.
//!
//! A worker thread owns a tokio runtime + the grammers client; the view
//! swaps commands and snapshots with it over std mpsc channels.

use std::sync::mpsc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::*;
use grammers_session::defs::PeerRef;
use verity_core::config::Config;
use verity_core::mtproto::{Tg, TopicInfo, TopicMessage};

enum Cmd {
    /// Re-list topics and re-fetch the selected topic's messages.
    Refresh,
    /// Fetch one topic's messages (also marks it selected worker-side).
    Select(i64),
    /// Post `text` into `topic` as the user, then refresh its messages.
    Send(i64, String),
}

enum Ev {
    /// Full refresh: topic list, current selection, its messages.
    Snapshot {
        topics: Vec<TopicInfo>,
        selected: Option<i64>,
        messages: Vec<TopicMessage>,
    },
    /// New messages for one topic (ignored if not selected anymore).
    Messages {
        topic: i64,
        messages: Vec<TopicMessage>,
    },
    /// One-line status / error for the footer.
    Status(String),
}

const HISTORY: i32 = 100;

/// Re-fetch topics and the selected topic's messages, emitting a Snapshot.
async fn refresh_all(tg: &Tg, peer: &PeerRef, selected: Option<i64>, tx: &mpsc::Sender<Ev>) {
    match tg.list_topics(*peer).await {
        Ok(topics) => {
            let selected = selected.or_else(|| topics.first().map(|t| t.id));
            let messages = match selected {
                Some(id) => tg
                    .topic_history(*peer, id, HISTORY)
                    .await
                    .unwrap_or_default(),
                None => vec![],
            };
            let _ = tx.send(Ev::Snapshot {
                topics,
                selected,
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
    let tg = match Config::load().and_then(|c| c.api_credentials().map(|(id, hash)| (c, id, hash)))
    {
        Ok((_, api_id, api_hash)) => {
            let path = match Config::session_path() {
                Ok(p) => p,
                Err(_) => return status!("no home dir"),
            };
            match Tg::connect(api_id, api_hash, &path).await {
                Ok(t) => t,
                Err(e) => return status!("connect: {e}"),
            }
        }
        Err(_) => return status!("no api credentials — run `verity config`"),
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

    let mut selected: Option<i64> = None;
    refresh_all(&tg, &peer, selected, &tx).await;

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Refresh => refresh_all(&tg, &peer, selected, &tx).await,
            Cmd::Select(topic) => {
                selected = Some(topic);
                match tg.topic_history(peer, topic, HISTORY).await {
                    Ok(messages) => {
                        let _ = tx.send(Ev::Messages { topic, messages });
                    }
                    Err(e) => status!("history: {e}"),
                }
            }
            Cmd::Send(topic, text) => {
                if let Err(e) = tg.send_message(peer, Some(topic), &text).await {
                    status!("send: {e}");
                    continue;
                }
                match tg.topic_history(peer, topic, HISTORY).await {
                    Ok(messages) => {
                        let _ = tx.send(Ev::Messages { topic, messages });
                    }
                    Err(e) => status!("history: {e}"),
                }
            }
        }
    }
}

struct Root {
    cmd_tx: mpsc::Sender<Cmd>,
    ev_rx: mpsc::Receiver<Ev>,
    topics: Vec<TopicInfo>,
    selected: Option<i64>,
    messages: Vec<TopicMessage>,
    input: String,
    status: String,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
}

impl Root {
    fn new(cmd_tx: mpsc::Sender<Cmd>, ev_rx: mpsc::Receiver<Ev>, cx: &mut Context<Self>) -> Self {
        let this = Self {
            cmd_tx,
            ev_rx,
            topics: vec![],
            selected: None,
            messages: vec![],
            input: String::new(),
            status: "connecting…".into(),
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::default(),
        };
        // Pump worker events into view state; nudge a refresh every ~5s.
        cx.spawn(async move |this: WeakEntity<Root>, cx: &mut AsyncApp| {
            let mut ticks = 0u32;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                ticks += 1;
                let refresh = ticks.is_multiple_of(16);
                let alive = this
                    .update(cx, |this, cx| {
                        this.drain_events();
                        if refresh {
                            let _ = this.cmd_tx.send(Cmd::Refresh);
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

    fn drain_events(&mut self) {
        while let Ok(ev) = self.ev_rx.try_recv() {
            match ev {
                Ev::Snapshot {
                    topics,
                    selected,
                    messages,
                } => {
                    self.topics = topics;
                    self.selected = selected;
                    self.messages = messages;
                    self.scroll.scroll_to_bottom();
                }
                Ev::Messages { topic, messages } => {
                    if self.selected == Some(topic) {
                        self.messages = messages;
                        self.scroll.scroll_to_bottom();
                    }
                }
                Ev::Status(s) => self.status = s,
            }
        }
    }

    fn select(&mut self, topic: i64, cx: &mut Context<Self>) {
        if self.selected != Some(topic) {
            self.selected = Some(topic);
            self.messages.clear();
            let _ = self.cmd_tx.send(Cmd::Select(topic));
            cx.notify();
        }
    }

    fn send_input(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        if let Some(topic) = self.selected {
            let _ = self.cmd_tx.send(Cmd::Send(topic, text));
            self.input.clear();
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "backspace" => {
                self.input.pop();
            }
            "enter" => self.send_input(),
            "escape" => self.input.clear(),
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
}

impl Focusable for Root {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected;
        let topics = div()
            .w(px(220.0))
            .h_full()
            .bg(rgb(0x1b1d23))
            .p_3()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().text_color(rgb(0x8b949e)).child("TOPICS"))
            .children(self.topics.iter().map(|t| {
                let id = t.id;
                div()
                    .id(ElementId::Name(format!("topic-{id}").into()))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected == Some(id), |d| d.bg(rgb(0x1f6feb)))
                    .child(t.title.clone())
                    .on_click(cx.listener(move |this, _ev: &ClickEvent, _w, cx| {
                        this.select(id, cx);
                    }))
            }));

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
                            .text_xs()
                            .text_color(rgb(0x58a6ff))
                            .child(m.sender.clone()),
                    )
                    .child(div().text_color(rgb(0xc9d1d9)).child(m.text.clone()))
            }))
            .when(self.messages.is_empty(), |d| {
                d.child(div().text_color(rgb(0x8b949e)).child(self.status.clone()))
            });

        let composer = div()
            .h(px(44.0))
            .bg(rgb(0x161b22))
            .px_4()
            .flex()
            .items_center()
            .child(
                div()
                    .text_color(if self.input.is_empty() {
                        rgb(0x8b949e)
                    } else {
                        rgb(0xc9d1d9)
                    })
                    .child(if self.input.is_empty() {
                        "Message…  (Enter sends, Esc clears)".to_string()
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
            .text_color(rgb(0xc9d1d9))
            .child(topics)
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
        let bounds = Bounds::centered(None, size(px(900.0), px(600.0)), cx);
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
