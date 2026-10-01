using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using ShojiWM.Wire;

namespace ShojiWM.Runtime;

/// Permanent bootstrap exports. All calls for a handle belong to its creating
/// OS thread. Inputs are borrowed for the call; outputs must use FreeBuffer.
public static unsafe class NativeEntryPoint
{
    public const int MaxMessageBytes = 8 * 1024 * 1024;
    // Construct inside the guarded call, avoiding an allocating type initializer
    // that could fail before an unmanaged entry point's catch block is entered.
    private static UTF8Encoding Utf8 => new(false, true);
    private sealed class HostOwner(string path)
    {
        public readonly ConfigurationHost Host = new(path);
        public readonly int ThreadId = Environment.CurrentManagedThreadId;
    }

    [UnmanagedCallersOnly]
    public static int CreateHost(byte* path, int length, nint* handle, byte** error, int* errorLength)
    {
        if (handle == null || error == null || errorLength == null) return -1;
        *handle = 0; *error = null; *errorLength = 0;
        try
        {
            var owner = new HostOwner(Read(path, length));
            try { *handle = GCHandle.ToIntPtr(GCHandle.Alloc(owner)); }
            catch { owner.Host.Dispose(); throw; }
            return 0;
        }
        catch (Exception exception) { return Failure(exception, error, errorLength); }
    }

    [UnmanagedCallersOnly]
    public static int Invoke(nint handle, byte* request, int length, byte** response, int* responseLength)
    {
        if (response == null || responseLength == null) return -1;
        *response = null; *responseLength = 0;
        try
        {
            var host = Owner(handle).Host;
            var json = Read(request, length);
            try
            {
                var result = host.HandleJson(json);
                Write(JsonSerializer.SerializeToUtf8Bytes(result, WireJson.Options), response, responseLength);
            }
            catch (Exception exception)
            {
                // Serialization/size errors are semantic rejection, too. A
                // candidate with excessive output must not disable the host
                // or prevent Rust from aborting it and retaining active config.
                ulong id = 0;
                var kind = "protocolError";
                using var document = JsonDocument.Parse(json);
                if (document.RootElement.TryGetProperty("requestId", out var value)) value.TryGetUInt64(out id);
                if (document.RootElement.TryGetProperty("kind", out value) && value.ValueKind == JsonValueKind.String) kind = value.GetString()!;
                var failure = new ExternalRuntimeResponse { RequestId = id, Kind = kind, Ok = false, Error = Diagnostic(exception), Actions = [] };
                Write(JsonSerializer.SerializeToUtf8Bytes(failure, WireJson.Options), response, responseLength);
            }
            return 0;
        }
        catch (Exception exception) { return Failure(exception, response, responseLength); }
    }

    [UnmanagedCallersOnly]
    public static int DestroyHost(nint handle, byte** error, int* errorLength)
    {
        if (error == null || errorLength == null) return -1;
        *error = null; *errorLength = 0;
        try
        {
            var owner = Owner(handle);
            try { owner.Host.Dispose(); }
            finally { GCHandle.FromIntPtr(handle).Free(); }
            return 0;
        }
        catch (Exception exception) { return Failure(exception, error, errorLength); }
    }

    [UnmanagedCallersOnly]
    public static void FreeBuffer(byte* buffer)
    {
        try { NativeMemory.Free(buffer); }
        catch (Exception) { /* No managed exception may cross even this void export. */ }
    }

    private static HostOwner Owner(nint handle)
    {
        if (handle == 0) throw new ArgumentException("null host handle");
        var owner = (HostOwner)(GCHandle.FromIntPtr(handle).Target ?? throw new ObjectDisposedException("host"));
        if (owner.ThreadId != Environment.CurrentManagedThreadId) throw new InvalidOperationException("host called from a different thread");
        return owner;
    }

    private static string Read(byte* bytes, int length)
    {
        if (bytes == null || length < 0 || length > MaxMessageBytes) throw new ArgumentException("invalid UTF-8 input or message exceeds 8 MiB limit");
        return Utf8.GetString(new ReadOnlySpan<byte>(bytes, length));
    }

    private static void Write(byte[] bytes, byte** output, int* length)
    {
        if (bytes.Length > MaxMessageBytes) throw new InvalidDataException("response exceeds 8 MiB message limit");
        var buffer = (byte*)NativeMemory.Alloc((nuint)Math.Max(1, bytes.Length));
        if (buffer == null) throw new OutOfMemoryException();
        bytes.CopyTo(new Span<byte>(buffer, bytes.Length));
        *output = buffer; *length = bytes.Length;
    }

    private static int Failure(Exception error, byte** output, int* length)
    {
        // Flatten exceptions here; never retain config exception/reflection roots.
        try { Write(Utf8.GetBytes(Diagnostic(error)), output, length); }
        catch { return -2; }
        return -1;
    }

    private static string Diagnostic(Exception error)
    {
        var message = error.Message;
        return message.Length <= 4096 ? message : message[..4096];
    }
}
