"""Exercise a real collector between legacy-SSH presence check and upload."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from typing import BinaryIO
from urllib.parse import quote


def exact(stream: BinaryIO, size: int) -> bytes:
    result = bytearray()
    while len(result) < size:
        chunk = stream.read(size - len(result))
        if not chunk:
            raise EOFError("truncated serve protocol")
        result.extend(chunk)
    return bytes(result)


def number(stream: BinaryIO) -> int:
    return int.from_bytes(exact(stream, 8), "little")


def encoded(value: int) -> bytes:
    return value.to_bytes(8, "little")


def paths(stream: BinaryIO) -> bytes:
    count = number(stream)
    result = bytearray(encoded(count))
    for _ in range(count):
        size = number(stream)
        result.extend(encoded(size))
        result.extend(exact(stream, size + (-size) % 8))
    return bytes(result)


def command(nix: str, store: Path) -> list[str]:
    return [nix, "--store", str(store), "--option", "min-free", "0", "--option", "max-free", "0", "--option", "substituters", ""]


def proxy(root: Path, nix: str, arguments: list[str]) -> None:
    child = subprocess.Popen([str(Path(nix).with_name("nix-store")), *arguments, "--option", "min-free", "0", "--option", "max-free", "0"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    assert child.stdin is not None and child.stdout is not None
    try:
        child.stdin.write(exact(sys.stdin.buffer, 16))
        child.stdin.flush()
        sys.stdout.buffer.write(exact(child.stdout, 16))
        sys.stdout.buffer.flush()
        request = number(sys.stdin.buffer)
        assert request == 1, f"expected QueryValidPaths, got {request}"
        lock = number(sys.stdin.buffer)
        substitute = number(sys.stdin.buffer)
        requested = paths(sys.stdin.buffer)
        child.stdin.write(encoded(request) + encoded(lock) + encoded(substitute) + requested)
        child.stdin.flush()
        response = paths(child.stdout)
        # The client has not seen this presence response yet. A missing path
        # still has to be uploaded, while an existing path must remain rooted.
        with (root / "gc.log").open("wb") as log:
            subprocess.run(command(nix, root / "destination") + ["store", "gc"], stdout=log, stderr=log, check=True, timeout=20)
        (root / "query.json").write_text(json.dumps({"lock": lock}))
        sys.stdout.buffer.write(response)
        sys.stdout.buffer.flush()

        def forward() -> None:
            try:
                while chunk := sys.stdin.buffer.read1(8192):
                    child.stdin.write(chunk)
                    child.stdin.flush()
            finally:
                child.stdin.close()

        writer = threading.Thread(target=forward, daemon=True)
        writer.start()
        while chunk := child.stdout.read1(8192):
            sys.stdout.buffer.write(chunk)
            sys.stdout.buffer.flush()
        child.wait(timeout=10)
        writer.join(timeout=10)
        assert child.returncode == 0
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=10)


def main(nix: str) -> None:
    with tempfile.TemporaryDirectory(prefix="legacy-ssh-gc-") as temporary:
        root = Path(temporary)
        source = root / "source"
        destination = root / "destination"
        selected = []
        for name in ["existing", "missing"]:
            payload = root / name
            payload.write_text(name)
            selected.append(subprocess.check_output(command(nix, source) + ["store", "add-path", str(payload)], text=True).strip())
        subprocess.run(command(nix, source) + ["copy", "--to", str(destination), selected[0]], check=True, timeout=20)
        wrapper = root / "remote-program"
        wrapper.write_text("#!" + sys.executable + "\nimport runpy,sys\nsys.argv=[" + repr(str(Path(__file__).resolve())) + ", 'proxy', " + repr(str(root)) + ", " + repr(nix) + ", *sys.argv[1:]]\nrunpy.run_path(sys.argv[0],run_name='__main__')\n")
        wrapper.chmod(0o700)
        remote = "ssh://localhost?remote-store=" + quote(str(destination), safe="") + "&remote-program=" + quote(str(wrapper), safe="")
        subprocess.run(command(nix, source) + ["copy", "--to", remote, *selected], check=True, timeout=30)
        print((root / "gc.log").read_text(), flush=True)
        subprocess.run(command(nix, destination) + ["path-info", *selected], check=True, timeout=10)
        assert json.loads((root / "query.json").read_text())["lock"] == 1, "legacy query did not retain existing inputs"
        subprocess.run(command(nix, destination) + ["store", "verify", *selected], check=True, timeout=10)
        # Closing the serve connection releases its temporary roots.
        subprocess.run(command(nix, destination) + ["store", "gc"], check=True, timeout=20)
        for path in selected:
            result = subprocess.run(command(nix, destination) + ["path-info", path], capture_output=True, timeout=10)
            assert result.returncode != 0, "connection roots leaked after close"
        print("LEGACY_SSH_EXISTING_INPUT_GC_PASS; MISSING_UPLOAD_PASS; ROOT_RELEASE_PASS")


if __name__ == "__main__":
    if sys.argv[1] == "proxy":
        proxy(Path(sys.argv[2]), sys.argv[3], sys.argv[4:])
    else:
        main(sys.argv[1])
