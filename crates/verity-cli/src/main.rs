//! verity — manage teammate bots that live in a Telegram forum group.

use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
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
    /// Set api_id/api_hash (my.telegram.org) and the host group
    Config {
        #[arg(long)]
        api_id: Option<i32>,
        #[arg(long)]
        api_hash: Option<String>,
        #[arg(long)]
        group_id: Option<i64>,
        #[arg(long)]
        pi_bin: Option<String>,
    },
    /// Log in to Telegram (MTProto) as yourself
    Login {
        #[arg(long)]
        phone: Option<String>,
    },
    /// Bot management
    Bot {
        #[command(subcommand)]
        cmd: BotCmd,
    },
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
    Send { bot: String, text: String },
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
    },
    /// List registered bots
    List,
    /// Remove a bot (does not delete its topic)
    Rm { name: String },
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

#[tokio::main]
async fn main() -> Result<()> {
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
            pi_bin,
        } => {
            let mut cfg = Config::load()?;
            if let Some(v) = api_id {
                cfg.api_id = Some(v);
            }
            if let Some(v) = api_hash {
                cfg.api_hash = Some(v);
            }
            if let Some(v) = group_id {
                cfg.group_id = Some(v);
            }
            if let Some(v) = pi_bin {
                cfg.pi_bin = Some(v);
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
                    })?;
                    println!("registered {name} (@{})", me.username.unwrap_or_default());
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
            let bridge = Bridge {
                group_id,
                db_path: Config::db_path()?,
                make_harness,
                tg,
                tg_peer,
            };
            tokio::select! {
                r = bridge.run() => { r?; }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down");
                }
            }
        }

        Cmd::Send { bot, text } => {
            let store = Store::open(&Config::db_path()?)?;
            let b = store.get_bot(&bot)?.context("no such bot")?;
            let cfg = Config::load()?;
            let group_id = cfg.group_id.context("no group_id")?;
            let topic = b
                .topic_id
                .context("bot has no topic yet — run `verity serve` once")?;
            let api = BotApi::new(&b.token);
            for part in verity_core::rich::split_markdown(&text, verity_core::rich::RICH_LIMIT) {
                api.send_rich_message(group_id, InputRichMessage::markdown(&part), Some(topic))
                    .await?;
            }
            println!("sent");
        }
    }
    Ok(())
}
