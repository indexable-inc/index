# The compiled `builtins.wasm` plugin for `.ix` imports: the ix2nix converter
# built for `wasm32-unknown-unknown` through a target-scoped unit graph over
# the same workspace source and lock the native graph uses (one source of
# truth; only the target, toolchain, and profile differ, which is exactly the
# unit-identity change that warrants a second `buildWorkspace`).
{
  ix,
  lib,
  ...
}: let
  inherit (ix) pkgs;

  target = "wasm32-unknown-unknown";
  workspace = ix.cargoUnit.buildWorkspace {
    pname = "ix2nix-wasm";
    inherit (ix.rustWorkspace) src;
    cargoLock.lockFile = ix.rustWorkspace.cargoLock;
    workspaceRoot = ix.rustWorkspace.root;
    cargoArgs = [
      "-p"
      "ix2nix-wasm"
    ];
    inherit target;
    # Same shape as the darwin cross graphs (lib/rust/workspace.nix): a
    # rust-overlay toolchain carrying the target's `rust-std`, pure-build
    # policy (the native graph already runs clippy/audit over these crates),
    # and input-addressed drvs so consumers substitute via plain narinfo.
    rustToolchain = ix.languages.rust.toolchain pkgs {
      channel = "stable";
      version = "latest";
      targets = [target];
    };
    # wasm32-unknown-unknown ships no unwinder; the root manifest's
    # `wasm-plugin` profile is release plus panic=abort.
    profile = "wasm-plugin";
    # `embedMetadata = true` because this graph pins a stable toolchain
    # and `-Zembed-metadata=no` is nightly-only: leaving the default
    # sends a `-Z` flag to a rustc that exits 1 on it (ENG-12992). The cost
    # is a fatter rlib on a graph nothing links against twice.
    policy = ix.cargoUnit.policyPresets.pureBuild // {compiler.embedMetadata = true;};
    contentAddressed = false;
  };
  unit = workspace.libraries.ix2nix_wasm;

  package =
    pkgs.runCommand "ix2nix-wasm"
    {
      strictDeps = true;
      meta = {
        description = "ix2nix compiled to Wasm for in-eval .ix imports via builtins.wasm";
        license = lib.licenses.mit;
      };
    }
    ''
      shopt -s nullglob
      artifacts=(${unit}/lib/*.wasm)
      if [ ''${#artifacts[@]} -ne 1 ]; then
        echo "expected exactly one .wasm artifact under ${unit}/lib, got ''${#artifacts[@]}" >&2
        ls -la ${unit}/lib >&2 || true
        exit 1
      fi
      install -m444 -D "''${artifacts[0]}" "$out/lib/ix2nix.wasm"
    '';

  # End-to-end over every boundary this package exists for: the patched
  # nix-ix evaluator runs the guest (`builtins.wasm` lives in the Rust
  # evaluator, hence `eval-backend rust` below), the shim's calling
  # convention matches the renderer's `{ __dir, __importIx, __ixTy }:`
  # wrapper, a relative `.ix` import recurses through the shim, a conversion
  # error surfaces its positioned diagnostic as a Nix eval error, type
  # annotations check in `assert` mode and cost nothing in `erase` mode, and
  # the `schema` export reproduces the crate's own golden.
  # Deliberately runs the freshly BUILT `${package}`, not the committed
  # `lib/ix2nix.wasm` the repo wires into `importIxWasm`: a converter
  # regression then fails here on the same PR that introduces it, before
  # anyone regenerates the committed copy, and `fresh` below separately
  # pins committed == built.
  # Client-side eval against a scratch store; no daemon. The crate's sibling
  # files are reached through the repo root (`../` literals are banned:
  # no-parent-path).
  crateDir = ix.paths.root + "/packages/ix2nix";

  e2e =
    pkgs.runCommand "ix2nix-wasm-e2e"
    {
      strictDeps = true;
      # The assembled fork client, not `repoPackages.nix-ix` (that recipe
      # throws un-overridden: its jjTree archive lives in ix). Only this e2e
      # is guest-Nix-dependent; the converter build above is not, which keeps
      # `.ix` example discovery evaluable on the standalone flake.
      nativeBuildInputs = [(ix.nixPackageFor "packages/ix2nix/wasm: e2e test")];
    }
    ''
      export HOME="$TMPDIR/home"
      export NIX_STORE_DIR="$TMPDIR/store" NIX_STATE_DIR="$TMPDIR/state" NIX_CONF_DIR="$TMPDIR/conf"
      mkdir -p "$HOME" "$NIX_CONF_DIR"

      evalIx() {
        nix eval \
          --extra-experimental-features 'nix-command wasm-builtin rust-eval' \
          --option eval-backend rust \
          --impure \
          --expr "let importIx = import ${crateDir}/import-ix.nix { converter = ${package}/lib/ix2nix.wasm; typeMode = \"$2\"; }; in importIx $1"
      }

      value=$(evalIx ${crateDir + "/examples"}/main.ix assert)
      expected='"doubled: 42"'
      if [ "$value" != "$expected" ]; then
        printf 'expected: %s\nactual:   %s\n' "$expected" "$value" >&2
        exit 1
      fi

      # Type annotations: assert mode passes a well-typed module ...
      value=$(evalIx ${crateDir + "/examples"}/typed.ix assert)
      if [ "$value" != "42" ]; then
        printf 'typed.ix: expected 42, got %s\n' "$value" >&2
        exit 1
      fi
      # ... fails an ill-typed one with a positioned error naming the module ...
      if evalIx ${crateDir + "/examples"}/typed-error.ix assert 2> typed.log; then
        echo "typed-error.ix unexpectedly passed its checks" >&2
        exit 1
      fi
      grep -F 'expected int, got string' typed.log
      grep -F '3:24 argument `b`' typed.log
      # ... and erase mode evaluates the same module with the checks free.
      value=$(evalIx ${crateDir + "/examples"}/typed-error.ix erase)
      if [ "$value" != "1" ]; then
        printf 'typed-error.ix under erase: expected 1, got %s\n' "$value" >&2
        exit 1
      fi

      # The second export, over the same artifact: `schema` must reproduce the
      # bytes `cargo test` pins for the same input, so a caller reaching the
      # converter through `builtins.wasm` and a caller reaching it through the
      # library cannot be told different things about one module's types.
      nix eval --raw \
        --extra-experimental-features 'nix-command wasm-builtin rust-eval' \
        --option eval-backend rust \
        --impure \
        --expr "(builtins.wasm { path = ${package}/lib/ix2nix.wasm; function = \"schema\"; }) (builtins.readFile ${crateDir + "/tests/golden"}/typed-surface.ix)" \
        > schema.json
      diff -u ${crateDir + "/tests/golden"}/typed-surface.schema.golden schema.json

      if evalIx ${crateDir + "/examples"}/strict-equality.ix 2> diagnostic.log; then
        echo "conversion of strict-equality.ix unexpectedly succeeded" >&2
        exit 1
      fi
      # The rendered ix2nix diagnostic (message and caret position) must reach
      # the Nix eval error verbatim.
      grep -F '`===` has no Nix equivalent; use `==`' diagnostic.log
      grep -F -- '--> 2:16' diagnostic.log

      mkdir -p "$out"
    '';
  # The `ix2nix-wasm-fresh` gate lived here until 2026-07-25. It byte-compared
  # a COMMITTED `lib/ix2nix.wasm` against this package's build (#4136). The
  # artifact is not bit-identical across build hosts, because the native
  # toolchain's store path feeds `-C metadata`, so the committed bytes were
  # pinned to x86_64-linux -- a pin `importIxWasm` still carries for the same
  # reason. Worse, the built bytes embed the ix2nix unit-source store path in
  # two panic-location strings, and that unit source is the whole
  # packages/ix2nix directory, so ANY edit here (or any toolchain or nixpkgs
  # bump) re-keyed the artifact and reddened the gate until someone ran
  # `nix run .#ix2nix-wasm-regen` by hand. `importIxWasm` now reads this
  # package's output directly, so the two cannot disagree and there is
  # nothing left to police.
in
  package.overrideAttrs (old: {
    passthru =
      (old.passthru or {})
      // {
        tests = {inherit e2e;};
        # This wasm32 graph is its own `buildWorkspace`, invisible to
        # per-system.nix's shared-workspace `crossIfdRoots`. Publishing these
        # as explicit roots went from important to load-bearing on 2026-07-25:
        # `importIxWasm` reads this package's output, so EVERY `.ix` eval
        # forces these drvs, not just the pre-#4125 scaffolded flakes that
        # interpolate the output into `converter` themselves. Without the
        # `workspacePackageIfdRoots` harvest each consumer re-vendors and
        # re-renders the graph before its first substitution (#4127; same
        # #1890 class as codex's second workspace).
        workspaceIfdRoots = {
          inherit (workspace) unitsNix unitGraphJson vendorDir;
        };
      };
  })
