<img src="assets/icon.svg" width="64" height="64" alt="">

# Verity

Named teammate bots that live in a Telegram forum group — one topic per bot,
a native Rust client, and a quiet bridge that does the work.

- **`verity-core`** — the library everything links: a thin Bot API client
  (rich messages, drafts, forum topics), a user-side MTProto client
  (`grammers`, reads full history bots can't), a SQLite brain, a `Harness`
  seam for agent providers, and the quiet-bot bridge loop.
- **`verity`** (CLI) — config, login, bot management, `serve`, and
  Telegram reads (`topics`, `history`) for humans.
- **`verity-gui`** — GPUI client that reads the supergroup straight from
  Telegram as *you*. Chat history needs no daemon.

## Design

```
You ──► verity-gui / verity (grammers, logged in as YOU)
          reads "Verity HQ" supergroup straight from Telegram cloud
          topics = per-bot threads · you type as yourself
                    ▲
        verity serve (Bot API, one bot token per teammate)
          bots watch their topic, work quietly, post one rich reply
          SQLite holds brains (memory, config) — chats live in Telegram
```

Quiet bots by design: a turn is 👀 reaction → typing → work → a single
rich message (32k chars, GFM markdown, LaTeX, collapsible thinking blocks).

## Setup

```bash
# api_id/api_hash from https://my.telegram.org, group_id = your forum supergroup
verity config --api-id <id> --api-hash <hash> --group-id <chat_id>
verity login --phone +15551234567
verity bot new scout --token <botfather-token> --instructions "You scout flights."
verity serve            # bots create their topics on first run
```

`--echo` on `serve` runs the echo harness — full plumbing, no provider.
Default harness drives `pi`/`pirs` (`--pi-bin` to point at the binary).

## License

MIT
