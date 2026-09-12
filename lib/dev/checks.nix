# Repo-policy checks (#3898): the whole-tree lint gate, the fixer-lane
# acceptance, the filename/dirname policies, and the stock-Nix parse gate.
# Imported by lib/per-system.nix into the per-system check catalog; every
# check builds through the shared mkScriptCheck shape (lib/checks.nix).
{
  lib,
  pkgs,
  paths,
  mkCheck,
  lint,
  lintStage,
  lintFix,
  ruffAnnArgs,
}: let
  fs = lib.fileset;

  lintSource = fs.toSource {
    inherit (paths) root;
    # The repo linter's subject is the whole tracked tree, so this is the one
    # source that legitimately spans it; every other check takes a scoped
    # fileset (#3896).
    # astlog-ignore: no-whole-repo-fileset-source
    fileset = paths.root;
  };

  # Every tracked `.nix` outside the tests/ fork-syntax island and outside
  # the vendored views' own test corpora. The stock-nix-parse shards below
  # define the stock-parseable surface as exactly this tree (index#3635).
  #
  # views/*/tests holds upstream's suites, and a parser test suite's whole
  # job is to carry inputs that must not parse (views/nix's
  # tests/functional/lang/eval-fail-*.nix are the fixtures asserting the
  # error message you get). They are data for the fork's tests, never
  # imported by any surface this gate protects, so scanning them reports a
  # failure for files that are correct exactly as they are. They arrived
  # under the gate when #9905 moved the forks into views/ and turned main
  # red (run 30997137503).
  stockParseFileset = fs.difference (
    fs.fileFilter (file: file.hasExt "nix") paths.root
  ) (fs.unions ([(paths.root + "/tests")] ++ viewTestDirs));

  # The views themselves stay in the gate: bootstrap builds
  # views/nix#nix-cli with whatever client the runner already has, which
  # may be stock, so that flake must parse everywhere.
  #
  # Only the fixture corpora come out, and only these two directory names.
  # A view whose fixtures live somewhere else fails this gate the day it
  # lands, with the offending path in the message, which is the loud
  # failure this list is allowed to have. The version with nothing to keep
  # correct is scanning each view's flake-reachable closure instead of
  # every tracked .nix under it; that is a bigger change than a red main
  # should wait for (ENG-12512).
  viewsRoot = paths.root + "/views";
  fixtureDirNames = ["tests" "test_data"];
  viewTestDirs =
    if builtins.pathExists viewsRoot
    then
      lib.pipe (builtins.readDir viewsRoot) [
        (lib.filterAttrs (_name: type: type == "directory"))
        lib.attrNames
        (lib.concatMap (view: map (dir: viewsRoot + "/${view}/${dir}") fixtureDirNames))
        (builtins.filter builtins.pathExists)
      ]
    else [];

  # The parse gate is sharded per top-level directory (#3929) so editing a
  # `.nix` file re-parses only its own subtree instead of every tracked
  # `.nix`. The shard set is discovered from the tree, never hand-listed,
  # so a new top-level directory cannot silently escape coverage; `root`
  # holds the top-level files themselves (flake.nix). tests/ needs no
  # special case: stockParseFileset already excludes it, so its shard is
  # empty and dropped along with the other `.nix`-free directories.
  topLevelDirs = lib.attrNames (
    lib.filterAttrs (_name: type: type == "directory") (builtins.readDir paths.root)
  );
  stockParseShards = lib.filterAttrs (_name: fileset: fs.toList fileset != []) (
    lib.genAttrs topLevelDirs (dir: fs.intersection stockParseFileset (paths.root + "/${dir}"))
    // {
      root = fs.difference stockParseFileset (fs.unions (map (dir: paths.root + "/${dir}") topLevelDirs));
    }
  );

  # The externally consumed surface (everything outside the tests/
  # fork-syntax island) must stay parseable by *stock* upstream Nix:
  # external flakes evaluate index with their own evaluator, and the
  # `nix-ix` bootstrap only works while its import closure parses on
  # stock Nix. CI runs the fork, so without this gate a stray
  # fork-only literal outside tests/ would ship silently and brick
  # onboarding and every external consumer (index#3635). `pkgs.nix`
  # is upstream Nix from the nixpkgs pin, never the fork.
  mkStockParseCheck = shardName: fileset: let
    shardSource = fs.toSource {
      inherit (paths) root;
      inherit fileset;
    };
  in
    mkCheck "stock-nix-parse-${shardName}" {
      nativeBuildInputs = [pkgs.nix];
      script = ''
        export HOME="$TMPDIR"
        export NIX_STORE_DIR="$TMPDIR/store" NIX_STATE_DIR="$TMPDIR/state" NIX_CONF_DIR="$TMPDIR/conf"
        fail=0
        while IFS= read -r -d "" f; do
          if ! nix-instantiate --parse "$f" > /dev/null 2> "$TMPDIR/err"; then
            echo "not stock-parseable: ''${f#${shardSource}/}" >&2
            cat "$TMPDIR/err" >&2
            fail=1
          fi
        done < <(find ${shardSource} -name '*.nix' -print0 | sort -z)
        [ "$fail" = 0 ]
      '';
    };

  # Coverage parity: the union of the shard filesets must be exactly the
  # stock-parse surface, so a shard-construction bug fails evaluation
  # instead of silently narrowing the gate.
  stockParseChecks = assert lib.assertMsg (
    lib.sort lib.lessThan (map toString (fs.toList (fs.unions (lib.attrValues stockParseShards))))
    == lib.sort lib.lessThan (map toString (fs.toList stockParseFileset))
  ) "stock-nix-parse shards do not cover the whole stock-parse surface (#3929)";
    lib.mapAttrs' (
      shardName: fileset: lib.nameValuePair "stock-nix-parse-${shardName}" (mkStockParseCheck shardName fileset)
    )
    stockParseShards;
  # The kernel's git guard (packages/mcp-ex, index#4225) enumerates the same
  # mutating git subcommands as the Bash-side hook's `mutates_checkout`
  # (packages/claude-hooks, index#4218). Two enumerations of one policy drift
  # silently -- a subcommand added to the hook keeps mutating shared checkouts
  # through the kernel -- so the sets are compared here rather than trusted.
  # Both extractions take the first quoted lowercase token of each line in the
  # relevant block, which is the arm head in Rust and the map key in Elixir.
  gitGuardParitySource = fs.toSource {
    inherit (paths) root;
    fileset = fs.unions [
      (paths.packagesRoot + "/claude-hooks/src/guards.rs")
      (paths.packagesRoot + "/mcp-ex/lib/ix_mcp/git_guard.ex")
    ];
  };
in
  {
    git-guard-parity = mkCheck "git-guard-parity" {
      nativeBuildInputs = [pkgs.coreutils pkgs.gnused pkgs.gnugrep pkgs.diffutils];
      script = ''
        heads() { grep -oE '^[[:space:]]*"[a-z][a-z0-9-]*"' | tr -d ' "' | sort -u; }

        sed -n '/^fn mutates_checkout/,/^}/p' \
          ${gitGuardParitySource}/packages/claude-hooks/src/guards.rs | heads > hook.txt
        sed -n '/^  @mutating %{/,/^  }/p' \
          ${gitGuardParitySource}/packages/mcp-ex/lib/ix_mcp/git_guard.ex | heads > kernel.txt

        # An empty side means the extraction stopped matching the source, which
        # would otherwise pass as "both agree on nothing".
        [ -s hook.txt ] || { echo "no subcommands found in guards.rs" >&2; exit 1; }
        [ -s kernel.txt ] || { echo "no subcommands found in git_guard.ex" >&2; exit 1; }

        if ! diff -u hook.txt kernel.txt; then
          echo "the hook and kernel git guards disagree on what mutates a checkout" >&2
          echo "(-: only in packages/claude-hooks, +: only in packages/mcp-ex)" >&2
          exit 1
        fi
      '';
    };

    lint = mkCheck "lint" {
      nativeBuildInputs = [pkgs.coreutils];
      script = ''
        cp -R ${lintSource} source
        chmod -R u+w source
        cd source
        ${lib.getExe lint}
      '';
    };

    # Acceptance for the fixer lanes (#3432), both halves in one gate: a
    # deliberately violating tree becomes clean under the same check
    # stages after one pass through its lane, and fixing the already-
    # fixed tree is byte-identical (the no-op property that makes a
    # clean tree a CA cache hit all the way down and `--fix` safe to
    # re-run). Fixtures are built inline rather than committed:
    # committed files with these violations would fail the repo's own
    # lint stages (the same reason the astlog fixtures live as
    # `.fixture`). Each fixture is first asserted to FAIL its check
    # stage, so tool or selector drift that stops exercising a fixer
    # turns this check red instead of silently proving nothing.
    lint-fix = let
      # One fixable finding per nix-lane tool: an unused binding
      # (deadnix --edit), useless parens (statix fix), misformatting
      # (alejandra). The unused lambda pattern also pins the lane's
      # ordering: deadnix deletes `unusedArg` leaving `{}:`, a fresh
      # empty_pattern finding only a statix fix run AFTER deadnix
      # repairs, so a statix-first lane fails this check's post-fix
      # statix stage.
      violatingNix = pkgs.writeTextDir "fixture.nix" ''
        {unusedArg}: let
          unused = 1;
          greeting = ("hello");
        in {   inherit greeting; }
      '';
      # UP024 (IOError -> OSError): inside the shared selector's UP
      # family with a SAFE autofix, so the lane's plain `--fix` applies
      # it. Not C408 and friends: their fixes are unsafe-gated, the lane
      # deliberately never passes `--unsafe-fixes`, and an unsafe-only
      # fixture would survive the lane and fail this check's post-fix
      # gate. Module-level so the ANN rules are satisfied without
      # annotations.
      violatingPython = pkgs.writeTextDir "fixture.py" ''
        raise IOError("fixture")
      '';
      # A one-crate workspace whose main.rs cargo fmt rewrites: the
      # manifest is what cargo-fmt's `cargo metadata --no-deps` walks,
      # so this also proves the lane works from manifests alone (no
      # Cargo.lock, no dependency sources, no network).
      violatingRust = pkgs.runCommand "rust-fixture" {__structuredAttrs = true;} ''
        mkdir -p "$out/src"
        cat > "$out/Cargo.toml" <<'EOF'
        [package]
        name = "fixture"
        version = "0.0.0"
        edition = "2021"
        EOF
        printf 'fn main(){println!("fixture") ;}\n' > "$out/src/main.rs"
      '';
      fixedNix = lintFix.lanes.nix violatingNix;
      fixedPython = lintFix.lanes.python violatingPython;
      fixedRust = lintFix.lanes.rust violatingRust;
      fixedTree = lintFix.unite "lint-fix-fixture-fixed" [
        fixedNix
        fixedPython
        fixedRust
      ];
    in
      mkCheck "lint-fix" {
        nativeBuildInputs = [
          pkgs.alejandra
          pkgs.deadnix
          pkgs.ruff
          pkgs.statix
          lintFix.rustFmtToolchain
        ];
        script = ''
          # Pre-fix: every tool must find its planted violation.
          if alejandra --check ${violatingNix}/fixture.nix; then
            echo "fixture stopped violating alejandra" >&2
            exit 1
          fi
          if statix check ${violatingNix}; then
            echo "fixture stopped violating statix" >&2
            exit 1
          fi
          if deadnix --fail ${violatingNix}; then
            echo "fixture stopped violating deadnix" >&2
            exit 1
          fi
          if ruff check ${ruffAnnArgs} --no-cache ${violatingPython}/fixture.py; then
            echo "fixture stopped violating ruff" >&2
            exit 1
          fi
          # cargo wants a writable home even for `fmt --check`.
          export CARGO_HOME="$TMPDIR/cargo-home"
          if (cd ${violatingRust} && cargo fmt --all --check); then
            echo "fixture stopped violating cargo fmt" >&2
            exit 1
          fi

          # Post-fix: the united tree passes the same check stages the
          # lint gate runs, through the same stage binary.
          cp -R ${fixedTree} fixed
          chmod -R u+w fixed
          cd fixed
          ${lib.getExe lintStage} alejandra
          ${lib.getExe lintStage} statix
          ${lib.getExe lintStage} deadnix
          ${lib.getExe lintStage} ruff
          # No rustfmt stage exists in the lint gate (#3433 ships the
          # fixer only), so the post-fix rust gate is cargo fmt itself,
          # from the same pinned toolchain the lane ran.
          cargo fmt --all --check
          cd ..

          # No-op: a second pass over each already-fixed lane output must
          # be byte-identical.
          diff -r ${fixedNix} ${lintFix.lanes.nix fixedNix}
          diff -r ${fixedPython} ${lintFix.lanes.python fixedPython}
          diff -r ${fixedRust} ${lintFix.lanes.rust fixedRust}
        '';
      };

    filename-policy = mkCheck "filename-policy" {
      nativeBuildInputs = [pkgs.coreutils];
      script = ''
        mkdir source
        cd source
        touch repository-config.json zellij-layout.kdl
        if ${lib.getExe lintStage} filenames >output 2>&1; then
          echo "filename policy accepted repository-config.json" >&2
          exit 1
        fi
        grep -F "repository-config.json" output
        grep -F "zellij-layout.kdl" output
      '';
    };

    # Both halves of the dirnames stage: a marker-less doubled segment is
    # flagged, an eponym package root (package.nix) is exempt.
    dirname-policy = mkCheck "dirname-policy" {
      nativeBuildInputs = [pkgs.coreutils];
      script = ''
        mkdir source
        cd source
        mkdir -p packages/foo/foo packages/bar/bar
        touch packages/bar/bar/package.nix
        if ${lib.getExe lintStage} dirnames >output 2>&1; then
          echo "dirname policy accepted packages/foo/foo" >&2
          exit 1
        fi
        grep -F "packages/foo/foo" output
        if grep -F "packages/bar/bar" output; then
          echo "dirname policy exempted nothing: flagged the eponym package packages/bar/bar" >&2
          exit 1
        fi
      '';
    };
  }
  // stockParseChecks
