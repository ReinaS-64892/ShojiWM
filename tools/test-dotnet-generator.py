#!/usr/bin/env python3
"""BCL/Python-only generator regression tests, with fail-closed syntax fixtures."""
import importlib.util
import pathlib
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("generator", pathlib.Path(__file__).with_name("generate-dotnet-bindings.py"))
generator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(generator)


class GeneratorTests(unittest.TestCase):
    def test_deterministic(self):
        first = generator.generate()
        self.assertEqual(first, generator.generate())

    def test_numeric_and_optional_mappings(self):
        output = generator.generate()
        self.assertIn("required ulong RequestId", output)
        self.assertIn("string? AppId", output)
        self.assertIn("List<byte>? Bytes", output)
        self.assertIn('JsonStringEnumMemberName("xdg-decoration-v1")', output)
        self.assertIn('JsonPropertyName("switch")', output)

    def test_new_fields_are_generated_and_unknown_types_fail(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            for source in generator.SOURCES:
                target = root / source
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text((generator.ROOT / source).read_text())
            target = root / "src/shojiwm/src/ssd/window_model.rs"
            original = target.read_text()
            target.write_text(original.replace("pub struct WaylandWindowSnapshot {", "pub struct WaylandWindowSnapshot {\n    pub new_field: Option<bool>,"))
            self.assertIn('public bool? NewField', generator.generate(root))
            target.write_text(original.replace("pub struct WaylandWindowSnapshot {", "pub struct WaylandWindowSnapshot {\n    pub unsupported: HashSet<String>,"))
            with self.assertRaisesRegex(ValueError, "unsupported Rust type"):
                generator.generate(root)


if __name__ == "__main__":
    unittest.main()
