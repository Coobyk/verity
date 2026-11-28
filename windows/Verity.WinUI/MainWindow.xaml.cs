using Microsoft.UI.Xaml;

namespace Verity.WinUI;

public sealed partial class MainWindow : Window
{
    private MainPage? _page;

    public MainWindow()
    {
        InitializeComponent();

        ExtendsContentIntoTitleBar = true;
        SetTitleBar(AppTitleBar);
        AppWindow.SetIcon("Assets/AppIcon.ico");

        RootFrame.Navigated += (_, args) => _page = args.Content as MainPage;
        RootFrame.Navigate(typeof(MainPage));
        Closed += (_, _) => _page?.Cleanup();
    }
}
