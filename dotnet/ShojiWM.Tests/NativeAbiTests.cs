using System.Text;
using System.Text.Json;
using ShojiWM.Runtime;

internal static unsafe class NativeAbiTests
{
    public static void Run(string config, string request)
    {
        delegate* unmanaged<byte*, int, nint*, byte**, int*, int> create = &NativeEntryPoint.CreateHost;
        delegate* unmanaged<nint, byte*, int, byte**, int*, int> invoke = &NativeEntryPoint.Invoke;
        delegate* unmanaged<nint, byte**, int*, int> destroy = &NativeEntryPoint.DestroyHost;
        delegate* unmanaged<byte*, void> free = &NativeEntryPoint.FreeBuffer;
        static void Check(bool value) { if (!value) throw new InvalidOperationException("native ABI assertion"); }
        var path = Encoding.UTF8.GetBytes(config);
        nint handle = 0;
        byte* response = null;
        var length = 0;
        fixed (byte* input = path) Check(create(input, path.Length, &handle, &response, &length) == 0 && handle != 0 && response == null);
        try
        {
            var json = Encoding.UTF8.GetBytes(request);
            fixed (byte* input = json)
            {
                Check(invoke(handle, input, json.Length, &response, &length) == 0);
                try
                {
                    using var document = JsonDocument.Parse(new ReadOnlySpan<byte>(response, length).ToArray());
                    Check(document.RootElement.GetProperty("requestId").GetUInt64() == 42);
                    Check(document.RootElement.GetProperty("ok").GetBoolean());
                }
                finally { free(response); }
                Check(invoke(handle, input, NativeEntryPoint.MaxMessageBytes + 1, &response, &length) != 0);
                free(response);
            }
            var invalid = new byte[] { 0xff };
            fixed (byte* input = invalid)
            {
                Check(invoke(handle, input, 1, &response, &length) != 0);
                free(response);
            }
            Check(invoke(0, null, 0, &response, &length) != 0);
            free(response);
        }
        finally { Check(destroy(handle, &response, &length) == 0); free(response); }
        fixed (byte* input = path)
        {
            Check(create(input, NativeEntryPoint.MaxMessageBytes + 1, &handle, &response, &length) != 0 && handle == 0);
            free(response);
        }
        free(null);
    }
}
