using ShojiWM.Wire;

namespace ShojiWM;

public interface IWindowConfig
{
    CompositionNode RenderWindow(WaylandWindow window, RenderContext context);
    void OnEnable(string reason) { }
    void OnDisable(string reason) { }
    void OnWindowClosed(string windowId) { }
}

public sealed record RenderContext(
    ulong NowMs,
    bool IsPreview,
    IReadOnlyDictionary<string, WaylandOutputSnapshot> Displays,
    IReadOnlyDictionary<string, RuntimeInputDeviceSnapshot> Inputs);

/// A value snapshot and commands for this runtime turn. Reactive state can be
/// added above this API later, without changing the serialized tree contract.
public sealed class WaylandWindow(WaylandWindowSnapshot snapshot, Action<RuntimeWindowAction> emit)
{
    public WaylandWindowSnapshot Snapshot { get; } = snapshot;
    public string Id => Snapshot.Id;
    public string Title => Snapshot.Title;
    public string? AppId => Snapshot.AppId;
    public bool IsFocused => Snapshot.IsFocused;
    public bool IsFloating => Snapshot.IsFloating;
    public bool IsMaximized => Snapshot.IsMaximized;
    public bool IsFullscreen => Snapshot.IsFullscreen;
    public WindowPositionSnapshot Rect => Snapshot.Rect;
    public void Close() => Send(WaylandWindowAction.Close);
    public void Maximize() => Send(WaylandWindowAction.Maximize);
    public void Unmaximize() => Send(WaylandWindowAction.Unmaximize);
    public void Minimize() => Send(WaylandWindowAction.Minimize);
    public void Fullscreen() => Send(WaylandWindowAction.Fullscreen);
    public void Unfullscreen() => Send(WaylandWindowAction.Unfullscreen);
    public void Focus() => Send(WaylandWindowAction.Focus);
    private void Send(WaylandWindowAction action) => emit(new() { WindowId = Id, Action = action });
}
