using System.Runtime.InteropServices;
using ShojiWM;

// Loaded into a collectible ALC by the tests (the executable entry point is
// never run there). Its resources deliberately root it until DisposeAsync.
public sealed class ReloadFixtureConfig : IWindowConfig, IAsyncDisposable
{
    private readonly string text = Environment.GetEnvironmentVariable("SHOJI_TEST_CONFIG_TEXT") ?? "original";
    private readonly string? marker = Environment.GetEnvironmentVariable("SHOJI_TEST_CONFIG_MARKER");
    private readonly string? failure = Environment.GetEnvironmentVariable("SHOJI_TEST_CONFIG_FAILURE");
    private readonly CancellationTokenSource cancellation = new();
    private Task? task;
    private Thread? thread;
    private Timer? timer;
    private GCHandle handle;
    private bool subscribed;

    public ReloadFixtureConfig()
    {
        if (failure == "constructor") throw new InvalidOperationException("fixture constructor failure");
    }

    public void OnEnable(string reason)
    {
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
        if (marker is not null) File.AppendAllText(marker, $"disable:{text}:{reason}\n");
        if (failure == "disable") throw new InvalidOperationException("fixture disable failure");
    }

    public CompositionNode RenderWindow(WaylandWindow window, RenderContext context)
    {
        if (failure == "render") throw new InvalidOperationException("fixture render failure");
        return new WindowBorder { Children = [new Label { Text = text }, new ClientWindow(), new Button { OnClick = window.Close }] };
    }

    public async ValueTask DisposeAsync()
    {
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
