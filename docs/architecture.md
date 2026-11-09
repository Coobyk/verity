# Verity architecture

Decisions captured from the design sessions. Update as the code moves.

## The idea

Akeru-style named teammate bots, but native (Rust + GPUI, no Electron) and
Telegram-first: a forum supergroup ("Verity HQ") is the shared surface —
one topic per bot, bots are real bot accounts with their own avatars.

## Layers

- **Telegram is the chat log.** Humans can read/write everything from any
  official Telegram client on any device, for free, forever-synced.
- **The GUI reads as the user.** Bots can't fetch history (Bot API: no
  `getChatHistory`, uncollected updates die after 24h), but user accounts
  can — so `verity-gui`/`verity` log in via MTProto (`grammers`) and read
  the supergroup directly. **No daemon needed to read chats.** The GUI
  and `verity say` also *write* as the user: posting into a topic is a
  normal message with `reply_to = topic_id`.
- **The bridge is a component, not a service.** `verity serve` is just
  `verity-core` running the bot loops in-process. Same lib inside the GUI
  later — no wire protocol until remote control is actually wanted.
- **The bridge borrows the user's eyes.** `serve` attaches the MTProto
  session so each turn carries the last ~30 topic messages; without it
  (`--no-context`, not logged in) turns degrade to context-free prompts.
- **SQLite is the brain, not the log.** Bot records, memory, config only.
  Conversation content lives in Telegram. Memory writes ride along in
  the reply stream: `:::remember k = v:::` lines are persisted to the
  `memory` table and stripped before send.
- **Quiet bots.** A turn: 👀 reaction → typing (kept alive every 4s) →
  work → rich replies split at the 32k cap, plain-text fallback split at
  4k. Offline backlog is acked-and-skipped on start; a crashed bot loop
  restarts after 10s.

## Hard constraints discovered

- Bot API has no history read → user-side MTProto is the read path.
- Bot-API `-100…` supergroup ids ≠ grammers `bare_id` — `chat_id_to_bare`
  normalizes before peer lookup so one `group_id` works for both paths.
- Bots can't see other bots' messages → bot-to-bot comms route through
  verity-core: `:::tell name: msg:::` runs a full turn for the teammate
  (its own instructions/memory/harness) and posts its reply in the
  calling topic as itself. Depth-capped at 2 so chains stay finite.
  A proper `verity` MCP server (`message_bot`, `spawn_helper`,
  `hand_off`, `list_bots`) remains the upgrade path once agents get
  real tool surfaces.
- Draft streaming (`sendMessageDraft`) is private-chat-only; whether
  `sendRichMessageDraft` works in forum supergroups is unverified.
  Group path implemented: turns >12s keep one "⏳ working…" message
  edited with elapsed time, deleted when the reply lands. ✅ reaction
  on success.
- Telegram Serverless (tgcloud) can't host agents: V8 JS isolate, no
  filesystem/subprocess. Could host a Mini App later.

## Message formats (Bot API 10.x)

`sendRichMessage`: up to 32,768 chars, GFM markdown or block input,
native LaTeX (`mathematical_expression`), tables, collapsible
`details`/`thinking` blocks, inline `RichBlockButtons` (permission
prompts). `sendRichMessageDraft` streams it; `can_stop` renders a
stop button and emits `MessageGenerationStopped`.

## Harness

`Harness` trait, one code path; each bot picks via its `provider` string:
- `pi` → `PiHarness` — drives `pi`/`pirs` binary per turn (works today,
  uses pi's own auth.json: custom API keys or subscription OAuth).
- `acp:<cmd> [args]` → `AcpHarness` — any ACP-speaking CLI
  (claude-code-acp, codex acp, `pi --acp`, …) over NDJSON stdio:
  `initialize` → `session/new` → `session/prompt`, accumulating
  `agent_message_chunk`s. Permission requests auto-approve the first
  `allow_*` option; other client-bound requests get method-not-found.
- `echo` → `EchoHarness` — end-to-end plumbing without a provider.
- Future: embed `pi-agent` crate (lassejlv/pi-rs) as default zero-install
  provider.

## Sub-agents

Owned at the verity layer (pi skips them by design): `:::spawn label:
task:::` markers fan out up to three helper turns on the same harness
with a minimal context; results fold into the reply as collapsible
`<details>` blocks. Helper output is raw — no marker recursion.

## Markers

Reply lines of the form `:::verb …:::` are stripped before send and
acted on by the bridge: `remember` (persist k=v), `tell` (route a turn
to a teammate), `spawn` (helper fan-out). Malformed markers never leak
to Telegram.
