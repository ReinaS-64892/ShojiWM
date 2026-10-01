# C# runtime backend (MVP)

ShojiWM still uses Rust/Smithay for the compositor, renderer and input. The
TypeScript runtime remains the default. The opt-in .NET worker runs a user
config assembly in a separate process and returns existing decoration trees.
No CoreCLR hosting, P/Invoke, ASP.NET, or external NuGet packages are used.

## Build and run

Install .NET SDK 10, Python 3, and the compositor's existing Rust dependencies.
The worker needs only `Microsoft.NETCore.App` at runtime. On Arch, the split SDK
packaging may also request `aspnet-targeting-pack` during restore; it is an SDK
pack, not an application/framework dependency of this runtime.

From the repository root:

```sh
python3 tools/generate-dotnet-bindings.py
dotnet build dotnet/ShojiWM.Tests/ShojiWM.Tests.csproj --disable-build-servers -m:1
cargo build -p shoji_wm
./target/debug/shoji_wm --runtime dotnet \
  --decoration-runtime "$PWD/dotnet/ShojiWM.Runtime/bin/Debug/net10.0/ShojiWM.Runtime" \
  --config "$PWD/dotnet/ShojiWM.Example/bin/Debug/net10.0/ShojiWM.Example.dll"
```

The existing `--config` and `--decoration-runtime` flags select the assembly and
worker executable. Their existing `SHOJI_CONFIG` / `SHOJI_DECORATION_RUNTIME`
fallbacks also apply. `--runtime typescript` or omitting `--runtime` preserves
the existing TypeScript configuration and V8 fast paths. `--runtime dotnet`
requires an explicit config assembly path. A worker executable named
`ShojiWM.Runtime` on PATH is used when `--decoration-runtime` is omitted. Use the
apphost executable, not the runtime DLL, for that argument.

The .NET example decorates each window with its title/app ID, a focus-colored
border, and a close button that calls a C# delegate. The Rust compositor handles
layout, drawing, move/resize hit testing, and applying the emitted close action.

### Displaying an application

The example supplies decorations only. It does not start applications, a panel,
wallpaper, or the TypeScript config's terminal/launcher key bindings. An empty
compositor screen is expected until a client connects.

Without `--tty`, the command above uses the WInit backend and requires an
existing graphical session (`WAYLAND_DISPLAY` or `DISPLAY`). The development
guide currently recommends the TTY backend because WInit can be unreliable.
For a nested test, also set `SHOJI_PUBLISH_ACTIVATION_ENV=0` on the compositor
command to keep its display variables out of the host session's D-Bus/systemd
activation environment.

ShojiWM logs to `~/shoji_wm/logs/latest.log`. After starting it, find its client
socket in another terminal:

```sh
rg 'starting shoji_wm|listening for Wayland clients|failed|panic' ~/shoji_wm/logs/latest.log
```

A line such as `socket=wayland-2` identifies the **new compositor**, which may
use a different name on every launch. Start a Wayland application against that
socket, using the name from your log:

```sh
env -u DISPLAY WAYLAND_DISPLAY=wayland-2 kitty
```

Use a locally installed Wayland terminal (for example `foot`) if `kitty` is not
installed. This command inherits `XDG_RUNTIME_DIR`; it must be the same directory
used by ShojiWM. The C# title bar and close button appear around that application.
Ordinary worker/config console messages go to the compositor's terminal stderr,
not its Rust tracing log. Capture stderr separately when diagnosing the worker.

### Starting from a real TTY

Log in as your normal user on a free virtual terminal, then run from the repo:

```sh
./target/debug/shoji_wm --tty --runtime dotnet \
  --decoration-runtime "$PWD/dotnet/ShojiWM.Runtime/bin/Debug/net10.0/ShojiWM.Runtime" \
  --config "$PWD/dotnet/ShojiWM.Example/bin/Debug/net10.0/ShojiWM.Example.dll" \
  2>/tmp/shoji-dotnet-stderr.log
```

`--tty` selects DRM/KMS via libseat; it does not require `DISPLAY` or an inherited
`WAYLAND_DISPLAY`. It does require `XDG_RUNTIME_DIR` for its own Wayland socket
and an active local seat/login session. Do not run the compositor with `sudo`.
On Arch with a normal systemd/PAM login, the runtime directory is usually
`/run/user/$(id -u)`. If the variable is missing, check that directory is owned
by your user and has mode `0700`, then export the existing directory:

```sh
export XDG_RUNTIME_DIR="/run/user/$(id -u)"
```

If that directory is absent, fix the login/session setup rather than creating a
different temporary directory for each process. If the error specifically names
a missing D-Bus session, run the same compositor command under `dbus-run-session`.
Neither setting creates a graphical parent display for WInit; `--tty` is still
needed. Read both `latest.log` and `/tmp/shoji-dotnet-stderr.log` for seat/GPU or
worker errors. Launch the demo client from a second terminal/TTY with the socket
name and runtime directory from the log; no C# launch key binding exists yet.

## Write a config

Create a .NET 10 class library with nullable reference types enabled and a
project reference to `ShojiWM/ShojiWM.csproj`. Export exactly one concrete
`IWindowConfig` with a public parameterless constructor:

```csharp
using ShojiWM;

public sealed class MyConfig : IWindowConfig
{
    public CompositionNode RenderWindow(WaylandWindow window, RenderContext context)
        => new WindowBorder
        {
            Children =
            [
                new Box
                {
                    Direction = LayoutDirection.Column,
                    Children =
                    [
                        new Label { Text = window.Title, Style = new() { Height = 28 } },
                        new ClientWindow(),
                    ],
                },
            ],
        };
}
```

Build the library and pass its DLL to `--config`. Keep its dependencies and
`.deps.json` beside the DLL. The worker uses `AssemblyDependencyResolver` and a
collectible `AssemblyLoadContext`, sharing the public `ShojiWM` assembly identity
with the host. Config code never runs in the compositor process. Collectibility
provides a future reload boundary; hot reload and automatic worker restart are
not implemented in this milestone. Restart ShojiWM after rebuilding a config.

`WaylandWindow` exposes typed snapshot values and close/maximize/minimize/focus/
fullscreen commands. `RenderContext` supplies time, preview status, and typed
display/input snapshots. `WindowBorder`, `Box`, `Label`, `Button`, `Image`,
`ClientWindow`, and `Style` form the public composition API. Use
`Button.OnClick = window.Close` or an ordinary synchronous `Action`. Generated
wire classes live in `ShojiWM.Wire`; config code does not need JSON dictionaries.
Numeric style lengths are integer logical pixels, matching the Rust decoder.
String dimension keywords are represented on the wire but currently rejected
by the existing Rust decoder. This is also a constraint of the TS wire path.
Image paths are currently passed through; use absolute paths.

## Runtime boundary and protocol

The existing `DecorationEvaluator` trait and `DecorationRuntimeEvaluator` enum
are the backend boundary. The enum now has `DotNet` alongside `Embedded` and
`Static`. `EmbeddedDecorationEvaluator`/`EmbeddedRuntime` continue using
RustyScript, deno_core and V8, with their native composition/effect/interaction/
scheduler paths, native patches and effect caches intact. Their TS config
import/preload, signal reconciliation, scheduler, lifecycle reload and handler
registries are unchanged.

The new `DotNetDecorationEvaluator` uses `ExternalRuntimeRequest` and
`ExternalRuntimeResponse` from `external_protocol.rs`. Pipe framing and process
management are confined to `external_transport.rs`. The C# `RuntimeSession` is
independent of `NdjsonTransport`, so the same semantics can later use a socket
or another transport.

Protocol v1 is UTF-8 NDJSON over stdin/stdout, one synchronous response per
request. All field names are camelCase, and `requestId` is an unsigned 64-bit
integer echoed exactly. Config `Console.WriteLine` and worker logs are redirected
to stderr before assembly loading; stdout carries protocol data exclusively.
Binary writers to standard output from config code are outside this contract.

```json
{"requestId":42,"kind":"evaluate","snapshot":{"id":"1","title":"Kitty","...":"see fixtures/window.json for the full snapshot"},"windowId":"1","nowMs":1234,"displayState":{},"inputState":{}}
```

```json
{"requestId":42,"kind":"evaluate","ok":true,"serialized":{"kind":"WindowBorder","nodeId":"root","props":{},"children":[{"kind":"Window","props":{},"children":[]}]},"actions":[]}
```

The snapshot above is abbreviated; the full required fields are generated from
`WaylandWindowSnapshot`, and a complete example is in `fixtures/window.json`.
`serialized` reuses the existing handler-response vocabulary. Its shape is
`WireDecorationNode` (`kind`, optional `nodeId`, `props`, `children`).
`ClientWindow` serializes as `Window`, as it does in TypeScript. Rust calls the
existing `TryFrom<WireDecorationNode>` decode and `DecorationTree::validate`;
there must be exactly one childless client slot.

Supported requests:

| Kind | Additional data | Behavior |
| --- | --- | --- |
| `drainPreload` | none | Confirms the config assembly was loaded |
| `lifecycleEnable` | optional `reason` | Calls `OnEnable` once |
| `lifecycleDisable` | optional `reason` | Calls `OnDisable`, clears windows/handlers |
| `evaluate` | `snapshot` | Renders and stores the latest snapshot/handlers |
| `evaluatePreview` | `snapshot` | Renders without replacing live handlers or emitting commands |
| `evaluateCached` | `snapshot` or known `windowId` | Renders using the supplied/latest snapshot |
| `invokeHandler` | `windowId`, `handlerId` | Runs a delegate, renders the updated tree, returns actions |
| `windowClosed` | `windowId` | Clears that window's registry and calls `OnWindowClosed` |

Requests include `nowMs`, `displayState`, and `inputState`. Successful responses
contain `requestId`, `kind`, `ok: true`, optional `serialized`/`invoked`, and
`actions` using the existing `RuntimeWindowAction` JSON. Failures use `ok: false`
and `error`. Unknown kinds and malformed requests produce a failure response;
if a malformed envelope has a readable ID/kind, those are preserved. Completely
unparseable JSON returns ID 0 / `protocolError`.

Callbacks serialize to the existing descriptor
`{"kind":"runtime-handler","id":"handler-42"}`. IDs stay stable at a
window/tree path while that handler remains present, and are scoped to the
window in dispatch. Preview does not replace live registrations. Rendering
replaces obsolete delegates; window close/lifecycle disable releases them.
The MVP assumes synchronous config/delegate execution on the worker thread.

Rust caches the last decoded evaluation. An unchanged cached request returns
no tree and emits no previously consumed actions. Changed snapshots and forced
reevaluations render a complete tree. Scheduler ticks currently use the trait's
no-op implementation and send no IPC; there are no timers or proactive dirty
notifications in the C# MVP.

Messages are limited to 8 MiB and decoded trees to the serializers' default
depth limits. A Rust pipe thread bounds both blocked writes and reads with a
two-second exchange deadline. On EOF, timeout, malformed response, ID/kind
mismatch, or worker failure, the transport kills/reaps the worker and reports
a runtime error. Protocol failures quarantine that backend until ShojiWM is
restarted, avoiding a process-spawn loop per frame. Existing compositor error/
static-decoration fallback paths apply. Normal final evaluator drop attempts
a bounded (100 ms) `lifecycleDisable` before terminating the process; cleanup
callbacks are best effort when the worker has failed.

## Binding generation

The source of truth for this MVP is the existing Rust serde DTOs, because the
actual decoder has narrower numeric/dimension/action semantics than the full
reactive TypeScript API. `tools/generate-dotnet-bindings.py` follows the DTO
dependency closure from the external envelope, through snapshots, props and
styles, and generates `ShojiWM/Generated/Protocol.g.cs`. Generated files carry
`// <auto-generated />` and are ignored by Git. Building the `ShojiWM` project
runs the generator automatically, including on a clean checkout. Python 3 is
therefore also a build prerequisite. Run the generator manually before browsing
the DTOs in an IDE without building.

Automatically generated: envelope fields, window/decoration/constraint/icon/
interaction snapshots, display/input snapshots, node/props/style/border/
transform DTOs, action descriptors, and their unit enums with exact serde
spellings. `Option<T>` becomes nullable, numbers retain Rust widths, and
`Vec<u8>` becomes `List<byte>` to preserve JSON number arrays (C# `byte[]` would
use base64).

Handwritten: the public node/config/command API, process transport/session,
assembly loader, and converters for the small untagged dimension/font/click/
resize unions. Effects and animation payloads remain opaque `JsonElement` in
the wire layer, without a supported public API. Primitive composition children
are excluded, matching the existing Rust decoder's rejection. This generator
is intentionally a restricted parser for named structs, unit enums and the
selected serde attributes, not a general Rust/TypeScript translator. New
unsupported types or attributes fail generation explicitly.

```sh
python3 tools/generate-dotnet-bindings.py
python3 tools/generate-dotnet-bindings.py --check
python3 tools/test-dotnet-generator.py
```

Generation is deterministic. CI generates the output, verifies it with
`--check`, and runs generator tests that verify deterministic output, new fields
propagating, and unsupported types failing. Rust DTO definitions and the
generator are versioned; generated C# output is not.

## Tests

```sh
dotnet build dotnet/ShojiWM.Tests/ShojiWM.Tests.csproj --disable-build-servers -m:1
dotnet run --project dotnet/ShojiWM.Tests/ShojiWM.Tests.csproj --no-build
python3 tools/test-dotnet-worker.py
cargo test -p shoji_wm
cargo test --workspace
```

When running in a sandbox or a desktop session, isolate the default TS config's
IPC socket from the session runtime directory and disable activation-environment
publication for tests:

```sh
install -d -m 700 /tmp/shoji-runtime-tests
XDG_RUNTIME_DIR=/tmp/shoji-runtime-tests SHOJI_PUBLISH_ACTIVATION_ENV=0 cargo test --workspace
```

Socket tests still need an execution environment that permits Unix sockets.

The .NET test executable requires no test-framework NuGet packages and returns
a failing exit code on any assertion failure. It covers serialization,
optionality, literal enum mapping, full `requestId` precision, composition,
handler cleanup/stability, malformed/unknown messages, lifecycle, assembly
loading, and NDJSON. Python tests the actual worker process. Rust fake workers
test existing decoding/validation, caching, malformed/mismatched responses,
worker death, and blocked stdin without needing a Wayland session.

Run the opt-in Rust → actual C# → existing Rust decoder integration test after
building the .NET projects:

```sh
SHOJI_TEST_DOTNET_RUNTIME="$PWD/dotnet/ShojiWM.Runtime/bin/Debug/net10.0/ShojiWM.Runtime" \
SHOJI_TEST_DOTNET_CONFIG="$PWD/dotnet/ShojiWM.Example/bin/Debug/net10.0/ShojiWM.Example.dll" \
cargo test -p shoji_wm real_dotnet_worker_decodes_example_and_dispatches_delegate -- --ignored
```

## Remaining work and performance

Not ported: reactive signals/computed/state hooks, node reconciliation/patches,
poll/timer scheduler, pointer/gesture callbacks, hover/active delegates,
managed-window layout/state and animations, lifecycle persisted state/hot
reload, key bindings, workspace/output/input configuration, shader/effect API,
process/env/IPC controllers, and asset resolution. This example is decoration
composition with window commands, not a port of the default hybrid tiling WM.

Next steps are a managed-window result/config API, scheduler dirty notification
semantics, hover/active dispatch, and an explicit process-restart/reload lifecycle.
Reactive values should evaluate to ordinary DTO values; signal engine internals
should stay out of the wire protocol. The existing shared DTO source can later
move to a versioned protocol crate/schema if more backends need it.

The .NET path serializes complete snapshots/trees and waits synchronously for
IPC, so it has higher latency and allocations than the embedded V8 native
paths. Even with bounded I/O, a slow config can stall a compositor turn for up
to the deadline. Benchmark and introduce asynchronous evaluations/dirty batches
before making this backend a default or targeting high-refresh workloads.
Socket/binary framing and source-generated JSON serializers remain follow-ups.

Before upstreaming beyond the MVP, agree on protocol capability/version
negotiation, request deadlines/recovery policy, supported API scope, install/
packaging locations, and whether the restricted generator should become a
schema exporter. Keep the Rust/C# roundtrip test in a compositor CI job with the
existing Smithay/V8 build dependencies; a real Wayland visual smoke test is
still needed for release validation.
