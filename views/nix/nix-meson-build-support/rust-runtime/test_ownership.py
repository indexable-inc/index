"""Configure tiny dependency graphs; no C++ or Rust compilation is involved."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class RuntimeOwnership(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.recorded = self.prefix("recorded")
        self.ambient = self.prefix("ambient")
        self.pc = self.root / "pkgconfig"
        self.pc.mkdir()
        self.pkgconfig("nix-host-rs", self.ambient)

    def prefix(self, name: str) -> Path:
        path = self.root / name
        (path / "lib").mkdir(parents=True)
        (path / "include").mkdir()
        for suffix in ("so", "dylib"):
            for runtime in ("host", "eval"):
                (path / "lib" / f"libnix_{runtime}_rs.{suffix}").touch()
        return path

    def pkgconfig(self, name: str, prefix: Path) -> None:
        (self.pc / f"{name}.pc").write_text(f"""prefix={prefix}
libdir=${{prefix}}/lib
includedir=${{prefix}}/include
rust_host_runtime_libdir=${{libdir}}
rust_host_runtime_includedir=${{includedir}}
rust_runtime_libdir=${{libdir}}
rust_runtime_includedir=${{includedir}}
Name: {name}
Description: ownership fixture
Version: 1.0
Libs:
Cflags:
""")

    def configure(
        self, project: str, prefix: str | Path = "", preamble: str = ""
    ) -> subprocess.CompletedProcess[str]:
        source = self.root / "src" / project
        source.mkdir(parents=True)
        helper = source / "runtime"
        helper.mkdir()
        for name in ("meson.build", "build.sh"):
            shutil.copyfile(Path(__file__).parent / name, helper / name)
        kind = "host" if project in ("nix-store", "nix-fetchers") else "eval"
        (self.root / "rust" / f"nix-{kind}-rs" / "include").mkdir(parents=True)
        (
            source / "meson.options"
        ).write_text(f"""option('rust-{kind}-prefix', type: 'string', value: '')
option('rust-{kind}-cargo-features', type: 'string', value: '')
""")
        (source / "meson.build").write_text(f"""project('{project}', version: '1.0')
deps_other = []
{preamble}
subdir('runtime')
message('selected-runtime=' + rust_runtime_install_rpath)
""")
        environment = os.environ | {
            "PKG_CONFIG_PATH": "",
            "PKG_CONFIG_LIBDIR": str(self.pc),
        }
        command = [
            shutil.which("meson") or "meson",
            "setup",
            str(self.root / "build"),
            str(source),
            "--backend=ninja",
            f"-Drust-{kind}-prefix={prefix}",
        ]
        return subprocess.run(
            command,
            env=environment,
            text=True,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )

    def test_owner_ignores_ambient_runtime(self) -> None:
        result = self.configure("nix-store", self.recorded)
        assert result.returncode == 0, result.stdout
        assert f"selected-runtime={self.recorded}/lib" in result.stdout

    def test_checkout_owner_builds_its_source_despite_installed_runtime(self) -> None:
        result = self.configure("nix-store")
        assert result.returncode == 0, result.stdout
        targets = json.loads(
            (self.root / "build/meson-info/intro-targets.json").read_text()
        )
        assert [target["name"] for target in targets] == ["nix-host-rs"]

    def test_fetcher_uses_store_identity_without_flake_dependency(self) -> None:
        self.pkgconfig("nix-store", self.recorded)
        result = self.configure("nix-fetchers")
        assert result.returncode == 0, result.stdout
        assert f"selected-runtime={self.recorded}/lib" in result.stdout

    def test_explicit_consumer_prefix_cannot_replace_provider(self) -> None:
        self.pkgconfig("nix-store", self.recorded)
        result = self.configure("nix-fetchers", self.ambient)
        assert result.returncode != 0, result.stdout
        assert (
            "prefix disagrees with the runtime recorded by nix-store" in result.stdout
        )

    def test_evaluator_owner_remains_independent_of_store_runtime(self) -> None:
        self.pkgconfig("nix-eval-rs", self.ambient)
        result = self.configure("nix-flake", self.recorded)
        assert result.returncode == 0, result.stdout
        assert f"selected-runtime={self.recorded}/lib" in result.stdout

    def test_command_uses_exact_evaluator_provider(self) -> None:
        self.pkgconfig("nix-flake", self.recorded)
        self.pkgconfig("nix-eval-rs", self.ambient)
        result = self.configure("nix-cmd")
        assert result.returncode == 0, result.stdout
        assert f"selected-runtime={self.recorded}/lib" in result.stdout

    def test_internal_runtime_must_match_provider_identity(self) -> None:
        preamble = f"""meson.override_dependency('nix-store', declare_dependency(variables: {{
  'rust_host_runtime_libdir': '{self.recorded}/lib',
  'rust_host_runtime_includedir': '{self.recorded}/include',
}}))
meson.override_dependency('nix-host-rs', declare_dependency(variables: {{
  'install_rpath': '{self.ambient}/lib', 'build_rpath': '{self.ambient}/lib',
}}), static: false)
"""
        result = self.configure("nix-fetchers", preamble=preamble)
        assert result.returncode != 0, result.stdout
        assert "Registered Rust runtime disagrees" in result.stdout


if __name__ == "__main__":
    unittest.main()
