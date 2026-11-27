# verity

Named AI teammates in Telegram. Each teammate is a real Telegram bot with
its own inbox and memory; direct messages are the primary interface. The
`pi` coding agent is the default brain, `echo` is useful for smoke tests,
and any CLI implementing the Agent Client Protocol can be plugged in with
`acp:<command>`.

## What you get

- one Telegram DM per teammate — no forum group or topic plumbing required
- user-side replies are delivered to the agent; agent replies come back as
  the bot
- queued turns survive restarts, with per-turn progress and interruption
  controls
- optional `/report` self-reflection posts written by the bots themselves
- roster snapshots, backups, restores, and account transfer through
  encrypted setup keys
- avatar generation, a web/WASM avatar demo, an Android TDLib client, and
  a currently shelved GPUI desktop client

## Quick start

Requirements: Rust, Telegram API credentials from
<https://my.telegram.org/apps>, and a phone-number-backed Telegram account.
Bot tokens come from [@BotFather](https://t.me/BotFather).

```bash
cargo build --release -p verity-cli

# one-time account login; writes ~/verity/config.toml and tg.session
verity login

# register a bot token under the teammate name "scout"
verity bot new scout --token <botfather-token>

# wake the bot and talk to it
verity say scout "say hello"
```

`login` uses an isolated `~/verity/pi` home on first use so it can install
the `pi` coding agent and the Telegram skills without changing your normal
pi configuration.

Useful commands:

```bash
verity serve                 # foreground bridge; Ctrl-C stops it
verity supervise             # self-restarting bridge (see Deployment)
verity bot list             # roster
verity say scout status      # send an instruction to scout
verity send scout "hello"    # post as scout in your DM with scout
verity backup                # create and upload an unencrypted backup archive
verity restore               # restore newest archive from the lead-bot DM
verity self-update           # fetch origin/main, rebuild, recycle serve
```

`serve` is the raw bridge. `supervise` is normally what you want for a
long-running installation: it starts `serve` as a child, restarts crashes
with backoff, notices stale heartbeats, honors deliberate shutdowns, and
recycles the child after self-updates.

## Configuration

`~/verity/config.toml`:

```toml
api_id = 123456
api_hash = "…"
pi_bin = "pi"                 # optional override
api_bind = "127.0.0.1:7742"   # optional; mobile JSON API
api_token = "…"               # optional bearer token for that API
```

`~/verity/state.sqlite` stores the roster, queued/completed turns, progress
checkpoints, and backup bookkeeping. `~/verity/tg.session` is the MTProto
user session. Both contain sensitive account material and are protected
with owner-only permissions on unix-like systems.

## Deployment without systemd

systemd is optional. The portable deployment path is:

```bash
cargo build --release -p verity-cli
./target/release/verity supervise --self-update
```

`--self-update` fetches and fast-forwards `origin/main`, rebuilds the
release binaries, and restarts the serve child. Update cadence defaults to
900 seconds and can be changed:

```bash
verity supervise --self-update --self-update-every 1800
```

Autostart is deliberately independent from Verity: use systemd if you have
it, but launchd, Windows Task Scheduler, tmux, a container orchestrator,
or any other process launcher can start `verity supervise`. The supervisor
itself only needs to spawn and monitor child processes.

### Windows/self-update note

Windows cannot overwrite a running executable. For self-updating Windows
installs, run the supervisor copy outside the repository and point it at
the rebuildable release binary:

```powershell
cargo build --release -p verity-cli
copy target\release\verity.exe $env:USERPROFILE\verity-supervisor.exe
$env:VERITY_SERVE_BIN = "$PWD\target\release\verity.exe"
& $env:USERPROFILE\verity-supervisor.exe supervise --self-update
```

On unix-like systems this split is usually unnecessary because the running
binary can be replaced in place.

### systemd compatibility

The older Linux integration remains available where systemd exists:
`verity watchdog` can inspect and restart `verity.service`, restores can
reenable restored user units, and self-update first tries
`systemctl --user try-restart verity.service`. These are compatibility
paths, not requirements. A machine without systemd uses `verity supervise`.

## Backups, restores, and moving machines

There are two related but different flows.

### Regular backups

```bash
verity backup                 # create + upload now
verity restore                # scan lead-bot DM for newest archive
verity restore backup.7z      # restore a local archive
```

Regular backups are intentionally plain, unencrypted 7z archives. They include the
MTProto user session, registered bot tokens, roster metadata, workspace
memory, and optional extra dotfiles. Verity's current threat model treats
Telegram as the transport and storage boundary for these files, so the
backup archive itself is not encrypted. **Anyone who obtains one of these
backup files can use the contained session and bot tokens.** Treat backup
files as highly sensitive account material.

While `serve` runs, an automatic backup runs every six hours and uploads the
archive to the owner's DM with the lead bot. `VERITY_BACKUP_EVERY` overrides
the cadence in seconds; `0` disables it.

### Encrypted setup keys

Setup keys are the machine-to-machine handoff flow and are encrypted.
`verity getsetupkey` prompts for a password on the source machine, uploads
the encrypted archive through the lead bot, and prints a `vsk1.` key. The
key contains both the password and file reference, so treat it as a full
credential. On the new machine, `verity --setupkey <key>` downloads and
restores it.

```bash
verity getsetupkey
verity --setupkey vsk1....   # on the new machine
```

### Restore

`verity restore` extracts archives through a staging directory and rollback
backup so unrelated files are not overwritten. Restored systemd user units
are enabled only when systemd is present; otherwise the restore still
succeeds and `verity supervise` is the portable way to run the bridge.

## Agent control surface

Bots stay quiet while a prompt is in progress. They publish one-line
progress markers into the user DM and flush progress every 30 seconds:

- `:::progress ...:::` / `:::endprogress:::`
- `:::restart:::`
- `:::shutdown:::`
- `:::backup:::`
- `:::report ...:::`
- `:::typing-start:::`, `:::typing-stop:::`, `:::notify:::`

Destructive actions require a human confirmation quiz. Permission prompts
from ACP-compatible agents are auto-approved with the most permissive
`allow_*` choice. That is intentional for autonomous operation, but it
means a bot's agent has broad host authority; run bots in a sandbox or on
an account/machine whose authority matches that trust model.

## Android

`android/` is a native TDLib client. It talks to Telegram directly rather
than proxying through the bridge host. See [android/README.md](android/README.md).
The Rust JSON API in `verity-core` remains available as a lightweight
companion/control API, but it is not required by the Android app.

## Other pieces

- `web/` — browser demo for the deterministic avatar renderer
- `crates/verity-gui` — shelved GPUI desktop client
- `crates/verity-math` — Typst/mitex LaTeX-to-PNG renderer, exposed to
  Android through JNI

## Development

```bash
cargo fmt
cargo test -p verity-core -p verity-cli
cargo test -p verity-pfp
cargo check -p verity-pfp-wasm
```

A JSON API harness and mock Bot API tests are included in `verity-core`.
