"""Exercise retained mappings through the real legacy remote build hook."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib.parse import quote


def encoded(value: int | str) -> bytes:
    if isinstance(value, int):
        return value.to_bytes(8, "little")
    raw = value.encode()
    return encoded(len(raw)) + raw + bytes((-len(raw)) % 8)


def check(nix: str, fixture: str, root: Path, mode: str) -> None:
    local = root / "local"
    remote = root / "remote"
    local.mkdir(parents=True)
    record = json.loads(subprocess.check_output([fixture, str(local), mode], text=True))
    uri = "ssh://localhost?remote-store=" + quote(str(remote), safe="") + "&remote-program=" + quote(str(Path(nix).parent / "nix-store"), safe="")
    settings = {"store": str(local), "builders": uri + " " + record["system"] + " - 1 1", "max-jobs": "0", "experimental-features": "nix-command ca-derivations", "sandbox": "false", "substituters": "", "min-free": "0", "max-free": "0"}
    prefix = [nix, "--store", str(local), "--option", "min-free", "0", "--option", "max-free", "0"]
    # Drive the real hook even for a valid local output: an ordinary build
    # would correctly reuse it and never exercise conflicting registration.
    payload = b"".join(encoded(1) + encoded(name) + encoded(value) for name, value in settings.items()) + encoded(0)
    request = encoded("try") + encoded(0) + encoded(record["system"]) + encoded(record["drv"]) + encoded(0)
    inputs_and_outputs = encoded(0) + encoded(1) + encoded("out")
    diagnostic = root / "builder-output"
    result = subprocess.run(["/bin/sh", "-c", 'exec "$1" __build-remote 0 4>"$2" 5<"$2"', "hook-driver", nix, str(diagnostic)], input=payload + request + inputs_and_outputs, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
    print(result.stderr.decode(), file=sys.stderr)
    print(diagnostic.read_text(), file=sys.stderr)
    assert b"# accept" in result.stderr, "remote hook did not accept fixture"
    mapped = subprocess.check_output(prefix + ["path-info", record["drv"] + "^out"], text=True).strip()
    historical = local / record["old"].lstrip("/")
    assert historical.read_text() == "historical payload", "historical object changed"
    if mode == "invalid":
        assert result.returncode == 0, "remote repair failed"
        assert mapped != record["old"], "remote repair retained invalid mapping"
        assert (local / mapped.lstrip("/")).read_text() == "remote payload"
        subprocess.run(prefix + ["store", "verify", "--no-trust", mapped], check=True)
    else:
        assert result.returncode > 0, "valid conflicting output accepted or hook aborted"
        assert mapped == record["old"], "valid mapping replaced"
        assert b"have another one locally" in result.stderr, "unrelated remote failure"
        subprocess.run(prefix + ["store", "verify", "--no-trust", mapped], check=True)
    print("REMOTE_RETAINED_" + mode.upper() + "_PASS", flush=True)


def main() -> None:
    os.environ["NIX_CONFIG"] = "experimental-features = nix-command ca-derivations\nmin-free = 0\nmax-free = 0\nsandbox = false\nbuild-users-group =\nsubstituters ="
    os.environ["NIX_USER_CONF_FILES"] = "/dev/null"
    with tempfile.TemporaryDirectory(prefix="remote-retained-") as directory:
        root = Path(directory)
        os.environ["NIX_CONF_DIR"] = str(root / "empty-conf")
        for mode in ["invalid", "valid"]:
            check(sys.argv[1], sys.argv[2], root / mode, mode)


if __name__ == "__main__":
    main()
