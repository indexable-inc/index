#!/usr/bin/env python3

import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

type Document = str | dict[str, Document]


def validate_document(value: object) -> Document:
    """Narrow the untyped JSON boundary before creating any files."""
    if isinstance(value, str):
        return value
    if not isinstance(value, dict):
        raise ValueError(
            "a manual document must be a string or a directory of documents"
        )
    result: dict[str, Document] = {}
    for name, child in value.items():
        if (
            not isinstance(name, str)
            or not name
            or name in {".", ".."}
            or any(char in name for char in ("/", "\\", "\0"))
        ):
            raise ValueError(f"invalid manual document name: {name!r}")
        result[name] = validate_document(child)
    return result


def write_document(path: Path, document: Document) -> None:
    if isinstance(document, str):
        path.write_text(document, encoding="utf-8")
    else:
        path.mkdir()
        for name, child in document.items():
            write_document(path / name, child)


def main() -> None:
    if len(sys.argv) < 4 or sys.argv[2] != "--":
        raise SystemExit("Usage: remove-before-wrapper <output> -- <nix command...>")
    output = Path(sys.argv[1])
    command = [arg for arg in sys.argv[3:] if arg != "--raw"]
    result = subprocess.run(
        [*command, "--json", "--builders", ""],
        check=True,
        stdout=subprocess.PIPE,
        encoding="utf-8",
    )
    document = validate_document(json.loads(result.stdout))
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix=f".{output.name}.", dir=output.parent
    ) as temporary:
        staged = Path(temporary) / "document"
        write_document(staged, document)
        if output.is_symlink() or output.is_file():
            output.unlink()
        elif output.exists():
            shutil.rmtree(output)
        staged.replace(output)


if __name__ == "__main__":
    main()
