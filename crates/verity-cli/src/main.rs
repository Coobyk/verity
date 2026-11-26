//! verity — manage teammate bots you chat with in 1:1 Telegram DMs.

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
#[command(
    name = "verity",
    about = "Named teammate bots in Telegram DMs",
    arg_required_else_help = true
)]
struct Cli {
    /// Set up this machine from a `vsk1.` setup key: downloads the
    /// encrypted archive it points at and restores everything —
    /// config, session, bots, units. Nothing needs configuring first.
    #[arg(long, value_name = "KEY")]
    setupkey: Option<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Set api_id/api_hash (my.telegram.org) and the harness binary
    Config {
        #[arg(long)]
        api_id: Option<i32>,
        #[arg(long)]
        api_hash: Option<String>,
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
    /// Wipe everything: delete the local brain — every bot registration,
    /// all memory. The Telegram bot accounts and your DMs with them
    /// stay; re-register and /start again (stop serve first)
    Reset,
    /// Clear a bot's memory
    Clear { name: String },
    /// Delete every message in your DM with a bot + wipe its memory
    ClearChat { name: String },
    /// Read recent messages of your DM with a bot (as your user)
    History {
        /// Roster name or @username
        bot: String,
        #[arg(long, default_value = "20")]
        limit: i32,
    },
    /// Post a message into your DM with a bot as yourself
    Say {
        /// Roster name or @username
        bot: String,
        /// Message text — or a path to upload as a document with --file
        text: String,
        /// Upload `text` as a file (MTProto, over the Bot API's 50MB cap)
        #[arg(long)]
        file: bool,
    },
    /// 7z ~/verity (minus target/) + $HOME settings into one archive;
    /// send it to your DM with the lead bot
    Backup,
    /// Restore from a backup archive — pass its path or https URL and
    /// it unpacks + re-enables itself (no Telegram login needed; the
    /// session rides inside the archive). No argument: pull the newest
    /// archive out of your DM with the lead bot instead.
    Restore {
        /// Local path or https URL of a verity-backup-*.7z
        archive: Option<String>,
        /// Scan mode only: whose DM to search (default: lead bot)
        #[arg(long)]
        bot: Option<String>,
    },
    /// Print a `vsk1.` setup key: packs ~/verity into an AES-encrypted
    /// 7z, uploads it to your DM with the lead bot, and the key carries
    /// everything `verity --setupkey <key>` needs on a new machine.
    /// The key is a full credential — keep it secret.
    #[command(name = "getsetupkey")]
    GetSetupKey,
    /// Liveness check: start the bridge if it's down, restart it if its
    /// heartbeat went stale. Run by verity-watchdog.timer once a minute
    /// — honors `~/.local/state/verity-asleep` and a disabled unit
    Watchdog,
    /// Fetch origin/main, fast-forward the tree, rebuild the release
    /// binaries, then try-restart the bridge. Run by
    /// verity-self-update.timer every ~15 min; a clean skip (offline,
    /// mid-git-op, already current, ahead of origin) exits quietly
    SelfUpdate,
    /// Watch ~/verity/deaths for new reports and post them into your
    /// DM as file documents. verity-death-watch.service runs it under
    /// Restart=always
    DeathWatch {
        /// Roster name or @username of the bot reports are posted as
        /// (default: the lead bot)
        bot: Option<String>,
    },
    /// Run the bridge: bots listen, work quietly, reply when done
    Serve {
        /// Use the echo harness (no agent provider needed)
        #[arg(long)]
        echo: bool,
        /// Don't attach the user session — turns get no DM transcript
        #[arg(long)]
        no_context: bool,
    },
    /// Post into your DM with a bot, as the bot
    Send {
        bot: String,
        /// Message text — or a path to upload with --file/--photo
        text: String,
        /// Upload `text` as a file (sendDocument) instead of a message
        #[arg(long)]
        file: bool,
        /// Upload `text` as a photo (sendPhoto) — shows inline
        #[arg(long)]
        photo: bool,
    },
}

#[derive(Subcommand)]
enum BotCmd {
    /// Register a bot by its BotFather token; /start its DM to chat
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
        /// Sent as the bot's first message when its DM opens
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
    /// Rename a bot (store name + memory keys)
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
    /// Remove a bot (does not delete its DM or Telegram account)
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

/// Look a bot up by roster name or @username (either accepts a bare
/// `@name`, case-insensitive).
fn find_bot(store: &Store, name: &str) -> Result<Bot> {
    let want = name.trim_start_matches('@').to_lowercase();
    store
        .list_bots()?
        .into_iter()
        .find(|b| {
            b.name.to_lowercase() == want
                || b.username
                    .as_deref()
                    .map(|u| u.to_lowercase() == want)
                    .unwrap_or(false)
        })
        .with_context(|| format!("no such bot: {name}"))
}

/// The user-side peer of a bot's DM with the owner, resolved by its
/// Bot-API user id. Fails when the owner never /started that bot —
/// the dialog doesn't exist yet.
async fn dm_peer(tg: &Tg, bot: &Bot) -> Result<grammers_session::defs::PeerRef> {
    let me = BotApi::new(&bot.token).get_me().await?;
    tg.peer_for_user(me.id).await.map_err(Into::into)
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

    let cli = Cli::parse();
    if let Some(key) = &cli.setupkey {
        if cli.cmd.is_some() {
            bail!("--setupkey can't be combined with a subcommand");
        }
        let files = verity_core::setupkey::apply(key).await?;
        println!(
            "restored {} files into {}",
            files.len(),
            Config::home()?.display()
        );
        for note in verity_core::backup::post_restore(&files) {
            println!("{note}");
        }
        return Ok(());
    }
    // Unreachable on a bare `verity` — arg_required_else_help exits
    // before this — but a key-less fallthrough stays impossible.
    let Some(cmd) = cli.cmd else { return Ok(()) };
    match cmd {
        Cmd::Config {
            api_id,
            api_hash,
            pi_bin,
            api_bind,
            api_token,
        } => {
            let mut cfg = Config::load()?;
            let interactive = api_id.is_none()
                && api_hash.is_none()
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
            // Local-only wipe: the db holds every bot registration and
            // all memory, so deleting it is the clean slate. Telegram
            // bot accounts and your DMs with them stay — re-register,
            // /start each DM again, restart serve.
            let db = Config::db_path()?;
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
            let (tg, _) = authorized_tg().await?;
            match dm_peer(&tg, &b).await {
                Ok(peer) => {
                    let n = tg.clear_chat(peer).await?;
                    println!("deleted {n} messages");
                }
                Err(e) => eprintln!("no DM to clear — skipped: {e}"),
            }
            store.clear_memory(&name)?;
            store.kv_del(&format!("greeted:{name}"))?;
            store.kv_del(&format!("pending:{name}"))?;
            store.kv_del_prefix(&format!("pending:{name}:"))?;
            println!("cleared {name}'s chat + memory — fresh start");
        }

        Cmd::History { bot, limit } => {
            let store = Store::open(&Config::db_path()?)?;
            let b = find_bot(&store, &bot)?;
            let (tg, _) = authorized_tg().await?;
            let peer = dm_peer(&tg, &b).await?;
            for m in tg.dm_history(peer, limit).await? {
                println!("[{}] {}: {}", m.id, m.sender, m.text);
            }
        }

        Cmd::Say { bot, text, file } => {
            let store = Store::open(&Config::db_path()?)?;
            let b = find_bot(&store, &bot)?;
            let (tg, _) = authorized_tg().await?;
            let peer = dm_peer(&tg, &b).await?;
            if file {
                tg.send_file(peer, std::path::Path::new(&text), "").await?;
            } else {
                tg.send_message(peer, &text).await?;
            }
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
                BotCmd::Rename { name, new_name } => {
                    if !store.rename_bot(&name, &new_name)? {
                        bail!("no such bot");
                    }
                    println!("renamed {name} → {new_name} (restart serve to apply)");
                }
                BotCmd::List => {
                    for b in store.list_bots()? {
                        let user = b
                            .username
                            .map(|u| format!("@{u}"))
                            .unwrap_or_else(|| "-".into());
                        let model = b.model.unwrap_or_else(|| "-".into());
                        println!(
                            "{:<16} {:<24} provider={} model={}",
                            b.name, user, b.provider, model
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
            let (tg, _) = authorized_tg().await?;
            let owner = tg.self_id().await?;
            let bot = Store::open(&Config::db_path()?)?
                .primary_bot()?
                .context("no bots registered — `verity bot new`")?;
            let files = verity_core::backup::upload(&BotApi::new(&bot.token), owner).await?;
            println!("uploaded to your DM with {}: {}", bot.name, files.join(", "));
        }

        Cmd::Restore { archive, bot } => {
            let files = match &archive {
                Some(a) => verity_core::backup::restore_arg(a).await?,
                None => {
                    let (tg, _) = authorized_tg().await?;
                    let store = Store::open(&Config::db_path()?)?;
                    let bot = match &bot {
                        Some(name) => find_bot(&store, name)?,
                        None => store
                            .primary_bot()?
                            .context("no bots registered — pass an archive path/URL instead")?,
                    };
                    let peer = dm_peer(&tg, &bot).await?;
                    verity_core::backup::download(&tg, peer).await?
                }
            };
            if files.is_empty() {
                println!("no backup documents found in the DM");
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

        Cmd::GetSetupKey => {
            let (tg, _) = authorized_tg().await?;
            let owner = tg.self_id().await?;
            let bot = Store::open(&Config::db_path()?)?
                .primary_bot()?
                .context("no bots registered — `verity bot new`")?;
            let key =
                verity_core::setupkey::issue(&BotApi::new(&bot.token), &bot.token, owner).await?;
            eprintln!(
                "archive uploaded to your DM with {} — the key below restores the whole \
                 install on another machine (`verity --setupkey <key>`). It decrypts to your \
                 full login + every bot token: keep it secret.",
                bot.name
            );
            println!("{key}");
        }

        Cmd::Watchdog => {
            verity_core::liveness::check().await?;
        }

        Cmd::SelfUpdate => {
            verity_core::selfupdate::run()?;
        }

        Cmd::DeathWatch { bot } => {
            let store = Store::open(&Config::db_path()?)?;
            let b = match &bot {
                Some(name) => find_bot(&store, name)?,
                None => store
                    .primary_bot()?
                    .context("no bots registered — `verity bot new`")?,
            };
            let (tg, _) = authorized_tg().await?;
            let owner = tg.self_id().await?;
            verity_core::deathwatch::watch(&BotApi::new(&b.token), owner).await?;
        }

        Cmd::Serve { echo, no_context } => {
            let cfg = Config::load()?;
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
            // Attach the user session so turns can read the DM transcript.
            // Degrades gracefully to context-free turns when unavailable.
            let tg = if no_context {
                None
            } else {
                match authorized_tg().await {
                    Ok((tg, _)) => Some(Arc::new(tg)),
                    Err(e) => {
                        tracing::warn!(error = %e, "no user session — turns run without transcript");
                        None
                    }
                }
            };
            // Our user id gates bot DMs. Normally it comes from the
            // logged-in session; config owner_id / VERITY_OWNER_ID
            // covers running with no user session at all.
            let explicit = cfg
                .owner_id
                .or_else(|| std::env::var("VERITY_OWNER_ID").ok().and_then(|v| v.parse().ok()));
            let owner_id = match explicit {
                Some(id) => Some(id),
                None => match &tg {
                    Some(tg) => match tg.self_id().await {
                        Ok(id) => Some(id),
                        Err(e) => {
                            tracing::warn!(error = %e, "get_me failed — bot DMs disabled");
                            None
                        }
                    },
                    None => None,
                },
            };
            if owner_id.is_none() {
                tracing::warn!("no owner id — every DM will be ignored");
            }
            let bridge = Bridge {
                db_path: Config::db_path()?,
                make_harness,
                tg,
                owner_id,
            };
            tokio::select! {
                r = bridge.run() => { r?; }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down");
                }
            }
        }

        Cmd::Send {
            bot,
            text,
            file,
            photo,
        } => {
            let store = Store::open(&Config::db_path()?)?;
            let b = find_bot(&store, &bot)?;
            let (tg, _) = authorized_tg().await?;
            let owner = tg.self_id().await?;
            let api = BotApi::new(&b.token);
            if photo {
                api.send_photo(owner, std::path::Path::new(&text), None)
                    .await?;
            } else if file {
                api.send_document(owner, std::path::Path::new(&text), None)
                    .await?;
            } else {
                for part in verity_core::rich::split_markdown(&text, verity_core::rich::RICH_LIMIT)
                {
                    api.send_rich_message(owner, InputRichMessage::markdown(&part), None)
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
