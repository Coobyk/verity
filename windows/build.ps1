$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Push-Location $Root
try {
    cargo build -p verity-winui-host
    dotnet restore windows\Verity.WinUI\Verity.WinUI.csproj -p:Platform=x64 -r win-x64
    dotnet build windows\Verity.WinUI\Verity.WinUI.csproj -p:Platform=x64 --no-restore
} finally {
    Pop-Location
}
