using System.ComponentModel;
using Microsoft.UI.Xaml.Media.Imaging;
using System.Collections.ObjectModel;

namespace Verity.WinUI.Models;

public sealed class ChatMessageViewModel
{
    public required int Id { get; init; }
    public required string Sender { get; init; }
    public required string Text { get; init; }
    public required string Time { get; init; }
    public required bool IsMine { get; init; }
    public required TopicMessage Message { get; init; }
    public ObservableCollection<ReactionViewModel> Reactions { get; } = new();
    public ObservableCollection<MathImageViewModel> MathImages { get; } = new();
}

public sealed class ReactionViewModel
{
    public required int MessageId { get; init; }
    public required string Emoji { get; init; }
    public required bool Chosen { get; init; }
    public required int Count { get; init; }

    public string Display => Count > 1 ? $"{Emoji} {Count}" : Emoji;
}

public sealed class MathImageViewModel : INotifyPropertyChanged
{
    private BitmapImage? _source;

    public required bool Display { get; init; }

    public BitmapImage? Source
    {
        get => _source;
        set
        {
            if (!ReferenceEquals(_source, value))
            {
                _source = value;
                PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(Source)));
            }
        }
    }

    public event PropertyChangedEventHandler? PropertyChanged;
}
