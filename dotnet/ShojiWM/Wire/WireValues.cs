using System.Text.Json;
using System.Text.Json.Serialization;

namespace ShojiWM.Wire;

public static class WireJson
{
    public static JsonSerializerOptions Options { get; } = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
        MaxDepth = 64,
    };
}

// These scalar/untagged unions are the explicit boundary of the DTO generator.
[JsonConverter(typeof(DimensionConverter))]
public readonly record struct Dimension
{
    public int? Pixels { get; }
    public string? Keyword { get; }
    public Dimension(int pixels) => Pixels = pixels;
    public Dimension(string keyword) => Keyword = keyword;
    public static implicit operator Dimension(int pixels) => new(pixels);
    public static implicit operator Dimension(string keyword) => new(keyword);
}

internal sealed class DimensionConverter : JsonConverter<Dimension>
{
    public override Dimension Read(ref Utf8JsonReader reader, Type type, JsonSerializerOptions options) => reader.TokenType switch
    {
        JsonTokenType.Number => new(reader.GetInt32()),
        JsonTokenType.String => new(reader.GetString()!),
        _ => throw new JsonException("dimension must be integer pixels or a keyword"),
    };
    public override void Write(Utf8JsonWriter writer, Dimension value, JsonSerializerOptions options)
    {
        if (value.Pixels is int pixels) writer.WriteNumberValue(pixels);
        else if (value.Keyword is string keyword) writer.WriteStringValue(keyword);
        else throw new JsonException("dimension has no value");
    }
}

[JsonConverter(typeof(FontFamilyConverter))]
public sealed record FontFamily(IReadOnlyList<string> Names)
{
    public static implicit operator FontFamily(string name) => new([name]);
}

internal sealed class FontFamilyConverter : JsonConverter<FontFamily>
{
    public override FontFamily Read(ref Utf8JsonReader reader, Type type, JsonSerializerOptions options) =>
        reader.TokenType == JsonTokenType.String ? new([reader.GetString()!]) :
        new(JsonSerializer.Deserialize<List<string>>(ref reader, options) ?? throw new JsonException("missing font families"));
    public override void Write(Utf8JsonWriter writer, FontFamily value, JsonSerializerOptions options) =>
        JsonSerializer.Serialize(writer, value.Names, options);
}

[JsonConverter(typeof(ClickDescriptorConverter))]
public sealed record ClickDescriptor
{
    public WireWindowAction? Action { get; }
    public WireRuntimeHandler? Handler { get; }
    public ClickDescriptor(WireWindowAction action) => Action = action;
    public ClickDescriptor(string handlerId) => Handler = new() { Kind = "runtime-handler", Id = handlerId };
}

internal sealed class ClickDescriptorConverter : JsonConverter<ClickDescriptor>
{
    public override ClickDescriptor Read(ref Utf8JsonReader reader, Type type, JsonSerializerOptions options)
    {
        if (reader.TokenType == JsonTokenType.String)
            return new(JsonSerializer.Deserialize<WireWindowAction>(ref reader, options));
        var handler = JsonSerializer.Deserialize<WireRuntimeHandler>(ref reader, options) ?? throw new JsonException("missing handler");
        if (handler.Kind != "runtime-handler") throw new JsonException("invalid runtime handler kind");
        return new(handler.Id);
    }
    public override void Write(Utf8JsonWriter writer, ClickDescriptor value, JsonSerializerOptions options)
    {
        if (value.Action is WireWindowAction action) JsonSerializer.Serialize(writer, action, options);
        else if (value.Handler is WireRuntimeHandler handler) JsonSerializer.Serialize(writer, handler, options);
        else throw new JsonException("click descriptor has no value");
    }
}

[JsonConverter(typeof(ResizeHitAreaConverter))]
public sealed record ResizeHitArea(int EdgePx, int? CornerPx = null);

internal sealed class ResizeHitAreaConverter : JsonConverter<ResizeHitArea>
{
    public override ResizeHitArea Read(ref Utf8JsonReader reader, Type type, JsonSerializerOptions options)
    {
        if (reader.TokenType == JsonTokenType.Number) return new(reader.GetInt32());
        using var document = JsonDocument.ParseValue(ref reader);
        var root = document.RootElement;
        return new(root.GetProperty("edgePx").GetInt32(), root.TryGetProperty("cornerPx", out var corner) && corner.ValueKind != JsonValueKind.Null ? corner.GetInt32() : null);
    }
    public override void Write(Utf8JsonWriter writer, ResizeHitArea value, JsonSerializerOptions options)
    {
        writer.WriteStartObject();
        writer.WriteNumber("edgePx", value.EdgePx);
        if (value.CornerPx is int corner) writer.WriteNumber("cornerPx", corner);
        writer.WriteEndObject();
    }
}
