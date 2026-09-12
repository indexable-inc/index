{
  lib,
  stdenv,
  mkMesonDerivation,

  meson,
  ninja,
  pkg-config,

  jq,
  git,
  mercurial,
  unixtools,
  util-linux,
  zstd,

  nix-store,
  nix-expr,
  nix-cli,

  busybox-sandbox-shell ? null,

  # Configuration Options

  pname ? "nix-functional-tests",
  version,

  # For running the functional tests against a different pre-built Nix.
  test-daemon ? null,
}:

let
  inherit (lib) fileset;
in

mkMesonDerivation (
  finalAttrs:
  {
    inherit pname version;

    workDir = ./.;
    fileset = fileset.unions [
      ../../nix-meson-build-support
      ../../scripts/nix-profile.sh.in
      ../../.version
      ../../tests/functional
      ./.
    ];

    nativeBuildInputs = [
      meson
      ninja
      pkg-config

      jq
      git
      mercurial
      # The jj client that fetchJj.sh, jj-colocated.sh and the jj-tree tests
      # need is deliberately NOT declared here. nixpkgs' modular packaging
      # vendors its own copy of this file and `overrideSource` swaps `src`
      # only, so this lambda is not called on the path that builds the fork;
      # the ix package scope appends `packages/jj-ix` to `nativeBuildInputs`
      # instead (`index/packages/nix/default.nix`, `nix-functional-tests`).
      # Declaring it here too would be a second author for one input, and as a
      # required formal that nothing in this tree can bind it threw at eval.
      # `requireJj` (common/functions.sh) fails rather than skips, so a build
      # that loses the client is loud rather than quietly green.
      unixtools.script
      # binary-cache.sh rewrites a NAR with the same compressor the cache used,
      # and the default is now zstd. stdenv puts `xz` on PATH but not `zstd`.
      zstd

      # Explicitly splice the hostHost variant to fix LLVM tests. The nix-cli
      # has to be in PATH, but must come from the host context where it's built
      # with libc++.
      (nix-cli.__spliced.hostHost or nix-cli)
    ]
    ++ lib.optionals stdenv.hostPlatform.isLinux [
      # For various sandboxing tests that needs a statically-linked shell,
      # etc.
      busybox-sandbox-shell
      # For Overlay FS tests need `mount`, `umount`, and `unshare`.
      # For `script` command (ensuring a TTY)
      # TODO use `unixtools` to be precise over which executables instead?
      util-linux
    ];

    buildInputs = [
      nix-store
      nix-expr
    ];

    preConfigure =
      # TEMP hack for Meson before make is gone, where
      # `src/nix-functional-tests` is during the transition a symlink and
      # not the actual directory directory.
      ''
        cd $(readlink -e $PWD)
        echo $PWD | grep tests/functional
      '';

    # nixpkgs' mesonCheckPhase appends `--timeout-multiplier=0` unless this is
    # set, and meson takes the LAST occurrence of a repeated option, so a value in
    # mesonCheckFlags alone is silently discarded. Verbatim from the hook
    # (nix-support/setup-hook in the meson package):
    #
    #     if [ -z "${dontAddTimeoutMultiplier:-}" ]; then
    #         flagsArray+=("--timeout-multiplier=0")
    #     fi
    #
    # Disabling every per-test timeout means an unbounded wait anywhere in 225
    # tests costs the whole CI job. On 2026-07-31 three store-side tests blocked
    # against an old daemon in the compat lane and run 30626908044 sat silent for
    # 38 minutes before its 90-minute wall; the same shape had already burned two
    # earlier runs while looking like slowness. With a bound, that is one named
    # test failing in minutes instead of a whole job dying anonymously.
    #
    # 3x rather than 1x because the authors' `timeout: 300` in
    # tests/functional/meson.build was chosen for a developer machine, and CI
    # runners are slower and shared: this is a hang detector, not a performance
    # budget, so it should only fire on something genuinely stuck.
    dontAddTimeoutMultiplier = true;

    mesonCheckFlags = [
      "--print-errorlogs"
      "--timeout-multiplier=3"
    ];

    doCheck = true;

    installPhase = ''
      mkdir $out
    '';

    meta = {
      platforms = lib.platforms.unix;
    };

  }
  // lib.optionalAttrs (test-daemon != null) {
    # TODO rename to _NIX_TEST_DAEMON_PACKAGE
    NIX_DAEMON_PACKAGE = test-daemon;
    _NIX_TEST_CLIENT_VERSION = nix-cli.version;
  }
)
