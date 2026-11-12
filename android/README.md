# Verity for Android

A native Android client for Verity — the named teammate bots that live in a
Telegram forum supergroup. The app talks **only** to the small HTTP JSON API
exposed by `verity serve` on your desktop; it never touches Telegram directly.

Grok-style UI: pure black, hairline dividers, topic + DM list, chat with
right-aligned pill bubbles for your own messages and a rounded composer.

## Prerequisites

- **JDK 17** (Gradle and AGP requirement)
- **Android SDK 34** — via Android Studio or `sdkmanager`
  (`platforms;android-34` + `build-tools`)
- The device must be on the same LAN as the machine running `verity serve`
  (plain HTTP is used — `usesCleartextTraffic` is enabled)

## Pointing the app at the bridge

`verity serve` exposes the API bound by the `api_bind` field in `config.toml`
(e.g. `api_bind = "0.0.0.0:8787"` — bind to a LAN-reachable address, not
localhost, or the phone can't reach it). If `api_token` is set there, the same
token is required by the app.

In the app: open the gear icon → **Base URL** (e.g. `http://192.168.1.10:8787`,
your desktop's LAN IP) and **API token** → Save. Both are persisted in
DataStore; the default is `http://192.168.1.100:8787` with no token.

Endpoints used:

| Endpoint | Purpose |
| --- | --- |
| `GET /api/health` | liveness (no auth) |
| `GET /api/topics` | `me` id + forum topic list |
| `GET /api/topics/{id}/messages?limit=50` | topic history |
| `GET /api/dms` | per-bot DM list (`available` greys a row) |
| `GET /api/dms/{user_id}/messages?limit=50` | DM history |
| `POST /api/messages` | `{"topic": id}` xor `{"dm": user_id}` + `"text"` |

All requests except `/api/health` send `Authorization: Bearer <token>`.

## Build

```bash
cd android

# generate the wrapper once (needs any installed Gradle 8.7+)
gradle wrapper

./gradlew assembleDebug
```

The APK lands at `app/build/outputs/apk/debug/app-debug.apk`.

## Install

```bash
adb install app/build/outputs/apk/debug/app-debug.apk
# or copy the APK to the phone and open it
```

## Behavior notes

- Open conversation polls every ~3 s; topic/DM lists every ~30 s and on resume.
  All polling pauses while the app is backgrounded.
- Sent messages appear instantly (optimistic) and are confirmed by a refetch.
- If the bridge is unreachable the app shows "Offline" and a transient status
  message — it never crashes on network failure.
