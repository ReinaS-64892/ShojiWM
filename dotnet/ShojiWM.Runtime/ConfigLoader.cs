using System.Reflection;
using System.Runtime.Loader;

namespace ShojiWM.Runtime;

public sealed class ConfigLoader : AssemblyLoadContext
{
    private readonly AssemblyDependencyResolver resolver;
    private readonly string path;

    public ConfigLoader(string configPath) : base(isCollectible: true)
    {
        path = Path.GetFullPath(configPath);
        if (!File.Exists(path)) throw new FileNotFoundException("config assembly does not exist", path);
        resolver = new(path);
    }

    protected override Assembly? Load(AssemblyName name)
    {
        // Config and worker must share the same public API assembly identity.
        if (name.Name == typeof(IWindowConfig).Assembly.GetName().Name) return typeof(IWindowConfig).Assembly;
        var dependency = resolver.ResolveAssemblyToPath(name);
        return dependency is null ? null : LoadFromAssemblyPath(dependency);
    }

    protected override nint LoadUnmanagedDll(string name)
    {
        var dependency = resolver.ResolveUnmanagedDllToPath(name);
        return dependency is null ? 0 : LoadUnmanagedDllFromPath(dependency);
    }

    public IWindowConfig CreateConfig()
    {
        var types = LoadFromAssemblyPath(path).GetExportedTypes()
            .Where(type => !type.IsAbstract && typeof(IWindowConfig).IsAssignableFrom(type)).ToArray();
        if (types.Length != 1) throw new InvalidOperationException("config assembly must export exactly one concrete IWindowConfig with a public parameterless constructor");
        return (IWindowConfig)(Activator.CreateInstance(types[0]) ?? throw new InvalidOperationException("could not create config"));
    }
}
