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
  the supergroup directly. **No daemon needed to read chats.**
- **The bridge is a component, not a service.** `verity serve` is just
  `verity-core` running the bot loops in-process. Same lib inside the GUI
  later — no wire protocol until remote control is actually wanted.
- **SQLite is the brain, not the log.** Bot records, memory, config only.
  Conversation content lives in Telegram.
- **Quiet bots.** A turn: 👀 reaction → typing → work → one rich reply.
  No tool-call telemetry into the group.

## Hard constraints discovered

- Bot API has no history read → user-side MTProto is the read path.
- Bots can't see other bots' messages → bot-to-bot comms must route
  through verity-core (planned `verity` MCP server injected into each
  agent session: `message_bot`, `spawn_helper`, `hand_off`, `list_bots`).
- Draft streaming (`sendMessageDraft`) is private-chat-only; whether
  `sendRichMessageDraft` works in forum supergroups is unverified.
  Group path: single edited progress message → final rich message.
- Telegram Serverless (tgcloud) can't host agents: V8 JS isolate, no
  filesystem/subprocess. Could host a Mini App later.

## Message formats (Bot API 10.x)

`sendRichMessage`: up to 32,768 chars, GFM markdown or block input,
native LaTeX (`mathematical_expression`), tables, collapsible
`details`/`thinking` blocks, inline `RichBlockButtons` (permission
prompts). `sendRichMessageDraft` streams it; `can_stop` renders a
stop button and emits `MessageGenerationStopped`.

## Harness

`Harness` trait, one code path:
- `PiHarness` — drives `pi`/`pirs` binary per turn (works today, uses
  pi's own auth.json: custom API keys or subscription OAuth).
- Future: embed `pi-agent` crate (lassejlv/pi-rs) as default zero-install
  provider; ACP for external CLIs (claude/codex/opencode/devin) —
  pi speaks ACP too, so one protocol covers all.
- `EchoHarness` — end-to-end plumbing without a provider.

## Sub-agents

Owned at the verity layer (pi skips them by design): verity-core spawns
child `Agent`s with their own context and folds results back — Akeru's
"helpers, up to three" model.
