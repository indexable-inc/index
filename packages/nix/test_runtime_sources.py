"""Exercise the production source selectors without evaluating a package graph."""

import json
from pathlib import Path
import subprocess
import tempfile
import unittest


class RuntimeSources(unittest.TestCase):
    def test_only_the_owning_runtime_changes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()

            def write(relative: str, text: str) -> None:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text)

            vm = "rust/nix-eval-rs/src/vm.rs"
            host = "rust/nix-host-rs/src/store_stream.rs"
            core = "rust/nix-host-rs/crates/store-stream/src/lib.rs"
            write(vm, "vm-v1")
            write(host, "host-v1")
            write(core, "core-v1")
            write("src/libexpr/fetchurl.nix", "builtin-v1")
            selector = Path(__file__).resolve().with_name("runtime-sources.nix")
            expression = f"""let
              hasPrefix = prefix: value: builtins.substring 0 (builtins.stringLength prefix) value == prefix;
              lib = {{
                inherit hasPrefix;
                removePrefix = prefix: value: if hasPrefix prefix value then
                  builtins.substring (builtins.stringLength prefix) (-1) value else value;
              }};
              sources = import (builtins.toPath {json.dumps(str(selector))}) {{
                inherit lib; nixSrc = builtins.toPath {json.dumps(str(root))};
              }};
            in {{ evaluator = toString sources.evaluator; host = toString sources.host; }}"""

            def selected() -> dict[str, str]:
                process = subprocess.run(
                    [
                        "nix-instantiate",
                        "--store",
                        "dummy://",
                        "--eval",
                        "--strict",
                        "--json",
                        "--expr",
                        expression,
                    ],
                    check=True,
                    text=True,
                    stdout=subprocess.PIPE,
                )
                parsed = json.loads(process.stdout)
                if not isinstance(parsed, dict) or not all(
                    isinstance(k, str) and isinstance(v, str) for k, v in parsed.items()
                ):
                    raise TypeError("source selector did not return string identities")
                return parsed

            original = selected()
            write(vm, "vm-v2")
            changed_vm = selected()
            assert original["host"] == changed_vm["host"]
            assert original["evaluator"] != changed_vm["evaluator"]
            write(host, "host-v2")
            changed_host = selected()
            assert changed_vm["evaluator"] == changed_host["evaluator"]
            assert changed_vm["host"] != changed_host["host"]
            write(core, "core-v2")
            changed_core = selected()
            assert changed_host["evaluator"] == changed_core["evaluator"]
            assert changed_host["host"] != changed_core["host"]
            write("src/libstore/store-api.cc", "unrelated-cpp")
            assert changed_core == selected()
            write("src/libexpr/fetchurl.nix", "builtin-v2")
            changed_builtin = selected()
            assert changed_core["host"] == changed_builtin["host"]
            assert changed_core["evaluator"] != changed_builtin["evaluator"]


if __name__ == "__main__":
    unittest.main()
