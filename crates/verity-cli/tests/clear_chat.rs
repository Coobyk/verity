use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::mpsc;
use verity_core::store::{Bot, Store};

const GROUP: i64 = -100123;
const OLD_TOPIC: i64 = 7;
const NEW_TOPIC: i64 = 8;

struct Request {
    method: String,
    body: Value,
    stream: TcpStream,
}

impl Request {
    async fn read(stream: TcpStream) -> Self {
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        let path = line.split_whitespace().nth(1).unwrap();
        let method = path.rsplit('/').next().unwrap().to_string();
        let mut len = 0;
        loop {
            line.clear();
            assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; len];
        stream.read_exact(&mut body).await.unwrap();
        Self {
            method,
            body: serde_json::from_slice(&body).unwrap(),
            stream: stream.into_inner(),
        }
    }

    async fn reply(mut self, result: Value) {
        let body = json!({"ok": true, "result": result}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        self.stream.write_all(response.as_bytes()).await.unwrap();
    }
}

async fn next_request(requests: &mut mpsc::UnboundedReceiver<Request>) -> Request {
    tokio::time::timeout(Duration::from_secs(10), requests.recv())
        .await
        .expect("waiting for Bot API request")
        .unwrap()
}

fn update(id: i64, topic: i64, text: &str) -> Value {
    json!({
        "update_id": id,
        "message": {
            "message_id": id,
            "message_thread_id": topic,
            "from": {"id": 2, "is_bot": false, "first_name": "Owner"},
            "chat": {"id": GROUP, "type": "supergroup"},
            "date": 0,
            "text": text,
        }
    })
}

#[tokio::test]
async fn clear_chat_replaces_the_topic_without_restarting_serve() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!("group_id = {GROUP}\n"),
    )
    .unwrap();
    let db = home.path().join("state.sqlite");
    Store::open(&db)
        .unwrap()
        .add_bot(&Bot {
            name: "scout".into(),
            token: "TEST_TOKEN".into(),
            username: None,
            topic_id: Some(OLD_TOPIC),
            instructions: String::new(),
            provider: "echo".into(),
            model: None,
            greeting: None,
        })
        .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (tx, mut requests) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            if tx.send(Request::read(stream).await).is_err() {
                break;
            }
        }
    });
    let cli = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_verity"));
        cmd.env("VERITY_HOME", home.path())
            .env("HOME", home.path())
            .env("VERITY_BACKUP_EVERY", "0")
            .env("VERITY_BOT_API_BASE", &base)
            .kill_on_drop(true);
        cmd
    };
    let mut serve = cli()
        .args(["serve", "--echo", "--no-context"])
        .spawn()
        .unwrap();
    let me = next_request(&mut requests).await;
    assert_eq!(me.method, "getMe");
    me.reply(json!({"id": 1, "is_bot": true, "first_name": "Scout"}))
        .await;
    let backlog = next_request(&mut requests).await;
    assert_eq!(backlog.method, "getUpdates");
    assert_eq!(backlog.body["offset"], -1);
    backlog.reply(json!([{"update_id": 40}])).await;

    // Leave the first long-poll open while the real clear-chat process runs.
    let held_poll = next_request(&mut requests).await;
    assert_eq!(held_poll.method, "getUpdates");
    assert_eq!(held_poll.body["offset"], 41);
    let mut clear = cli().args(["clear-chat", "scout"]).spawn().unwrap();
    let delete = next_request(&mut requests).await;
    assert_eq!(delete.method, "deleteForumTopic");
    assert_eq!(delete.body["message_thread_id"], OLD_TOPIC);
    delete.reply(json!(true)).await;
    assert!(tokio::time::timeout(Duration::from_secs(10), clear.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    held_poll
        .reply(json!([update(41, OLD_TOPIC, "stale")]))
        .await;

    let create = next_request(&mut requests).await;
    assert_eq!(
        create.method, "createForumTopic",
        "must replace the cleared topic"
    );
    assert_eq!(create.body["name"], "scout");
    create
        .reply(json!({"message_thread_id": NEW_TOPIC, "name": "scout", "icon_color": 0}))
        .await;

    let poll = next_request(&mut requests).await;
    assert_eq!(poll.method, "getUpdates", "must ignore the stale message");
    assert_eq!(poll.body["offset"], 42, "must keep the live polling offset");
    let stored = Store::open(&db).unwrap().get_bot("scout").unwrap().unwrap();
    assert_eq!(stored.topic_id, Some(NEW_TOPIC));
    poll.reply(json!([update(42, NEW_TOPIC, "hello replacement")]))
        .await;

    let mut pending_poll = None;
    let mut replied = false;
    loop {
        let request = next_request(&mut requests).await;
        match request.method.as_str() {
            "getUpdates" => {
                assert_eq!(request.body["offset"], 43);
                assert!(pending_poll.replace(request).is_none());
            }
            "sendChatAction" => request.reply(json!(true)).await,
            "setMessageReaction" => {
                assert_eq!(
                    request.body["message_id"], 42,
                    "must ignore the stale message"
                );
                let finished = request.body["reaction"] == json!([]);
                request.reply(json!(true)).await;
                if finished {
                    assert!(replied);
                    break;
                }
            }
            "sendRichMessage" => {
                assert_eq!(request.body["message_thread_id"], NEW_TOPIC);
                assert_eq!(request.body["reply_parameters"]["message_id"], 42);
                assert!(request.body["rich_message"]["markdown"]
                    .as_str()
                    .unwrap()
                    .contains("heard: hello replacement"));
                request
                    .reply(json!({
                        "message_id": 100,
                        "chat": {"id": GROUP, "type": "supergroup"},
                        "date": 0,
                    }))
                    .await;
                replied = true;
            }
            method => panic!("unexpected Bot API method: {method}"),
        }
    }
    assert!(serve.try_wait().unwrap().is_none());
    println!("deleted topic {OLD_TOPIC}; ignored stale update; echoed in topic {NEW_TOPIC} (same serve, offset 42)");
    serve.kill().await.unwrap();
}
