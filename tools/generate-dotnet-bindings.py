#!/usr/bin/env python3
"""Generate the external runtime's DTO closure from existing Rust serde models.

Deliberately restricted parser: named structs, unit enums, Option/Vec/BTreeMap,
references and scalar fields. Unsupported syntax/serde attributes fail generation.
Untagged unions have explicit, reviewed converters in WireValues.cs. Effects and
animation payloads are opaque until that API is implemented. No TS reactive types
are translated, and no third handwritten copy of snapshot/style DTOs is needed.
"""
import argparse
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SOURCES = [
    "src/shojiwm/src/ssd/external_protocol.rs",
    "src/shojiwm/src/ssd/bridge.rs",
    "src/shojiwm/src/ssd/window_model.rs",
    "src/shojiwm/src/ssd/interaction.rs",
    "src/shojiwm/src/ssd/evaluator.rs",
    "src/shojiwm/src/runtime_input.rs",
]
OUTPUT = ROOT / "dotnet/ShojiWM/Generated/Protocol.g.cs"
ROOT_TYPES = ["ExternalRuntimeRequest", "ExternalRuntimeResponse", "WireRuntimeHandler", "WireWindowAction"]
# These unions are consciously handled outside the structural generator.
CUSTOM = {
    "WireDecorationChild": "WireDecorationNode",  # MVP: node children only
    "WireDimension": "Dimension",
    "WireFontFamily": "FontFamily",
    "WireOnClick": "ClickDescriptor",
    "WireResizeHitArea": "ResizeHitArea",
    "WireCompiledEffect": "JsonElement",
    "ManagedWindowAnimationSnapshot": "JsonElement",
}
SCALARS = {"String": "string", "str": "string", "bool": "bool", "i32": "int",
           "u32": "uint", "u64": "ulong", "u8": "byte", "f32": "float",
           "f64": "double", "serde_json::Value": "JsonElement"}


def split_top(text, separator=","):
    parts, start, depth, quoted = [], 0, 0, False
    for i, char in enumerate(text):
        if char == '"':
            quoted = not quoted
        if quoted:
            continue
        if char in "<([{":
            depth += 1
        elif char in ">)]}":
            depth -= 1
        elif char == separator and depth == 0:
            parts.append(text[start:i].strip())
            start = i + 1
    parts.append(text[start:].strip())
    return [part for part in parts if part]


def attributes(text):
    attrs = {}
    for body in re.findall(r"#\[serde\((.*?)\)\]", text, re.S):
        for item in split_top(body):
            key, _, value = item.partition("=")
            key = key.strip()
            if key not in {"rename", "rename_all", "default", "skip_serializing_if"}:
                raise ValueError(f"unsupported serde attribute: {item}")
            attrs[key] = value.strip().strip('"') if value else True
    return attrs


def rename(name, mode):
    words = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", name).split("_")
    if mode == "camelCase":
        return words[0].lower() + "".join(word.capitalize() for word in words[1:])
    if mode == "kebab-case":
        return "-".join(word.lower() for word in words)
    if mode == "lowercase":
        return name.lower()
    if mode is not None:
        raise ValueError(f"unsupported rename_all: {mode}")
    return name


def pascal(name):
    return "".join(word[:1].upper() + word[1:] for word in name.split("_"))


def load_models(root):
    models = {}
    for source in SOURCES:
        text = (root / source).read_text()
        text = re.sub(r"//[^\n]*", "", text)
        pattern = r"((?:\s*#\[[^\]]*\])*)\s*pub (struct|enum) (\w+)(?:<[^>{}]*>)?\s*\{"
        for match in re.finditer(pattern, text):
            start, depth, end = match.end(), 1, match.end()
            while depth and end < len(text):
                if text[end] == "{": depth += 1
                if text[end] == "}": depth -= 1
                end += 1
            models[match[3]] = (match[2], match[1], text[start:end - 1], source)
    return models


def generate(root=ROOT):
    models, pending, emitted = load_models(root), list(ROOT_TYPES), {}

    def cs_type(rust):
        rust = re.sub(r"&\s*(?:'\w+\s*)?", "", rust.strip())
        if rust.startswith("Option<") and rust.endswith(">"):
            return cs_type(rust[7:-1]) + "?"
        if rust.startswith("Vec<") and rust.endswith(">"):
            # List<byte> intentionally serializes as the Rust JSON number array,
            # unlike System.Text.Json's base64 representation of byte[].
            return "List<" + cs_type(rust[4:-1]) + ">"
        if rust.startswith("BTreeMap<") and rust.endswith(">"):
            return "Dictionary<" + ", ".join(cs_type(t) for t in split_top(rust[9:-1])) + ">"
        if rust in SCALARS:
            return SCALARS[rust]
        if rust in CUSTOM:
            mapped = CUSTOM[rust]
            if mapped in models: pending.append(mapped)
            return mapped
        if rust not in models:
            raise ValueError(f"unsupported Rust type: {rust}")
        pending.append(rust)
        return rust

    while pending:
        name = pending.pop(0)
        if name in emitted:
            continue
        kind, raw_attrs, body, source = models[name]
        attrs = attributes(raw_attrs)
        lines = [f"// Source: {source} :: {name}"]
        if kind == "enum":
            lines += [f"[JsonConverter(typeof({name}Converter))]", f"public enum {name}", "{"]
            for item in split_top(body):
                raw = re.sub(r"#\[[^\]]*\]", "", item).strip()
                if not re.fullmatch(r"\w+", raw):
                    raise ValueError(f"unsupported data enum: {name}: {raw}")
                wire = attributes(item).get("rename", rename(raw, attrs.get("rename_all")))
                lines += [f'    [JsonStringEnumMemberName("{wire}")]', f"    {raw},"]
            lines += ["}", f"internal sealed class {name}Converter() : JsonStringEnumConverter<{name}>(allowIntegerValues: false);"]
        else:
            lines += [f"public class {name}", "{"]
            for item in split_top(body):
                raw = re.sub(r"#\[[^\]]*\]", "", item).strip()
                field = re.fullmatch(r"pub (\w+)\s*:\s*(.+)", raw, re.S)
                if not field:
                    raise ValueError(f"unsupported field: {name}: {raw}")
                field_name, rust_type = field.groups()
                field_attrs = attributes(item)
                wire = field_attrs.get("rename", rename(field_name, attrs.get("rename_all")))
                mapped = cs_type(rust_type)
                default = "default" in attrs or "default" in field_attrs
                required = not mapped.endswith("?") and not default
                init = ""
                if default and not mapped.endswith("?"):
                    if mapped.startswith(("List<", "Dictionary<")): init = " = [];"
                    elif mapped not in {"int", "uint", "ulong", "byte", "float", "double", "bool", "JsonElement"}:
                        init = ' = "";' if mapped == "string" else " = new();"
                lines += [f'    [JsonPropertyName("{wire}")]',
                          f"    public {'required ' if required else ''}{mapped} {pascal(field_name)} {{ get; init; }}{init}"]
            lines += ["}"]
        emitted[name] = "\n".join(lines)
    header = "// <auto-generated />\n// Run: python3 tools/generate-dotnet-bindings.py\n#nullable enable\nusing System.Text.Json;\nusing System.Text.Json.Serialization;\n\nnamespace ShojiWM.Wire;\n\n"
    return header + "\n\n".join(emitted[name] for name in sorted(emitted)) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    result = generate()
    if args.check:
        if not OUTPUT.exists() or OUTPUT.read_text() != result:
            sys.exit("Generated bindings are stale; run python3 tools/generate-dotnet-bindings.py")
    else:
        OUTPUT.parent.mkdir(parents=True, exist_ok=True)
        OUTPUT.write_text(result)


if __name__ == "__main__":
    main()
