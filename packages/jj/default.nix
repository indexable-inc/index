{
  ix,
  lib,
}: let
  # The jj view carries the submodule commits.
  source = ix.jjSrc;

  workspace = ix.cargoUnit.buildWorkspace {
    pname = "jj";
    src = source;
    workspaceRoot = source;
    cargoLock = source + "/Cargo.lock";
    # jj-cli is the CLI crate; its `jj` bin target is the only one outside the
    # test-fakes feature, and the rest of the workspace is libraries.
    cargoArgs = [
      "-p"
      "jj-cli"
    ];
    cargoTargets = [
      [
        "-p"
        "jj-cli"
      ]
      # Our crate is not in the `-p jj-cli` graph and so had no units to
      # lint: jj-cli reaches jj-vfs only behind its optional `fs` feature. A
      # second cargo execution puts it in the merged graph. Widening
      # `cargoTargets` leaves the jj-cli roots byte-identical (see the
      # `cargoTargets` doc), so this costs a unit-graph query and no rebuild
      # of the shipped binary.
      [
        "-p"
        "jj-vfs"
      ]
      # A third execution, with `--tests`, is what makes the fork's own test
      # suites exist as units at all. Without it `workspace.testChecksByTarget`
      # is empty and `ciChecks.rust-jj` holds exactly one entry, clippy
      # (clippy-jj-vfs), so a change to the fork compiles and
      # nothing runs it. Measured on the workspace-add fix (a9df7a0ec4d1, +182
      # /-20 with 138 new lines in cli/tests/test_workspaces.rs): the gate saw
      # candidate=923 identical=908 changed=15 and index-rust-jj was not among
      # the 15 -- the only fork derivation it rebuilt was the release binary.
      # jj-lib is listed because its `runner` target is where the backend and
      # git-interop tests live; widening here leaves the shipped roots
      # byte-identical, same as the execution above.
      [
        "-p"
        "jj-cli"
        "-p"
        "jj-lib"
        "-p"
        "jj-vfs"
        "--tests"
      ]
    ];
    # jj's own test suites shell out to the `git` binary: testutils/src/git.rs
    # spawns `git clone`. The nix test sandbox has no git otherwise, so those tests panic
    # spawning it (`Os { code: 2, NotFound }`) rather than failing on anything
    # jj did. Same shape as the `clone-cli` and `mirror` entries in
    # lib/rust/workspace.nix.
    packageTestInputs = {
      jj-cli = [ix.pkgs.git];
      # jj-lib's test_ssh_signing drives `ssh-keygen`/`ssh` as well.
      jj-lib = [
        ix.pkgs.git
        ix.pkgs.openssh
      ];
    };
    # Two clusters that cargo-unit's out-of-band runner cannot satisfy, and
    # that say nothing about jj. Both were measured, not guessed; the count is
    # the failing-test count each accounts for.
    testPolicyByPackage.jj-cli.skip = [
      # 14 tests. assert_cmd's `cargo_bin()` reads `CARGO_BIN_EXE_fake-formatter`
      # from the ENVIRONMENT at run time. Cargo injects that variable only when
      # cargo itself runs the integration test; cargo-unit compiles the test
      # binary and hands it to nextest, so it is unset and the tests panic
      # before exercising anything.
      "test_run_command::"
      # 1 test. insta resolves `snapshot_path => "."` by asking `cargo metadata`
      # for the workspace root. There is no cargo in the test sandbox, so insta
      # logs "cargo metadata failed ... will use manifest directory as fallback"
      # and looks for cli-reference@.md.snap in the wrong directory, reporting
      # the whole (present, current) file as a new snapshot.
      "test_generate_md_cli_help::"
    ];
    # The repo-owned quality gates are for crates we own; upstream jj answers
    # to its own CI.
    policy = {
      denyUnusedCrateDependencies = false;
      cargoAudit.enable = false;
      cargoMachete.enable = false;
      clippy = {
        # Clippy is the one gate that does apply, because one of this
        # workspace's members is ours. `packages` is what keeps it to that
        # one rather than adopting upstream's whole tree; cargo PACKAGE names,
        # so hyphens, not the underscored unit keys.
        packages = [
          "jj-vfs"
        ];
        # jj writes its `[workspace.lints.clippy]` at `warn` and its CI runs
        # `cargo clippy --all-features --workspace --all-targets -- -D warnings`
        # (.github/workflows/ci.yml), so upstream's real bar is that every one
        # of those is an error. Without this the manifest levels arrive as `-W`
        # and the gate could only ever fail on clippy's deny-by-default
        # correctness group -- much weaker than "our crates pass jj's lint
        # policy", which is what wiring it here is supposed to mean.
        deniedLints = ["warnings"];
      };
    };
  };
in
  workspace.binaries.jj.overrideAttrs (old: {
    passthru =
      (old.passthru or {})
      // {
        inherit workspace;
        # `ciChecks` enumerates `passthru.tests`, and the per-crate gates it
        # picks up on its own are the ROOT package's -- jj-cli, which we do not
        # own and do not gate. The jj-vfs gate is reachable only
        # from here, so without this the flags above build nothing and the gate
        # passes by never running. Wiring the whole map is safe precisely
        # because `policy.clippy.packages` already narrowed it to our crates,
        # and it means adding a crate there is the only edit needed.
        tests =
          (old.passthru.tests or {})
          // lib.mapAttrs' (name: lib.nameValuePair "clippy-${name}") workspace.clippyByPackage
          # Same reason the clippy map is wired by hand: `ciChecks` only picks up
          # the ROOT package's own gates. `testChecksByTarget` is one nextest
          # derivation per test TARGET (19 of them here), which is deliberate --
          # the per-#[test] `tests.<target>.cases` map goes through a shared
          # manifest IFD that builds every test binary in the graph, and buys
          # nothing a whole-target run does not already report.
          // lib.mapAttrs' (name: lib.nameValuePair "test-${name}") workspace.testChecksByTarget;
      };
    meta =
      (old.meta or {})
      // {
        description = "Jujutsu with index's submodule workflows";
        homepage = "https://github.com/jj-vcs/jj";
        license = lib.licenses.asl20;
        mainProgram = "jj";
        platforms = lib.platforms.unix;
      };
  })
