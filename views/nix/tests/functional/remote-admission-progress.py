"""Native hook admission and diagnostic progress against disposable stores."""

import fcntl
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import time
from typing import BinaryIO


def number(stream: BinaryIO) -> int:
    raw = stream.read(8)
    if len(raw) != 8:
        raise EOFError("truncated hook integer")
    return int.from_bytes(raw, "little")


def string(stream: BinaryIO) -> str:
    size = number(stream)
    raw = stream.read(size)
    if len(raw) != size or len(stream.read((-size) % 8)) != (-size) % 8:
        raise EOFError("truncated hook string")
    return raw.decode()


def strings(stream: BinaryIO) -> list[str]:
    return [string(stream) for _ in range(number(stream))]


def encoded(value: int | str) -> bytes:
    if isinstance(value, int):
        return value.to_bytes(8, "little")
    raw = value.encode()
    return encoded(len(raw)) + raw + bytes((-len(raw)) % 8)


def hook(root: Path) -> None:
    stream = sys.stdin.buffer
    while number(stream):
        string(stream)
        string(stream)
    while True:
        try:
            assert string(stream) == "try"
        except EOFError:
            return
        number(stream)
        string(stream)
        string(stream)
        strings(stream)
        try:
            descriptor = os.open(root / "first", os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        except FileExistsError:
            if not (root / "drained").exists():
                with (root / "negotiations-before-drain").open("ab") as count:
                    count.write(b"x")
            time.sleep(0.03)
            print("# decline", file=sys.stderr, flush=True)
            continue
        os.close(descriptor)
        print("# accept\nfixture", file=sys.stderr, flush=True)
        strings(stream)
        strings(stream)
        # Exceed an ordinary pipe by enough that one accidental read cannot
        # complete the producer. Every byte remains a normal diagnostic.
        fcntl.fcntl(sys.stderr.fileno(), fcntl.F_SETPIPE_SZ, 4096)
        diagnostic = "@nix " + json.dumps({"action": "msg", "level": 0, "msg": "hook-progress-diagnostic"}) + "\n"
        sys.stderr.write(diagnostic * 1024)
        sys.stderr.flush()
        (root / "drained").touch()
        return


def line(process: subprocess.Popen[bytes], deadline: float) -> str:
    assert process.stderr is not None
    data = bytearray()
    while time.monotonic() < deadline:
        if not select.select([process.stderr], [], [], max(0, deadline - time.monotonic()))[0]:
            break
        char = os.read(process.stderr.fileno(), 1)
        if not char:
            raise AssertionError("hook exited before reply")
        if char == b"\n":
            text = data.decode()
            if text.startswith("# "):
                return text
            print(text, file=sys.stderr)
            data.clear()
        else:
            data.extend(char)
    raise AssertionError("hook reply deadline exceeded")


def test_upload(nix: str, root: Path, store: Path, drv: str, system: str) -> None:
    invoked = root / "remote-invoked"
    remote = root / "remote"
    remote.write_text("#!/bin/sh\n: > " + str(invoked) + "\nexit 1\n")
    remote.chmod(0o700)
    uri = "ssh://localhost?remote-program=" + str(remote)
    load = store / "nix/var/nix/current-load"
    load.mkdir(parents=True, exist_ok=True)
    lock = (load / (uri.replace("/", "_") + ".upload-lock")).open("w")
    fcntl.flock(lock, fcntl.LOCK_EX)
    process = subprocess.Popen(["/bin/sh", "-c", 'exec "$1" __build-remote 0 4>"$2" 5<"$2"', "hook-driver", nix, str(root / "builder-output")], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    assert process.stdin is not None
    try:
        settings = {"store": str(store), "builders": uri + " " + system + " - 4 1", "max-jobs": "0", "experimental-features": "nix-command ca-derivations"}
        payload = b"".join(encoded(1) + encoded(name) + encoded(value) for name, value in settings.items()) + encoded(0)
        request = encoded("try") + encoded(0) + encoded(system) + encoded(drv) + encoded(0)
        process.stdin.write(payload + request)
        process.stdin.flush()
        reply = line(process, time.monotonic() + 5)
        assert reply == "# postpone", f"busy upload reply={reply}; remote invoked={invoked.exists()}; diagnostic={(root / 'builder-output').read_text()}"
        assert not invoked.exists(), "busy upload opened a remote connection"
        # Keep contention alive across a second negotiation, then release.
        time.sleep(0.1)
        process.stdin.write(request)
        process.stdin.flush()
        assert line(process, time.monotonic() + 5) == "# postpone"
        assert not invoked.exists()
        lock.close()
        process.stdin.write(request)
        process.stdin.flush()
        assert line(process, time.monotonic() + 5) == "# decline"
        assert invoked.exists(), "released upload never reached remote connection"
    finally:
        lock.close()
        process.stdin.close()
        process.wait(timeout=5)


def main() -> None:
    nix = sys.argv[1]
    with tempfile.TemporaryDirectory(prefix="remote-admission-progress-") as temporary:
        root = Path(temporary)
        store = root / "store"
        options = [nix, "--store", str(store), "--option", "substituters", "", "--option", "builders", "", "--option", "min-free", "0", "--option", "max-free", "0"]
        expression = '{ name = "hook-fixture"; system = builtins.currentSystem; builder = "/bin/sh"; args = [ "-c" "exit 1" ]; }'
        drv = subprocess.check_output(options + ["eval", "--raw", "--impure", "--expr", "(builtins.derivation " + expression + ").drvPath"], text=True).strip()
        system = subprocess.check_output(options + ["eval", "--raw", "--impure", "--expr", "builtins.currentSystem"], text=True).strip()
        if "--fairness-only" not in sys.argv:
            test_upload(nix, root, store, drv, system)
        wrapper = root / "hook"
        wrapper.write_text("#!" + sys.executable + "\nimport runpy, sys\nsys.argv = [" + repr(__file__) + ", 'hook', " + repr(str(root)) + "]\nrunpy.run_path(" + repr(__file__) + ", run_name='__main__')\n")
        wrapper.chmod(0o700)
        expression = "builtins.derivation (" + expression + ' // { name = "hook-root"; inputs = builtins.genList (i: builtins.derivation (' + expression + ' // { name = "hook-${toString i}"; })) 64; })'
        with (root / "diagnostics.log").open("wb") as log:
            result = subprocess.run(options + ["build", "--impure", "--expr", expression, "--no-link", "--keep-going", "--max-jobs", "0", "--option", "build-hook", str(wrapper)], stdout=log, stderr=log, timeout=30)
        assert result.returncode != 0, "fixture hooks intentionally do not produce outputs"
        diagnostics = (root / "diagnostics.log").read_text()
        assert (root / "drained").exists(), diagnostics[-4000:]
        assert diagnostics.count("hook-progress-diagnostic") == 1024, "hook diagnostics lost"
        count = root / "negotiations-before-drain"
        negotiations = len(count.read_bytes()) if count.exists() else 0
        assert negotiations < 32, f"accepted hook starved behind {negotiations} negotiations"
        print("REMOTE_ADMISSION_PROGRESS_PASS: upload contention/release and diagnostic fairness")


if __name__ == "__main__":
    if sys.argv[1] == "hook":
        hook(Path(sys.argv[2]))
    else:
        main()
