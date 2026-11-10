<img src="assets/icon.svg" width="64" height="64" alt="">

# Verity

Named teammate bots that live in a Telegram forum group — one topic per bot,
a native Rust client, and a quiet bridge that does the work.

- **`verity-core`** — the library everything links: a thin Bot API client
  (rich messages, drafts, forum topics), a user-side MTProto client
  (`grammers`, reads full history bots can't), a SQLite brain, a `Harness`
  seam for agent providers, and the quiet-bot bridge loop.
- **`verity`** (CLI) — config, login, bot management, `serve`, and
  Telegram as *you* (`groups`, `rename`, `topics`, `history`, `say`).
- **`verity-gui`** — GPUI client, shelved for now (chat history needs
  no daemon; the CLI covers the read/write path meanwhile).

## Design

```
You ──► verity (grammers, logged in as YOU)
          reads "Verity" supergroup straight from Telegram cloud
          topics = per-bot threads · you type as yourself
                    ▲
        verity serve (Bot API, one bot token per teammate)
          bots watch their topic, work quietly, post rich replies
          SQLite holds brains (memory, config) — chats live in Telegram
```

Quiet bots by design: a turn is 🕒 while queued → 👀 → typing (one ⏳ progress
message on slow turns, deleted after) → ✅ → rich replies (32k chars,
GFM markdown, LaTeX, collapsible thinking blocks, auto-split at the
cap). Each turn sees the recent topic transcript — bots can't read
history, so `serve` lends them your MTProto session.

Replies can carry `:::` markers, stripped before send:

- `:::remember k = v:::` — persist a fact across turns
- `:::tell name: task:::` — hand work to a teammate; it replies in
  the topic as itself (bots can't see each other, so verity routes)
- `:::spawn label: task:::` — fan out up to 3 helpers; results fold
  back into the reply as collapsible blocks
- `:::bot_new name | token | instructions | provider:::` — register a
  brand-new teammate mid-run; the bridge validates the token and it
  creates its own topic seconds later
- `:::backup:::` — upload `~/verity` (config, session, sqlite brain)
  as documents in General

Bots registered while `serve` runs start automatically — the bridge
re-scans the roster every 10s.

## Backup

`~/verity` is the whole install — `config.toml`, `tg.session` (your
MTProto login), and `state.sqlite` (bot roster + memory). Back it up
into the group's General topic and pull it back on another machine:

```bash
verity backup      # upload via any registered bot (also: /backup in a topic)
verity restore     # download the newest set via your user session
```

While `serve` runs it also re-uploads every 6h; tune with
`VERITY_BACKUP_EVERY=<seconds>` (`0` disables). `state.sqlite` goes
through `VACUUM INTO` so the copy is consistent mid-write.

Note the backup carries `tg.session` (full account access) and the bot
tokens — it's only as private as the group.

## Setup

```bash
# api_id/api_hash from https://my.telegram.org, group_id = your forum supergroup
verity config --api-id <id> --api-hash <hash> --group-id <chat_id>
verity login --phone +15551234567
verity bot new scout --token <botfather-token> --instructions "You scout flights."
verity serve            # bots create their topics on first run
```

`--echo` on `serve` runs the echo harness — full plumbing, no provider.
`--no-context` skips attaching your session (turns get no transcript).

Each bot picks its provider at registration (`bot new --provider`):
`pi` (default; `--pi-bin` points at the binary), `echo`, or
`acp:<cmd> [args]` for any ACP-speaking agent — `acp:claude-code-acp`,
`acp:codex acp`, `acp:pi --acp`, `acp:devin acp --model swe-2-high`.

`bot new --greeting "<text>"` posts a greeting into the bot's topic
the first time it's created — that's how the Personal Helper Friend
introduces itself.

Talk to a bot from the terminal without opening a client:

```bash
verity say scout "find me a cheap flight to Lisbon next month"
```

## License

MIT
