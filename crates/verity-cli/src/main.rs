//! verity — manage teammate bots that live in a Telegram forum group.

use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::Path;
use std::sync::Arc;
use verity_core::botapi::{BotApi, InputRichMessage};
use verity_core::bridge::Bridge;
use verity_core::config::Config;
use verity_core::harness::{AcpHarness, EchoHarness, PiHarness};
use verity_core::mtproto::Tg;
use verity_core::store::{Bot, Store};

#[derive(Parser)]
#[command(name = "verity", about = "Named teammate bots in a Telegram group")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Set api_id/api_hash (my.telegram.org) and the host group —
    /// or pass --create-group <title> to make a new forum supergroup
    Config {
        #[arg(long)]
        api_id: Option<i32>,
        #[arg(long)]
        api_hash: Option<String>,
        #[arg(long)]
        group_id: Option<i64>,
        /// Create a new forum supergroup with this title and use it
        #[arg(long)]
        create_group: Option<String>,
        #[arg(long)]
        pi_bin: Option<String>,
        /// Bind address for the mobile-app JSON API (e.g. 0.0.0.0:8787)
        #[arg(long)]
        api_bind: Option<String>,
        /// Bearer token the API requires on every route but /api/health
        #[arg(long)]
        api_token: Option<String>,
    },
    /// Log in to Telegram (MTProto) as yourself
    Login {
        #[arg(long)]
        phone: Option<String>,
    },
    /// Restyle your own profile picture — `random` (default) rolls a
    /// fresh seed and colour, or pass a seed number for a known face
    Pfp {
        /// `random` or a seed number
        what: Option<String>,
    },
    /// Bot management
    Bot {
        #[command(subcommand)]
        cmd: BotCmd,
    },
    /// Wipe everything: kick all bots but the lead, delete all topics +
    /// messages, then the local brain — clean slate (stop serve first)
    Reset,
    /// Clear a bot's memory
    Clear { name: String },
    /// Delete a bot's chat topic too (it recreates fresh on next message)
    ClearChat { name: String },
    /// List your supergroups with Bot-API chat ids (find your group_id)
    Groups,
    /// Rename the configured group (or one by id/title)
    Rename {
        title: String,
        group: Option<String>,
    },
    /// List forum topics in the configured group
    Topics {
        /// Group id or title substring (defaults to config group_id)
        group: Option<String>,
    },
    /// Read recent messages of a topic (as your user)
    History {
        /// Topic id or title substring
        topic: String,
        #[arg(long, default_value = "20")]
        limit: i32,
        group: Option<String>,
    },
    /// Post a message into a topic as yourself
    Say {
        /// Topic id or title substring
        topic: String,
        /// Message text
        text: String,
        group: Option<String>,
    },
    /// 7z ~/verity (minus target/) + $HOME settings into one archive;
    /// send it to the group's General topic
    Backup,
    /// Restore from a backup archive — pass its path or https URL and
    /// it unpacks + re-enables itself (no Telegram login needed; the
    /// session rides inside the archive). No argument: pull the newest
    /// archive out of General instead.
    Restore {
        /// Local path or https URL of a verity-backup-*.7z
        archive: Option<String>,
        /// General-scan mode only: group id or title
        #[arg(long)]
        group: Option<String>,
    },
    /// Liveness check: start the bridge if it's down, restart it if its
    /// heartbeat went stale. Run by verity-watchdog.timer once a minute
    /// — honors `~/.local/state/verity-asleep` and a disabled unit
    Watchdog,
    /// Run the bridge: bots listen, work quietly, reply when done
    Serve {
        /// Use the echo harness (no agent provider needed)
        #[arg(long)]
        echo: bool,
        /// Don't attach the user session — turns get no topic transcript
        #[arg(long)]
        no_context: bool,
    },
    /// Post into a topic as a bot
    Send {
        bot: String,
        /// Message text — or a path to upload as a document with --file
        text: String,
        /// Upload `text` as a file (sendDocument) instead of a message
        #[arg(long)]
        file: bool,
    },
}

#[derive(Subcommand)]
enum BotCmd {
    /// Register a bot by its BotFather token; creates its forum topic on first serve
    New {
        name: String,
        #[arg(long)]
        token: String,
        #[arg(long, default_value = "")]
        instructions: String,
        /// Agent provider: `pi`, `echo`, or `acp:<cmd> [args]` (e.g. `acp:claude-code-acp`)
        #[arg(long, default_value = "pi")]
        provider: String,
        #[arg(long)]
        model: Option<String>,
        /// Posted into the bot's topic once, when it's created
        #[arg(long)]
        greeting: Option<String>,
    },
    /// Create a bot via @BotFather (as your user) and register it
    Create {
        name: String,
        /// Bot username; must end in "bot"
        #[arg(long)]
        username: String,
        #[arg(long, default_value = "")]
        instructions: String,
        /// Agent provider: `pi`, `echo`, or `acp:<cmd> [args]`
        #[arg(long, default_value = "pi")]
        provider: String,
        #[arg(long)]
        greeting: Option<String>,
    },
    /// Invite a bot into the group + grant admin (manage topics)
    Invite {
        /// Bot username (with or without @)
        username: String,
        group: Option<String>,
    },
    /// Rename a bot's forum topic
    Topic {
        name: String,
        #[arg(long)]
        title: String,
    },
    /// Rename a bot (store name, memory keys, and its topic title if set)
    Rename { name: String, new_name: String },
    /// Regenerate the bot's profile picture (BallsMaker style) and upload it
    Pfp {
        name: String,
        /// Fixed seed — default derives from the bot's name, so the
        /// same name always gets the same face unless a seed is given
        #[arg(long)]
        seed: Option<u64>,
        /// Base hue in degrees (0-360) — paints the shape one solid
        /// colour; default is near-black on white
        #[arg(long)]
        hue: Option<f64>,
    },
    /// List registered bots
    List,
    /// Remove a bot (does not delete its topic)
    Rm { name: String },
    /// Delete the bot at @BotFather too — unregisters it and kills the
    /// Telegram account for good. Irreversible.
    Delete {
        /// Roster name, @username, or BotFather picker label
        name: String,
    },
    /// Register a Mini App at @BotFather (/newapp) — prints the
    /// t.me/<bot>/<short> link you can drop anywhere to launch it
    App {
        /// Roster name or @username of the bot that owns the app
        bot: String,
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        desc: String,
        /// Cover photo BotFather asks for (png/jpg)
        #[arg(long)]
        photo: String,
        /// https URL the app is served from
        #[arg(long)]
        url: String,
        /// Short name — the link becomes t.me/<bot>/<short>
        #[arg(long)]
        short: String,
    },
    /// Show a Mini App's @BotFather card (/myapps), or repoint the
    /// URL it's served from when one is given
    Appurl {
        /// Roster name or @username of the bot that owns the app
        bot: String,
        /// App title or short name — matched against button labels
        app: String,
        /// New https URL; omit to just print the current card
        url: Option<String>,
    },
    /// Send a raw line to @BotFather (repair a stuck flow)
    Bf { text: String },
    /// List every bot you own at @BotFather (not just registered ones)
    Mine,
    /// Show/edit a bot's memory
    Memory {
        name: String,
        #[arg(long)]
        set: Option<Vec<String>>,
    },
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}: ");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

/// Interactive config prompt: shows the current value (masked when
/// `secret`), empty input keeps it and returns None.
fn prompt_keep(label: &str, current: Option<String>, secret: bool) -> Result<Option<String>> {
    let hint = match (current, secret) {
        (Some(v), false) => format!(" [{v}]"),
        (Some(v), true) if v.len() > 4 => format!(" [••••{}]", &v[v.len() - 4..]),
        (Some(_), true) => " [••••]".to_string(),
        (None, _) => String::new(),
    };
    let s = prompt(&format!("{label}{hint}"))?;
    Ok((!s.is_empty()).then_some(s))
}

async fn tg() -> Result<(Tg, Config)> {
    let cfg = Config::load()?;
    let (api_id, api_hash) = cfg.api_credentials()?;
    let tg = Tg::connect(api_id, api_hash, &Config::session_path()?).await?;
    Ok((tg, cfg))
}

async fn authorized_tg() -> Result<(Tg, Config)> {
    let (tg, cfg) = tg().await?;
    if !tg.is_authorized().await? {
        bail!("not logged in — run `verity login --phone <number>`");
    }
    Ok((tg, cfg))
}

/// Create a forum supergroup using cfg's own credentials — `tg()`
/// reloads from disk, which would miss values being set in the same
/// `verity config` run.
async fn create_group_with(cfg: &Config, title: &str) -> Result<i64> {
    let (api_id, api_hash) = cfg.api_credentials()?;
    let tg = Tg::connect(api_id, api_hash, &Config::session_path()?).await?;
    if !tg.is_authorized().await? {
        bail!("not logged in — run `verity login --phone <number>`");
    }
    tg.create_group(title).await.map_err(Into::into)
}

fn group_of(cfg: &Config, arg: Option<String>) -> Result<String> {
    if let Some(g) = arg {
        return Ok(g);
    }
    cfg.group_id
        .map(|g| g.to_string())
        .context("no group configured — `verity config --group-id <id>` or pass one")
}

/// Numeric topic ids pass through; anything else is matched against
/// topic titles (case-insensitive substring).
async fn resolve_topic(
    tg: &Tg,
    peer: &grammers_session::defs::PeerRef,
    topic: &str,
) -> Result<i64> {
    if let Ok(id) = topic.parse::<i64>() {
        return Ok(id);
    }
    tg.list_topics(*peer)
        .await?
        .into_iter()
        .find(|t| t.title.to_lowercase().contains(&topic.to_lowercase()))
        .map(|t| t.id)
        .context("no such topic")
}

/// Render a BallsMaker avatar for `bot`, save pfp-<name>.png next to
/// the db, and upload it via the Bot API.
async fn set_pfp(bot: &Bot, seed: u64, hue: Option<f64>) -> Result<()> {
    let png = verity_core::pfp::render_opts(seed, 512, hue)?;
    let out = Config::db_path()?
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("pfp-{}.png", bot.name));
    std::fs::write(&out, &png)?;
    BotApi::new(&bot.token).set_profile_photo(png).await?;
    println!("pfp set for {} (seed {seed}) → {}", bot.name, out.display());
    Ok(())
}

/// Entropy for `verity pfp random` — /dev/urandom, time as fallback.
fn rand_u64() -> u64 {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut b = [0u8; 8];
        if f.read_exact(&mut b).is_ok() {
            return u64::from_le_bytes(b);
        }
    }
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e3779b97f4a7c15);
    n ^ (std::process::id() as u64).wrapping_mul(0x2545F4914F6CDD1D)
}

fn pfp_style(what: Option<&str>, roll: impl FnOnce() -> u64) -> Result<(u64, f64)> {
    let seed = match what {
        None | Some("random") => roll().max(1),
        Some(w) => w.parse().context("use `random` or a seed number")?,
    };
    Ok((seed, (seed % 360) as f64))
}

/// Tokio worker threads default to a 2 MiB stack; deep synchronous
/// chains (dependency parsers, drop glue unwinding a burst of failed
/// futures) can exhaust it — that aborts the whole process, no unwind.
/// Give workers the same headroom the main thread gets.
/// `VERITY_STACK_MIB` overrides.
fn worker_stack() -> usize {
    std::env::var("VERITY_STACK_MIB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(8)
        * 1024
        * 1024
}

fn main() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(worker_stack())
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().cmd {
        Cmd::Config {
            api_id,
            api_hash,
            group_id,
            create_group,
            pi_bin,
            api_bind,
            api_token,
        } => {
            let mut cfg = Config::load()?;
            let interactive = api_id.is_none()
                && api_hash.is_none()
                && group_id.is_none()
                && create_group.is_none()
                && pi_bin.is_none()
                && api_bind.is_none()
                && api_token.is_none();
            if interactive {
                println!("leave blank to keep the current value");
                if let Some(v) = prompt_keep(
                    "api_id (my.telegram.org)",
                    cfg.api_id.map(|v| v.to_string()),
                    false,
                )? {
                    cfg.api_id = Some(v.parse().context("api_id must be a number")?);
                }
                if let Some(v) = prompt_keep("api_hash", cfg.api_hash.clone(), true)? {
                    cfg.api_hash = Some(v);
                }
                if let Some(v) = prompt_keep(
                    "group_id (Bot-API id, see `verity groups`; 'new' to create)",
                    cfg.group_id.map(|v| v.to_string()),
                    false,
                )? {
                    if v == "new" || v.starts_with("new ") {
                        let title = match v.strip_prefix("new").unwrap_or("").trim() {
                            "" => prompt("new group title")?,
                            t => t.to_string(),
                        };
                        let id = create_group_with(&cfg, &title).await?;
                        println!("created '{title}' ({id})");
                        cfg.group_id = Some(id);
                    } else {
                        cfg.group_id = Some(v.parse().context("group_id must be a number")?);
                    }
                }
                if let Some(v) = prompt_keep("pi_bin", cfg.pi_bin.clone(), false)? {
                    cfg.pi_bin = Some(v);
                }
            } else {
                if let Some(v) = api_id {
                    cfg.api_id = Some(v);
                }
                if let Some(v) = api_hash {
                    cfg.api_hash = Some(v);
                }
                if let Some(v) = group_id {
                    cfg.group_id = Some(v);
                }
                if let Some(title) = create_group {
                    let id = create_group_with(&cfg, &title).await?;
                    println!("created '{title}' ({id})");
                    cfg.group_id = Some(id);
                }
                if let Some(v) = pi_bin {
                    cfg.pi_bin = Some(v);
                }
                if let Some(v) = api_bind {
                    cfg.api_bind = Some(v);
                }
                if let Some(v) = api_token {
                    cfg.api_token = Some(v);
                }
            }
            cfg.save()?;
            println!("saved {}", Config::home()?.join("config.toml").display());
        }

        Cmd::Login { phone } => {
            let (tg, _) = tg().await?;
            if tg.is_authorized().await? {
                println!("already logged in");
                return Ok(());
            }
            let phone = phone
                .map(Ok)
                .unwrap_or_else(|| prompt("Phone (+country code)"))?;
            tg.sign_in(&phone, |what| {
                prompt(what).map_err(|e| verity_core::Error::Other(e.to_string()))
            })
            .await?;
            tg.save_session(&Config::session_path()?)?;
            println!("logged in; session saved");
        }

        Cmd::Pfp { what } => {
            let bot = Store::open(&Config::db_path()?)?
                .primary_bot()?
                .context("no bots registered — `verity bot new`")?;
            let (seed, hue) = pfp_style(what.as_deref(), rand_u64)?;
            set_pfp(&bot, seed, Some(hue)).await?;
            println!("hue {hue:.0}° — `verity pfp {seed}` replays this face");
        }

        Cmd::Reset => {
            let db = Config::db_path()?;
            // Wipe the Telegram side while the roster still exists —
            // every bot member kicked but the lead, every topic
            // deleted, General purged. The db dies last.
            match authorized_tg().await {
                Ok((tg, cfg)) => {
                    let keep = Store::open(&db)
                        .and_then(|s| s.primary_bot())
                        .ok()
                        .flatten();
                    match cfg.group_id {
                        Some(group_id) => match tg.peer(&group_id.to_string()).await {
                            Ok(peer) => {
                                let keep_id = match &keep {
                                    Some(k) => {
                                        BotApi::new(&k.token).get_me().await.ok().map(|u| u.id)
                                    }
                                    None => None,
                                };
                                let rep = tg.wipe_group(peer, keep_id).await;
                                println!(
                                    "group wiped: {} bots kicked, {} topics, {} General messages{}",
                                    rep.bots_kicked,
                                    rep.topics_deleted,
                                    rep.general_messages,
                                    keep.map(|k| format!(" — kept {}", k.name))
                                        .unwrap_or_default()
                                );
                            }
                            Err(e) => eprintln!("can't resolve group — skipped wipe: {e}"),
                        },
                        None => eprintln!("no group_id configured — skipped group wipe"),
                    }
                }
                Err(e) => eprintln!("{e} — skipped group wipe"),
            }
            if db.exists() {
                std::fs::remove_file(&db)?;
            }
            println!("wiped {db:?} — clean slate (re-register bots, restart serve)");
        }

        Cmd::Clear { name } => {
            let store = Store::open(&Config::db_path()?)?;
            store.clear_memory(&name)?;
            store.kv_del(&format!("greeted:{name}"))?;
            store.kv_del(&format!("pending:{name}"))?;
            store.kv_del_prefix(&format!("pending:{name}:"))?;
            println!("cleared {name}'s memory");
        }

        Cmd::ClearChat { name } => {
            let store = Store::open(&Config::db_path()?)?;
            let b = store.get_bot(&name)?.context("no such bot")?;
            if let Some(t) = b.topic_id {
                let gid = Config::load()?.group_id.context("no group_id")?;
                BotApi::new(&b.token).delete_forum_topic(gid, t).await?;
            }
            store.clear_bot_topic(&name)?;
            store.clear_memory(&name)?;
            store.kv_del(&format!("greeted:{name}"))?;
            store.kv_del(&format!("pending:{name}"))?;
            store.kv_del_prefix(&format!("pending:{name}:"))?;
            println!("cleared {name}'s topic + memory — fresh start");
        }

        Cmd::Groups => {
            let (tg, _) = authorized_tg().await?;
            for g in tg.list_groups().await? {
                println!("{:<16} {:<8} {}", g.chat_id, g.kind, g.title);
            }
        }

        Cmd::Rename { title, group } => {
            let (tg, cfg) = authorized_tg().await?;
            let peer = tg.peer(&group_of(&cfg, group)?).await?;
            tg.rename_chat(peer, &title).await?;
            println!("renamed to {title}");
        }

        Cmd::Topics { group } => {
            let (tg, cfg) = authorized_tg().await?;
            let peer = tg.peer(&group_of(&cfg, group)?).await?;
            for t in tg.list_topics(peer).await? {
                println!("{:>8}  {}", t.id, t.title);
            }
        }

        Cmd::History {
            topic,
            limit,
            group,
        } => {
            let (tg, cfg) = authorized_tg().await?;
            let peer = tg.peer(&group_of(&cfg, group)?).await?;
            let topic_id = resolve_topic(&tg, &peer, &topic).await?;
            for m in tg.topic_history(peer, topic_id, limit).await? {
                println!("[{}] {}: {}", m.id, m.sender, m.text);
            }
        }

        Cmd::Say { topic, text, group } => {
            let (tg, cfg) = authorized_tg().await?;
            let peer = tg.peer(&group_of(&cfg, group)?).await?;
            let topic_id = resolve_topic(&tg, &peer, &topic).await?;
            tg.send_message(peer, Some(topic_id), &text).await?;
            println!("sent");
        }

        Cmd::Bot { cmd } => {
            let store = Store::open(&Config::db_path()?)?;
            match cmd {
                BotCmd::New {
                    name,
                    token,
                    instructions,
                    provider,
                    model,
                    greeting,
                } => {
                    let api = BotApi::new(&token);
                    let me = api.get_me().await.context("invalid bot token?")?;
                    store.add_bot(&Bot {
                        name: name.clone(),
                        token,
                        username: me.username.clone(),
                        topic_id: None,
                        instructions,
                        provider,
                        model,
                        greeting,
                    })?;
                    println!("registered {name} (@{})", me.username.unwrap_or_default());
                }
                BotCmd::Create {
                    name,
                    username,
                    instructions,
                    provider,
                    greeting,
                } => {
                    if !username.ends_with("bot") {
                        bail!("bot usernames must end in \"bot\"");
                    }
                    let (tg, _) = authorized_tg().await?;
                    println!("asking @BotFather…");
                    let token = tg.create_bot(&name, &username).await?;
                    let api = BotApi::new(&token);
                    let me = api.get_me().await?;
                    store.add_bot(&Bot {
                        name: name.clone(),
                        token,
                        username: me.username.clone(),
                        topic_id: None,
                        instructions,
                        provider,
                        model: None,
                        greeting,
                    })?;
                    println!(
                        "created + registered {name} (@{})",
                        me.username.unwrap_or_default()
                    );
                }
                BotCmd::Invite { username, group } => {
                    let (tg, cfg) = authorized_tg().await?;
                    let peer = tg.peer(&group_of(&cfg, group)?).await?;
                    tg.invite_bot_admin(peer, &username).await?;
                    println!("invited @{username} + granted admin");
                }
                BotCmd::Topic { name, title } => {
                    let b = store.get_bot(&name)?.context("no such bot")?;
                    let topic = b
                        .topic_id
                        .context("bot has no topic yet — run `verity serve` once")?;
                    let group_id = Config::load()?.group_id.context("no group_id")?;
                    BotApi::new(&b.token)
                        .edit_forum_topic(group_id, topic, &title)
                        .await?;
                    println!("topic renamed to {title}");
                }
                BotCmd::Rename { name, new_name } => {
                    if !store.rename_bot(&name, &new_name)? {
                        bail!("no such bot");
                    }
                    if let Some(b) = store.get_bot(&new_name)? {
                        if let Some(t) = b.topic_id {
                            let gid = Config::load()?.group_id.context("no group_id")?;
                            let _ = BotApi::new(&b.token)
                                .edit_forum_topic(gid, t, &new_name)
                                .await;
                        }
                    }
                    println!("renamed {name} → {new_name} (restart serve to apply)");
                }
                BotCmd::List => {
                    for b in store.list_bots()? {
                        let user = b
                            .username
                            .map(|u| format!("@{u}"))
                            .unwrap_or_else(|| "-".into());
                        let topic = b
                            .topic_id
                            .map(|t| t.to_string())
                            .unwrap_or_else(|| "-".into());
                        let model = b.model.unwrap_or_else(|| "-".into());
                        println!(
                            "{:<16} {:<24} topic={:<6} provider={} model={}",
                            b.name, user, topic, b.provider, model
                        );
                    }
                }
                BotCmd::Rm { name } => {
                    println!(
                        "{}",
                        if store.remove_bot(&name)? {
                            "removed"
                        } else {
                            "not found"
                        }
                    );
                }
                BotCmd::Delete { name } => {
                    // Resolve roster names to usernames so the picker
                    // match hits even when BotFather labels differ.
                    let want = store
                        .get_bot(&name)
                        .ok()
                        .flatten()
                        .and_then(|b| b.username)
                        .unwrap_or_else(|| name.clone());
                    let (tg, _) = authorized_tg().await?;
                    println!("asking @BotFather to delete {want}…");
                    tg.delete_bot(&want).await?;
                    let _ = store.remove_bot(&name);
                    println!("deleted {want} for good — username freed");
                }
                BotCmd::App {
                    bot,
                    title,
                    desc,
                    photo,
                    url,
                    short,
                } => {
                    let want = store
                        .get_bot(&bot)
                        .ok()
                        .flatten()
                        .and_then(|b| b.username)
                        .unwrap_or_else(|| bot.clone());
                    let (tg, _) = authorized_tg().await?;
                    println!("asking @BotFather…");
                    let done = tg
                        .create_mini_app(
                            &want,
                            &title,
                            &desc,
                            std::path::Path::new(&photo),
                            &url,
                            &short,
                        )
                        .await?;
                    println!("{done}");
                }
                BotCmd::Appurl { bot, app, url } => {
                    let want = store
                        .get_bot(&bot)
                        .ok()
                        .flatten()
                        .and_then(|b| b.username)
                        .unwrap_or_else(|| bot.clone());
                    let (tg, _) = authorized_tg().await?;
                    println!("asking @BotFather…");
                    match &url {
                        Some(u) => println!("{}", tg.mini_app_set_url(&want, &app, u).await?),
                        None => println!("{}", tg.mini_app_card(&want, &app).await?),
                    }
                }
                BotCmd::Bf { text } => {
                    let (tg, _) = authorized_tg().await?;
                    println!("{}", tg.bf_send(&text).await?);
                }
                BotCmd::Mine => {
                    let (tg, _) = authorized_tg().await?;
                    for label in tg.my_bots().await? {
                        println!("{label}");
                    }
                }
                BotCmd::Pfp { name, seed, hue } => {
                    let bot = store.get_bot(&name)?.context("no such bot")?;
                    let seed = seed.unwrap_or_else(|| verity_core::pfp::name_seed(&bot.name));
                    set_pfp(&bot, seed, hue).await?;
                }
                BotCmd::Memory { name, set } => {
                    if let Some(pairs) = set {
                        for p in pairs {
                            let (k, v) = p.split_once('=').context("use --set key=value")?;
                            store.remember(&name, k, v)?;
                        }
                        println!("remembered");
                    }
                    for (k, v) in store.recall(&name)? {
                        println!("{k} = {v}");
                    }
                }
            }
        }

        Cmd::Backup => {
            let cfg = Config::load()?;
            let group_id = cfg
                .group_id
                .context("no group_id — `verity config --group-id`")?;
            let bot = Store::open(&Config::db_path()?)?
                .primary_bot()?
                .context("no bots registered — `verity bot new`")?;
            let files = verity_core::backup::upload(&BotApi::new(&bot.token), group_id).await?;
            println!("uploaded to General: {}", files.join(", "));
        }

        Cmd::Restore { archive, group } => {
            let files = match &archive {
                Some(a) => verity_core::backup::restore_arg(a).await?,
                None => {
                    let (tg, cfg) = authorized_tg().await?;
                    let peer = tg.peer(&group_of(&cfg, group)?).await?;
                    verity_core::backup::download(&tg, peer).await?
                }
            };
            if files.is_empty() {
                println!("no backup documents found in General");
            } else {
                println!(
                    "restored {} files into {}",
                    files.len(),
                    Config::home()?.display()
                );
                for note in verity_core::backup::post_restore(&files) {
                    println!("{note}");
                }
            }
        }

        Cmd::Watchdog => {
            verity_core::liveness::check().await?;
        }

        Cmd::Serve { echo, no_context } => {
            let cfg = Config::load()?;
            let group_id = cfg
                .group_id
                .context("no group_id — `verity config --group-id`")?;
            // Per-bot provider string → harness. `--echo` overrides all.
            let pi_bin = cfg.pi_bin.clone();
            let make_harness: Arc<verity_core::bridge::MakeHarness> =
                Arc::new(move |provider: &str| {
                    if echo || provider == "echo" {
                        Arc::new(EchoHarness)
                    } else if let Some(spec) = provider.strip_prefix("acp:") {
                        Arc::new(AcpHarness::new(spec))
                    } else {
                        Arc::new(PiHarness::new(pi_bin.clone()))
                    }
                });
            // Attach the user session so turns can read the topic transcript.
            // Degrades gracefully to context-free turns when unavailable.
            let (tg, tg_peer) = if no_context {
                (None, None)
            } else {
                match authorized_tg().await {
                    Ok((tg, _)) => match tg.peer(&group_id.to_string()).await {
                        Ok(peer) => (Some(Arc::new(tg)), Some(peer)),
                        Err(e) => {
                            tracing::warn!(error = %e, "can't resolve group peer — no transcripts");
                            (Some(Arc::new(tg)), None)
                        }
                    },
                    Err(e) => {
                        tracing::warn!(error = %e, "no user session — turns run without transcript");
                        (None, None)
                    }
                }
            };
            // Our user id gates bot DMs — without a session there is
            // no trusted identity, so DMs stay ignored.
            let owner_id = match &tg {
                Some(tg) => match tg.self_id().await {
                    Ok(id) => Some(id),
                    Err(e) => {
                        tracing::warn!(error = %e, "get_me failed — bot DMs disabled");
                        None
                    }
                },
                None => None,
            };
            let bridge = Bridge {
                group_id,
                db_path: Config::db_path()?,
                make_harness,
                tg,
                tg_peer,
                owner_id,
            };
            tokio::select! {
                r = bridge.run() => { r?; }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down");
                }
            }
        }

        Cmd::Send { bot, text, file } => {
            let store = Store::open(&Config::db_path()?)?;
            let b = store.get_bot(&bot)?.context("no such bot")?;
            let cfg = Config::load()?;
            let group_id = cfg.group_id.context("no group_id")?;
            let topic = b
                .topic_id
                .context("bot has no topic yet — run `verity serve` once")?;
            let api = BotApi::new(&b.token);
            if file {
                api.send_document(group_id, std::path::Path::new(&text), None, Some(topic))
                    .await?;
            } else {
                for part in verity_core::rich::split_markdown(&text, verity_core::rich::RICH_LIMIT)
                {
                    api.send_rich_message(
                        group_id,
                        InputRichMessage::markdown(&part),
                        Some(topic),
                        None,
                    )
                    .await?;
                }
            }
            println!("sent");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::pfp_style;

    #[test]
    fn random_pfp_replays_with_the_same_hue() {
        for what in [Some("random"), None] {
            let mut entropy = [42, 180].into_iter();
            let draw = pfp_style(what, || entropy.next().unwrap()).unwrap();
            let replay = pfp_style(Some(&draw.0.to_string()), || unreachable!()).unwrap();
            assert_eq!(draw, replay, "pfp {what:?}");
        }
    }
}
