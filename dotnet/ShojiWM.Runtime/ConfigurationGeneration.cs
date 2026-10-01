using System.Runtime.CompilerServices;

namespace ShojiWM.Runtime;

/// Owns one config ALC and its session; never owns the worker or transport.
internal sealed class ConfigurationGeneration
{
    private ConfigLoader? loader;
    private RuntimeSession? session;
    public RuntimeSession Session => session ?? throw new ObjectDisposedException(nameof(ConfigurationGeneration));

    [MethodImpl(MethodImplOptions.NoInlining)]
    public static ConfigurationGeneration Load(string path, ICollection<WeakReference> retired)
    {
        var generation = new ConfigurationGeneration();
        try
        {
            generation.loader = new(path);
            generation.session = new(generation.loader.CreateConfig());
            return generation;
        }
        catch (Exception error)
        {
            // Convert errors to host-owned strings; an Exception/Type from the
            // config must never survive in the host and root its failed ALC.
            var message = error.GetBaseException().Message;
            retired.Add(generation.Release("load-failed"));
            throw new ConfigurationException("assemblyLoad", message);
        }
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    public WeakReference Release(string reason)
    {
        var previousSession = session;
        session = null;
        if (previousSession is not null)
        {
            var response = previousSession.Handle(new()
            {
                Kind = "lifecycleDisable", RequestId = 0, Reason = reason, NowMs = 0,
                DisplayState = [], InputState = [],
            });
            if (!response.Ok) Console.Error.WriteLine($"ShojiWM .NET shutdown: {response.Error}");
            try { previousSession.Dispose(); }
            catch (Exception error) { Console.Error.WriteLine($"ShojiWM .NET dispose: {error.Message}"); }
        }
        var previousLoader = loader;
        loader = null;
        var weak = new WeakReference(previousLoader, trackResurrection: true);
        previousLoader?.Unload();
        return weak;
    }
}

/// .NET-specific stage and text, without retaining managed config exceptions.
public sealed class ConfigurationException(string stage, string message)
    : Exception($"{stage}: {message}")
{
    public string Stage { get; } = stage;
}
