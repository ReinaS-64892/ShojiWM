using System.Text;
using System.Text.Json;
using ShojiWM.Wire;

namespace ShojiWM.Runtime;

public sealed class NdjsonTransport(Stream input, Stream output)
{
    public const int MaxFrameBytes = 8 * 1024 * 1024;
    private readonly byte[] buffer = new byte[4096];
    private int offset;
    private int available;
    private static readonly UTF8Encoding Utf8 = new(false, true);

    public string? ReadFrame()
    {
        using var frame = new MemoryStream();
        while (true)
        {
            if (offset == available)
            {
                available = input.Read(buffer);
                offset = 0;
                if (available == 0)
                {
                    if (frame.Length == 0) return null;
                    throw new InvalidDataException("incomplete NDJSON frame");
                }
            }
            var newline = Array.IndexOf(buffer, (byte)'\n', offset, available - offset);
            var end = newline < 0 ? available : newline;
            if (frame.Length + end - offset >= MaxFrameBytes) throw new InvalidDataException("NDJSON frame exceeds 8 MiB limit");
            frame.Write(buffer, offset, end - offset);
            offset = newline < 0 ? end : end + 1;
            if (newline >= 0) return Utf8.GetString(frame.GetBuffer(), 0, checked((int)frame.Length));
        }
    }

    public void WriteFrame(ExternalRuntimeResponse response)
    {
        var bytes = JsonSerializer.SerializeToUtf8Bytes(response, WireJson.Options);
        if (bytes.Length >= MaxFrameBytes) throw new InvalidDataException("NDJSON response exceeds 8 MiB limit");
        output.Write(bytes);
        output.WriteByte((byte)'\n');
        output.Flush();
    }
}
