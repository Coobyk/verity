//! verity-gui — GPUI client. Reads the bot supergroup as *you* (MTProto),
//! so chat history comes straight from Telegram: no daemon required to read.
//!
//! Skeleton: one blocking fetch at startup, topics left / messages right.

use gpui::*;
use gpui::prelude::FluentBuilder;
use verity_core::config::Config;
use verity_core::mtproto::{Tg, TopicInfo, TopicMessage};

#[derive(Debug)]
struct Snapshot {
    topics: Vec<TopicInfo>,
    messages: Vec<TopicMessage>,
    status: String,
}

struct Root {
    snapshot: Snapshot,
}

fn fetch_snapshot(group: String) -> Snapshot {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => return empty(format!("tokio: {e}")),
    };
    rt.block_on(async {
        let cfg = match Config::load() {
            Ok(c) => c,
            Err(_) => return empty("no config — run `verity config`".into()),
        };
        let Ok((api_id, api_hash)) = cfg.api_credentials() else {
            return empty("no api credentials — run `verity config`".into());
        };
        let Ok(path) = Config::session_path() else {
            return empty("no home dir".into());
        };
        let tg = match Tg::connect(api_id, api_hash, &path).await {
            Ok(t) => t,
            Err(e) => return empty(format!("connect: {e}")),
        };
        match tg.is_authorized().await {
            Ok(false) => return empty("not logged in — run `verity login`".into()),
            Err(e) => return empty(format!("auth: {e}")),
            _ => {}
        }
        match tg.list_topics(&group).await {
            Ok(topics) => {
                let messages = match topics.first() {
                    Some(t) => tg.topic_history(&group, t.id, 50).await.unwrap_or_default(),
                    None => vec![],
                };
                Snapshot { topics, messages, status: "ok".into() }
            }
            Err(e) => empty(format!("topics: {e}")),
        }
    })
}

fn empty(status: String) -> Snapshot {
    Snapshot { topics: vec![], messages: vec![], status }
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let topics = div()
            .w(px(220.0))
            .h_full()
            .bg(rgb(0x1b1d23))
            .p_3()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().text_color(rgb(0x8b949e)).child("TOPICS"))
            .children(self.snapshot.topics.iter().map(|t| {
                div().px_2().py_1().rounded_md().child(t.title.clone())
            }));

        let messages = div()
            .flex_1()
            .h_full()
            .bg(rgb(0x0d1117))
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .children(self.snapshot.messages.iter().map(|m| {
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_xs().text_color(rgb(0x58a6ff)).child(m.sender.clone()))
                    .child(div().text_color(rgb(0xc9d1d9)).child(m.text.clone()))
            }))
            .when(self.snapshot.messages.is_empty(), |d| {
                d.child(div().text_color(rgb(0x8b949e)).child(self.snapshot.status.clone()))
            });

        div()
            .size_full()
            .flex()
            .text_color(rgb(0xc9d1d9))
            .child(topics)
            .child(messages)
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
    let snapshot = fetch_snapshot(group);

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
            move |_, cx| cx.new(|_| Root { snapshot }),
        )
        .unwrap();
        cx.activate(true);
    });
}
