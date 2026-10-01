using System.Text.Json;
using ShojiWM.Wire;

namespace ShojiWM.Runtime;

/// Semantic requests are independent of native hosting. Config callbacks stay
/// managed; only handler IDs are returned to Rust.
public sealed class RuntimeSession : IDisposable
{
    private IWindowConfig? config;
    public RuntimeSession(IWindowConfig config) => this.config = config;
    private sealed record WindowEntry(WaylandWindowSnapshot Snapshot, Dictionary<string, (string Id, Action Callback)> Handlers);
    private readonly Dictionary<string, WindowEntry> windows = [];
    private readonly List<RuntimeWindowAction> actions = [];
    private ulong nextHandlerId;
    private readonly string generationId = Guid.NewGuid().ToString("N");
    private bool enabled;

    public ExternalRuntimeResponse HandleJson(string json)
    {
        ulong requestId = 0;
        var kind = "protocolError";
        try
        {
            if (config is null) throw new ObjectDisposedException(nameof(RuntimeSession));
            using var document = JsonDocument.Parse(json);
            var root = document.RootElement;
            if (root.TryGetProperty("requestId", out var id) && id.TryGetUInt64(out var value)) requestId = value;
            if (root.TryGetProperty("kind", out var k) && k.ValueKind == JsonValueKind.String) kind = k.GetString()!;
            var request = JsonSerializer.Deserialize<ExternalRuntimeRequest>(json, WireJson.Options) ?? throw new JsonException("missing request");
            return Handle(request);
        }
        catch (Exception error)
        {
            return new() { RequestId = requestId, Kind = kind, Ok = false, Error = error.Message };
        }
    }

    public ExternalRuntimeResponse Handle(ExternalRuntimeRequest request)
    {
        actions.Clear();
        try
        {
            if (config is null) throw new ObjectDisposedException(nameof(RuntimeSession));
            WireDecorationNode? serialized = null;
            bool? invoked = null;
            switch (request.Kind)
            {
                case "drainPreload":
                    break; // Assembly was already loaded, so startup errors precede this ACK.
                case "lifecycleEnable":
                    if (!enabled) { enabled = true; config.OnEnable(request.Reason ?? "initial"); }
                    break;
                case "lifecycleDisable":
                    Disable(request.Reason ?? "shutdown");
                    break;
                case "evaluate":
                case "evaluatePreview":
                case "evaluateCandidatePreview":
                case "evaluateCached":
                    var snapshot = request.Snapshot ??
                        (request.WindowId is string cachedId && windows.TryGetValue(cachedId, out var cached) ? cached.Snapshot : null)
                        ?? throw new JsonException("evaluate requires a snapshot or a known windowId");
                    if (string.IsNullOrEmpty(snapshot.Id) || request.WindowId is string requestedId && requestedId != snapshot.Id)
                        throw new JsonException("snapshot id must match windowId");
                    serialized = Render(snapshot, request, request.Kind is "evaluatePreview" or "evaluateCandidatePreview");
                    break;
                case "invokeHandler":
                    var windowId = request.WindowId ?? throw new JsonException("invokeHandler requires windowId");
                    var handlerId = request.HandlerId ?? throw new JsonException("invokeHandler requires handlerId");
                    invoked = false;
                    if (windows.TryGetValue(windowId, out var entry))
                    {
                        var handler = entry.Handlers.Values.FirstOrDefault(handler => handler.Id == handlerId);
                        if (handler.Callback is not null)
                        {
                            handler.Callback();
                            invoked = true;
                            serialized = Render(entry.Snapshot, request, false);
                        }
                    }
                    break;
                case "windowClosed":
                    var closedId = request.WindowId ?? throw new JsonException("windowClosed requires windowId");
                    if (windows.Remove(closedId)) config.OnWindowClosed(closedId);
                    break;
                default:
                    throw new JsonException($"unsupported runtime message kind: {request.Kind}");
            }
            return new() { RequestId = request.RequestId, Kind = request.Kind, Ok = true, Serialized = serialized, Invoked = invoked, Actions = [.. actions] };
        }
        catch (Exception error)
        {
            actions.Clear();
            return new() { RequestId = request.RequestId, Kind = request.Kind, Ok = false, Error = error.Message };
        }
    }

    private WireDecorationNode Render(WaylandWindowSnapshot snapshot, ExternalRuntimeRequest request, bool preview)
    {
        var context = new RenderContext(request.NowMs, preview, request.DisplayState, request.InputState);
        var window = new WaylandWindow(snapshot, action => { if (!preview) actions.Add(action); });
        var composition = config!.RenderWindow(window, context) ?? throw new InvalidOperationException("config returned no composition");
        var handlers = new Dictionary<string, (string Id, Action Callback)>();
        windows.TryGetValue(snapshot.Id, out var previous);
        var tree = composition.ToWire((key, callback) =>
        {
            var id = previous is not null && previous.Handlers.TryGetValue(key, out var prior)
                ? prior.Id : $"handler-{generationId}-{checked(++nextHandlerId)}";
            handlers.Add(key, (id, callback));
            return id;
        });
        if (!preview) windows[snapshot.Id] = new(snapshot, handlers);
        return tree;
    }

    private void Disable(string reason)
    {
        try { if (enabled) config?.OnDisable(reason); }
        finally { enabled = false; windows.Clear(); actions.Clear(); }
    }

    public void Dispose()
    {
        var previous = config;
        try { Disable("shutdown"); }
        finally
        {
            // Remove all host-held config/delegate references before ALC.Unload.
            config = null;
            if (previous is IAsyncDisposable asyncDisposable)
                asyncDisposable.DisposeAsync().AsTask().GetAwaiter().GetResult();
            else if (previous is IDisposable disposable) disposable.Dispose();
        }
    }
}
