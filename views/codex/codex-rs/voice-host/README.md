# Private voice helper foundation

`codex-voice-host` establishes the inherited-pipe lifecycle for the proposed
bundled voice process. It does not open devices, load native plugins, negotiate
WebRTC, or enable voice in the TUI. The existing CLI is unchanged.

Frames are a big-endian u32 length followed by at most 256 bytes of JSON. The
parent sends `hello` with protocol `1` and the helper's exact `buildCommit` before
receiving `ready`. It then sends `close` and receives `closed` before process exit.
Unknown fields, incompatible builds, invalid order and oversized frames fail
closed without echoing input. EOF exits even when the main worker cannot progress.

Bazel stamps the binary with `STABLE_GIT_COMMIT`. Cargo builders must provide the
same variable; an unstamped source build reports `dev` via `--build-commit` and is
not a distributable build identity. The client/control crate has no native audio
dependencies. `VoiceHost` resolves only the physical package's
`codex-resources/voice/bin/codex-voice-host[.exe]`, filters the child environment,
and owns process cleanup through `codex-utils-pty`. Its runtime must remain alive
to reap a dropped helper; explicit `close` waits for process exit.

For private feasibility artifacts, `third_party/voice/assemble_package.py` copies
an existing validated package into a fresh output and adds the helper. Supply
`--package`, `--helper`, `--voice-target`, `--build-commit`, and `--output`.
Linux MUSL apps require same-architecture GNU helpers; other targets must match.
The package version must end in `+<build-commit>`. The manifest records declared
build provenance and file hashes, not authentication or binary architecture proof.
Add `--runtime <prepared-runtime>` to include the platform preparer's selected
libraries and `runtime.json` beside the helper. The assembler checks the target,
pinned source manifest, plugin list, relative paths and file hashes, then checks
the copied hashes again. It preserves `lib/` and `plugins/` on macOS, `lib/` and
`lib/gstreamer-1.0/` on Linux, and the shared `bin/` on Windows. Unlisted files are
not copied. The package manifest records every included runtime file and the
unchanged runtime receipt.
Omitting `--runtime` retains helper-only assembly.

This accepts a development runtime receipt, not an authenticated release. It
does not repeat native loader inspection or establish trust in the build inputs.
The helper still does not load these files; native loading, media/privacy controls,
linking against the prepared SDK and actual audio proof remain integration stages.
