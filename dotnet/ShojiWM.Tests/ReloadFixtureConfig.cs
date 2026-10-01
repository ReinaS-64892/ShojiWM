using System.Runtime.InteropServices;
using ShojiWM;

// Loaded into a collectible ALC by the tests (the executable entry point is
// never run there). Its resources deliberately root it until DisposeAsync.
public sealed class ReloadFixtureConfig : IWindowConfig, IAsyncDisposable
{
    // Native-host tests use per-staging files: CoreCLR maintains its own
    // environment cache, so Rust setenv after CLR initialization is unsuitable.
    private static string? Setting(string key, string variable)
    {
        var path = Path.Combine(Path.GetDirectoryName(typeof(ReloadFixtureConfig).Assembly.Location)!, "fixture-settings.json");
        if (!File.Exists(path)) return Environment.GetEnvironmentVariable(variable);
        using var document = System.Text.Json.JsonDocument.Parse(File.ReadAllText(path));
        return document.RootElement.GetProperty(key).GetString();
    }

    private readonly string? threadMarker = Environment.GetEnvironmentVariable("SHOJI_TEST_THREAD_MARKER");
    private readonly string text = Setting("text", "SHOJI_TEST_CONFIG_TEXT") ?? "original";
    private readonly string? marker = Environment.GetEnvironmentVariable("SHOJI_TEST_CONFIG_MARKER");
    private readonly string? failure = Setting("failure", "SHOJI_TEST_CONFIG_FAILURE");
    private readonly CancellationTokenSource cancellation = new();
    private Task? task;
    private Thread? thread;
    private Timer? timer;
    private GCHandle handle;
    private bool subscribed;

    private void Trace(string operation)
    {
        if (threadMarker is not null) File.AppendAllText(threadMarker, $"{operation}:{Environment.CurrentManagedThreadId}\n");
    }

    public ReloadFixtureConfig()
    {
        Trace("constructor");
        if (failure == "constructor") throw new InvalidOperationException("fixture constructor failure");
    }

    public void OnEnable(string reason)
    {
        Trace("enable");
        handle = GCHandle.Alloc(this);
        AppDomain.CurrentDomain.ProcessExit += ProcessExit;
        subscribed = true;
        timer = new Timer(_ => GC.KeepAlive(this), null, 10, 10);
        thread = new Thread(() => { cancellation.Token.WaitHandle.WaitOne(); GC.KeepAlive(this); });
        thread.Start();
        task = RunAsync();
        if (failure == "enable") throw new InvalidOperationException("fixture enable failure");
    }

    private async Task RunAsync()
    {
        try { await Task.Delay(Timeout.Infinite, cancellation.Token); }
        catch (OperationCanceledException) { }
        GC.KeepAlive(this);
    }

    private void ProcessExit(object? sender, EventArgs args) => GC.KeepAlive(this);
    public void OnDisable(string reason)
    {
        Trace("disable");
        if (marker is not null) File.AppendAllText(marker, $"disable:{text}:{reason}\n");
        if (failure == "disable") throw new InvalidOperationException("fixture disable failure");
    }

    public CompositionNode RenderWindow(WaylandWindow window, RenderContext context)
    {
        Trace("render");
        if (failure == "tree") return new WindowBorder();
        if (failure == "render") throw new InvalidOperationException("fixture render failure");
        return new WindowBorder { Children = [new Label { Text = text }, new ClientWindow(), new Button { OnClick = () => {
            Trace("handler");
            if (AppDomain.CurrentDomain.GetData("ShojiWM.TestCleanup") is Action cleanup) {
                AppDomain.CurrentDomain.SetData("ShojiWM.TestCleanup", null);
                cleanup();
            }
            window.Close();
        } }] };
    }

    public async ValueTask DisposeAsync()
    {
        Trace("dispose");
        cancellation.Cancel();
        if (timer is not null) await timer.DisposeAsync();
        if (task is not null) await task;
        thread?.Join();
        if (subscribed)
        {
            if (failure == "leak")
                AppDomain.CurrentDomain.SetData("ShojiWM.TestCleanup", (Action)(() => AppDomain.CurrentDomain.ProcessExit -= ProcessExit));
            else AppDomain.CurrentDomain.ProcessExit -= ProcessExit;
        }
        if (handle.IsAllocated) handle.Free();
        cancellation.Dispose();
        if (marker is not null) File.AppendAllText(marker, $"dispose:{text}\n");
    }
}
