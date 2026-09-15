"""Path shim: prefer the sibling ``src`` tree when switchboard isn't installed.

Local runs (``pytest packages/switchboard/tests``) import straight from the
checkout; the nix test derivation copies only this tests directory, so the
shim finds nothing there and the interpreter's site-packages copy is used.
"""

from __future__ import annotations

import sys
from pathlib import Path

_SRC = Path(__file__).resolve().parent.parent / "src"
if _SRC.is_dir() and str(_SRC) not in sys.path:
    sys.path.insert(0, str(_SRC))
