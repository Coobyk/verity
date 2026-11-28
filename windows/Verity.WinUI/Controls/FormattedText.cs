using System;
using System.Collections.Generic;
using System.Linq;
using Microsoft.UI;
using System.Text;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Media;
using Verity.WinUI.Models;
using Windows.UI;

namespace Verity.WinUI.Controls;

/// <summary>
/// RichTextBlock which expands Verity's Telegram entity spans into WinUI
/// inlines. Spans arrive as UTF-8 byte offsets; WinUI strings use UTF-16,
/// so rendering first builds a byte-to-string-index map.
/// </summary>
public sealed class FormattedText : UserControl
{
    private static readonly SolidColorBrush AccentBrush = new(ColorHelper.FromArgb(255, 29, 155, 240));
    private static readonly SolidColorBrush SpoilerBrush = new(ColorHelper.FromArgb(255, 33, 38, 43));
    private static readonly FontFamily MonoFont = new("Cascadia Mono");

    private readonly RichTextBlock _rich = new()
    {
        Foreground = new SolidColorBrush(ColorHelper.FromArgb(255, 231, 233, 234)),
        IsTextSelectionEnabled = true,
        TextWrapping = TextWrapping.Wrap,
    };

    public FormattedText()
    {
        Content = _rich;
    }

    public static readonly DependencyProperty MessageProperty = DependencyProperty.Register(
        nameof(Message),
        typeof(TopicMessage),
        typeof(FormattedText),
        new PropertyMetadata(null, OnMessageChanged));

    public TopicMessage? Message
    {
        get => (TopicMessage?)GetValue(MessageProperty);
        set => SetValue(MessageProperty, value);
    }

    private static void OnMessageChanged(DependencyObject sender, DependencyPropertyChangedEventArgs args)
    {
        ((FormattedText)sender).RenderMessage();
    }

    private void RenderMessage()
    {
        _rich.Blocks.Clear();
        TopicMessage? message = Message;
        if (message is null)
        {
            return;
        }

        var paragraph = new Paragraph();
        string text = message.Text ?? "";
        if (text.Length == 0)
        {
            _rich.Blocks.Add(paragraph);
            return;
        }

        int[] map = ByteToUtf16Map(text);
        var converted = new List<(int Start, int End, FmtSpan Span)>();
        foreach (FmtSpan span in message.Spans)
        {
            int start = MapOffset(map, span.Start);
            int end = MapOffset(map, span.Start + span.Length);
            if (start < end && start >= 0 && end <= text.Length)
            {
                converted.Add((start, end, span));
            }
        }

        var boundaries = new SortedSet<int> { 0, text.Length };
        foreach ((int start, int end, _) in converted)
        {
            boundaries.Add(start);
            boundaries.Add(end);
        }
        int[] points = boundaries.ToArray();

        for (int i = 0; i < points.Length - 1; i++)
        {
            int start = points[i];
            int end = points[i + 1];
            if (start == end)
            {
                continue;
            }
            var active = converted
                .Where(s => s.Start <= start && s.End >= end)
                .Select(s => s.Span)
                .ToList();
            paragraph.Inlines.Add(BuildInline(text[start..end], active));
        }

        _rich.Blocks.Add(paragraph);
    }

    private static Inline BuildInline(string text, List<FmtSpan> active)
    {
        var run = new Run { Text = text };
        if (active.Any(s => s.Kind == "bold"))
        {
            run.FontWeight = FontWeights.Bold;
        }
        if (active.Any(s => s.Kind is "italic" or "blockquote"))
        {
            run.FontStyle = Windows.UI.Text.FontStyle.Italic;
        }
        if (active.Any(s => s.Kind is "code" or "pre"))
        {
            run.FontFamily = MonoFont;
        }
        if (active.Any(s => s.Kind == "underline"))
        {
            run.TextDecorations = Windows.UI.Text.TextDecorations.Underline;
        }
        if (active.Any(s => s.Kind == "strike"))
        {
            run.TextDecorations = Windows.UI.Text.TextDecorations.Strikethrough;
        }
        if (active.Any(s => s.Kind == "spoiler"))
        {
            run.Foreground = SpoilerBrush;
        }
        else if (active.Any(s => s.Kind is "accent" or "custom_emoji"))
        {
            run.Foreground = AccentBrush;
        }

        FmtSpan? link = active.FirstOrDefault(s => s.Kind == "link" && !string.IsNullOrWhiteSpace(s.Url));
        if (link?.Url is not string url || !Uri.TryCreate(url, UriKind.Absolute, out Uri? uri))
        {
            return run;
        }

        var hyperlink = new Hyperlink
        {
            NavigateUri = uri,
            Foreground = AccentBrush,
        };
        hyperlink.Inlines.Add(run);
        return hyperlink;
    }

    private static int[] ByteToUtf16Map(string text)
    {
        int byteCount = Encoding.UTF8.GetByteCount(text);
        int[] map = new int[byteCount + 1];
        int bytePosition = 0;
        int utf16Position = 0;
        foreach (Rune rune in text.EnumerateRunes())
        {
            int utf8Length = rune.Utf8SequenceLength;
            for (int i = 0; i < utf8Length; i++)
            {
                map[bytePosition + i] = utf16Position;
            }
            bytePosition += utf8Length;
            utf16Position += rune.Utf16SequenceLength;
        }
        map[byteCount] = text.Length;
        return map;
    }

    private static int MapOffset(int[] map, int byteOffset) =>
        byteOffset >= 0 && byteOffset < map.Length ? map[byteOffset] : -1;
}
