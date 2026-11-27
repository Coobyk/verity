# Verity for Android

Native Android client for Verity's Telegram teammate bots. The app logs in
directly with TDLib and talks to the bots in their Telegram DMs — it does
not need `verity serve`, the Rust JSON API, or a bridge-hosted HTTP proxy.

The bridge and the Android app are independent views of the same Telegram
account/bot DMs. The Rust `api.rs` server remains available for companion
tooling, but this app does not use it.

UI direction: Grok-style black surfaces, hairline dividers, bot/DM list,
right-aligned outgoing bubbles, and a rounded composer.

## Prerequisites

- JDK 17
- Android SDK 35 (`platforms;android-35` plus build tools)
- Telegram `api_id` and `api_hash` from <https://my.telegram.org> →
  *API development tools*

The repository already vendors the TDLib Java client and native libraries
used by the app:

```text
app/src/main/java/org/drinkless/tdlib/Client.java
app/src/main/java/org/drinkless/tdlib/TdApi.java
app/src/main/jniLibs/arm64-v8a/libtdjni.so
app/src/main/jniLibs/arm64-v8a/libverity_math.so
```

Only `arm64-v8a` native libraries are currently included. Physical arm64
phones are supported; x86/x86_64 emulators need matching TDLib binaries
copied into `app/src/main/jniLibs/<abi>/` before the app can run there.

## API credentials

Credentials are baked into `BuildConfig` at build time. Do not commit real
values. Put them in either `android/local.properties` or
`~/.gradle/gradle.properties`:

```properties
tdlib.apiId=123456
tdlib.apiHash=0123456789abcdef0123456789abcdef
```

`local.properties` is gitignored. `app/build.gradle.kts` reads it first and
then falls back to Gradle properties. The Settings screen shows the baked
API ID so you can verify that the values were picked up.

## Build

Linux/macOS:

```bash
cd android
./gradlew assembleDebug
```

Windows:

```powershell
cd android
.\gradlew.bat assembleDebug
```

The debug APK is written to
`app/build/outputs/apk/debug/app-debug.apk`.

## Login and session

First launch follows TDLib's authorization state machine:

```text
phone number → code → optional two-step password → ready
```

The session is stored in TDLib's database under `filesDir/td` and survives
process restarts. Logout uses `TdApi.LogOut`, wipes that session, and
creates a fresh client.

The TDLib database encryption key is currently empty, matching TDLib's
usual unencrypted local-database mode. Android cloud/device backup is
disabled in the manifest so those session/database files are not exported
through Android Auto Backup. Treat the device itself as the session
boundary.

## Runtime structure

- `data/TdRepository.kt` owns the single TDLib `Client`, the authorization
  state machine, chat/user caches, and update subscriptions. The app is
  update-driven rather than polling.
- Direct teammate chats are private chats whose peer user is a bot
  (`ChatTypePrivate` + `UserTypeBot`).
- Forum-topic support still exists in the UI/data layer as compatibility
  surface, but the Verity backend is now DM-first.
- DM history uses `GetChatHistory`; forum history uses
  `GetMessageThreadHistory` when that compatibility surface is used.
- `UpdateConnectionState` drives the connectivity indicator.
- Sends are optimistic: a temporary negative-id row is replaced by the
  `SendMessage` result, with deduplication if `UpdateNewMessage` arrives
  first.
- Unknown message content renders as a `[type]` placeholder instead of
  crashing.

`TdRepository.kt` marks TDLib-version-sensitive call sites with
`// INTEGRATION:` comments (`SetTdlibParameters`, `GetForumTopics`,
`GetMessageThreadHistory`, `SendMessage`, and username access).

## Updating TDLib

If TDLib needs an upgrade, replace all of these together so the generated
Java API and native ABI match:

1. `org/drinkless/tdlib/Client.java`
2. `org/drinkless/tdlib/TdApi.java`
3. `jniLibs/<abi>/libtdjni.so` for every supported ABI

Then review the `// INTEGRATION:` call sites. Do not update only the `.so`
or only `TdApi.java`; they are generated/built as a pair.
