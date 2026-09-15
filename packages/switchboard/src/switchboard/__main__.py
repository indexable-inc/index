"""``python -m switchboard``: version + wiring one-liner (smoke entrypoint)."""

from __future__ import annotations

from . import __version__


def main() -> int:
    print(f"switchboard {__version__}: platform frontends -> canonical IR -> router -> backends")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
