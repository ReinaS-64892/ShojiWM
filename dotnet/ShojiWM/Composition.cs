using ShojiWM.Wire;

namespace ShojiWM;

// Config authors use typed properties; all style fields come from serde DTOs.
public sealed class Style : WireStyle;

public abstract class CompositionNode
{
    public string? Id { get; init; }
    public Style? Style { get; init; }
    public IReadOnlyList<CompositionNode> Children { get; init; } = [];
    protected abstract string Kind { get; }
    protected virtual WireProps Props(Func<string, Action, string> register, string path) => new() { Id = Id, Style = Style ?? new() };

    public WireDecorationNode ToWire(Func<string, Action, string> register, string path = "root") => new()
    {
        Kind = Kind,
        NodeId = path,
        Props = Props(register, path),
        Children = Children.Select((child, index) => child.ToWire(register, $"{path}.{index}")).ToList(),
    };
}

public sealed class WindowBorder : CompositionNode
{
    public ResizeHitArea? ResizeHitArea { get; init; }
    protected override string Kind => "WindowBorder";
    protected override WireProps Props(Func<string, Action, string> register, string path) => new()
    {
        Id = Id, Style = Style ?? new(),
        Interaction = ResizeHitArea is null ? null : new() { ResizeHitArea = ResizeHitArea },
    };
}

public enum LayoutDirection { Row, Column }

public sealed class Box : CompositionNode
{
    public LayoutDirection Direction { get; init; } = LayoutDirection.Row;
    protected override string Kind => "Box";
    protected override WireProps Props(Func<string, Action, string> register, string path) => new()
    {
        Id = Id, Style = Style ?? new(), Direction = Direction == LayoutDirection.Column ? "column" : "row",
    };
}

public sealed class Label : CompositionNode
{
    public required string Text { get; init; }
    protected override string Kind => "Label";
    protected override WireProps Props(Func<string, Action, string> register, string path) => new() { Id = Id, Style = Style ?? new(), Text = Text };
}

public sealed class Button : CompositionNode
{
    public Action? OnClick { get; init; }
    public WireWindowAction? WindowAction { get; init; }
    protected override string Kind => "Button";
    protected override WireProps Props(Func<string, Action, string> register, string path)
    {
        if (OnClick is not null && WindowAction is not null) throw new InvalidOperationException("specify either OnClick or WindowAction");
        return new()
        {
            Id = Id, Style = Style ?? new(),
            OnClick = OnClick is not null ? new(register(path + ".onClick", OnClick)) :
                WindowAction is WireWindowAction action ? new(action) : null,
        };
    }
}

public sealed class ClientWindow : CompositionNode
{
    // TS's ClientWindow serializes to "Window", the Rust decoder's slot kind.
    protected override string Kind => "Window";
}

public sealed class Image : CompositionNode
{
    public required string Src { get; init; }
    protected override string Kind => "Image";
    protected override WireProps Props(Func<string, Action, string> register, string path) => new() { Id = Id, Style = Style ?? new(), Src = Src };
}
