namespace ShojiWM.Runtime;

internal static class Program
{
    public static int Main(string[] args)
    {
        // Capture protocol streams before loading any user code. Console logs
        // (including config constructors) go to stderr; stdout is NDJSON only.
        var protocolOutput = Console.OpenStandardOutput();
        Console.SetOut(Console.Error);
        try
        {
            if (args.Length != 2 || args[0] != "--config")
                throw new ArgumentException("usage: ShojiWM.Runtime --config /path/to/Config.dll");
            using var host = new ConfigurationHost(args[1]);
            var transport = new NdjsonTransport(Console.OpenStandardInput(), protocolOutput);
            while (transport.ReadFrame() is string frame) transport.WriteFrame(host.HandleJson(frame));
            return 0;
        }
        catch (Exception error)
        {
            Console.Error.WriteLine($"ShojiWM .NET runtime: {error}");
            return 1;
        }
    }
}
