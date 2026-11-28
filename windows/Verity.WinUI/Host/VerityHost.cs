using System;
using System.Diagnostics;
using System.IO;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace Verity.WinUI.Host;

/// <summary>
/// Owns the verity-winui-host child process and its JSON-lines protocol.
/// Events are delivered as raw JSON; the window parses and dispatches them
/// on the UI thread.
/// </summary>
public sealed class VerityHost : IDisposable
{
    private static readonly System.Text.Encoding Utf8NoBom = new System.Text.UTF8Encoding(false);
    private readonly SemaphoreSlim _writeLock = new(1, 1);
    private Process? _process;
    private bool _disposed;

    public event Action<string>? EventReceived;
    public event Action<int>? Exited;

    public void Start()
    {
        if (_process is { HasExited: false })
        {
            return;
        }

        string path = ResolveHostPath();
        var start = new ProcessStartInfo
        {
            FileName = path,
            UseShellExecute = false,
            RedirectStandardInput = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            CreateNoWindow = true,
            // Encoding.UTF8 emits a BOM on the first write, which corrupts
            // the first JSON line the host parses. Use BOM-less UTF-8.
            StandardInputEncoding = Utf8NoBom,
            StandardOutputEncoding = Utf8NoBom,
            StandardErrorEncoding = Utf8NoBom,
        };
        start.Environment["RUST_BACKTRACE"] = "0";

        _process = Process.Start(start) ?? throw new InvalidOperationException("failed to start verity-winui-host");
        _process.EnableRaisingEvents = true;
        _process.Exited += (sender, _) =>
        {
            int exitCode = -1;
            if (sender is Process process)
            {
                try
                {
                    exitCode = process.ExitCode;
                }
                catch (InvalidOperationException)
                {
                    // Includes ObjectDisposedException during window shutdown.
                }
            }
            Exited?.Invoke(exitCode);
        };
        _ = Task.Run(ReadStdoutAsync);
        _ = Task.Run(ReadStderrAsync);
    }

    public async Task SendAsync(object command)
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        if (_process is not { HasExited: false })
        {
            throw new InvalidOperationException("verity-winui-host is not running");
        }

        string json = JsonSerializer.Serialize(command);
        await _writeLock.WaitAsync().ConfigureAwait(false);
        try
        {
            await _process.StandardInput.WriteLineAsync(json).ConfigureAwait(false);
            await _process.StandardInput.FlushAsync().ConfigureAwait(false);
        }
        finally
        {
            _writeLock.Release();
        }
    }

    private async Task ReadStdoutAsync()
    {
        Process? process = _process;
        if (process is null)
        {
            return;
        }
        try
        {
            while (await process.StandardOutput.ReadLineAsync().ConfigureAwait(false) is { } line)
            {
                if (!string.IsNullOrWhiteSpace(line))
                {
                    EventReceived?.Invoke(line);
                }
            }
        }
        catch (Exception ex) when (ex is IOException or InvalidOperationException or ObjectDisposedException)
        {
            // Normal during window shutdown.
        }
    }

    private async Task ReadStderrAsync()
    {
        Process? process = _process;
        if (process is null)
        {
            return;
        }
        try
        {
            // The protocol reserves stdout for JSON. Rust panics and diagnostics
            // arrive here; keep them in debug output rather than corrupting UI state.
            while (await process.StandardError.ReadLineAsync().ConfigureAwait(false) is { } line)
            {
                Debug.WriteLine("verity-winui-host: " + line);
            }
        }
        catch (Exception ex) when (ex is IOException or InvalidOperationException or ObjectDisposedException)
        {
            // Normal during window shutdown.
        }
    }

    private static string ResolveHostPath()
    {
        string? configured = Environment.GetEnvironmentVariable("VERITY_WINUI_HOST");
        if (!string.IsNullOrWhiteSpace(configured) && File.Exists(configured))
        {
            return Path.GetFullPath(configured);
        }

        string baseDir = AppContext.BaseDirectory;
        string besideApp = Path.Combine(baseDir, "verity-winui-host.exe");
        if (File.Exists(besideApp))
        {
            return besideApp;
        }

        // Development checkout fallback: walk upward from the build output
        // until a Rust workspace target directory is recognizable.
        var dir = new DirectoryInfo(baseDir);
        for (int i = 0; i < 10 && dir is not null; i++, dir = dir.Parent)
        {
            string candidate = Path.Combine(dir.FullName, "target", "debug", "verity-winui-host.exe");
            if (File.Exists(candidate))
            {
                return candidate;
            }
        }

        throw new FileNotFoundException(
            "verity-winui-host.exe was not found. Build it with `cargo build -p verity-winui-host` " +
            "or set VERITY_WINUI_HOST to its full path.");
    }

    public void Dispose()
    {
        _disposed = true;
        try
        {
            if (_process is { HasExited: false })
            {
                _process.Kill(entireProcessTree: true);
            }
        }
        catch
        {
            // The window is closing; best-effort cleanup is enough.
        }
        _process?.Dispose();
        _writeLock.Dispose();
    }
}
