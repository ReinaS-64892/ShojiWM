using System.Text;
using System.Text.Json;
using System.Runtime.CompilerServices;
using ShojiWM;
using ShojiWM.Example;
using ShojiWM.Runtime;
using ShojiWM.Wire;

// BCL-only executable test harness: any failed assertion makes the process fail.
var passed = 0;
void Test(string name, Action test)
{
    test();
    Console.WriteLine($"PASS {name}");
    passed++;
}
void Check(bool condition, string message = "assertion failed")
{
    if (!condition) throw new InvalidOperationException(message);
}
void Throws(Action action)
{
    try { action(); }
    catch (JsonException) { return; }
    throw new InvalidOperationException("expected JsonException");
}
string Json<T>(T value) => JsonSerializer.Serialize(value, WireJson.Options);
T Parse<T>(string value) => JsonSerializer.Deserialize<T>(value, WireJson.Options)!;
var fixture = File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "window.json"));
var snapshot = Parse<WaylandWindowSnapshot>(fixture);
ExternalRuntimeRequest Request(string kind, ulong id = 42, WaylandWindowSnapshot? window = null, string? handler = null) => new()
{
    Kind = kind, RequestId = id, Snapshot = window, WindowId = window?.Id ?? "1",
    HandlerId = handler, NowMs = 1234, DisplayState = [], InputState = [],
};
IEnumerable<WireDecorationNode> Nodes(WireDecorationNode node) => new[] { node }.Concat(node.Children.SelectMany(Nodes));

Test("generated snapshot roundtrip and enum spellings", () =>
{
    var again = Parse<WaylandWindowSnapshot>(Json(snapshot));
    Check(again.Title == "Kitty 日本語" && again.AppId == "kitty" && again.IsFocused);
    Check(again.Rect.X == 0.5 && again.Decoration.Mode == WindowDecorationModeSnapshot.Server);
    Check(Json(WindowDecorationProtocolSnapshot.KdeServerDecoration) == "\"kde-server-decoration\"");
    Check(Json(OutputTransformSnapshot.Rotate90) == "\"rotate-90\"");
    Check(Json(OutputSubpixelSnapshot.HorizontalRgb) == "\"horizontal-rgb\"");
    Check(Json(WaylandWindowAction.ScheduleAnimation) == "\"scheduleAnimation\"");
    Throws(() => Parse<WindowDecorationModeSnapshot>("\"bogus\""));
    Throws(() => Parse<WindowDecorationModeSnapshot>("7"));
});

Test("optional and null properties; Rust icon bytes remain number arrays", () =>
{
    var noApp = Parse<WaylandWindowSnapshot>(fixture.Replace("\"appId\": \"kitty\",", ""));
    Check(noApp.AppId is null && !Json(noApp).Contains("appId"));
    Check(Parse<WaylandWindowSnapshot>(fixture.Replace("\"kitty\"", "null")).AppId is null);
    Check(Parse<WireStyle>("{}").Width is null && Parse<WireStyle>("{\"width\":null}").Width is null);
    var icon = new WindowIconSnapshot { Bytes = [0, 128, 255] };
    Check(Json(icon) == "{\"bytes\":[0,128,255]}");
});

Test("requestId retains full u64 precision", () =>
{
    var request = Request("evaluate", ulong.MaxValue, snapshot);
    Check(Parse<ExternalRuntimeRequest>(Json(request)).RequestId == ulong.MaxValue);
    using var session = new RuntimeSession(new ExampleConfig());
    var response = Parse<ExternalRuntimeResponse>(Json(session.HandleJson(Json(request))));
    Check(response.Ok && response.RequestId == ulong.MaxValue && response.Kind == "evaluate");
});

Test("composition, styles and untagged unions roundtrip", () =>
{
    var style = Parse<WireStyle>(Json(new Style { Width = 28, FontFamily = "sans", Border = new() { Px = 1, Color = "#fff" } }));
    Check(style.Width?.Pixels == 28 && style.FontFamily?.Names[0] == "sans");
    Check(Parse<Dimension>("\"unsupported-keyword\"").Keyword == "unsupported-keyword");
    var action = Parse<ClickDescriptor>(Json(new ClickDescriptor(WireWindowAction.Close)));
    Check(action.Action == WireWindowAction.Close);
    Check(Parse<ClickDescriptor>(Json(new ClickDescriptor("handler-42"))).Handler?.Id == "handler-42");
    Throws(() => Parse<ClickDescriptor>("{\"kind\":\"wrong\",\"id\":\"1\"}"));
    using var session = new RuntimeSession(new ExampleConfig());
    var tree = Parse<WireDecorationNode>(Json(session.Handle(Request("evaluate", window: snapshot)).Serialized));
    Check(tree.Kind == "WindowBorder" && Nodes(tree).Count(node => node.Kind == "Window") == 1);
    Check(Nodes(tree).Single(node => node.Kind == "Label").Props.Text!.Contains(snapshot.Title));
});

Test("callback IDs stable across render and isolated from preview/window close", () =>
{
    using var session = new RuntimeSession(new ExampleConfig());
    string Handler(WireDecorationNode tree) => Nodes(tree).Single(node => node.Kind == "Button").Props.OnClick!.Handler!.Id;
    var id = Handler(session.Handle(Request("evaluate", window: snapshot)).Serialized!);
    Check(id == Handler(session.Handle(Request("evaluateCached", window: snapshot)).Serialized!));
    session.Handle(Request("evaluatePreview", window: snapshot));
    var invoked = session.Handle(Request("invokeHandler", handler: id));
    Check(invoked.Ok && invoked.Invoked == true && invoked.Serialized is not null);
    Check(invoked.Actions.Single().Action == WaylandWindowAction.Close && invoked.Actions[0].WindowId == snapshot.Id);
    Check(session.Handle(Request("invokeHandler", handler: "unknown")).Invoked == false);
    session.Handle(Request("windowClosed"));
    Check(session.Handle(Request("invokeHandler", handler: id)).Invoked == false);
});

Test("malformed requests and unknown kinds return correlated errors", () =>
{
    using var session = new RuntimeSession(new ExampleConfig());
    var unknown = session.HandleJson(Json(Request("unknown", 99)));
    Check(!unknown.Ok && unknown.RequestId == 99 && unknown.Kind == "unknown" && unknown.Error!.Contains("unsupported"));
    Check(!session.HandleJson("{").Ok);
    Check(!session.HandleJson("{\"requestId\":100,\"kind\":\"evaluate\"}").Ok);
    Check(session.HandleJson("{\"requestId\":100,\"kind\":\"evaluate\"}").RequestId == 100);
    Check(!session.Handle(Request("evaluate")).Ok);
});

Test("preview delegates do not replace live delegates; removed handlers are released", () =>
{
    var config = new HandlerConfig();
    using var session = new RuntimeSession(config);
    var tree = session.Handle(Request("evaluate", window: snapshot)).Serialized!;
    var handler = Nodes(tree).Single(node => node.Kind == "Button").Props.OnClick!.Handler!.Id;
    var preview = session.Handle(Request("evaluatePreview", window: snapshot));
    Check(preview.Actions.Count == 0);
    session.Handle(Request("invokeHandler", handler: handler));
    Check(config.LiveClicks == 1 && config.PreviewClicks == 0);
    config.ShowButton = false;
    session.Handle(Request("evaluate", window: snapshot));
    Check(session.Handle(Request("invokeHandler", handler: handler)).Invoked == false);
});

Test("handler IDs cannot invoke another window's delegate", () =>
{
    using var session = new RuntimeSession(new ExampleConfig());
    var another = Parse<WaylandWindowSnapshot>(fixture.Replace("\"id\": \"1\"", "\"id\": \"2\""));
    var tree = session.Handle(Request("evaluate", window: snapshot)).Serialized!;
    var id = Nodes(tree).Single(node => node.Kind == "Button").Props.OnClick!.Handler!.Id;
    session.Handle(Request("evaluate", window: another));
    var response = session.Handle(new ExternalRuntimeRequest
    {
        RequestId = 43, Kind = "invokeHandler", WindowId = "2", HandlerId = id,
        NowMs = 1235, DisplayState = [], InputState = [],
    });
    Check(response.Ok && response.Invoked == false && response.Actions.Count == 0);
});

Test("native ABI JSON, buffer ownership, size guards and exception containment", () => NativeAbiTests.Run(typeof(ExampleConfig).Assembly.Location, Json(Request("evaluate", window: snapshot))));

Test("config assembly loading shares API identity", () =>
{
    var loader = new ConfigLoader(typeof(ExampleConfig).Assembly.Location);
    Check(loader.CreateConfig() is IWindowConfig);
    loader.Unload();
});

Test("lifecycle enable/disable called exactly once per generation", () =>
{
    var config = new LifecycleConfig();
    using var session = new RuntimeSession(config);
    Check(session.Handle(Request("lifecycleEnable")).Ok);
    Check(session.Handle(Request("lifecycleEnable")).Ok);
    Check(session.Handle(Request("lifecycleDisable")).Ok);
    Check(session.Handle(Request("lifecycleDisable")).Ok);
    Check(config.Enables == 1 && config.Disables == 1);
});

Test("handler IDs do not cross runtime generations", () =>
{
    using var oldSession = new RuntimeSession(new HandlerConfig());
    using var nextSession = new RuntimeSession(new HandlerConfig());
    var oldTree = oldSession.Handle(Request("evaluate", window: snapshot)).Serialized!;
    var nextTree = nextSession.Handle(Request("evaluate", window: snapshot)).Serialized!;
    var oldHandler = Nodes(oldTree).Single(node => node.Kind == "Button").Props.OnClick!.Handler!.Id;
    var nextHandler = Nodes(nextTree).Single(node => node.Kind == "Button").Props.OnClick!.Handler!.Id;
    Check(oldHandler != nextHandler);
    Check(nextSession.Handle(Request("invokeHandler", handler: oldHandler)).Invoked == false);
    Check(nextSession.Handle(Request("invokeHandler", handler: nextHandler)).Invoked == true);
});

Test("collectible assembly reload releases handlers, tasks, timer, thread, event and GCHandle", () =>
{
    var marker = Path.GetTempFileName();
    Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_MARKER", marker);
    Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_TEXT", "original");
    try
    {
        using var host = new ConfigurationHost(typeof(ReloadFixtureConfig).Assembly.Location);
        Check(host.Handle(Request("lifecycleEnable")).Ok);
        var first = host.Handle(Request("evaluate", window: snapshot)).Serialized!;
        var oldHandler = Nodes(first).Single(node => node.Kind == "Button").Props.OnClick!.Handler!.Id;
        for (var i = 0; i < 20; i++)
        {
            var text = $"generation-{i}";
            Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_TEXT", text);
            var prepared = host.Handle(new ExternalRuntimeRequest
            {
                RequestId = (ulong)i, Kind = "prepareAssembly", ConfigPath = typeof(ReloadFixtureConfig).Assembly.Location,
                NowMs = 0, DisplayState = [], InputState = [],
            });
            Check(prepared.Ok, prepared.Error ?? "prepare failed");
            var preview = host.Handle(Request("evaluateCandidatePreview", window: snapshot));
            Check(preview.Ok && Nodes(preview.Serialized!).Single(node => node.Kind == "Label").Props.Text == text);
            Check(host.Handle(Request("invokeHandler", handler: oldHandler)).Invoked == (i == 0));
            Check(host.Handle(Request("commitAssembly")).Ok);
            Check(host.PendingUnloadCount == 0, "old ALC still rooted");
            Check(host.UnloadedCount == i + 1);
            var rendered = host.Handle(Request("evaluate", window: snapshot));
            Check(Nodes(rendered.Serialized!).Single(node => node.Kind == "Label").Props.Text == text);
            Check(host.Handle(Request("invokeHandler", handler: oldHandler)).Invoked == false);
        }
        Check(host.Handle(Request("shutdownAssemblies")).Ok);
        host.Dispose();
        Check(host.PendingUnloadCount == 0 && host.UnloadedCount == 21);
        var lines = File.ReadAllLines(marker);
        Check(lines.Count(line => line.StartsWith("dispose:")) == 21);
        Check(lines.Count(line => line.StartsWith("disable:")) == 21);
    }
    finally
    {
        Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_MARKER", null);
        Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_TEXT", null);
        File.Delete(marker);
    }
});

Test("missing, broken, wrong entry and managed failures preserve active assembly and allow retry", () =>
{
    var broken = Path.GetTempFileName();
    File.WriteAllText(broken, "not an assembly");
    Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_TEXT", "retained");
    try
    {
        using var host = new ConfigurationHost(typeof(ReloadFixtureConfig).Assembly.Location);
        Check(host.Handle(Request("lifecycleEnable")).Ok);
        ExternalRuntimeResponse Prepare(string path) => host.Handle(new()
        {
            RequestId = 987, Kind = "prepareAssembly", ConfigPath = path, NowMs = 0, DisplayState = [], InputState = [],
        });
        foreach (var path in new[] { broken + ".missing", broken, typeof(IWindowConfig).Assembly.Location })
        {
            var result = Prepare(path);
            Check(!result.Ok && result.RequestId == 987 && result.Kind == "prepareAssembly");
            Check(host.PendingUnloadCount == 0);
        }
        foreach (var failure in new[] { "constructor", "enable", "render" })
        {
            Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_FAILURE", failure);
            var result = Prepare(typeof(ReloadFixtureConfig).Assembly.Location);
            if (failure == "render")
            {
                Check(result.Ok);
                Check(!host.Handle(Request("evaluateCandidatePreview", window: snapshot)).Ok);
                Check(host.Handle(Request("abortAssembly")).Ok);
            }
            else Check(!result.Ok && result.Error!.Contains(failure));
            Check(host.PendingUnloadCount == 0, $"failed {failure} ALC leaked");
            var old = host.Handle(Request("evaluate", window: snapshot));
            Check(old.Ok && Nodes(old.Serialized!).Single(node => node.Kind == "Label").Props.Text == "retained");
        }
        Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_FAILURE", "disable");
        Check(Prepare(typeof(ReloadFixtureConfig).Assembly.Location).Ok);
        Check(host.Handle(Request("commitAssembly")).Ok);
        Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_FAILURE", null);
        Check(Prepare(typeof(ReloadFixtureConfig).Assembly.Location).Ok);
        Check(host.Handle(Request("commitAssembly")).Ok); // Throwing OnDisable still disposes/unloads.
        Check(host.PendingUnloadCount == 0);
        Check(!host.Handle(Request("commitAssembly")).Ok);
        Check(!host.Handle(Request("evaluateCandidatePreview", window: snapshot)).Ok);
        Check(host.HandleJson("{\"requestId\":123,\"kind\":\"prepareAssembly\"}").RequestId == 123);
    }
    finally
    {
        Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_FAILURE", null);
        Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_TEXT", null);
        File.Delete(broken);
    }
});

[MethodImpl(MethodImplOptions.NoInlining)]
void ClearLeakedSubscription()
{
    var cleanup = (Action)AppDomain.CurrentDomain.GetData("ShojiWM.TestCleanup")!;
    AppDomain.CurrentDomain.SetData("ShojiWM.TestCleanup", null);
    cleanup();
}

Test("unload failure is diagnosed and prevents accumulating leaked generations", () =>
{
    try
    {
        Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_FAILURE", "leak");
        using var host = new ConfigurationHost(typeof(ReloadFixtureConfig).Assembly.Location);
        Check(host.Handle(Request("lifecycleEnable")).Ok);
        Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_FAILURE", null);
        ExternalRuntimeResponse Prepare() => host.Handle(new()
        {
            RequestId = 1001, Kind = "prepareAssembly", ConfigPath = typeof(ReloadFixtureConfig).Assembly.Location,
            NowMs = 0, DisplayState = [], InputState = [],
        });
        Check(Prepare().Ok);
        Check(host.Handle(Request("commitAssembly")).Ok);
        Check(host.PendingUnloadCount == 1);
        var rejected = Prepare();
        Check(!rejected.Ok && rejected.Error!.StartsWith("unload:") && host.PendingUnloadCount == 1);
        Check(host.Handle(Request("evaluate", window: snapshot)).Ok);
        ClearLeakedSubscription();
        host.VerifyUnloads();
        Check(host.PendingUnloadCount == 0);
        Check(Prepare().Ok);
        Check(host.Handle(Request("abortAssembly")).Ok);
        Check(host.PendingUnloadCount == 0);
    }
    finally
    {
        Environment.SetEnvironmentVariable("SHOJI_TEST_CONFIG_FAILURE", null);
        if (AppDomain.CurrentDomain.GetData("ShojiWM.TestCleanup") is not null) ClearLeakedSubscription();
    }
});

Console.WriteLine($"{passed} tests passed");

sealed class LifecycleConfig : IWindowConfig
{
    public int Enables { get; private set; }
    public int Disables { get; private set; }
    public void OnEnable(string reason) => Enables++;
    public void OnDisable(string reason) => Disables++;
    public CompositionNode RenderWindow(WaylandWindow window, RenderContext context) => new ClientWindow();
}

sealed class HandlerConfig : IWindowConfig
{
    public int LiveClicks { get; private set; }
    public int PreviewClicks { get; private set; }
    public bool ShowButton { get; set; } = true;
    public CompositionNode RenderWindow(WaylandWindow window, RenderContext context)
    {
        if (context.IsPreview) window.Close(); // Preview commands are suppressed.
        return new WindowBorder
        {
            Children = ShowButton
                ? [new ClientWindow(), new Button { OnClick = () => { if (context.IsPreview) PreviewClicks++; else LiveClicks++; } }]
                : [new ClientWindow()],
        };
    }
}
