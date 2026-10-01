using System.Runtime.CompilerServices;
using System.Text.Json;
using ShojiWM.Wire;

namespace ShojiWM.Runtime;

/// Sequential assembly transaction manager. The native ABI and file
/// watcher are independent; only semantic commands reach this host.
public sealed class ConfigurationHost : IDisposable
{
    private ConfigurationGeneration? current;
    private ConfigurationGeneration? candidate;
    private readonly List<WeakReference> retired = [];
    public int PendingUnloadCount => retired.Count;
    public int UnloadedCount { get; private set; }

    public ConfigurationHost(string configPath) => current = ConfigurationGeneration.Load(configPath, retired);

    public ExternalRuntimeResponse HandleJson(string json)
    {
        ulong id = 0;
        var kind = "protocolError";
        try
        {
            using var document = JsonDocument.Parse(json);
            if (document.RootElement.TryGetProperty("requestId", out var value)) value.TryGetUInt64(out id);
            if (document.RootElement.TryGetProperty("kind", out value) && value.ValueKind == JsonValueKind.String) kind = value.GetString()!;
            var request = JsonSerializer.Deserialize<ExternalRuntimeRequest>(json, WireJson.Options) ?? throw new JsonException("missing request");
            return Handle(request);
        }
        catch (Exception error) { return Failure(id, kind, error.Message); }
    }

    public ExternalRuntimeResponse Handle(ExternalRuntimeRequest request)
    {
        if (request.Kind == "prepareAssembly" && retired.Count != 0)
        {
            VerifyUnloads();
            if (retired.Count != 0)
                return Failure(request.RequestId, request.Kind, "unload: previous ALC still referenced; refusing to accumulate generations");
        }
        var response = HandleCore(request);
        if (request.Kind is "prepareAssembly" or "commitAssembly" or "abortAssembly" or "shutdownAssemblies") VerifyUnloads();
        return response;
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private ExternalRuntimeResponse HandleCore(ExternalRuntimeRequest request)
    {
        try
        {
            if (current is null) throw new ObjectDisposedException(nameof(ConfigurationHost));
            switch (request.Kind)
            {
                case "prepareAssembly":
                    Prepare(request);
                    return Ack(request);
                case "evaluateCandidatePreview":
                    return CandidateSession.Handle(request);
                case "commitAssembly":
                    Commit();
                    return Ack(request);
                case "abortAssembly":
                    Abort();
                    return Ack(request);
                case "shutdownAssemblies":
                    ReleaseAll();
                    return Ack(request);
                default:
                    return current.Session.Handle(request);
            }
        }
        catch (Exception error)
        {
            var message = error.Message;
            // A failed prepare owns no active resources. Collection happens
            // only at lifecycle boundaries, after load/enable stack unwinds.
            return Failure(request.RequestId, request.Kind, message);
        }
    }

    private RuntimeSession CandidateSession => candidate?.Session ?? throw new ConfigurationException("reload", "no prepared assembly");

    [MethodImpl(MethodImplOptions.NoInlining)]
    private void Prepare(ExternalRuntimeRequest request)
    {
        ReleaseCandidate();
        var path = request.ConfigPath ?? throw new ConfigurationException("assemblyLoad", "prepareAssembly requires configPath");
        candidate = ConfigurationGeneration.Load(path, retired);
        var enabled = candidate.Session.Handle(new()
        {
            Kind = "lifecycleEnable", RequestId = request.RequestId, Reason = "reload",
            NowMs = request.NowMs, DisplayState = request.DisplayState, InputState = request.InputState,
        });
        if (!enabled.Ok || enabled.Actions.Count != 0)
        {
            var error = enabled.Error ?? "OnEnable returned unsupported window actions";
            ReleaseCandidate();
            throw new ConfigurationException("initialization", error);
        }
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private void SwitchGeneration()
    {
        var next = candidate ?? throw new ConfigurationException("reload", "no prepared assembly");
        candidate = null;
        var previous = current;
        current = next;
        if (previous is not null) retired.Add(previous.Release("reload"));
    }

    private void Commit()
    {
        SwitchGeneration();
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private void ReleaseCandidate()
    {
        var previous = candidate;
        candidate = null;
        if (previous is not null) retired.Add(previous.Release("reload-rejected"));
    }

    private void Abort()
    {
        ReleaseCandidate();
    }

    public void VerifyUnloads()
    {
        // Bounded GC at reload/shutdown only. Leaking config Tasks/statics
        // cannot force an unbounded wait or silently masquerade as unloaded.
        for (var attempt = 0; retired.Any(weak => weak.IsAlive) && attempt < 3; attempt++)
        {
            GC.Collect();
            GC.WaitForPendingFinalizers();
            GC.Collect();
        }
        UnloadedCount += retired.RemoveAll(weak => !weak.IsAlive);
        if (retired.Count != 0)
            Console.Error.WriteLine($"ShojiWM .NET unload: {retired.Count} ALC(s) still referenced; config must dispose tasks, timers and external subscriptions");
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private void ReleaseAll()
    {
        ReleaseCandidate();
        var previous = current;
        current = null;
        if (previous is not null) retired.Add(previous.Release("shutdown"));
    }

    public void Dispose()
    {
        ReleaseAll();
        VerifyUnloads();
    }

    private static ExternalRuntimeResponse Ack(ExternalRuntimeRequest request) => new()
    { RequestId = request.RequestId, Kind = request.Kind, Ok = true, Actions = [] };
    private static ExternalRuntimeResponse Failure(ulong id, string kind, string error) => new()
    { RequestId = id, Kind = kind, Ok = false, Error = error, Actions = [] };
}
