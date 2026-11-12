# Verity for Android

A native Android client for Verity — the named teammate bots that live in a
Telegram forum supergroup. The app logs into Telegram **directly via TDLib**
and chats with the bots itself — no HTTP bridge, no `verity serve` needed.

Grok-style UI: pure black, hairline dividers, topic + DM list, chat with
right-aligned pill bubbles for your own messages and a rounded composer.

## Prerequisites

- **JDK 17** (Gradle and AGP requirement)
- **Android SDK 34** — via Android Studio or `sdkmanager`
  (`platforms;android-34` + `build-tools`)
- A **Telegram API credential pair** (`api_id` + `api_hash`) — create one at
  <https://my.telegram.org> → *API development tools*. Any app name works.
- **TDLib sources + native library** (see below — there is no trustworthy
  maven artifact, so TDLib is vendored).

## TDLib: what to vendor

The app compiles against `org.drinkless.tdlib.Client` and the generated
`org.drinkless.td.libcore.telegram.TdApi`. Get them from a build of the
[`td`](https://github.com/tdlib/td) repository (the `example/android` project
shows the standard layout):

```bash
git clone https://github.com/tdlib/td.git
cd td
# generates Java sources incl. org/drinkless/tdlib/Client.java and
# org/drinkless/td/libcore/telegram/TdApi.java, and cross-builds libtdjni.so
# per ABI — see td/example/android/README.md for the cmake/NDBK steps
```

Then drop into this project:

```
android/app/src/main/java/org/drinkless/tdlib/Client.java
android/app/src/main/java/org/drinkless/td/libcore/telegram/TdApi.java
android/app/src/main/jniLibs/arm64-v8a/libtdjni.so        # + other ABIs as needed
```

`TdRepository.kt` calls `System.loadLibrary("tdjni")` at start and reports a
login-screen error if the .so is absent instead of crashing.

## API credentials

Baked into `BuildConfig` at build time. **Do not commit them** — this repo's
`android/gradle.properties` is tracked and only carries commented placeholders.
Set the real values in either of (both are gitignored):

```properties
# android/local.properties          (alongside sdk.dir)
tdlib.apiId=123456
tdlib.apiHash=0123456789abcdef0123456789abcdef
```

```properties
# or ~/.gradle/gradle.properties
tdlib.apiId=123456
tdlib.apiHash=0123456789abcdef0123456789abcdef
```

`app/build.gradle.kts` reads `local.properties` first, then Gradle properties,
and injects `BuildConfig.TDLIB_API_ID` / `BuildConfig.TDLIB_API_HASH`.
The Settings screen shows the baked API ID so you can verify it took.

## Build

```bash
cd android
gradle wrapper          # once, needs any installed Gradle 8.7+
./gradlew assembleDebug
```

The APK lands at `app/build/outputs/apk/debug/app-debug.apk`.

## Login & session

First launch walks you through TDLib's auth state machine:

```
phone number → SMS/Telegram code → (optional) two-step password → Ready
```

The session lives in TDLib's own database at `filesDir/td` (message DB, chat
info DB, file DB all enabled), so it survives process death — after the first
login the app goes straight to `Ready`. Log out from the gear icon;
`TdApi.LogOut` wipes the session and the client is recreated for a fresh
login.

## How it works

- `data/TdRepository.kt` owns the single `Client`, the auth state machine and
  caches of chats/users (`UpdateNewChat`, `UpdateUser`). Everything else is
  update-driven — **no polling**.
- **Direct** section = private chats whose user is a bot
  (`ChatTypePrivate` + `UserTypeBot`). For a private chat, `chat.id` equals
  `user.id`.
- **Topics** section = `GetForumTopics` on every non-channel supergroup in the
  main chat list; rows are `ForumTopicInfo.messageThreadId` + `name`, mapped
  back to their forum's `chatId`.
- History: `GetChatHistory` for DMs, `GetMessageThreadHistory` for forum
  topics. Incoming `UpdateNewMessage` events append to the open conversation.
- `UpdateConnectionState` drives the "Connected"/"Offline" header.
- Optimistic send uses negative ids; `SendMessage`'s returned `Message`
  replaces the optimistic row (deduped if `UpdateNewMessage` arrives first).

## Behavior notes

- Errors from TDLib (`TdApi.Error`) surface as snackbar/status text — the app
  never crashes on network failure; TDLib retries internally.
- Unknown content (photos, stickers, polls…) renders as a `[type]` placeholder.
- If the vendored `TdApi` version drifts, the only signature-sensitive spots
  are marked `// INTEGRATION:` in `TdRepository.kt` (`SetTdlibParameters`,
  `GetForumTopics`, `GetMessageThreadHistory`, `SendMessage`, `User.usernames`).
