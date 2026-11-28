# Architecture

## The basic unit

One teammate bot is:

- a Bot API identity and DM inbox (`bots` table)
- a per-bot `agent/` workspace under `~/verity/workspaces/<name>/`
- a `memory.md` file in the DM with the owner
- a pluggable `Harness` process — `pi`, `echo`, or `acp:<command>`
- an optional persona prompt
- a turn loop that listens in the DM, works quietly, and reports back

Direct messages are the primary interface. A new bot begins asleep after
registration and wakes when the owner first talks to it. The first wake
writes a soul/skills scaffold into the bot workspace and seeds `memory.md`.
After that the bot maintains its own workspace and memory.

Multiple bots can coexist in one `verity serve` process. Each bot has an
independent Telegram update stream and harness subprocesses.

## Why DMs

The DM is already the natural Telegram unit for talking to a bot: it has
one inbox, one ownership boundary, one memory file, one workspace, and one
set of notifications. Forum topics add addressing and synchronization
complexity without improving that model, so they are no longer the primary
architecture.

The shared substrate is the MTProto user session in `~/verity/tg.session`.
The bridge uses it to post owner-side text into bot DMs, inspect sender
identity, fetch unread history across restarts, read memory files, and run
backup/report flows. Bot API connections are per bot.

## Processes

```text
operator / OS service launcher
    │
    ▼
verity supervise                         optional but recommended
    │ spawns/monitors
    ▼
verity serve
    ├── MTProto user connection          shared owner account
    ├── Bot API long-poll task           one per bot
    │       └── turn harness subprocess  pi/echo/ACP, one per turn
    ├── backup loop                      every six hours
    ├── report loop                      periodic self-reflection posts
    └── optional JSON API                localhost bearer-token control API
```

`serve` is deliberately simple. `supervise` is a portable wrapper around
it: it restarts crashed children with backoff, checks the heartbeat file,
honors the asleep flag, reacts to restart requests, and optionally runs
self-update. It does not require systemd.

Any OS launcher can autostart `verity supervise`: systemd, launchd,
Windows Task Scheduler, tmux, Docker, or a manually managed process.
Linux systemd units from older installations still work; systemd is now a
compatibility path rather than a hard dependency.

## Turn lifecycle

1. A bot update arrives in its DM.
2. Noise such as progress chatter is suppressed.
3. Interrupts (`stop`, `undo`, `wait`, `/name`) are handled specially.
4. Ordinary text/media becomes a turn and is stored durably in SQLite
   before work starts.
5. If another turn is active, the new turn waits in the per-bot queue.
6. The bridge builds a prompt containing persona, memory, wake context,
   DM transcript context, and the user's request.
7. A harness subprocess starts in the bot workspace.
8. Agent output is parsed for control markers. Ordinary assistant output
   is not relayed while work is running.
9. `:::endprogress:::` posts the user-facing completion message. Timeout
   or process exit without the marker posts an interrupted/degraded
   completion instead.
10. Completion, interrupt state, stdout tails, and progress checkpoints
    are persisted so restarts can recover honestly.

Turns are globally semaphore-limited. On boot, interrupted turns are
requeued; unread DM history is reconciled against SQLite so messages
received while the bridge was down are not lost.

## Control markers

Markers are parsed even when they appear inside fenced code blocks:

- `:::progress ...:::` updates progress state
- `:::endprogress:::` posts completion text
- `:::restart:::` exits the bridge so the supervisor/service can restart it
- `:::shutdown:::` writes the asleep flag and exits deliberately
- `:::backup:::` requests an immediate backup sync
- `:::report ...:::` posts a status digest as the bot
- `:::typing-start:::` / `:::typing-stop:::` controls typing state
- `:::notify:::` requests a phone notification

Because model output is untrusted, destructive host actions are not direct
Telegram commands. They require a human confirmation quiz. ACP permission
prompts are auto-approved with the most permissive `allow_*` choice. This
is intentional for autonomous operation and means the harness effectively
has host-level authority; deployment should sandbox it accordingly.

## Harnesses

`Harness::Pi` runs the `pi` coding agent with the isolated `~/verity/pi`
home. That home is bootstrapped by `pi boot`, including pi telegram skills
and API-key propagation from the outer pi installation when available.

`Harness::Echo` is a deterministic smoke-test harness.

`Harness::Acp` runs any Agent Client Protocol command. It initializes an
ACP session in the bot workspace, streams prompts, accepts permission
requests automatically, and collects assistant output.

Media and files are downloaded into `inbox/` in the bot workspace and
referenced from the prompt as local paths.

## State and recovery

SQLite (`~/verity/state.sqlite`) is the source of truth for:

- roster metadata
- active, queued, interrupted, and completed turns
- interrupt flags
- progress checkpoints
- backup bookkeeping

Files under `~/verity` hold the MTProto session, configuration, workspaces,
and logs. `~/.local/state/verity-heartbeat.json` records process liveness;
`~/.local/state/verity-asleep` records intentional shutdown;
`~/.local/state/verity-restart` is a one-shot portable restart request.

The supervisor uses heartbeat age plus PID matching. A missing or malformed
heartbeat is not enough to kill a child because boot may legitimately take
time. A stale heartbeat belonging to the supervised child is treated as a
wedged process and killed.

## Backups and setup keys

Regular backups are plain, unencrypted 7z archives by design. They include the
MTProto session, bot tokens, roster, workspace memory, and selected extra
dotfiles, and are uploaded to the owner's DM with the lead bot every six
hours while `serve` runs. Verity treats Telegram as the transport/storage
boundary for this flow, but the archive content is still extremely
sensitive: possession of a backup file is possession of the account session
and bot tokens.

Restores use a staging directory and rollback copy. Legacy sensitive files
keep owner-only permissions where the platform supports them. Restored
systemd units are enabled only when systemd exists; absence of systemd is
not a restore failure.

Setup keys are separate and encrypted. They exist for machine-to-machine
handoff: the source encrypts a small archive with a user-entered password,
uploads it through the lead bot, and prints a `vsk1.` key containing both
the password and file reference. The target passes that key to
`verity --setupkey`, which downloads and decrypts the archive. The key is
therefore a full credential even though the archive itself is encrypted.

## Self-update

`verity self-update`:

1. resolves the repo (`VERITY_REPO`, then `~/verity`)
2. refuses dirty or detached/non-main states
3. fetches `origin/main`
4. fast-forwards only
5. rebuilds release binaries
6. asks the active runtime to recycle

On systemd systems it first tries `systemctl --user try-restart
verity.service`. Without systemd it writes the portable restart request.
`verity supervise` consumes that file, kills the serve child, and starts a
new one. `VERITY_SERVE_BIN` can point the supervisor at the rebuilt release
binary when the supervisor itself is a stable copy outside `target/`, which
is especially important on Windows.

## Interfaces beyond the bridge

- `android/` is a native TDLib Android client and talks to Telegram
  directly. It does not need the bridge JSON API.
- `crates/verity-core/src/api.rs` is an optional localhost/bearer-token
  JSON control API for companion tooling.
- `web/` demonstrates the deterministic avatar renderer through WASM.
- `windows/Verity.WinUI` is the native WinUI 3 Windows shell. It owns only
  presentation and talks to `crates/verity-winui-host` over newline-delimited
  JSON; the host owns Telegram, sessions, configuration, and roster state.
- `crates/verity-gui` is a shelved GPUI desktop client.
- `crates/verity-math` renders LaTeX through Typst/mitex and exposes the
  renderer to Android through JNI.
