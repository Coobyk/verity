//! Provider seam. Every bot runs behind a `Harness`; the bridge doesn't
//! care whether the agent is an embedded pi-agent or an external ACP CLI.
//!
//! Providers (the `provider` column on a bot record):
//! - `pi` — drives the `pi`/`pirs` binary in print mode (one subprocess
//!   per turn; works today, uses pi's own auth.json)
//! - `echo` — plumbing test, no agent
//! - `acp:<cmd> [args]` — any agent CLI speaking the Agent Client
//!   Protocol over stdio (claude-code-acp, codex acp, `pi --acp`, …)

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::info;

use crate::{Error, Result};

/// One completed turn of agent work.
#[derive(Debug)]
pub struct TurnOutput {
    /// Final reply, as Telegram Rich Markdown.
    pub reply: String,
}

/// Shared one-line "what the agent is doing right now". Harnesses that
/// stream events (ACP) write it on every tool call / text chunk; the
/// bridge's ⏳ progress message reads it each tick. Providers with no
/// event stream just leave it empty → "working".
#[derive(Debug, Clone, Default)]
pub struct Activity(Arc<Mutex<String>>);

impl Activity {
    pub fn set(&self, label: impl Into<String>) {
        *self.0.lock().unwrap() = label.into();
    }
    pub fn get(&self) -> String {
        self.0.lock().unwrap().clone()
    }
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
    /// True on the bot's first-ever user turn: greet the sender by name
    /// this once; never repeat it later.
    pub first_contact: bool,
    /// Working directory for the agent.
    pub cwd: std::path::PathBuf,
    pub model: Option<String>,
    /// Out-channel for progress labels; see [`Activity`].
    pub activity: Activity,
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
        "You are {}, a teammate bot in a Telegram group. \
         This is chat, not email: reply casually and directly. \
         No salutations or greeting prefixes — don't open by \
         addressing the sender by name. Get straight to it.\n\
         Follow these standing instructions:\n{}\n",
        ctx.bot_name, ctx.instructions
    );
    if ctx.first_contact {
        s.push_str(
            "\nThis is the first message this user has ever sent you — \
             open by addressing them by name with a quick warm hello, \
             then be casual. Only this once.\n",
        );
    }
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
             `:::spawn label: task:::`; their results are folded into your reply. \
             To register a brand-new teammate bot (when given a BotFather token), emit \
             `:::bot_new name | token | instructions | provider:::` — provider optional, \
             it defaults to yours.",
            ctx.peers.join(", ")
        ));
    }
    s.push_str(
        "\nReply for Telegram Rich Markdown: GFM, tables, and LaTeX formulas are supported. Keep it concise. \
         To persist a fact, put it on its own line as `:::remember key = value:::` — it is stored as memory and stripped from your reply. \
         To archive all of ~/verity (and your $HOME settings) into the group's General topic (backup), emit `:::backup:::`. \
         To restart the bridge (e.g. after rebuilding its binary), emit `:::restart:::`; to stop it for good, `:::shutdown:::` — \
         both fire only after your reply posts, so never `systemctl` the verity service yourself.",
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
            reply: strip_thinking(String::from_utf8_lossy(&out.stdout).trim()),
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
/// `VERITY_TURN_TIMEOUT` (seconds) overrides; default 15m.
fn acp_timeout() -> Duration {
    std::env::var("VERITY_TURN_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(15 * 60))
}

/// Owns the turn's agent child so dropping it — on timeout, error, or
/// early return — kills the whole process group, not just the direct
/// child. `kill_on_drop` alone leaves grandchildren (shells, browsers,
/// dev servers the agent spawned) running as orphans.
struct ChildGroup {
    child: tokio::process::Child,
}

impl ChildGroup {
    fn spawn(cmd: &mut Command) -> std::io::Result<Self> {
        #[cfg(unix)]
        cmd.process_group(0); // group leader: pgid == child pid
        cmd.spawn().map(|child| Self { child })
    }
}

impl std::ops::Deref for ChildGroup {
    type Target = tokio::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl std::ops::DerefMut for ChildGroup {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for ChildGroup {
    fn drop(&mut self) {
        let Some(pid) = self.child.id() else { return };
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

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
        let limit = acp_timeout();
        match tokio::time::timeout(limit, run).await {
            Ok(r) => r,
            Err(_) => Err(Error::Harness(format!(
                "{}: turn exceeded {}s",
                self.argv.first().map(String::as_str).unwrap_or("acp"),
                limit.as_secs()
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
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .current_dir(&ctx.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = ChildGroup::spawn(&mut cmd)
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

        let mut acc = ReplyAccumulator::new();
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
                let t = line.trim_end();
                info!("acp -> {}", &t[..t.floor_char_boundary(t.len().min(300))]);
                sent += 1;
            }

            let line = match lines.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => break, // agent exited
                Err(e) => return Err(Error::Harness(format!("acp read: {e}"))),
            };
            info!(
                "acp <- {}",
                &line[..line.floor_char_boundary(line.len().min(300))]
            );
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
                    if let Some(label) = activity_label(u) {
                        ctx.activity.set(label);
                    }
                    match u["sessionUpdate"].as_str() {
                        Some("agent_message_chunk") => {
                            if let Some(t) = u["content"]["text"].as_str() {
                                acc.chunk(t);
                            }
                        }
                        // Thoughts are never the reply. Any other update
                        // (tool_call, plan, …) means work happened between
                        // text blocks — text before it was narration.
                        Some("agent_thought_chunk") | None => {}
                        Some(_) => acc.boundary(),
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

        drop(stdin); // close the pipe so the server can exit
                     // Per-turn agent: don't wait indefinitely — kill if it lingers.
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        let _ = child.kill().await;
        let stderr = stderr_buf.await.unwrap_or_default();
        let reply = strip_thinking(acc.finish().trim());
        if reply.is_empty() && !stderr.is_empty() {
            tracing::debug!(stderr = %stderr.chars().take(500).collect::<String>(), "acp: empty reply");
        }
        Ok(TurnOutput { reply })
    }
}

/// Collects the reply text from `agent_message_chunk` updates. Agents
/// narrate mid-turn ("checking the file…") as message chunks too, so
/// concatenating every chunk leaks working traces. Each update that
/// isn't message/thought text — a tool call, plan change, mode switch —
/// closes the current segment; the reply is the last non-empty segment:
/// the final uninterrupted run of message chunks.
#[derive(Default)]
struct ReplyAccumulator {
    current: String,
    last: String,
}

impl ReplyAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn chunk(&mut self, text: &str) {
        self.current.push_str(text);
    }

    fn boundary(&mut self) {
        if !self.current.trim().is_empty() {
            self.last = std::mem::take(&mut self.current);
        } else {
            self.current.clear();
        }
    }

    fn finish(self) -> String {
        if !self.current.trim().is_empty() {
            self.current
        } else {
            self.last
        }
    }
}

/// Turn an ACP `session/update` payload into a short ⏳ label —
/// "editing mtproto.rs", "running cargo test". `None` for updates that
/// carry no new activity (usage counters, bare status flips), so they
/// don't overwrite a more specific label with "working".
fn activity_label(u: &serde_json::Value) -> Option<String> {
    let label = match u["sessionUpdate"].as_str()? {
        "tool_call" | "tool_call_update" => {
            let file = tool_path(u);
            match u["kind"].as_str() {
                Some("read") => file.map(|f| format!("reading {f}")),
                Some("edit") => file.map(|f| format!("editing {f}")),
                // devin titles execute calls "Ran cmd1, cmd2".
                Some("execute") => u["title"]
                    .as_str()
                    .and_then(|t| t.strip_prefix("Ran "))
                    .map(|c| format!("running {c}")),
                _ => None,
            }
            .or_else(|| u["title"].as_str().and_then(gerundize))?
        }
        "agent_thought_chunk" => "thinking".into(),
        "agent_message_chunk" => "writing reply".into(),
        "plan" => "planning".into(),
        _ => return None,
    };
    Some(truncate(&label, 48))
}

/// File a tool call touches: basename of `locations[].path`, a diff
/// `content[].path`, or `rawInput.file_path` — whichever shows up first.
fn tool_path(u: &serde_json::Value) -> Option<String> {
    let path = u["locations"]
        .as_array()
        .and_then(|l| l.iter().find_map(|l| l["path"].as_str()))
        .or_else(|| {
            u["content"]
                .as_array()
                .and_then(|c| c.iter().find_map(|c| c["path"].as_str()))
        })
        .or_else(|| u["rawInput"]["file_path"].as_str())?;
    std::path::Path::new(path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
}

/// Best-effort present participle for the past-tense titles agents
/// report ("Ran" is handled by the execute branch; the rest lands here).
/// Unknown titles pass through with the first letter lowered.
fn gerundize(title: &str) -> Option<String> {
    if title.is_empty() {
        return None;
    }
    let (verb, rest) = title.split_once(' ').unwrap_or((title, ""));
    let doing = match verb {
        "Ran" | "Run" => "running",
        "Read" => "reading",
        "Edit" | "Edited" => "editing",
        "Wrote" | "Write" => "writing",
        "Listed" => "listing",
        "Called" | "Calling" => "calling",
        "Invoked" => "invoking",
        "Created" => "creating",
        "Fetched" | "Fetching" => "fetching",
        "Searched" | "Searching" => "searching",
        "Deleted" => "deleting",
        "Moved" => "moving",
        _ => {
            let mut t = title.to_string();
            if let Some(r) = t.get_mut(0..1) {
                r.make_ascii_lowercase();
            }
            return Some(t);
        }
    };
    Some(if rest.is_empty() {
        doing.into()
    } else {
        format!("{doing} {rest}")
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut t = s[..s.floor_char_boundary(max)].to_string();
    t.push('…');
    t
}

/// Drop `<think>…</think>` / `<thinking>…</thinking>` blocks some models
/// inline into their visible output. An unclosed tag truncates the rest —
/// everything after it is still thinking.
fn strip_thinking(s: &str) -> String {
    let mut out = s.to_string();
    for (open, close) in [("<think>", "</think>"), ("<thinking>", "</thinking>")] {
        loop {
            let Some(start) = out.find(open) else { break };
            let Some(end) = out[start + open.len()..].find(close) else {
                out.truncate(start);
                break;
            };
            out.replace_range(start..start + open.len() + end + close.len(), "");
        }
    }
    out.trim().to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_is_last_segment_only() {
        let mut acc = ReplyAccumulator::new();
        acc.chunk("let me look at the code…");
        acc.boundary(); // tool_call
        acc.chunk("found it — fixing now");
        acc.boundary(); // tool_call
        acc.chunk("done, the fix is in harness.rs");
        assert_eq!(acc.finish(), "done, the fix is in harness.rs");
    }

    #[test]
    fn reply_single_segment_when_no_tools() {
        let mut acc = ReplyAccumulator::new();
        acc.chunk("sure, ");
        acc.chunk("here you go");
        assert_eq!(acc.finish(), "sure, here you go");
    }

    #[test]
    fn reply_falls_back_when_turn_ends_on_tool() {
        let mut acc = ReplyAccumulator::new();
        acc.chunk("narration");
        acc.boundary();
        acc.chunk("real reply");
        acc.boundary(); // trailing tool_call, no closing message
        assert_eq!(acc.finish(), "real reply");
    }

    #[test]
    fn whitespace_segments_are_dropped() {
        let mut acc = ReplyAccumulator::new();
        acc.chunk("answer");
        acc.boundary();
        acc.chunk("   \n  ");
        acc.boundary();
        acc.chunk("final");
        assert_eq!(acc.finish(), "final");
    }

    #[test]
    fn labels_tool_calls() {
        let u = json!({"sessionUpdate": "tool_call", "kind": "edit",
            "content": [{"type": "diff", "path": "/a/b/mtproto.rs"}]});
        assert_eq!(activity_label(&u).as_deref(), Some("editing mtproto.rs"));
        let u = json!({"sessionUpdate": "tool_call", "kind": "read",
            "locations": [{"path": "/a/b/main.rs"}]});
        assert_eq!(activity_label(&u).as_deref(), Some("reading main.rs"));
        let u = json!({"sessionUpdate": "tool_call", "kind": "execute",
            "title": "Ran cargo test"});
        assert_eq!(activity_label(&u).as_deref(), Some("running cargo test"));
        let u = json!({"sessionUpdate": "tool_call",
            "title": "Calling get_me from instabox"});
        assert_eq!(
            activity_label(&u).as_deref(),
            Some("calling get_me from instabox")
        );
    }

    #[test]
    fn bare_status_updates_keep_prior_label() {
        let u = json!({"sessionUpdate": "tool_call_update", "status": "completed"});
        assert_eq!(activity_label(&u), None);
        let u = json!({"sessionUpdate": "usage_update"});
        assert_eq!(activity_label(&u), None);
    }

    #[test]
    fn strips_think_blocks() {
        let s = "hi<think>hmm, the user wants…</think>there";
        assert_eq!(strip_thinking(s), "hithere");
        let s = "<thinking>deep thoughts</thinking>\n\nanswer";
        assert_eq!(strip_thinking(s), "answer");
        assert_eq!(strip_thinking("ok<think>never closed"), "ok");
        assert_eq!(strip_thinking("plain"), "plain");
    }
}
