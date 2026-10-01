using System.Text;
using System.Text.Json;
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

Test("NDJSON framing with Unicode, coalesced requests and clean EOF", () =>
{
    var request = Json(Request("evaluate", window: snapshot));
    using var input = new MemoryStream(Encoding.UTF8.GetBytes(request + "\n{}\n"));
    using var output = new MemoryStream();
    var transport = new NdjsonTransport(input, output);
    Check(transport.ReadFrame() == request && transport.ReadFrame() == "{}" && transport.ReadFrame() is null);
    transport.WriteFrame(new() { RequestId = 123, Kind = "evaluate", Ok = true });
    Check(Encoding.UTF8.GetString(output.ToArray()).EndsWith('\n'));
    Check(Parse<ExternalRuntimeResponse>(Encoding.UTF8.GetString(output.ToArray())).RequestId == 123);
});

Test("config assembly loading shares API identity", () =>
{
    var loader = new ConfigLoader(typeof(ExampleConfig).Assembly.Location);
    Check(loader.CreateConfig() is IWindowConfig);
    loader.Unload();
});

Test("transport rejects partial and oversized frames", () =>
{
    using var output = new MemoryStream();
    foreach (var bytes in new[] { Encoding.UTF8.GetBytes("{}"), new byte[NdjsonTransport.MaxFrameBytes] })
    {
        using var input = new MemoryStream(bytes);
        try { new NdjsonTransport(input, output).ReadFrame(); }
        catch (InvalidDataException) { continue; }
        throw new InvalidOperationException("invalid frame accepted");
    }
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
