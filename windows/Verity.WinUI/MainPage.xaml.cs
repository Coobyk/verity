using System.IO;
using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.Linq;
using System.Text.Json;
using System.Runtime.InteropServices.WindowsRuntime;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media.Imaging;
using Verity.WinUI.Host;
using Verity.WinUI.Models;
using Windows.System;
using Windows.Storage.Streams;

namespace Verity.WinUI;

public sealed partial class MainPage : Page
{
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNameCaseInsensitive = false,
    };

    private readonly VerityHost _host = new();
    private readonly ObservableCollection<DmRow> _bots = new();
    private readonly ObservableCollection<ChatMessageViewModel> _messages = new();
    private readonly DispatcherTimer _pollTimer = new() { Interval = TimeSpan.FromSeconds(3) };

    private OnboardingStep _onboardingStep = OnboardingStep.Busy;
    private string? _apiId;
    private long? _me;
    private long? _selectedUserId;
    private bool _updatingBots;
    private bool _loggedIn;
    private bool _snapshotReceived;
    private bool _hasBots;
    private bool _setupSkipped;
    private bool _setupBusy;
    private bool _installBusy;
    private readonly Dictionary<string, HarnessItem> _harnesses = new();

    private sealed class HarnessItem
    {
        public string Id { get; set; } = "";
        public string Bin { get; set; } = "";
        public bool Installed { get; set; }
        public bool Installable { get; set; }
        public string Note { get; set; } = "";
    }

    private enum OnboardingStep
    {
        ApiId,
        ApiHash,
        Phone,
        Code,
        Password,
        Busy,
        Failed,
    }

    public MainPage()
    {
        InitializeComponent();
        BotsList.ItemsSource = _bots;
        MessagesList.ItemsSource = _messages;
        ProviderBox.SelectedIndex = 0;
        UpdateHarnessFieldVisibility();

        _host.EventReceived += line => DispatcherQueue.TryEnqueue(() => HandleHostEvent(line));
        _host.Exited += code => DispatcherQueue.TryEnqueue(() => HostExited(code));
        _pollTimer.Tick += (_, _) =>
        {
            if (_loggedIn)
            {
                SendCommand(new { type = "poll" });
            }
        };

        ShowBusy("Starting the Verity desktop bridge…");
        try
        {
            _host.Start();
        }
        catch (Exception ex)
        {
            ShowFailure("The Verity desktop bridge is missing", ex.Message);
        }

    }

    public void Cleanup()
    {
        _pollTimer.Stop();
        _host.Dispose();
    }

    private async void SendCommand(object command)
    {
        try
        {
            await _host.SendAsync(command).ConfigureAwait(false);
        }
        catch (Exception ex)
        {
            DispatcherQueue.TryEnqueue(() => SetStatus(ex.Message));
        }
    }

    private void HandleHostEvent(string line)
    {
        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(line);
        }
        catch
        {
            SetStatus("the desktop bridge sent malformed JSON");
            return;
        }

        using (document)
        {
            JsonElement root = document.RootElement;
            if (!root.TryGetProperty("type", out JsonElement typeElement))
            {
                return;
            }

            switch (typeElement.GetString())
            {
                case "status":
                    SetStatus(root.GetProperty("message").GetString() ?? "");
                    break;
                case "need_api_keys":
                    if (_onboardingStep is not OnboardingStep.ApiId and not OnboardingStep.ApiHash)
                    {
                        ShowOnboarding(
                            OnboardingStep.ApiId,
                            "Connect Verity to Telegram",
                            "Paste the API ID from my.telegram.org → API development tools.",
                            secret: false,
                            placeholder: "API ID");
                    }
                    break;
                case "need_phone":
                    ShowOnboarding(
                        OnboardingStep.Phone,
                        "Log in to Telegram",
                        "Enter your phone number in international format, including the leading +.",
                        secret: false,
                        placeholder: "+15551234567");
                    break;
                case "need_code":
                    string delivery = root.GetProperty("delivery").GetString() ?? "Telegram";
                    string? next = root.TryGetProperty("next_delivery", out JsonElement nextElement)
                        ? nextElement.GetString()
                        : null;
                    string description = next is null
                        ? $"Telegram sent the login code via {delivery}."
                        : $"Telegram sent the login code via {delivery}. Type 'resend' to send it again via {next}.";
                    ShowOnboarding(OnboardingStep.Code, "Check Telegram", description, false, "Login code");
                    break;
                case "need_password":
                    ShowOnboarding(
                        OnboardingStep.Password,
                        "Two-factor authentication",
                        "Enter your Telegram 2FA password. It is used only for this login.",
                        secret: true,
                        placeholder: "2FA password");
                    break;
                case "logged_in":
                    EnterChatMode();
                    break;
                case "snapshot":
                    HandleSnapshot(root);
                    break;
                case "messages":
                    HandleMessages(root);
                    break;
                case "bot_ready":
                    _setupBusy = false;
                    SetupCreate.IsEnabled = true;
                    break;
                case "bot_setup_failed":
                    _setupBusy = false;
                    SetupCreate.IsEnabled = true;
                    SetupStatus.Text = "setup failed — see the status line, adjust and retry";
                    break;
                case "harnesses":
                    HandleHarnesses(root);
                    break;
            }
        }
    }

    private void HandleSnapshot(JsonElement root)
    {
        _me = NullableInt64(root, "me");
        _selectedUserId = NullableInt64(root, "selected_user_id");

        List<DmRow> rows = JsonSerializer.Deserialize<List<DmRow>>(root.GetProperty("dms").GetRawText(), JsonOptions) ?? new();
        _snapshotReceived = true;
        _hasBots = rows.Count > 0;
        UpdateSetupVisibility();
        _updatingBots = true;
        try
        {
            _bots.Clear();
            foreach (DmRow row in rows)
            {
                _bots.Add(row);
            }
            DmRow? selected = rows.FirstOrDefault(r => r.UserId == _selectedUserId);
            BotsList.SelectedItem = selected;
            ChatTitle.Text = selected?.Label ?? "Verity";
        }
        finally
        {
            _updatingBots = false;
        }

        ReplaceMessages(root.GetProperty("messages"));
    }

    private void HandleMessages(JsonElement root)
    {
        long userId = root.GetProperty("user_id").GetInt64();
        if (_selectedUserId != userId)
        {
            return;
        }
        ReplaceMessages(root.GetProperty("messages"));
    }

    private void ReplaceMessages(JsonElement element)
    {
        List<TopicMessage> messages = JsonSerializer.Deserialize<List<TopicMessage>>(element.GetRawText(), JsonOptions) ?? new();
        _messages.Clear();
        foreach (TopicMessage message in messages)
        {
            var viewModel = new ChatMessageViewModel
            {
                Id = message.Id,
                Sender = string.IsNullOrWhiteSpace(message.Sender) ? "unknown" : message.Sender,
                Text = message.Text,
                Time = DateTimeOffset.FromUnixTimeSeconds(message.Date).UtcDateTime.ToString("HH:mm"),
                IsMine = _me.HasValue && message.SenderId == _me.Value,
                Message = message,
            };
            foreach (ReactionInfo reaction in message.Reactions)
            {
                viewModel.Reactions.Add(new ReactionViewModel
                {
                    MessageId = message.Id,
                    Emoji = reaction.Emoji,
                    Chosen = reaction.Chosen,
                    Count = reaction.Count,
                });
            }
            foreach (MathImage math in message.Math)
            {
                if (!string.IsNullOrWhiteSpace(math.Path))
                {
                    var image = new MathImageViewModel { Display = math.Display };
                    viewModel.MathImages.Add(image);
                    _ = LoadMathImageAsync(image, math.Path);
                }
            }
            _messages.Add(viewModel);
        }

        if (_messages.Count > 0)
        {
            MessagesList.ScrollIntoView(_messages[^1]);
        }
    }

    private static async Task LoadMathImageAsync(MathImageViewModel image, string path)
    {
        try
        {
            byte[] bytes = await File.ReadAllBytesAsync(path);
            using InMemoryRandomAccessStream stream = new();
            await stream.WriteAsync(bytes.AsBuffer());
            stream.Seek(0);
            var bitmap = new BitmapImage();
            await bitmap.SetSourceAsync(stream);
            image.Source = bitmap;
        }
        catch
        {
            // A missing or corrupt cache image should not drop the message.
        }
    }

    private static long? NullableInt64(JsonElement root, string property)
    {
        if (!root.TryGetProperty(property, out JsonElement element) || element.ValueKind == JsonValueKind.Null)
        {
            return null;
        }
        return element.GetInt64();
    }

    private void EnterChatMode()
    {
        _loggedIn = true;
        OnboardingPanel.Visibility = Visibility.Collapsed;
        ChatRoot.Visibility = Visibility.Visible;
        UpdateSetupVisibility();
        _pollTimer.Start();
    }

    private void UpdateSetupVisibility()
    {
        bool show = _loggedIn && _snapshotReceived && !_hasBots && !_setupSkipped;
        bool appearing = show && SetupPanel.Visibility != Visibility.Visible;
        SetupPanel.Visibility = show ? Visibility.Visible : Visibility.Collapsed;
        if (appearing)
        {
            ShowSetupStep(StepHarness);
            SendCommand(new { type = "detect_harnesses" });
        }
    }

    private void UpdateHarnessFieldVisibility()
    {
        string tag = SelectedHarnessTag();
        PiBinBox.Visibility = tag == "pi" ? Visibility.Visible : Visibility.Collapsed;
        AcpSpecBox.Visibility = tag == "acp-custom" ? Visibility.Visible : Visibility.Collapsed;
        UpdateHarnessDetectRow();
    }

    private string SelectedHarnessTag() =>
        (ProviderBox.SelectedItem as ComboBoxItem)?.Tag as string ?? "pi";

    private void HandleHarnesses(JsonElement root)
    {
        _installBusy = false;
        _harnesses.Clear();
        foreach (JsonElement item in root.GetProperty("items").EnumerateArray())
        {
            var info = new HarnessItem
            {
                Id = item.GetProperty("id").GetString() ?? "",
                Bin = item.GetProperty("bin").GetString() ?? "",
                Installed = item.GetProperty("installed").GetBoolean(),
                Installable = item.GetProperty("installable").GetBoolean(),
                Note = item.GetProperty("note").GetString() ?? "",
            };
            _harnesses[info.Id] = info;
        }
        UpdateHarnessDetectRow();
    }

    private void UpdateHarnessDetectRow()
    {
        string tag = SelectedHarnessTag();
        if (tag == "acp-custom")
        {
            HarnessDetectStatus.Text = "enter any ACP-speaking command below — detection doesn't apply";
            HarnessInstall.Visibility = Visibility.Collapsed;
            return;
        }
        if (!_harnesses.TryGetValue(tag, out HarnessItem? info))
        {
            HarnessDetectStatus.Text = _installBusy ? "installing…" : "detecting…";
            HarnessInstall.Visibility = Visibility.Collapsed;
            return;
        }
        if (info.Installed)
        {
            HarnessDetectStatus.Text = info.Id == "echo"
                ? "✓ built-in — always available"
                : $"✓ detected: {info.Bin}";
            HarnessInstall.Visibility = Visibility.Collapsed;
        }
        else
        {
            HarnessDetectStatus.Text = _installBusy
                ? $"installing {info.Id} — watch the status line…"
                : $"not detected — install with: {info.Note}";
            HarnessInstall.Visibility = info.Installable && !_installBusy
                ? Visibility.Visible
                : Visibility.Collapsed;
            HarnessInstall.IsEnabled = true;
        }
    }

    private void HarnessInstall_Click(object sender, RoutedEventArgs e)
    {
        if (_installBusy)
        {
            return;
        }
        string tag = SelectedHarnessTag();
        if (!_harnesses.TryGetValue(tag, out HarnessItem? info) || !info.Installable)
        {
            return;
        }
        _installBusy = true;
        HarnessInstall.IsEnabled = false;
        HarnessInstall.Visibility = Visibility.Collapsed;
        HarnessDetectStatus.Text = $"installing {info.Id} — this can take a few minutes…";
        SendCommand(new { type = "install_harness", id = info.Id });
    }

    private void ProviderBox_SelectionChanged(object sender, SelectionChangedEventArgs e) =>
        UpdateHarnessFieldVisibility();

    private bool _createMode = true;

    private void ShowSetupStep(FrameworkElement step)
    {
        StepHarness.Visibility = Visibility.Collapsed;
        StepSource.Visibility = Visibility.Collapsed;
        StepDetails.Visibility = Visibility.Collapsed;
        step.Visibility = Visibility.Visible;
    }

    private void HarnessNext_Click(object sender, RoutedEventArgs e) => ShowSetupStep(StepSource);

    private void BackToHarness_Click(object sender, RoutedEventArgs e) => ShowSetupStep(StepHarness);

    private void BackToSource_Click(object sender, RoutedEventArgs e) => ShowSetupStep(StepSource);

    private void SourceCreate_Click(object sender, RoutedEventArgs e) => EnterDetailsStep(true);

    private void SourceRegister_Click(object sender, RoutedEventArgs e) => EnterDetailsStep(false);

    private void EnterDetailsStep(bool create)
    {
        _createMode = create;
        StepDetailsTitle.Text = create ? "Create your bot" : "Register your bot";
        BotUsernameBox.Visibility = create ? Visibility.Visible : Visibility.Collapsed;
        BotTokenBox.Visibility = create ? Visibility.Collapsed : Visibility.Visible;
        SetupCreate.Content = create ? "Create bot" : "Register bot";
        SetupStatus.Text = "";
        ShowSetupStep(StepDetails);
    }

    private void OptionalToggle_Click(object sender, RoutedEventArgs e)
    {
        OptionalToggle.Visibility = Visibility.Collapsed;
        OptionalFields.Visibility = Visibility.Visible;
    }

    private void SetupSkip_Click(object sender, RoutedEventArgs e)
    {
        _setupSkipped = true;
        UpdateSetupVisibility();
    }

    private void SetupCreate_Click(object sender, RoutedEventArgs e)
    {
        if (_setupBusy)
        {
            return;
        }

        string name = BotNameBox.Text.Trim();
        if (name.Length == 0)
        {
            SetupStatus.Text = "bot name is required";
            return;
        }

        string tag = SelectedHarnessTag();
        string provider;
        switch (tag)
        {
            case "echo":
                provider = "echo";
                break;
            case "devin":
                provider = "acp:devin acp";
                break;
            case "claude-code-acp":
                provider = "acp:claude-code-acp";
                break;
            case "codex":
                provider = "acp:codex acp";
                break;
            case "acp-custom":
                string spec = AcpSpecBox.Text.Trim();
                if (spec.Length == 0)
                {
                    SetupStatus.Text = "the ACP command is required";
                    return;
                }
                provider = "acp:" + spec;
                break;
            default:
                provider = "pi";
                if (!string.IsNullOrWhiteSpace(PiBinBox.Text))
                {
                    SendCommand(new { type = "set_pi_bin", bin = PiBinBox.Text.Trim() });
                }
                break;
        }

        string? model = string.IsNullOrWhiteSpace(ModelBox.Text) ? null : ModelBox.Text.Trim();
        string? greeting = string.IsNullOrWhiteSpace(GreetingBox.Text) ? null : GreetingBox.Text.Trim();
        string instructions = InstructionsBox.Text.Trim();
        bool create = _createMode;

        object command;
        if (create)
        {
            string username = BotUsernameBox.Text.Trim();
            if (!username.EndsWith("bot", StringComparison.OrdinalIgnoreCase))
            {
                SetupStatus.Text = "bot usernames must end in 'bot'";
                return;
            }
            command = new
            {
                type = "create_bot",
                name,
                username,
                instructions,
                provider,
                model,
                greeting,
            };
        }
        else
        {
            string token = BotTokenBox.Text.Trim();
            if (token.Length == 0)
            {
                SetupStatus.Text = "token is required";
                return;
            }
            command = new
            {
                type = "register_bot",
                name,
                token,
                instructions,
                provider,
                model,
                greeting,
            };
        }

        _setupBusy = true;
        SetupCreate.IsEnabled = false;
        SetupStatus.Text = create
            ? "asking @BotFather — this can take up to a minute…"
            : "registering…";
        SendCommand(command);
    }

    private void ShowBusy(string message)
    {
        _onboardingStep = OnboardingStep.Busy;
        OnboardingPanel.Visibility = Visibility.Visible;
        ChatRoot.Visibility = Visibility.Collapsed;
        OnboardingTitle.Text = "Working";
        OnboardingDescription.Text = message;
        OnboardingInput.Visibility = Visibility.Collapsed;
        OnboardingPassword.Visibility = Visibility.Collapsed;
        OnboardingContinue.IsEnabled = false;
        OnboardingStatus.Text = message;
    }

    private void ShowFailure(string title, string message)
    {
        _onboardingStep = OnboardingStep.Failed;
        OnboardingPanel.Visibility = Visibility.Visible;
        ChatRoot.Visibility = Visibility.Collapsed;
        OnboardingTitle.Text = title;
        OnboardingDescription.Text = message;
        OnboardingInput.Visibility = Visibility.Collapsed;
        OnboardingPassword.Visibility = Visibility.Collapsed;
        OnboardingContinue.IsEnabled = false;
        OnboardingStatus.Text = message;
    }

    private void ShowOnboarding(
        OnboardingStep step,
        string title,
        string description,
        bool secret,
        string placeholder)
    {
        _onboardingStep = step;
        OnboardingPanel.Visibility = Visibility.Visible;
        ChatRoot.Visibility = Visibility.Collapsed;
        OnboardingTitle.Text = title;
        OnboardingDescription.Text = description;
        OnboardingInput.Visibility = secret ? Visibility.Collapsed : Visibility.Visible;
        OnboardingPassword.Visibility = secret ? Visibility.Visible : Visibility.Collapsed;
        OnboardingInput.PlaceholderText = placeholder;
        OnboardingPassword.PlaceholderText = placeholder;
        OnboardingInput.Text = "";
        OnboardingPassword.Password = "";
        OnboardingContinue.IsEnabled = true;
        if (secret)
        {
            OnboardingPassword.Focus(FocusState.Programmatic);
        }
        else
        {
            OnboardingInput.Focus(FocusState.Programmatic);
        }
    }

    private void SetStatus(string message)
    {
        StatusText.Text = message;
        OnboardingStatus.Text = message;
    }

    private void HostExited(int exitCode)
    {
        bool wasLoggedIn = _loggedIn;
        _loggedIn = false;
        _pollTimer.Stop();
        if (wasLoggedIn)
        {
            SetStatus($"verity-winui-host exited with code {exitCode}");
        }
        else
        {
            ShowFailure("The Verity desktop bridge stopped", $"verity-winui-host exited with code {exitCode}.");
        }
    }

    private void SubmitOnboarding()
    {
        bool secret = _onboardingStep is OnboardingStep.ApiHash or OnboardingStep.Password;
        string value = secret ? OnboardingPassword.Password : OnboardingInput.Text;
        value = value.Trim();
        if (value.Length == 0)
        {
            return;
        }

        switch (_onboardingStep)
        {
            case OnboardingStep.ApiId:
                if (!int.TryParse(value, out int apiId) || apiId <= 0)
                {
                    OnboardingStatus.Text = "api_id must be a positive number";
                    return;
                }
                _apiId = value;
                ShowOnboarding(
                    OnboardingStep.ApiHash,
                    "Connect Verity to Telegram",
                    "Now paste the API hash from the same Telegram page. It is stored locally in ~/verity/config.toml.",
                    secret: true,
                    placeholder: "API hash");
                break;
            case OnboardingStep.ApiHash:
                if (_apiId is null)
                {
                    ShowOnboarding(
                        OnboardingStep.ApiId,
                        "Connect Verity to Telegram",
                        "Paste the API ID from my.telegram.org → API development tools.",
                        secret: false,
                        placeholder: "API ID");
                    return;
                }
                SendCommand(new { type = "configure", api_id = _apiId, api_hash = value });
                ShowBusy("Saving API credentials…");
                break;
            case OnboardingStep.Phone:
                SendCommand(new { type = "login_phone", phone = value });
                ShowBusy("Sending login code…");
                break;
            case OnboardingStep.Code:
                SendCommand(new { type = "login_code", code = value });
                ShowBusy(value.Equals("resend", StringComparison.OrdinalIgnoreCase)
                    ? "Resending login code…"
                    : "Checking login code…");
                break;
            case OnboardingStep.Password:
                SendCommand(new { type = "login_password", password = value });
                ShowBusy("Checking 2FA password…");
                break;
        }
    }

    private void OnboardingContinue_Click(object sender, RoutedEventArgs e) => SubmitOnboarding();

    private void OnboardingInput_KeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key == VirtualKey.Enter)
        {
            e.Handled = true;
            SubmitOnboarding();
        }
    }

    private void BotsList_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_updatingBots || BotsList.SelectedItem is not DmRow row)
        {
            return;
        }
        _selectedUserId = row.UserId;
        ChatTitle.Text = row.Label;
        SendCommand(new { type = "select", user_id = row.UserId });
    }

    private void SendCurrentMessage()
    {
        string text = Composer.Text.Trim();
        if (text.Length == 0)
        {
            return;
        }
        Composer.Text = "";
        SendCommand(new { type = "send", text });
    }

    private void Send_Click(object sender, RoutedEventArgs e) => SendCurrentMessage();

    private void Composer_KeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key == VirtualKey.Enter)
        {
            e.Handled = true;
            SendCurrentMessage();
        }
    }

    private void Refresh_Click(object sender, RoutedEventArgs e) => SendCommand(new { type = "refresh" });

    private void Reaction_Click(object sender, RoutedEventArgs e)
    {
        if (sender is not Button { Tag: ReactionViewModel reaction })
        {
            return;
        }
        SendCommand(new
        {
            type = "react",
            msg_id = reaction.MessageId,
            emoji = reaction.Emoji,
            chosen = reaction.Chosen,
        });
    }
}
