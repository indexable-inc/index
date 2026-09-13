{
  ix,
  lib,
  pkgs,
}: let
  scriptreplay =
    if pkgs.stdenv.hostPlatform.isLinux
    then lib.getExe' pkgs.util-linux "scriptreplay"
    else "scriptreplay";
  # #2759: Rust links libiconv through the raw Darwin compiler, which does not
  # consume NIX_LDFLAGS from the surrounding profile.
  wrapperArgs =
    [
      "--set"
      "IX_RUN_SCRIPTREPLAY"
      scriptreplay
    ]
    ++ lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
      "--prefix"
      "LIBRARY_PATH"
      ":"
      "${lib.getLib pkgs.libiconv}/lib"
    ];
  unwrapped = ix.writePythonApplication pkgs {
    name = "run-unwrapped";
    src = ./run.py;
    pyChecker = "zuban";
    meta.description = "Record a command's terminal session, timing, and queryable output events";
  };
  package =
    pkgs.runCommand "run"
    {
      nativeBuildInputs = [pkgs.makeWrapper];
      strictDeps = true;
      meta = {
        mainProgram = "run";
        description = "Run a command with terminal replay files and structured JSONL output";
      };
    }
    ''
      mkdir -p $out/bin
      makeWrapper ${lib.getExe unwrapped} $out/bin/run \
        ${lib.escapeShellArgs wrapperArgs}
    '';
  recordsSession =
    pkgs.runCommand "run-records-session"
    {
      nativeBuildInputs = [
        package
        pkgs.coreutils
        pkgs.python3
      ];
      strictDeps = true;
    }
    ''
      export HOME=$TMPDIR/home
      export IX_RUN_DIR=$TMPDIR/runs
      export IX_RUN_HEAD_LINES=2
      export IX_RUN_TAIL_LINES=2
      mkdir -p "$HOME"

      run ${lib.getExe pkgs.bash} -c 'for n in 1 2 3 4 5 6; do printf "line-%s\n" "$n"; done' >stdout 2>stderr

      session=$(readlink "$IX_RUN_DIR/latest")
      test -d "$session"
      test -s "$session/output.log"
      test -s "$session/typescript"
      test -s "$session/timing.log"
      test -s "$session/events.jsonl"
      test -s "$session/lines.jsonl"
      test -s "$session/session.cast"
      test -x "$session/replay"
      test -x "$session/live"

      grep -q '"status": "exited"' "$session/summary.json"
      grep -q '"exit_code": 0' "$session/summary.json"
      grep -q '"line_no":6' "$session/lines.jsonl"
      grep -q 'line-4' "$session/output.log"
      grep -q 'line-1' stdout
      grep -q 'line-2' stdout
      grep -q 'line-5' stdout
      grep -q 'line-6' stdout
      if grep -q 'line-3' stdout; then
        echo "middle output leaked into summarized stdout" >&2
        exit 1
      fi
      grep -q 'Full live output\|live stream' stderr

      printf 'redirected stdin\n' >stdin-input
      timeout 10s run ${lib.getExe' pkgs.coreutils "cat"} <stdin-input >stdin-stdout 2>stdin-stderr
      grep -q 'redirected stdin' stdin-stdout
      session=$(readlink "$IX_RUN_DIR/latest")
      grep -q 'redirected stdin' "$session/output.log"

      i=0
      while [ "$i" -lt 40000 ]; do
        printf '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n'
        i=$((i + 1))
      done >large-stdin-input
      large_size=$(wc -c <large-stdin-input | tr -d ' ')
      export IX_RUN_DIR=$TMPDIR/runs-large-stdin
      timeout 20s run ${lib.getExe pkgs.bash} -c 'stty -echo; sleep 1; cat >large-stdin-copy; wc -c <large-stdin-copy' \
        <large-stdin-input >large-stdin-stdout 2>large-stdin-stderr
      grep -q "^$large_size" large-stdin-stdout

      export IX_RUN_DIR=$TMPDIR/runs-nonblocking-stdin
      python3 - <<'PY'
      import fcntl
      import os
      import subprocess
      import time

      read_fd, write_fd = os.pipe()
      flags = fcntl.fcntl(read_fd, fcntl.F_GETFL)
      fcntl.fcntl(read_fd, fcntl.F_SETFL, flags | os.O_NONBLOCK)

      env = os.environ.copy()
      payload = b"delayed nonblocking stdin\n"
      proc = subprocess.Popen(
          ["run", "${lib.getExe pkgs.bash}", "-c", "cat"],
          stdin=read_fd,
          stdout=subprocess.PIPE,
          stderr=subprocess.PIPE,
          env=env,
      )
      os.close(read_fd)
      time.sleep(0.2)
      os.write(write_fd, payload)
      os.close(write_fd)

      stdout, stderr = proc.communicate(timeout=10)
      if proc.returncode != 0:
          raise SystemExit(
              f"run exited {proc.returncode}; stdout={stdout!r}; stderr={stderr!r}"
          )
      if payload.strip() not in stdout:
          raise SystemExit(f"delayed stdin was not forwarded: stdout={stdout!r}")
      PY

      export IX_RUN_DIR=$TMPDIR/runs-closed-stdin
      (
        exec 0<&-
        timeout 10s run ${lib.getExe pkgs.bash} -c 'printf "closed-stdin\n"'
      ) >closed-stdin-stdout 2>closed-stdin-stderr
      grep -q 'closed-stdin' closed-stdin-stdout

      export IX_RUN_DIR=$TMPDIR/runs-closed-stdout
      (
        exec 1>&-
        timeout 10s run ${lib.getExe pkgs.bash} -c 'printf "closed-stdout\n"'
      ) 2>closed-stdout-stderr
      session=$(readlink "$IX_RUN_DIR/latest")
      grep -q 'closed-stdout' "$session/output.log"

      RUN_PY=${./run.py} python3 - <<'PY'
      import errno
      import importlib.util
      import os
      import sys
      from pathlib import Path

      spec = importlib.util.spec_from_file_location("run_module", os.environ["RUN_PY"])
      module = importlib.util.module_from_spec(spec)
      assert spec.loader is not None
      sys.modules[spec.name] = module
      spec.loader.exec_module(module)

      display = module.DisplayLimiter(
          head_lines=80,
          tail_lines=80,
          print_mode="full",
          output_path=Path("output.log"),
          stderr_fd=2,
          stdout_fd=1,
      )

      original_write = module.os.write

      def raise_eio(fd, data):
          raise OSError(errno.EIO, "simulated stdout device error")

      module.os.write = raise_eio
      try:
          try:
              display.emit_line(1, b"visible output\n")
          except OSError as exc:
              if exc.errno != errno.EIO:
                  raise
          else:
              raise AssertionError("stdout EIO was suppressed")
      finally:
          module.os.write = original_write
      PY

      mkdir -p "$out"
    '';
  darwinLibiconv =
    pkgs.runCommand "run-darwin-libiconv"
    {
      nativeBuildInputs = [
        package
        pkgs.darwin.binutils
        pkgs.llvmPackages.clang-unwrapped
      ];
      strictDeps = true;
    }
    ''
      cat >main.c <<'EOF'
      extern void *iconv_open(const char *, const char *);
      void *probe(void) { return iconv_open("UTF-8", "UTF-8"); }
      EOF

      cat >link-iconv <<'EOF'
      #!${lib.getExe pkgs.bash}
      ${lib.getExe' pkgs.coreutils "env"} -i \
        HOME="$TMPDIR" \
        LIBRARY_PATH="$LIBRARY_PATH" \
        PATH="$PATH" \
        TMPDIR="$TMPDIR" \
        ${lib.getExe pkgs.llvmPackages.clang-unwrapped} \
          -shared -nostdlib main.c -liconv -o iconv-smoke.dylib \
          2>"$TMPDIR/link-iconv.stderr"
      EOF
      chmod +x link-iconv

      export IX_RUN_DIR="$TMPDIR/runs"
      if ! LIBRARY_PATH= run "$PWD/link-iconv"; then
        cat "$TMPDIR/link-iconv.stderr" >&2
        exit 1
      fi
      ${lib.getExe' pkgs.darwin.cctools "otool"} -L iconv-smoke.dylib \
        | ${lib.getExe' pkgs.gnugrep "grep"} -F ${lib.escapeShellArg "${lib.getLib pkgs.libiconv}/lib/libiconv.2.dylib"}
      mkdir -p "$out"
    '';
in
  package.overrideAttrs (old: {
    passthru =
      (old.passthru or {})
      // {
        tests =
          {
            inherit recordsSession;
          }
          // lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin {
            inherit darwinLibiconv;
          };
      };
  })
