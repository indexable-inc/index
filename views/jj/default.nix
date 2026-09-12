{
  lib,
  stdenv,
  rustPlatform,
  gitRev ? null,
  git,
  gnupg,
  installShellFiles,
  mold,
  openssh,
}: let
  packageVersion = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
  filterSrc = src: regexes:
    lib.cleanSourceWith {
      inherit src;
      filter = path: type: let
        relPath = lib.removePrefix (toString src + "/") (toString path);
      in
        lib.all (re: builtins.match re relPath == null) regexes;
    };
in
  rustPlatform.buildRustPackage {
    pname = "jujutsu";
    version = "${packageVersion}-unstable-${
      if gitRev != null
      then gitRev
      else "dirty"
    }";

    # Named rather than left to default, so the fake editors the test suite
    # builds do not get installed.
    cargoBuildFlags = ["--bin" "jj"];
    useNextest = true;
    cargoTestFlags = ["--profile" "ci"];
    src = filterSrc ./. [
      ".*\\.nix$"
      "^.jj/"
      "^flake\\.lock$"
      "^target/"
    ];

    cargoLock.lockFile = ./Cargo.lock;
    nativeBuildInputs =
      [
        installShellFiles
      ]
      ++ lib.optionals stdenv.isLinux [
        mold
      ];
    buildInputs = [];
    nativeCheckInputs = [
      # for signing tests
      gnupg
      openssh

      # for git subprocess test
      git
    ];

    env = {
      RUST_BACKTRACE = 1;
      CARGO_INCREMENTAL = "0"; # https://github.com/rust-lang/rust/issues/139110
      RUSTFLAGS = lib.optionalString stdenv.isLinux "-C link-arg=-fuse-ld=mold";
      NIX_JJ_GIT_HASH = gitRev;
    };

    postInstall = ''
      # `cargoBuildFlags` names the binaries rather than leaving them to the
      # default, and the failure mode of editing that list is silent: the
      # package builds and the missing binary is discovered by whoever reached
      # for it. Check here instead.
      for bin in jj; do
        test -x "$out/bin/$bin" || {
          echo "$bin was not installed; check cargoBuildFlags" >&2
          exit 1
        }
      done

      $out/bin/jj util install-man-pages man
      installManPage ./man/man1/*

      installShellCompletion --cmd jj \
        --bash <(COMPLETE=bash $out/bin/jj) \
        --fish <(COMPLETE=fish $out/bin/jj) \
        --zsh <(COMPLETE=zsh $out/bin/jj)
    '';

    meta = {
      description = "Git-compatible DVCS that is both simple and powerful";
      homepage = "https://github.com/jj-vcs/jj";
      license = lib.licenses.asl20;
      mainProgram = "jj";
    };
  }
