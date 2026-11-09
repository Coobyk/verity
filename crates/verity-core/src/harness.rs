//! Provider seam. Every bot runs behind a `Harness`; the bridge doesn't
//! care whether the agent is an embedded pi-agent or an external ACP CLI.
//!
//! Providers (the `provider` column on a bot record):
//! - `pi` — drives the `pi`/`pirs` binary in print mode (one subprocess
//!   per turn; works today, uses pi's own auth.json)
//! - `echo` — plumbing test, no agent
//! - `acp:<cmd> [args]` — any agent CLI speaking the Agent Client
//!   Protocol over stdio (claude-code-acp, codex acp, `pi --acp`, …)

use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
    /// Recent topic messages as `(sender, text)`, oldest-first. The human's
    /// current request is included as the last entry — the agent replies to it.
    pub history: Vec<(String, String)>,
    /// Names of the other bots sharing the group (this bot excluded) —
    /// teammates reachable via `:::tell name: …:::`.
    pub peers: Vec<String>,
    /// Working directory for the agent.
    pub cwd: std::path::PathBuf,
    pub model: Option<String>,
}

#[async_trait]
pub trait Harness: Send + Sync {
    async fn run_turn(&self, ctx: &TurnContext, prompt: &str) -> Result<TurnOutput>;
}

/// The system prompt shared by all harnesses: identity, instructions,
/// memory, transcript, teammate roster, and the marker syntax the bridge
/// strips and acts on.
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
    if !ctx.history.is_empty() {
        s.push_str("\nRecent messages in your topic (oldest first):\n");
        for (sender, text) in &ctx.history {
            s.push_str(&format!("{sender}: {text}\n"));
        }
    }
    if !ctx.peers.is_empty() {
        s.push_str(&format!(
            "\nYou share this group with teammates: {}. \
             To hand work to one, emit `:::tell name: what to do:::` on its own line — \
             they run the task and reply in the topic as themselves. \
             To fan out up to three short-lived helpers on part of the task, emit \
             `:::spawn label: task:::`; their results are folded into your reply.",
            ctx.peers.join(", ")
        ));
    }
    s.push_str(
        "\nReply for Telegram Rich Markdown: GFM, tables, and LaTeX formulas are supported. Keep it concise. \
         To persist a fact, put it on its own line as `:::remember key = value:::` — it is stored as memory and stripped from your reply.",
    );
    s
}

fn full_prompt(ctx: &TurnContext, prompt: &str) -> String {
    format!("{}\n\n---\n\n{}", system_prompt(ctx), prompt)
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
}

#[async_trait]
impl Harness for PiHarness {
    async fn run_turn(&self, ctx: &TurnContext, prompt: &str) -> Result<TurnOutput> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--mode")
            .arg("print")
            .arg("-p")
            .arg(full_prompt(ctx, prompt))
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
                String::from_utf8_lossy(&out.stderr)
                    .chars()
                    .take(500)
                    .collect::<String>()
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

/// How long a single ACP turn may run before the child is killed.
const ACP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Drives any agent CLI speaking the Agent Client Protocol: newline-
/// delimited JSON-RPC over stdio (`initialize` → `session/new` →
/// `session/prompt`, accumulating `agent_message_chunk` updates).
///
/// `session/request_permission` calls are auto-approved (first allow_*
/// option) — the same trust level as letting `pi` run tools in `cwd`.
/// Other client-bound requests (fs, terminal) get a method-not-found
/// error; the agent falls back to its own tools.
pub struct AcpHarness {
    pub argv: Vec<String>,
}

impl AcpHarness {
    /// `spec` is everything after `acp:` — `"claude-code-acp"`,
    /// `"pi --acp"`, `"codex acp --experimental"`.
    pub fn new(spec: &str) -> Self {
        Self {
            argv: spec.split_whitespace().map(String::from).collect(),
        }
    }
}

#[async_trait]
impl Harness for AcpHarness {
    async fn run_turn(&self, ctx: &TurnContext, prompt: &str) -> Result<TurnOutput> {
        let run = self.run_turn_inner(ctx, prompt);
        match tokio::time::timeout(ACP_TIMEOUT, run).await {
            Ok(r) => r,
            Err(_) => Err(Error::Harness(format!(
                "{}: turn exceeded {}s",
                self.argv.first().map(String::as_str).unwrap_or("acp"),
                ACP_TIMEOUT.as_secs()
            ))),
        }
    }
}

impl AcpHarness {
    async fn run_turn_inner(&self, ctx: &TurnContext, prompt: &str) -> Result<TurnOutput> {
        let (bin, args) = self
            .argv
            .split_first()
            .ok_or_else(|| Error::Harness("empty acp: provider spec".into()))?;
        let mut child = Command::new(bin)
            .args(args)
            .current_dir(&ctx.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::Harness(format!("failed to spawn {bin}: {e}")))?;

        let mut stdin = child.stdin.take().expect("piped stdin");
        let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
        // Drain stderr so a chatty agent can't block on a full pipe.
        let mut stderr = child.stderr.take().expect("piped stderr");
        let stderr_buf = tokio::spawn(async move {
            let mut s = String::new();
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut s).await;
            s
        });

        let mut reply = String::new();
        // (rpc id, method, params) requests in flight order
        let mut requests = [
            json!({"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false}}),
            json!({"cwd": ctx.cwd, "mcpServers": []}),
            ValuePrompt::params(&full_prompt(ctx, prompt)),
        ];
        let methods = ["initialize", "session/new", "session/prompt"];

        let mut pending = 0usize; // next request index to send
        let mut sent = 0usize; // requests fully written
                               // Phase 1: send initialize, then drive the read/write loop.
        loop {
            if sent < requests.len() && pending == sent {
                let req = json!({"jsonrpc": "2.0", "id": sent, "method": methods[sent], "params": requests[sent]});
                let mut line =
                    serde_json::to_string(&req).map_err(|e| Error::Harness(e.to_string()))?;
                line.push('\n');
                stdin
                    .write_all(line.as_bytes())
                    .await
                    .map_err(|e| Error::Harness(format!("acp write: {e}")))?;
                sent += 1;
            }

            let line = match lines.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => break, // agent exited
                Err(e) => return Err(Error::Harness(format!("acp read: {e}"))),
            };
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue; // not JSON-RPC — ignore stray output
            };

            if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                if let Some(id) = msg.get("id").cloned() {
                    // Agent → client request: auto-answer.
                    let resp = if method == "session/request_permission" {
                        permission_response(id, &msg)
                    } else {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not supported"}})
                    };
                    let mut l =
                        serde_json::to_string(&resp).map_err(|e| Error::Harness(e.to_string()))?;
                    l.push('\n');
                    stdin
                        .write_all(l.as_bytes())
                        .await
                        .map_err(|e| Error::Harness(format!("acp write: {e}")))?;
                } else if method == "session/update" {
                    let u = &msg["params"]["update"];
                    if u["sessionUpdate"].as_str() == Some("agent_message_chunk") {
                        if let Some(t) = u["content"]["text"].as_str() {
                            reply.push_str(t);
                        }
                    }
                }
                continue;
            }

            // A response to one of our requests.
            if let Some(id) = msg.get("id").and_then(|i| i.as_u64()) {
                let id = id as usize;
                if id != pending {
                    continue;
                }
                if let Some(err) = msg.get("error") {
                    return Err(Error::Harness(format!(
                        "acp {}: {}",
                        methods[id],
                        err.get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("error")
                    )));
                }
                if methods[id] == "session/new" {
                    // Patch the prompt request with the real session id.
                    requests[2]["sessionId"] = msg["result"]["sessionId"].clone();
                }
                pending += 1;
                if pending == requests.len() {
                    break; // prompt response arrived — turn complete
                }
            }
        }

        let _ = child.wait().await;
        let stderr = stderr_buf.await.unwrap_or_default();
        if reply.is_empty() && !stderr.is_empty() {
            tracing::debug!(stderr = %stderr.chars().take(500).collect::<String>(), "acp: empty reply");
        }
        Ok(TurnOutput {
            reply: reply.trim().to_string(),
        })
    }
}

/// Placeholder for the `session/prompt` params; the sessionId is filled
/// in once `session/new` responds.
struct ValuePrompt;

impl ValuePrompt {
    fn params(text: &str) -> serde_json::Value {
        json!({"sessionId": "", "prompt": [{"type": "text", "text": text}]})
    }
}

/// Pick the first `allow_*` option in a `session/request_permission`
/// call; respond method-not-found when none exists.
fn permission_response(id: serde_json::Value, msg: &serde_json::Value) -> serde_json::Value {
    let allow = msg["params"]["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|o| {
            o["kind"]
                .as_str()
                .map(|k| k.starts_with("allow_"))
                .unwrap_or(false)
        })
        .max_by_key(|o| match o["kind"].as_str() {
            Some("allow_always") => 2,
            _ => 1,
        });
    match allow.and_then(|o| o["optionId"].as_str()) {
        Some(option_id) => json!({
            "jsonrpc": "2.0", "id": id,
            "result": {"outcome": {"outcome": "selected", "optionId": option_id}}
        }),
        None => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no allow option"}})
        }
    }
}
