//! JSON-lines host for the WinUI 3 desktop shell.
//!
//! The WinUI process owns presentation only. This binary owns Telegram,
//! the local Verity configuration, and the bot roster. Commands arrive on
//! stdin as one JSON object per line; events leave on stdout the same way.
//! Keep every diagnostic off stdout — it is part of the wire protocol.

use grammers_session::defs::PeerRef;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use verity_core::botapi::BotApi;
use verity_core::config::Config;
use verity_core::mtproto::{split_math, Blk, LoginCodeResult, Tg, TgEvent, TopicMessage};
use verity_core::store::{Bot, Store};
use verity_core::{Error, Result};

const HISTORY: i32 = 100;

/// Commands the WinUI shell can send. The serde representation is the
/// stable wire protocol; keep variants snake_case and additive.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Cmd {
    /// Save Telegram API credentials entered during onboarding.
    Configure {
        api_id: String,
        api_hash: String,
    },
    LoginPhone {
        phone: String,
    },
    LoginCode {
        code: String,
    },
    LoginPassword {
        password: String,
    },
    Refresh,
    Poll,
    Select {
        user_id: i64,
    },
    /// Create a bot through @BotFather and register it in the roster.
    CreateBot {
        name: String,
        username: String,
        instructions: String,
        provider: String,
        model: Option<String>,
        greeting: Option<String>,
    },
    /// Register an existing BotFather token in the roster.
    RegisterBot {
        name: String,
        token: String,
        instructions: String,
        provider: String,
        model: Option<String>,
        greeting: Option<String>,
    },
    /// Override the pi binary path used by the pi harness.
    SetPiBin {
        bin: String,
    },
    /// Re-probe PATH for known agent harnesses and emit `harnesses`.
    DetectHarnesses,
    /// Install a known harness (`pi`, `claude-code-acp`, `codex`).
    InstallHarness {
        id: String,
    },
    Send {
        text: String,
    },
    React {
        msg_id: i32,
        emoji: String,
        chosen: bool,
    },
}

/// One bot row in the WinUI sidebar.
#[derive(Debug, Clone, Serialize)]
struct DmRow {
    user_id: i64,
    label: String,
    available: bool,
}

/// A rendered formula in the local cache. Sending paths instead of image
/// bytes keeps the JSON protocol small; the shell and host always run on
/// the same machine.
#[derive(Debug, Clone, Serialize)]
struct MathImage {
    path: String,
    display: bool,
}

/// `TopicMessage` plus host-rendered math attachments. Flattening keeps
/// the existing message fields stable while adding `math` additively.
#[derive(Debug, Serialize)]
struct WireMessage {
    #[serde(flatten)]
    message: TopicMessage,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    math: Vec<MathImage>,
}

/// Events emitted to the WinUI shell. Like `Cmd`, this enum is the wire
/// protocol; additive changes are safe, renames are not.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Ev {
    Snapshot {
        me: Option<i64>,
        dms: Vec<DmRow>,
        selected_user_id: Option<i64>,
        messages: Vec<WireMessage>,
    },
    Messages {
        user_id: i64,
        messages: Vec<WireMessage>,
    },
    Status {
        message: String,
    },
    NeedApiKeys,
    NeedPhone,
    NeedCode {
        delivery: String,
        next_delivery: Option<String>,
    },
    NeedPassword,
    LoggedIn,
    /// A bot was just created/registered — the shell can leave its
    /// setup screen. A roster snapshot follows.
    BotReady {
        name: String,
        username: Option<String>,
    },
    /// Bot creation/registration failed; the shell re-enables its form.
    /// Details are in the preceding status events.
    BotSetupFailed,
    /// Detection results for the known agent harnesses.
    Harnesses {
        items: Vec<HarnessInfo>,
    },
}

/// One known agent harness and whether it is usable right now.
#[derive(Debug, Clone, Serialize)]
struct HarnessInfo {
    id: String,
    label: String,
    /// Binary or shell command the harness would run.
    bin: String,
    installed: bool,
    /// True when the host knows an automated install recipe.
    installable: bool,
    note: String,
}

fn status(tx: &mpsc::UnboundedSender<Ev>, message: impl Into<String>) {
    let _ = tx.send(Ev::Status {
        message: message.into(),
    });
}

/// Is `bin` discoverable on PATH? PATHEXT-aware on Windows.
fn on_path(bin: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.BAT;.CMD".into())
            .split(';')
            .map(|e| e.to_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&paths).any(|dir| {
        let cand = dir.join(bin);
        if cand.is_file() {
            return true;
        }
        exts.iter()
            .any(|ext| !ext.is_empty() && dir.join(format!("{bin}{ext}")).is_file())
    })
}

/// Probe PATH for every harness the setup screen knows about.
fn probe_harnesses() -> Vec<HarnessInfo> {
    let pi_found = on_path("pi");
    let pirs_found = on_path("pirs");
    vec![
        HarnessInfo {
            id: "pi".into(),
            label: "pi — pi agent CLI".into(),
            bin: if pi_found {
                "pi"
            } else if pirs_found {
                "pirs"
            } else {
                "pi"
            }
            .into(),
            installed: pi_found || pirs_found,
            installable: true,
            note: "cargo install pi-rs".into(),
        },
        HarnessInfo {
            id: "devin".into(),
            label: "devin — Devin over ACP".into(),
            bin: "devin".into(),
            installed: on_path("devin"),
            installable: true,
            note: "cli.devin.ai installer".into(),
        },
        HarnessInfo {
            id: "echo".into(),
            label: "echo — test harness".into(),
            bin: String::new(),
            installed: true,
            installable: false,
            note: "built-in".into(),
        },
        HarnessInfo {
            id: "claude-code-acp".into(),
            label: "claude-code-acp — Claude over ACP".into(),
            bin: "claude-code-acp".into(),
            installed: on_path("claude-code-acp"),
            installable: true,
            note: "npm install -g @zed-industries/claude-code-acp".into(),
        },
        HarnessInfo {
            id: "codex".into(),
            label: "codex acp — Codex over ACP".into(),
            bin: "codex".into(),
            installed: on_path("codex"),
            installable: true,
            note: "npm install -g @openai/codex".into(),
        },
    ]
}

fn emit_harnesses(tx: &mpsc::UnboundedSender<Ev>) {
    let _ = tx.send(Ev::Harnesses {
        items: probe_harnesses(),
    });
}

/// Run the install recipe for a known harness, streaming output lines as
/// status events. Always re-probes and re-emits `harnesses` when done —
/// the shell treats that event as end-of-busy regardless of outcome.
async fn install_harness(id: &str, tx: &mpsc::UnboundedSender<Ev>) {
    let (prog, args): (&str, Vec<&str>) = match id {
        "pi" => ("cargo", vec!["install", "pi-rs"]),
        "claude-code-acp" => (
            "npm",
            vec!["install", "-g", "@zed-industries/claude-code-acp"],
        ),
        "devin" if cfg!(windows) => (
            "powershell",
            vec!["-c", "irm https://cli.devin.ai/install.ps1 | iex"],
        ),
        "devin" => (
            "sh",
            vec!["-c", "curl -fsSL https://cli.devin.ai/install.sh | sh"],
        ),
        "codex" => ("npm", vec!["install", "-g", "@openai/codex"]),
        other => {
            status(tx, format!("no install recipe for '{other}'"));
            emit_harnesses(tx);
            return;
        }
    };
    if !on_path(prog) {
        status(
            tx,
            format!("can't install {id}: '{prog}' itself is not on PATH"),
        );
        emit_harnesses(tx);
        return;
    }
    status(
        tx,
        format!("installing {id} — this can take a few minutes…"),
    );

    // npm is a batch script on Windows: route it through cmd.exe there.
    let mut cmd = if cfg!(windows) && prog == "npm" {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/c").arg(prog).args(&args);
        c
    } else {
        let mut c = tokio::process::Command::new(prog);
        c.args(&args);
        c
    };
    let mut child = match cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            status(tx, format!("install {id}: {e}"));
            emit_harnesses(tx);
            return;
        }
    };

    fn forward<S>(stream: S, tx: mpsc::UnboundedSender<Ev>) -> tokio::task::JoinHandle<()>
    where
        S: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let t = line.trim();
                if !t.is_empty() {
                    status(&tx, t.to_string());
                }
            }
        })
    }
    let mut tasks = Vec::new();
    if let Some(out) = child.stdout.take() {
        tasks.push(forward(out, tx.clone()));
    }
    if let Some(err) = child.stderr.take() {
        tasks.push(forward(err, tx.clone()));
    }
    let result = child.wait().await;
    for task in tasks {
        let _ = task.await;
    }

    match result {
        Ok(exit) if exit.success() => {
            status(tx, format!("{id} installed"));
            // `cargo install pi-rs` provides `pirs`, not `pi` — point the
            // harness at it when upstream pi itself is absent.
            if id == "pi" && !on_path("pi") && on_path("pirs") {
                if let Ok(mut cfg) = Config::load() {
                    cfg.pi_bin = Some("pirs".into());
                    if let Err(e) = cfg.save() {
                        status(tx, format!("save pi_bin: {e}"));
                    }
                }
            }
        }
        Ok(exit) => status(tx, format!("install {id} failed ({exit})")),
        Err(e) => status(tx, format!("install {id}: {e}")),
    }
    emit_harnesses(tx);
}

type MathRenderCache = Mutex<HashMap<(String, bool), Option<PathBuf>>>;

fn math_render_cache() -> &'static MathRenderCache {
    static CACHE: OnceLock<MathRenderCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Render one formula into `~/verity/math-cache`. Compile failures are
/// cached as `None` too, so one malformed formula does not cost a Typst
/// compile on every refresh.
fn render_math_image(tex: &str, display: bool) -> Option<PathBuf> {
    let key = (tex.to_string(), display);
    if let Ok(cache) = math_render_cache().lock() {
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
    }

    let rendered = (|| {
        let png = verity_math::render_latex(tex, display, 1.0, true).ok()?;
        let digest = Sha256::digest(format!("{display}\0{tex}"));
        let dir = Config::home().ok()?.join("math-cache");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{digest:x}.png"));
        if !path.exists() {
            std::fs::write(&path, png).ok()?;
        }
        Some(path)
    })();

    if let Ok(mut cache) = math_render_cache().lock() {
        cache.insert(key, rendered.clone());
    }
    rendered
}

/// Convert Telegram messages into the shell's wire form, rendering any
/// LaTeX blocks the core splitter finds.
fn wire_messages(messages: Vec<TopicMessage>) -> Vec<WireMessage> {
    messages
        .into_iter()
        .map(|mut message| {
            let mut display_text = String::new();
            let mut display_spans = Vec::new();
            let mut math = Vec::new();
            for block in split_math(&message) {
                match block {
                    Blk::Text { text, spans } => {
                        let base = display_text.len();
                        display_text.push_str(&text);
                        display_spans.extend(spans.into_iter().map(|(_, mut span)| {
                            span.start += base;
                            span
                        }));
                    }
                    Blk::Math { tex, display } => {
                        if let Some(path) = render_math_image(&tex, display) {
                            math.push(MathImage {
                                path: path.to_string_lossy().into_owned(),
                                display,
                            });
                        } else {
                            // Rendering failure should not silently delete the
                            // formula; fall back to its source text.
                            display_text.push_str(&tex);
                        }
                    }
                }
            }
            // The shell displays math as images, so the raw TeX delimiters
            // must not appear a second time in the text run.
            message.text = display_text;
            message.spans = display_spans;
            WireMessage { message, math }
        })
        .collect()
}

/// Registered bots from the local roster. This is a synchronous sqlite
/// read, so keep it before any await and never hold the store across one.
fn roster() -> Vec<Bot> {
    Config::db_path()
        .and_then(|p| Store::open(&p))
        .and_then(|s| s.list_bots())
        .unwrap_or_default()
}

/// A registered bot's DM target: display label plus the resolved peer of
/// your 1:1 chat, when Telegram already has one.
struct Dm {
    user_id: i64,
    label: String,
    peer: Option<PeerRef>,
}

/// Build the sidebar: one row per registered bot. Bots whose tokens or
/// chats are unavailable stay visible but greyed out.
async fn build_dms(tg: &Tg) -> Vec<Dm> {
    let mut out = Vec::new();
    for b in roster() {
        let mut username = b.username.clone();
        let user_id = match BotApi::new(&b.token).get_me().await {
            Ok(me) => {
                username = username.or(me.username);
                Some(me.id)
            }
            Err(_) => None,
        };
        let label = match &username {
            Some(u) => format!("@{u}"),
            None => b.name.clone(),
        };
        let dm = match user_id {
            Some(id) => {
                let mut peer = tg.peer_for_user(id).await.ok();
                if peer.is_none() {
                    // No DM yet: Telegram bots can't write first, so the
                    // user side (us) opens the chat with /start. Once the
                    // dialog exists this branch never fires again.
                    if let Some(uname) = &username {
                        if let Ok(Some(p)) = tg.resolve_username_peer(uname).await {
                            if tg.send_message(p, "/start").await.is_ok() {
                                peer = Some(p);
                            }
                        }
                    }
                }
                Dm {
                    user_id: id,
                    label,
                    peer,
                }
            }
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

/// Network calls in grammers wait forever when Telegram is unreachable —
/// the sender pool retries internally and never surfaces an error. Cap
/// every RPC so the shell can say what is wrong instead of hanging.
async fn rpc<T>(what: &str, fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    rpc_within(what, Duration::from_secs(20), fut).await
}

async fn rpc_within<T>(
    what: &str,
    dur: Duration,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(dur, fut).await {
        Ok(r) => r,
        Err(_) => Err(Error::Mtproto(format!(
            "{what}: Telegram did not answer within {}s",
            dur.as_secs()
        ))),
    }
}

/// Shared tail of bot creation/registration: verify the token, insert
/// into the roster, roll a profile picture, tell the shell. Returns
/// true when the bot landed in the roster.
#[allow(clippy::too_many_arguments)]
async fn adopt_bot(
    tg: &Tg,
    tx: &mpsc::UnboundedSender<Ev>,
    name: String,
    token: String,
    instructions: String,
    provider: String,
    model: Option<String>,
    greeting: Option<String>,
) -> bool {
    let me = match rpc("getMe", BotApi::new(&token).get_me()).await {
        Ok(m) => m,
        Err(e) => {
            status(tx, format!("getMe: {e}"));
            let _ = tx.send(Ev::BotSetupFailed);
            return false;
        }
    };
    let bot = Bot {
        name,
        token,
        username: me.username,
        instructions,
        provider,
        model,
        greeting,
    };
    let added = Config::db_path()
        .and_then(|p| Store::open(&p))
        .and_then(|s| s.add_bot(&bot));
    if let Err(e) = added {
        status(tx, format!("register: {e}"));
        let _ = tx.send(Ev::BotSetupFailed);
        return false;
    }
    status(tx, format!("registered {}", bot.name));
    // Profile picture is a nice-to-have; never block setup on it.
    if let Ok(png) =
        verity_core::pfp::render_opts(verity_core::pfp::name_seed(&bot.name), 512, None)
    {
        if let Err(e) = rpc("set pfp", BotApi::new(&bot.token).set_profile_photo(png)).await {
            status(tx, format!("pfp skipped: {e}"));
        }
    }
    // Bootstrap the 1:1 DM: Telegram bots can't write first, so the
    // user side (us) sends /start — without this the bridge has no
    // chat to talk in and every poll errors. Nice-to-have: a failure
    // here only means the user must press start themselves.
    if let Some(uname) = bot.username.clone() {
        match rpc("resolve bot", tg.resolve_username_peer(&uname)).await {
            Ok(Some(peer)) => {
                if let Err(e) = rpc("send start", tg.send_message(peer, "/start")).await {
                    status(
                        tx,
                        format!("dm bootstrap: {e} — send /start to @{uname} manually"),
                    );
                }
            }
            Ok(None) => status(
                tx,
                format!("@{uname} not resolvable yet — send /start manually"),
            ),
            Err(e) => status(tx, format!("dm bootstrap: {e}")),
        }
    }
    let _ = tx.send(Ev::BotReady {
        name: bot.name.clone(),
        username: bot.username.clone(),
    });
    true
}

/// Messages for one DM; retries peer resolution so a chat created after
/// startup appears on the next refresh.
async fn fetch_view(tg: &Tg, dms: &mut [Dm], user_id: i64) -> Result<Vec<TopicMessage>> {
    let dm = dms
        .iter_mut()
        .find(|d| d.user_id == user_id)
        .ok_or_else(|| Error::Mtproto("bot is gone".into()))?;
    if dm.peer.is_none() && user_id > 0 {
        dm.peer = rpc("resolve peer", tg.peer_for_user(user_id)).await.ok();
    }
    match dm.peer {
        Some(p) => rpc("history", tg.dm_history(p, HISTORY)).await,
        None => Err(Error::Mtproto(format!("no 1:1 chat with {} yet", dm.label))),
    }
}

/// Re-list DMs and re-fetch the selected conversation.
async fn refresh_all(
    tg: &Tg,
    dms: &mut Vec<Dm>,
    selected: &mut Option<i64>,
    tx: &mpsc::UnboundedSender<Ev>,
) {
    let me = tg.self_id().await.ok();
    *dms = build_dms(tg).await;
    let still_there = selected.is_some_and(|id| dms.iter().any(|d| d.user_id == id));
    if !still_there {
        *selected = dms.first().map(|d| d.user_id);
    }
    let messages = match *selected {
        Some(id) => fetch_view(tg, dms, id).await.unwrap_or_default(),
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
        selected_user_id: *selected,
        messages: wire_messages(messages),
    });
}

/// Load API credentials, asking through the WinUI shell and saving
/// `config.toml` when they are missing. Environment credentials still win.
async fn ensure_api_credentials(
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    tx: &mpsc::UnboundedSender<Ev>,
) -> Option<(i32, String)> {
    let mut cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            status(tx, format!("config: {e}"));
            return None;
        }
    };
    loop {
        if let Ok(creds) = cfg.api_credentials() {
            return Some(creds);
        }
        let _ = tx.send(Ev::NeedApiKeys);
        status(tx, "Telegram API credentials needed");
        loop {
            let cmd = rx.recv().await?;
            if let Cmd::Configure { api_id, api_hash } = cmd {
                let api_id = match api_id.trim().parse::<i32>() {
                    Ok(v) if v > 0 => v,
                    _ => {
                        status(tx, "api_id must be a positive number");
                        continue;
                    }
                };
                let api_hash = api_hash.trim().to_string();
                if api_hash.is_empty() {
                    status(tx, "api_hash cannot be empty");
                    continue;
                }
                cfg.api_id = Some(api_id);
                cfg.api_hash = Some(api_hash);
                match cfg.save() {
                    Ok(()) => {
                        status(tx, "API credentials saved");
                        break;
                    }
                    Err(e) => status(tx, format!("save config: {e}")),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginOutcome {
    Done,
    Aborted,
    InvalidCredentials,
}

fn is_api_credential_error(e: &Error) -> bool {
    let text = e.to_string().to_ascii_lowercase();
    text.contains("api_id_invalid") || text.contains("api_hash_invalid")
}

/// Remove only credentials stored in `config.toml`; environment-provided
/// credentials cannot be rewritten and therefore stop the retry loop.
fn clear_configured_api_credentials() -> bool {
    let Ok(mut cfg) = Config::load() else {
        return false;
    };
    let had_configured = cfg.api_id.is_some() || cfg.api_hash.is_some();
    cfg.api_id = None;
    cfg.api_hash = None;
    had_configured && cfg.save().is_ok()
}

/// Interactive Telegram login driven by JSON commands from the shell.
async fn telegram_login(
    tg: &Tg,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    tx: &mpsc::UnboundedSender<Ev>,
) -> LoginOutcome {
    'phone: loop {
        let _ = tx.send(Ev::NeedPhone);
        let phone = loop {
            match rx.recv().await {
                Some(Cmd::LoginPhone { phone }) if !phone.trim().is_empty() => {
                    break phone.trim().to_string()
                }
                Some(_) => {}
                None => return LoginOutcome::Aborted,
            }
        };

        status(tx, "sending login code…");
        let mut challenge = match rpc("send code", tg.login_send_code(&phone)).await {
            Ok(c) => c,
            Err(e) if is_api_credential_error(&e) => {
                status(tx, format!("Telegram rejected the API credentials: {e}"));
                return LoginOutcome::InvalidCredentials;
            }
            Err(e) => {
                status(tx, format!("send code: {e}"));
                continue 'phone;
            }
        };

        'code: loop {
            let _ = tx.send(Ev::NeedCode {
                delivery: challenge.delivery.clone(),
                next_delivery: challenge.next_delivery.clone(),
            });
            let input = loop {
                match rx.recv().await {
                    Some(Cmd::LoginCode { code }) if !code.trim().is_empty() => {
                        break code.trim().to_string()
                    }
                    Some(Cmd::LoginPhone { .. }) => continue 'phone,
                    Some(_) => {}
                    None => return LoginOutcome::Aborted,
                }
            };

            if input.eq_ignore_ascii_case("resend") {
                status(tx, "resending login code…");
                match rpc("resend code", tg.login_resend_code(&phone, &challenge)).await {
                    Ok(c) => {
                        challenge = c;
                        status(tx, "code resent");
                    }
                    Err(e) => status(tx, format!("resend: {e}")),
                }
                continue 'code;
            }

            status(tx, "checking login code…");
            match rpc(
                "check code",
                tg.login_submit_code(&phone, &challenge, &input),
            )
            .await
            {
                Ok(LoginCodeResult::Done) => {
                    let _ = tx.send(Ev::LoggedIn);
                    return LoginOutcome::Done;
                }
                Ok(LoginCodeResult::PasswordNeeded) => loop {
                    let _ = tx.send(Ev::NeedPassword);
                    let password = loop {
                        match rx.recv().await {
                            Some(Cmd::LoginPassword { password }) if !password.is_empty() => {
                                break password
                            }
                            Some(_) => {}
                            None => return LoginOutcome::Aborted,
                        }
                    };
                    status(tx, "checking 2FA password…");
                    match rpc("check password", tg.login_submit_password(&password)).await {
                        Ok(()) => {
                            let _ = tx.send(Ev::LoggedIn);
                            return LoginOutcome::Done;
                        }
                        Err(e) => status(tx, format!("2FA: {e}")),
                    }
                },
                Err(e) => {
                    status(tx, format!("{e}"));
                    continue 'code;
                }
            }
        }
    }
}

async fn worker(mut rx: mpsc::UnboundedReceiver<Cmd>, tx: mpsc::UnboundedSender<Ev>) {
    let path = match Config::session_path() {
        Ok(p) => p,
        Err(_) => return status(&tx, "no home dir"),
    };
    if let Ok(home) = Config::home() {
        let _ = std::fs::create_dir_all(home);
    }

    let tg = 'connect: loop {
        status(&tx, "connecting…");
        let Some((api_id, api_hash)) = ensure_api_credentials(&mut rx, &tx).await else {
            return;
        };
        let tg = match Tg::connect(api_id, api_hash, &path).await {
            Ok(t) => t,
            Err(e) => return status(&tx, format!("connect: {e}")),
        };
        // The first RPC is what actually performs the MTProto handshake.
        // grammers retries internally forever, so keep retrying here with
        // a visible status rather than hanging silently.
        let authorized = loop {
            match rpc("auth check", tg.is_authorized()).await {
                Ok(v) => break v,
                Err(e) => {
                    // A wrong system clock makes Telegram reject every
                    // message as "msg_id too high" and looks exactly like
                    // this; say so instead of looping mysteriously.
                    status(
                        &tx,
                        format!("{e} — retrying (if this persists, check that your system clock is correct)"),
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        };
        match authorized {
            true => break 'connect tg,
            false => match telegram_login(&tg, &mut rx, &tx).await {
                LoginOutcome::Done => {
                    match Config::session_path() {
                        Ok(path) => {
                            if let Err(e) = tg.save_session(&path) {
                                status(&tx, format!("save session: {e}"));
                            }
                        }
                        Err(e) => status(&tx, format!("save session: {e}")),
                    }
                    break 'connect tg;
                }
                LoginOutcome::InvalidCredentials => {
                    if clear_configured_api_credentials() {
                        status(&tx, "re-enter Telegram API credentials");
                        continue 'connect;
                    }
                    return status(
                        &tx,
                        "Telegram rejected the environment-provided API credentials",
                    );
                }
                LoginOutcome::Aborted => return,
            },
        }
    };
    let _ = tx.send(Ev::LoggedIn);
    status(&tx, "");

    let mut dms: Vec<Dm> = Vec::new();
    let mut selected: Option<i64> = None;
    refresh_all(&tg, &mut dms, &mut selected, &tx).await;
    emit_harnesses(&tx);

    loop {
        match rx.try_recv() {
            Ok(cmd) => {
                match cmd {
                    Cmd::Refresh => refresh_all(&tg, &mut dms, &mut selected, &tx).await,
                    Cmd::Poll => {
                        if let Some(id) = selected {
                            match fetch_view(&tg, &mut dms, id).await {
                                Ok(messages) => {
                                    let _ = tx.send(Ev::Messages {
                                        user_id: id,
                                        messages: wire_messages(messages),
                                    });
                                }
                                Err(e) => status(&tx, format!("poll: {e}")),
                            }
                        }
                    }
                    Cmd::Select { user_id } => {
                        selected = Some(user_id);
                        match fetch_view(&tg, &mut dms, user_id).await {
                            Ok(messages) => {
                                let _ = tx.send(Ev::Messages {
                                    user_id,
                                    messages: wire_messages(messages),
                                });
                            }
                            Err(e) => status(&tx, format!("history: {e}")),
                        }
                    }
                    Cmd::Send { text } => {
                        let Some(id) = selected else {
                            status(&tx, "nothing selected");
                            continue;
                        };
                        let sent = match dms.iter().find(|d| d.user_id == id).and_then(|d| d.peer) {
                            Some(p) => tg.send_message(p, &text).await,
                            None => Err(Error::Mtproto("no 1:1 chat to send into".into())),
                        };
                        if let Err(e) = sent {
                            status(&tx, format!("send: {e}"));
                            continue;
                        }
                        match fetch_view(&tg, &mut dms, id).await {
                            Ok(messages) => {
                                let _ = tx.send(Ev::Messages {
                                    user_id: id,
                                    messages: wire_messages(messages),
                                });
                            }
                            Err(e) => status(&tx, format!("history: {e}")),
                        }
                    }
                    Cmd::React {
                        msg_id,
                        emoji,
                        chosen,
                    } => {
                        let Some(id) = selected else {
                            status(&tx, "nothing selected");
                            continue;
                        };
                        let target = dms.iter().find(|d| d.user_id == id).and_then(|d| d.peer);
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
                                    status(&tx, format!("reaction: {e}"));
                                    continue;
                                }
                                match fetch_view(&tg, &mut dms, id).await {
                                    Ok(messages) => {
                                        let _ = tx.send(Ev::Messages {
                                            user_id: id,
                                            messages: wire_messages(messages),
                                        });
                                    }
                                    Err(e) => status(&tx, format!("history: {e}")),
                                }
                            }
                            None => status(&tx, "no 1:1 chat to react in"),
                        }
                    }
                    Cmd::CreateBot {
                        name,
                        username,
                        instructions,
                        provider,
                        model,
                        greeting,
                    } => {
                        status(&tx, "asking @BotFather — this takes up to a minute…");
                        // The BotFather conversation is several sequential
                        // RPCs with reply waits; budget accordingly.
                        let created = rpc_within(
                            "create bot",
                            Duration::from_secs(120),
                            tg.create_bot(&name, &username),
                        )
                        .await;
                        match created {
                            Err(e) => {
                                status(&tx, format!("create bot: {e}"));
                                let _ = tx.send(Ev::BotSetupFailed);
                            }
                            Ok(token) => {
                                if adopt_bot(
                                    &tg,
                                    &tx,
                                    name,
                                    token,
                                    instructions,
                                    provider,
                                    model,
                                    greeting,
                                )
                                .await
                                {
                                    refresh_all(&tg, &mut dms, &mut selected, &tx).await;
                                }
                            }
                        }
                    }
                    Cmd::RegisterBot {
                        name,
                        token,
                        instructions,
                        provider,
                        model,
                        greeting,
                    } => {
                        if adopt_bot(
                            &tg,
                            &tx,
                            name,
                            token,
                            instructions,
                            provider,
                            model,
                            greeting,
                        )
                        .await
                        {
                            refresh_all(&tg, &mut dms, &mut selected, &tx).await;
                        }
                    }
                    Cmd::SetPiBin { bin } => match Config::load() {
                        Ok(mut cfg) => {
                            cfg.pi_bin = Some(bin);
                            match cfg.save() {
                                Ok(()) => status(&tx, "pi binary saved"),
                                Err(e) => status(&tx, format!("save config: {e}")),
                            }
                        }
                        Err(e) => status(&tx, format!("config: {e}")),
                    },
                    Cmd::DetectHarnesses => emit_harnesses(&tx),
                    Cmd::InstallHarness { id } => install_harness(&id, &tx).await,
                    Cmd::Configure { .. }
                    | Cmd::LoginPhone { .. }
                    | Cmd::LoginCode { .. }
                    | Cmd::LoginPassword { .. } => status(&tx, "already logged in"),
                }
                continue;
            }
            Err(mpsc::error::TryRecvError::Empty) => {}
            Err(mpsc::error::TryRecvError::Disconnected) => return,
        }

        // Live push: grammers buffers socket updates. Drain them and
        // refetch when one lands in the open conversation.
        match tg.updates().await {
            Ok(events) => {
                let hit = events.iter().any(|ev| match ev {
                    TgEvent::Message { chat_hint, .. } => selected == Some(*chat_hint),
                    TgEvent::Other => false,
                });
                if hit {
                    if let Some(id) = selected {
                        match fetch_view(&tg, &mut dms, id).await {
                            Ok(messages) => {
                                let _ = tx.send(Ev::Messages {
                                    user_id: id,
                                    messages: wire_messages(messages),
                                });
                            }
                            Err(e) => status(&tx, format!("history: {e}")),
                        }
                    }
                }
            }
            Err(e) => status(&tx, format!("updates: {e}")),
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

#[tokio::main]
async fn main() {
    // Diagnostics must stay on stderr; stdout is the wire protocol.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Ev>();

    // stdin reader: malformed input is reported as a protocol event rather
    // than written directly, because stdout is reserved for JSON.
    let stdin_events = ev_tx.clone();
    tokio::spawn(async move {
        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) if !line.trim().is_empty() => {
                    // Tolerate a stray UTF-8 BOM from Windows-side writers.
                    let line = line.trim_start_matches('\u{feff}');
                    match serde_json::from_str::<Cmd>(line) {
                        Ok(cmd) => {
                            if cmd_tx.send(cmd).is_err() {
                                break;
                            }
                        }
                        Err(e) => status(&stdin_events, format!("bad command json: {e}")),
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });

    // Single stdout writer, so events can never interleave mid-line.
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(ev) = ev_rx.recv().await {
            let mut line = match serde_json::to_vec(&ev) {
                Ok(v) => v,
                Err(_) => continue,
            };
            line.push(b'\n');
            if stdout.write_all(&line).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });

    worker(cmd_rx, ev_tx).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_wire_names_are_stable() {
        let cmd: Cmd = serde_json::from_str(r#"{"type":"login_code","code":"12345"}"#).unwrap();
        assert!(matches!(cmd, Cmd::LoginCode { .. }));
        let cmd: Cmd =
            serde_json::from_str(r#"{"type":"react","msg_id":7,"emoji":"👍","chosen":true}"#)
                .unwrap();
        assert!(matches!(cmd, Cmd::React { msg_id: 7, .. }));
    }

    #[test]
    fn wire_message_flattens_topic_fields() {
        let wire = wire_messages(vec![TopicMessage {
            id: 1,
            date: 0,
            sender: "test".into(),
            sender_id: Some(1),
            text: "plain text".into(),
            spans: vec![],
            reactions: vec![],
        }]);
        let json = serde_json::to_value(&wire[0]).unwrap();
        assert_eq!(json["text"], "plain text");
        assert_eq!(json["sender"], "test");
        assert!(json.get("math").is_none());
    }

    #[test]
    fn event_wire_names_are_stable() {
        let json = serde_json::to_value(Ev::NeedCode {
            delivery: "telegram".into(),
            next_delivery: None,
        })
        .unwrap();
        assert_eq!(json["type"], "need_code");
        assert!(json.get("next_delivery").is_some());
    }
}
