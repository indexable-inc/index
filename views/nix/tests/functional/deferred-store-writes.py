"""A deferred derivation batch restores registered but missing child bytes."""
import os
import json
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib.parse import quote



def check_input_closure(nix: str, root: Path) -> None:
    local = root / "closure-local"
    command = [nix, "--store", str(local), "--option", "sandbox", "false"]
    expression = 'let child = builtins.toFile "transitive-child" "retained child"; in builtins.toFile "present-parent" "${child}"'
    parent = subprocess.check_output(command + ["eval", "--impure", "--raw", "--expr", expression], text=True).strip()
    parent_object = local / parent.lstrip("/")
    child = parent_object.read_text()
    child_object = local / child.lstrip("/")
    cache = (root / "closure-cache").as_uri()
    subprocess.run(command + ["copy", "--to", cache, parent], check=True)
    original_parent = parent_object.read_bytes()

    script = 'read -r child < "$dependency"; read -r value < "$child"; printf "%s" "$value" > "$out"'
    drv = 'builtins.derivation { name = "closure-consumer"; system = builtins.currentSystem; builder = "/bin/sh"; dependency = builtins.storePath ' + json.dumps(parent) + '; args = [ "-c" ' + json.dumps(script) + ' ]; }'
    target = subprocess.check_output(command + ["eval", "--raw", "--impure", "--expr", "(" + drv + ").drvPath"], text=True).strip()

    # Both the ordinary input-closure boundary and an explicit present-root
    # request must restore an absent descendant, without replacing its parent.
    for mode in ["build", "root"]:
        child_object.unlink()
        subprocess.run(command + ["path-info", child], check=True)
        if mode == "build":
            refused = subprocess.run(command + ["build", "--no-link", "--option", "substitute", "false", target + "^out"], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            assert refused.returncode != 0 and "substitution is disabled" in refused.stderr, refused.stderr
            assert not child_object.exists(), "disabled substitution restored a missing input"
            build = command + ["build", "--no-link", "--print-out-paths", "--option", "substitute", "true", "--option", "substituters", cache, target + "^out"]
            output = subprocess.check_output(build, text=True).strip()
            assert (local / output.lstrip("/")).read_text() == "retained child"
        else:
            subprocess.run(command + ["build", "--no-link", "--option", "substitute", "true", "--option", "substituters", cache, parent], check=True)
        assert child_object.read_text() == "retained child"
        assert parent_object.read_bytes() == original_parent
        subprocess.run(command + ["store", "verify", "--no-trust", parent, child], check=True)
        print("REGISTERED_TRANSITIVE_INPUT_RESTORED_PASS " + mode, flush=True)


def main() -> None:
    nix = sys.argv[1]
    os.environ["NIX_CONFIG"] = "experimental-features = nix-command\nmin-free = 0\nmax-free = 0\nsubstituters =\nbuild-users-group ="
    os.environ["NIX_USER_CONF_FILES"] = "/dev/null"
    with tempfile.TemporaryDirectory(prefix="deferred-store-writes-") as directory:
        root = Path(directory)
        os.environ["NIX_CONF_DIR"] = str(root / "empty-conf")
        check_input_closure(nix, root)
        local = root / "local"
        command = [nix, "--store", str(local)]
        evaluate = command + ["eval", "--raw", "--impure", "--expr"]
        child = 'builtins.derivation { name = "missing-child"; system = builtins.currentSystem; builder = "/bin/sh"; args = ["-c" "exit 0"]; }'
        path = subprocess.check_output(evaluate + ["(" + child + ").drvPath"], text=True).strip()
        actual = local / path.lstrip("/")
        original = actual.read_bytes()
        actual.unlink()
        # Only the disposable fixture loses bytes; its registered metadata
        # deliberately remains, matching the failed native batch.
        assert subprocess.check_output(command + ["path-info", path], text=True).strip() == path
        expression = "let child = " + child + '; in (builtins.derivation { name = "parent"; system = builtins.currentSystem; builder = "/bin/sh"; dependency = child; }).drvPath'
        parent = subprocess.check_output(evaluate + [expression], text=True).strip()
        assert actual.read_bytes() == original, "child derivation was not restored exactly"
        assert (local / parent.lstrip("/")).is_file(), "parent was not persisted"
        subprocess.run(command + ["store", "verify", "--no-trust", path, parent], check=True)
        # The same writer must be reachable through the fetcher's trusted-hash
        # fast path, not only through deferred derivation writes.
        payload = root / "source-payload"
        payload.mkdir()
        (payload / "Cargo.lock").write_text("preserved source bytes")
        source = subprocess.check_output(command + ["store", "add-path", str(payload)], text=True).strip()
        metadata = json.loads(subprocess.check_output(command + ["path-info", "--json", "--json-format", "1", source], text=True))[source]
        stored = local / source.lstrip("/")
        stored.chmod(0o755)
        shutil.rmtree(stored)
        assert subprocess.check_output(command + ["path-info", source], text=True).strip() == source
        fetch = 'builtins.path { path = ' + json.dumps(str(payload)) + '; name = "source-payload"; sha256 = ' + json.dumps(metadata["narHash"]) + '; }'
        restored = subprocess.check_output(evaluate + [fetch], text=True).strip()
        assert restored == source, f"restored {restored}, expected {source}"
        assert (stored / "Cargo.lock").read_text() == "preserved source bytes"
        subprocess.run(command + ["store", "verify", "--no-trust", source], check=True)
        print("REGISTERED_MISSING_FETCH_SOURCE_RESTORED_PASS", flush=True)
        # Both real transport protocols must ask the owning store. A client
        # filesystem check would inspect the source and incorrectly skip copy.
        for protocol in ["ssh", "ssh-ng"]:
            destination = root / (protocol + "-destination")
            executable = str(Path(nix).with_name("nix-store" if protocol == "ssh" else "nix-daemon"))
            remote = protocol + "://localhost?remote-store=" + quote(str(destination), safe="") + "&remote-program=" + quote(executable, safe="")
            subprocess.run(command + ["copy", "--to", remote, source], check=True, timeout=30)
            remote_object = destination / source.lstrip("/")
            remote_object.chmod(0o755)
            shutil.rmtree(remote_object)
            # Metadata remains queryable even though the object's bytes vanished.
            subprocess.run([nix, "--store", remote, "path-info", source], check=True, timeout=10)
            subprocess.run(command + ["copy", "--to", remote, source], check=True, timeout=30)
            assert (remote_object / "Cargo.lock").read_text() == "preserved source bytes"
            subprocess.run([nix, "--store", str(destination), "store", "verify", "--no-trust", source], check=True, timeout=30)
            print("REGISTERED_MISSING_REMOTE_RESTORED_PASS " + protocol, flush=True)

        print("DEFERRED_REGISTERED_MISSING_RESTORED_PASS", flush=True)


if __name__ == "__main__":
    main()
