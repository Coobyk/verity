using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace Verity.WinUI.Models;

public sealed class DmRow
{
    [JsonPropertyName("user_id")]
    public long UserId { get; set; }

    [JsonPropertyName("label")]
    public string Label { get; set; } = "";

    [JsonIgnore]
    public string Initial => Label.Length > 0 ? Label.TrimStart('@')[..1].ToUpperInvariant() : "?";

    [JsonPropertyName("available")]
    public bool Available { get; set; }
}

public sealed class ReactionInfo
{
    [JsonPropertyName("emoji")]
    public string Emoji { get; set; } = "";

    [JsonPropertyName("count")]
    public int Count { get; set; }

    [JsonPropertyName("chosen")]
    public bool Chosen { get; set; }
}

public sealed class FmtSpan
{
    [JsonPropertyName("kind")]
    public string Kind { get; set; } = "";

    [JsonPropertyName("start")]
    public int Start { get; set; }

    [JsonPropertyName("len")]
    public int Length { get; set; }

    [JsonPropertyName("url")]
    public string? Url { get; set; }

    [JsonPropertyName("lang")]
    public string? Lang { get; set; }
}

public sealed class MathImage
{
    [JsonPropertyName("path")]
    public string Path { get; set; } = "";

    [JsonPropertyName("display")]
    public bool Display { get; set; }
}

public sealed class TopicMessage
{
    [JsonPropertyName("id")]
    public int Id { get; set; }

    [JsonPropertyName("date")]
    public long Date { get; set; }

    [JsonPropertyName("sender")]
    public string Sender { get; set; } = "";

    [JsonPropertyName("sender_id")]
    public long? SenderId { get; set; }

    [JsonPropertyName("text")]
    public string Text { get; set; } = "";

    [JsonPropertyName("spans")]
    public List<FmtSpan> Spans { get; set; } = new();

    [JsonPropertyName("reactions")]
    public List<ReactionInfo> Reactions { get; set; } = new();

    [JsonPropertyName("math")]
    public List<MathImage> Math { get; set; } = new();
}
