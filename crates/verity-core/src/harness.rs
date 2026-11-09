//! Provider seam. Every bot runs behind a `Harness`; the bridge doesn't
//! care whether the agent is an embedded pi-agent or an external ACP CLI.
//!
//! V1 ships `PiHarness`, which drives the `pi`/`pirs` binary in print mode
//! (one subprocess per turn). Embedding pi-agent as a crate comes next.

use async_trait::async_trait;
use tokio::process::Command;

use crate::{Error, Result};

/// One completed turn of agent work.
#[derive(Debug)]
pub struct TurnOutput {
    /// Final reply, as Telegram Rich Markdown.
    pub reply: String,
}

/// Context handed to the harness for one turn.
#[derive(Debug, Clone)]
pub struct TurnContext {
    pub bot_name: String,
    pub instructions: String,
    pub memory: Vec<(String, String)>,
    /// Working directory for the agent.
    pub cwd: std::path::PathBuf,
    pub model: Option<String>,
}

#[async_trait]
pub trait Harness: Send + Sync {
    async fn run_turn(&self, ctx: &TurnContext, prompt: &str) -> Result<TurnOutput>;
}

/// Drives the `pi`/`pirs` binary (`pi --mode print -p "<prompt>"`).
/// The process's own auth.json carries the credentials — custom API keys
/// or subscription OAuth, per pi's provider config.
pub struct PiHarness {
    pub bin: String,
}

impl PiHarness {
    pub fn new(bin: Option<String>) -> Self {
        Self {
            bin: bin.unwrap_or_else(|| "pi".into()),
        }
    }

    fn system_prompt(ctx: &TurnContext) -> String {
        let mut s = format!(
            "You are {}, a teammate bot in a Telegram group. Follow these standing instructions:\n{}\n",
            ctx.bot_name, ctx.instructions
        );
        if !ctx.memory.is_empty() {
            s.push_str("\nWhat you remember:\n");
            for (k, v) in &ctx.memory {
                s.push_str(&format!("- {k}: {v}\n"));
            }
        }
        s.push_str("\nReply for Telegram Rich Markdown: GFM, tables, and LaTeX formulas are supported. Keep it concise.");
        s
    }
}

#[async_trait]
impl Harness for PiHarness {
    async fn run_turn(&self, ctx: &TurnContext, prompt: &str) -> Result<TurnOutput> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--mode").arg("print")
            .arg("-p")
            .arg(format!("{}\n\n---\n\n{}", Self::system_prompt(ctx), prompt))
            .current_dir(&ctx.cwd)
            .kill_on_drop(true);
        if let Some(m) = &ctx.model {
            cmd.arg("--model").arg(m);
        }
        let out = cmd
            .output()
            .await
            .map_err(|e| Error::Harness(format!("failed to spawn {}: {e}", self.bin)))?;
        if !out.status.success() {
            return Err(Error::Harness(format!(
                "{} exited {}: {}",
                self.bin,
                out.status,
                String::from_utf8_lossy(&out.stderr).chars().take(500).collect::<String>()
            )));
        }
        Ok(TurnOutput {
            reply: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        })
    }
}

/// Echoes back; lets the bridge run end-to-end without a provider.
pub struct EchoHarness;

#[async_trait]
impl Harness for EchoHarness {
    async fn run_turn(&self, ctx: &TurnContext, prompt: &str) -> Result<TurnOutput> {
        Ok(TurnOutput {
            reply: format!("*{}* heard: {}", ctx.bot_name, prompt),
        })
    }
}
