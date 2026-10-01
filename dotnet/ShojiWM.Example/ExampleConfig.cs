namespace ShojiWM.Example;

public sealed class ExampleConfig : IWindowConfig
{
    // The worker redirects ordinary config console output to stderr.
    public void OnEnable(string reason) => Console.WriteLine($"C# config enabled: {reason}");

    public CompositionNode RenderWindow(WaylandWindow window, RenderContext context) => new WindowBorder
    {
        Style = new() { Border = new() { Px = 1, Color = window.IsFocused ? "#88c0d0" : "#4c566a" } },
        Children =
        [
            new Box
            {
                Direction = LayoutDirection.Column,
                Children =
                [
                    new Box
                    {
                        Style = new() { Height = 28, PaddingX = 8, Gap = 8, Background = "#2e3440" },
                        Children =
                        [
                            new Label { Text = $"{window.Title} · {window.AppId ?? "Wayland"}", Style = new() { Color = "#eceff4", FlexGrow = 1 } },
                            new Button
                            {
                                Id = "close", OnClick = window.Close,
                                Style = new() { Width = 20, Height = 20, Background = "#bf616a" },
                            },
                        ],
                    },
                    new ClientWindow(),
                ],
            },
        ],
    };
}
