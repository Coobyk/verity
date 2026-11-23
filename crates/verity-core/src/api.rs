//! Authenticated JSON API for the mobile companion app. `verity serve`
//! binds it when `api_bind` is set in config.toml; every route except
//! `/api/health` needs `Authorization: Bearer <api_token>` — with no
//! token configured the API refuses everything (never run it open).

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use crate::botapi::BotApi;
use crate::mtproto::Tg;
use crate::store::{Bot, Store};
use crate::{Error, Result};

/// Shared handler state — everything a route needs besides the request.
pub struct Api {
    /// User-side session the API reads and writes Telegram through.
    pub tg: Arc<Tg>,
    /// sqlite path — opened per request, like the bridge's `db_call`.
    pub db: PathBuf,
    /// Bearer token from `api_token`; `None` refuses all but /api/health.
    pub token: Option<String>,
}

type Resp = Response<Full<Bytes>>;

fn reply(status: StatusCode, body: Value) -> Resp {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("static response")
}

fn fail(status: StatusCode, e: impl std::fmt::Display) -> Resp {
    reply(status, json!({ "error": e.to_string() }))
}

/// Upstream (MTProto/Bot API/sqlite) failure — the bridge is alive but
/// Telegram isn't cooperating right now.
fn respond(r: Result<Value>) -> Resp {
    match r {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => fail(StatusCode::BAD_GATEWAY, e),
    }
}

/// Accept connections forever — run under `tokio::spawn` from `serve`.
pub async fn serve(bind: &str, api: Api) -> Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!(addr = %bind, "api listening");
    let api = Arc::new(api);
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                warn!(error = %e, "api accept failed");
                continue;
            }
        };
        let api = api.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let api = api.clone();
                async move { Ok::<_, std::convert::Infallible>(route(api, req).await) }
            });
            if let Err(e) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .await
            {
                debug!(error = %e, "api connection ended");
            }
        });
    }
}

async fn route(api: Arc<Api>, req: Request<Incoming>) -> Resp {
    // The health probe stays open — the only unauthenticated route.
    if req.method() == Method::GET && req.uri().path() == "/api/health" {
        return reply(StatusCode::OK, json!({ "ok": true }));
    }
    let authed = match &api.token {
        Some(token) => req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| t == token.as_str()),
        // No token configured: refuse everything but the health probe.
        None => false,
    };
    if !authed {
        return fail(StatusCode::UNAUTHORIZED, "unauthorized");
    }

    // Clone the routing inputs so the POST arm can move `req` (for
    // `into_body`) — a scrutinee borrow would outlive the match.
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (method, segs.as_slice()) {
        (Method::GET, ["api", "dms"]) => respond(dms(&api).await),
        (Method::GET, ["api", "dms", uid, "messages"]) => match uid.parse::<i64>() {
            Ok(uid) => dm_history(&api, uid, limit(&req)).await,
            Err(_) => fail(StatusCode::BAD_REQUEST, "bad user id"),
        },
        (Method::POST, ["api", "messages"]) => send(api, req).await,
        _ => fail(StatusCode::NOT_FOUND, "not found"),
    }
}

/// `?limit=N` — default 50, clamped to 200.
fn limit(req: &Request<Incoming>) -> i32 {
    req.uri()
        .query()
        .and_then(|q| {
            q.split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == "limit")
                .map(|(_, v)| v)
        })
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(50)
        .clamp(1, 200)
}

/// Port of the GUI's `build_dms` (crates/verity-gui/src/main.rs): one
/// row per registered bot. The bot's user id comes from `getMe` on its
/// token; `available` means the owner's 1:1 dialog with that user
/// resolved. A bot whose account can't be identified keeps a row with a
/// negative sentinel id (`available: false`) so the roster isn't
/// silently missing one.
async fn dms(api: &Api) -> Result<Value> {
    let db = api.db.clone();
    let bots =
        tokio::task::spawn_blocking(move || -> Result<Vec<Bot>> { Store::open(&db)?.list_bots() })
            .await
            .map_err(|e| Error::Other(e.to_string()))??;

    let mut out = Vec::new();
    for b in bots {
        let mut username = b.username.clone();
        let mut user_id = None;
        // A registered bot's token always knows its own id.
        if let Ok(me) = BotApi::new(&b.token).get_me().await {
            user_id = Some(me.id);
            username = username.or(me.username);
        }
        let label = match username {
            Some(u) => format!("@{u}"),
            None => b.name.clone(),
        };
        // The 1:1 chat is one of the owner's dialogs — resolve by bare id.
        let (user_id, available) = match user_id {
            Some(id) => (id, api.tg.peer(&id.to_string()).await.is_ok()),
            None => (-(out.len() as i64) - 1, false),
        };
        out.push(json!({
            "user_id": user_id,
            "name": b.name,
            "label": label,
            "available": available,
        }));
    }
    Ok(json!({ "dms": out }))
}

/// 1:1 history with a bot user — `no dm` when the owner's dialog with
/// that id can't be resolved (never opened, or not a user).
async fn dm_history(api: &Api, user_id: i64, limit: i32) -> Resp {
    match api.tg.peer(&user_id.to_string()).await {
        Ok(peer) => respond(
            api.tg
                .dm_history(peer, limit)
                .await
                .map(|m| json!({ "messages": m })),
        ),
        Err(_) => fail(StatusCode::NOT_FOUND, "no dm"),
    }
}

#[derive(Deserialize)]
struct SendBody {
    /// Bot user id whose 1:1 chat gets the message.
    dm: i64,
    text: String,
}

async fn send(api: Arc<Api>, req: Request<Incoming>) -> Resp {
    let body = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return fail(StatusCode::BAD_REQUEST, e),
    };
    let SendBody { dm, text } = match serde_json::from_slice::<SendBody>(&body) {
        Ok(b) => b,
        Err(e) => return fail(StatusCode::BAD_REQUEST, e),
    };
    let peer = match api.tg.peer(&dm.to_string()).await {
        Ok(p) => p,
        Err(_) => return fail(StatusCode::NOT_FOUND, "no dm"),
    };
    respond(
        api.tg
            .send_message(peer, &text)
            .await
            .map(|_| json!({ "ok": true })),
    )
}
