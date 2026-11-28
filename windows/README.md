# Verity for Windows (WinUI 3)

`windows/Verity.WinUI` is the native Windows desktop shell. It exists
alongside `crates/verity-gui`, the cross-platform GPUI client; the GPUI
client is still available and is not replaced by this project.

The WinUI shell is deliberately thin. Telegram, configuration, session
state, and the bot roster stay in Rust in `crates/verity-winui-host`. The
C# process launches that host and communicates with it as newline-delimited
JSON over stdin/stdout.

## Build

Requirements:

- Windows 10 1809+ (Windows 11 recommended)
- Rust stable
- .NET 10 SDK
- Windows App SDK build tools, restored through NuGet
- optional but recommended: `winapp` CLI 0.8+

From the repository root:

```powershell
cargo build -p verity-winui-host
dotnet restore windows\Verity.WinUI\Verity.WinUI.csproj -p:Platform=x64 -r win-x64
dotnet build windows\Verity.WinUI\Verity.WinUI.csproj -p:Platform=x64 --no-restore
```

or run:

```powershell
powershell -ExecutionPolicy Bypass -File windows\build.ps1
```

The project copies `target\debug\verity-winui-host.exe` beside the WinUI
output when that file exists. To use another host binary, set:

```powershell
$env:VERITY_WINUI_HOST = "C:\path\to\verity-winui-host.exe"
```

## Run

With the `winapp` CLI installed:

```powershell
cd windows\Verity.WinUI
winapp run
```

`winapp run` registers a development package identity, so Windows Developer
Mode must be enabled for that command. Alternatively package and install the
app with `winapp package` and a signing certificate.

The first launch asks for Telegram API credentials when they are missing,
then phone login, the Telegram code, and the account's 2FA password when
required. Credentials and the MTProto session are the same local files used
by the other Verity clients under `~/verity`.

## Feature parity

Implemented now: onboarding/login, bot DM sidebar, history, sending,
reactions, periodic refresh, live update refresh, and basic rich text
formatting (bold, italic, underline, strikethrough, code, accents, links,
and concealed spoilers).
The GPUI client still has richer presentation: interactive spoiler reveal and
richer blockquote styling. WinUI renders LaTeX math as images too, though
inline formulas are laid out as separate blocks rather than inline in text.

## Protocol

The host protocol is additive JSON Lines. Examples:

```json
{"type":"configure","api_id":"123456","api_hash":"…"}
{"type":"login_phone","phone":"+15551234567"}
{"type":"select","user_id":123456789}
{"type":"send","text":"hello"}
{"type":"create_bot","name":"rey","username":"rey_helper_bot","instructions":"you are helpful","provider":"pi","model":null,"greeting":null}
{"type":"register_bot","name":"rey","token":"123456:ABC…","instructions":"","provider":"echo","model":null,"greeting":null}
{"type":"set_pi_bin","bin":"pi"}
{"type":"detect_harnesses"}
{"type":"install_harness","id":"pi"}
```

Events include `need_api_keys`, `need_phone`, `need_code`, `need_password`,
`logged_in`, `snapshot`, `messages`, `status`, `bot_ready`, and
`bot_setup_failed`. `stdout` is reserved for
protocol JSON; Rust diagnostics must stay on `stderr`.
