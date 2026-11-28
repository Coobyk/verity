using System;
using Microsoft.UI;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Data;
using Microsoft.UI.Xaml.Media;
using Windows.UI;

namespace Verity.WinUI.Converters;

public sealed class BoolToVisibilityConverter : IValueConverter
{
    public object Convert(object value, Type targetType, object parameter, string language) =>
        value is true ? Visibility.Visible : Visibility.Collapsed;

    public object ConvertBack(object value, Type targetType, object parameter, string language) =>
        value is Visibility.Visible;
}

public sealed class BoolToHorizontalAlignmentConverter : IValueConverter
{
    public object Convert(object value, Type targetType, object parameter, string language) =>
        value is true ? HorizontalAlignment.Right : HorizontalAlignment.Left;

    public object ConvertBack(object value, Type targetType, object parameter, string language) =>
        value is HorizontalAlignment.Right;
}

public sealed class BoolToBubbleBrushConverter : IValueConverter
{
    // Unigram Night theme, ThemeService.Defaults.cs: MessageBackgroundOutgoing / Incoming.
    private static readonly SolidColorBrush Mine = new(ColorHelper.FromArgb(0xFF, 0x45, 0x6A, 0x93));
    private static readonly SolidColorBrush Theirs = new(ColorHelper.FromArgb(0xFF, 0x1C, 0x27, 0x33));

    public object Convert(object value, Type targetType, object parameter, string language) =>
        value is true ? Mine : Theirs;

    public object ConvertBack(object value, Type targetType, object parameter, string language) =>
        throw new NotSupportedException();
}

public sealed class BoolToAvailabilityTextConverter : IValueConverter
{
    public object Convert(object value, Type targetType, object parameter, string language) =>
        value is true ? "" : "offline";

    public object ConvertBack(object value, Type targetType, object parameter, string language) =>
        throw new NotSupportedException();
}

public sealed class BoolToTimestampBrushConverter : IValueConverter
{
    // Unigram Night: MessageSubtleLabelOutgoing / MessageSubtleLabelIncoming.
    private static readonly SolidColorBrush Mine = new(ColorHelper.FromArgb(0xFF, 0x91, 0xAF, 0xC8));
    private static readonly SolidColorBrush Theirs = new(ColorHelper.FromArgb(0xFF, 0x7D, 0x8E, 0x98));

    public object Convert(object value, Type targetType, object parameter, string language) =>
        value is true ? Mine : Theirs;

    public object ConvertBack(object value, Type targetType, object parameter, string language) =>
        throw new NotSupportedException();
}
