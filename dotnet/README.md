# C# runtime backend (MVP)

ShojiWM's opt-in .NET backend hosts CoreCLR **inside the Rust/Smithay compositor
process**, using hostfxr and `netcorehost 0.22.0`. TypeScript is still the default;
its RustyScript/V8 native paths and configuration lifecycle are unchanged.
No ASP.NET or external managed NuGet dependency is used.

```text
ShojiWM process
  Rust compositor → Rust channel → dotnet-runtime thread
                                    hostfxr / CoreCLR
                                      ShojiWM.Runtime.dll (permanent bootstrap)
                                        ConfigurationHost
                                          active/candidate collectible config ALCs
```

## Build and run

Install .NET SDK 10, Python 3 and the existing Rust/compositor dependencies.
The running backend needs `Microsoft.NETCore.App` and installed hostfxr.

```sh
python3 tools/generate-dotnet-bindings.py
dotnet build dotnet/ShojiWM.Tests/ShojiWM.Tests.csproj -c Release --disable-build-servers -m:1
cargo build -p shoji_wm --offline
./target/debug/shoji_wm --runtime dotnet \
  --decoration-runtime "$PWD/dotnet/ShojiWM.Runtime/bin/Release/net10.0/ShojiWM.Runtime.dll" \
  --config "$PWD/dotnet/ShojiWM.Example/bin/Release/net10.0/ShojiWM.Example.dll"
```

`--decoration-runtime` / `SHOJI_DECORATION_RUNTIME` now specify the managed
**bootstrap DLL**, not an executable. Its directory must contain
`ShojiWM.Runtime.runtimeconfig.json`, `ShojiWM.Runtime.deps.json`, `ShojiWM.dll`
and bootstrap dependencies. The SDK library build generates these files.
The previous extensionless apphost spelling resolves its sibling `.dll` for
compatibility; it is never executed. If omitted, `ShojiWM.Runtime.dll` is located
in the working directory or PATH. `--config` / `SHOJI_CONFIG` still select a user
config DLL. No new CLI switch is introduced.

Hostfxr discovery uses explicit `DOTNET_ROOT` exclusively when set. Otherwise
it checks the resolved PATH `dotnet` directory, then `/usr/share/dotnet`,
`/usr/lib/dotnet` and `/opt/dotnet`, selecting the highest installed stable numeric
version under `host/fxr`. Set `DOTNET_ROOT` for custom installations or Nix.
The runtimeconfig selects the required framework version; finding hostfxr alone
does not guarantee that .NET 10 is installed.

`netcorehost` has default features disabled and only the `net8_0` API feature
(which includes the runtimeconfig and UnmanagedCallersOnly APIs used here).
It hosts .NET 10 through the runtimeconfig. Neither `nethost` nor
`nethost-download` is enabled: no build script downloads native hosting assets.
As for other Cargo dependencies, a fresh machine must fetch/vendor the pinned
Cargo.lock dependencies once before `cargo build --offline`. Nix's existing
cargoLock supplies them; no system nethost link input is required. An optional
`dotnetRuntime` parameter in `nix/package.nix` sets the wrapper's DOTNET_ROOT;
otherwise provide it in the launch environment and build the managed DLLs
separately. The Nix derivation does not build/package C# configurations.

The example uses title/app ID, focus-colored borders and a delegate close button.

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
Config Console output uses the process's normal stdout/stderr; no stream carries protocol data. Managed host diagnostics use stderr, separately from Rust tracing.

### Starting from a real TTY

Log in as your normal user on a free virtual terminal, then run from the repo:

```sh
./target/debug/shoji_wm --tty --runtime dotnet \
  --decoration-runtime "$PWD/dotnet/ShojiWM.Runtime/bin/Debug/net10.0/ShojiWM.Runtime.dll" \
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
managed hosting errors. Launch the demo client from a second terminal/TTY with the socket
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

Build the library and pass its DLL to `--config`. Keep dependencies and
`.deps.json` beside it. `ConfigLoader` uses `AssemblyDependencyResolver` and a
collectible ALC, sharing `typeof(IWindowConfig).Assembly` from the permanent
bootstrap; user config is **never** passed to hostfxr's function-pointer loader.
Rust stages immutable dependency copies for generation lifetime.

Implement `IDisposable` or `IAsyncDisposable` for owned resources. The host calls
OnDisable, clears session callbacks/snapshots, disposes the config (async wins
if both interfaces exist), requests ALC unload and verifies weak references.
Cancel/await tasks, stop/join threads, dispose timers, unsubscribe external
subscriptions and release handles. Failed enable also receives cleanup;
constructors that throw must release their partially constructed resources.

## Development reload

`Super+Shift+R` reloads the DLL; with `--dotnet-project`, it builds first.
Automatic source reload uses the existing .NET watcher with `--dev`:

```sh
./target/debug/shoji_wm --runtime dotnet --dev \
  --decoration-runtime "$PWD/dotnet/ShojiWM.Runtime/bin/Release/net10.0/ShojiWM.Runtime.dll" \
  --config "$PWD/dotnet/ShojiWM.Example/bin/Release/net10.0/ShojiWM.Example.dll" \
  --dotnet-project "$PWD/dotnet/ShojiWM.Example/ShojiWM.Example.csproj"
```

The watcher polls every 100 ms, debounces for 400 ms, and serializes builds and
pending activations. Source/project/props/targets/JSON/DLL changes under the
project root trigger reload; bin/obj/Generated/git/build staging are excluded.
Without a project it watches the config directory and stages already built DLLs.
`dotnet build` still runs as a subprocess with a 120 s deadline and process-group
cancellation; this is independent of in-process config execution. Failed builds
include an 8 KiB diagnostic tail and keep the active config.

Reload stays prepare → enable candidate → live-window preview/Rust validation →
commit → disable/dispose old → unload/verify. Candidate errors or invalid trees
abort the candidate; old config and live handlers remain. CoreCLR/bootstrap and
the host thread are not recreated. An uncollected ALC is logged and subsequent
prepares are refused until its roots are released. Bootstrap/API changes require
a compositor restart. See [RELOAD.md](RELOAD.md) for ownership and test details.

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

## Native ABI and JSON

`ssd/dotnet/host.rs` loads bootstrap-only `[UnmanagedCallersOnly]` exports through
hostfxr. `ShojiWM.Runtime` is a library; no Program, NDJSON transport, stdin/stdout
loop or config worker executable is present.

| Export | ABI / ownership |
|---|---|
| CreateHost | borrowed UTF-8 path + int32 length; out opaque pointer-sized GCHandle and error buffer; int32 status |
| Invoke | opaque handle, borrowed UTF-8 JSON + int32 length; out response buffer + int32 length; int32 status |
| DestroyHost | opaque handle; disposes host and frees GCHandle; out error buffer/status |
| FreeBuffer | frees a buffer using the same managed allocator that created it |

Status 0 means successful ABI execution; -1 indicates an error with UTF-8 text,
-2 indicates an error whose diagnostic could not be allocated. Invoke success
can still contain semantic JSON `ok:false`. No managed exception unwinds through
the ABI. Rust owns request bytes until return; managed allocates response/error
buffers with `NativeMemory.Alloc`. Rust copies and invokes FreeBuffer via RAII,
including error paths. Never use Rust/free() to release those pointers. The
bootstrap GCHandle roots only ConfigurationHost/host ownership, not native
config delegates. DestroyHost frees it even if disposal throws.

All initialization, bootstrap loading, host creation, rendering, handler/reload
calls and destruction run on one Rust `dotnet-runtime` thread. Compositor/reload
threads send requests through a Rust channel. This preserves sequential managed
callback execution and thread affinity; config-owned async continuations/tasks
still obey normal .NET scheduling. Native exports enforce host thread identity.
Host destruction joins that thread. CoreCLR and loaded hostfxr are retained for
process lifetime; closing the initialization context is not CLR shutdown.

The existing `ExternalRuntimeRequest` / `ExternalRuntimeResponse` names and
camelCase JSON representation remain. They now mean semantic JSON messages,
not an external-process transport. There is no newline framing. The 8 MiB limit
applies to input and output byte buffers. RequestId/kind correlation is checked.
`serialized` remains a WireDecorationNode, decoded and validated with the same
Rust path (exactly one childless Window client slot). No direct FFI DTO redesign
or shared-language/backend trait was introduced.

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
| `prepareAssembly` | `configPath` | Loads/initializes one candidate ALC without switching the active config |
| `evaluateCandidatePreview` | `snapshot` | Renders candidate for Rust validation without live registration/actions |
| `commitAssembly` | none | Switches to candidate and disposes/unloads the previous generation |
| `abortAssembly` | none | Disposes/unloads the candidate, retaining the active config |
| `shutdownAssemblies` | none | Disposes/unloads all config generations before host disposal |

Failures use a correlated `ok:false` and error string. Unparseable JSON returns
ID 0 / protocolError. Normal managed exceptions keep the host usable; protocol
or ABI failures are reported without a worker quarantine/restart loop.
Handler descriptors remain `{ "kind":"runtime-handler", "id":"handler-…" }`;
IDs are window/path stable within a generation and cannot invoke a newer
session's delegate. Preview suppresses commands/live registration. Rust caches
evaluations and does not reapply consumed actions. Full C# scheduler/reactivity,
keybindings, output/workspace management, shaders/animation and proactive dirty
notifications remain outside the MVP; no C# APIs were generalized in this change.

## Fault domain

C# config now runs **in the compositor process**. There is no 2 s managed-call
deadline, thread abandonment, worker kill/restart or CLR restart. A forever
blocked constructor/callback/disposal/finalizer can block the host and compositor
work waiting on it; native faults, Environment.FailFast or process exit can end
ShojiWM. Exception rollback and cooperative ALC leak detection do not provide
process fault isolation. Shared/static side effects are not transactionally
restored. GC verification runs only at lifecycle boundaries, at most three
collect/finalizer/collect passes; a nonreturning finalizer can still hang them.

## Binding generation

The source of truth for this MVP is the existing Rust serde DTOs, because the
actual decoder has narrower numeric/dimension/action semantics than the full
reactive TypeScript API. `tools/generate-dotnet-bindings.py` follows the DTO
dependency closure from the JSON envelope, through snapshots, props and
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

Handwritten: the public node/config/command API, native bootstrap/session,
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
dotnet build dotnet/ShojiWM.Tests/ShojiWM.Tests.csproj -c Release --disable-build-servers -m:1
dotnet run --project dotnet/ShojiWM.Tests/ShojiWM.Tests.csproj -c Release --no-build
python3 tools/generate-dotnet-bindings.py --check
python3 tools/test-dotnet-generator.py
cargo test -p shoji_wm --offline 'ssd::dotnet'
cargo test --workspace --offline
cargo build -p shoji_wm --offline
```

Run the real hostfxr test harness (no Wayland/V8 build needed):

```sh
cargo run --locked --offline --manifest-path dotnet/NativeHost.Tests/Cargo.toml -- \
  "$PWD/dotnet/ShojiWM.Runtime/bin/Release/net10.0/ShojiWM.Runtime.dll" \
  "$PWD/dotnet/ShojiWM.Tests/bin/Release/net10.0/ShojiWM.Tests.dll"
```

NativeHost.Tests is a test-only executable that compiles production host.rs by
path, not a separated backend crate. CI runs it with .NET 10 and Rust installed.
It checks 20 swaps, callback thread identity, handlers/actions, rollback,
cooperative leak/refusal/root-release/retry, byte limits and absence of child
processes. Managed tests additionally inspect WeakReference collection and
resource disposal. Native export tests call UCO functions through function
pointers and check exception containment/buffer frees.

The compositor integration tests exercise the real existing Rust decoder/layout,
source build/watcher/rollback, invalid trees, staging lifetimes and shared host
identity. Enable them explicitly (same bootstrap DLL path for both):

```sh
SHOJI_TEST_DOTNET_RUNTIME="$PWD/dotnet/ShojiWM.Runtime/bin/Release/net10.0/ShojiWM.Runtime.dll" \
SHOJI_TEST_DOTNET_CONFIG="$PWD/dotnet/ShojiWM.Example/bin/Release/net10.0/ShojiWM.Example.dll" \
cargo test -p shoji_wm --offline 'ssd::dotnet' -- --ignored --nocapture --test-threads=1
```

Actual visual/TTY reload and long-running RSS measurements need separate
manual validation. Future optimizations can reduce JSON allocations, use
System.Text.Json source generation, or introduce direct FFI DTOs independently
of this hosting change.
