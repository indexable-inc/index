# ix/images public lib. Helpers documented per binding with RFC-0145
# doc-comments below; the file's job is to wire them together.
{
  nixpkgs,
  paths,
  # `system: <ix2nix-wasm package>`; see the note at `importIxWasmFor`.
  ix2nixWasmFor,
  rust-overlay,
  sdk-prebuilt-nixpkgs,
  sdk-prebuilt-rust-overlay,
  home-manager,
  hermes-agent,
  # flox CLI flake (github:flox/flox release tag). Its own nixpkgs pin is kept
  # (no `follows`) so the closure substitutes from cache.flox.dev via the ix
  # pull-through; the base profile consumes the built package as
  # `ix.floxPackage`.
  flox,
  drgn-src,
  perftest-src,
  fff-src,
  nu-jupyter-kernel-src,
  nix-ninja-src,
  snix-src,
  # The Nix every image runs as `nix.package` (modules/profiles/base). Injected
  # rather than read out of `packages` inside the module: the package the
  # module would otherwise reach for, `nix-ix`, links the jj tree ABI
  # (`libjj_tree.a`), whose crate lives in the ix repository outside this
  # flake's source root. `packages/nix/default.nix` takes that archive as an
  # argument and throws when it is forced without one, so `nix-ix` is
  # assembled by whoever HAS the archive and handed in here. One formal, no
  # default. The flake supplies it: `null` for index's own outputs, the
  # assembled fork through `index.withNixPackage { nixPackage = ...; }`
  # (flake.nix `outputsWith`), which is the only place a consumer hands it
  # in; nothing imports this file with a hand-picked Nix. Its readers, all
  # of them: modules/profiles/base consumes it (`nix.package`, refusing a
  # null), `nixPackageFor` below hands it to the packages that need the
  # ASSEMBLED client (the nix-eval-jobs build, every prebuilt-pin
  # updateScript), lib/per-system.nix null-checks it twice (the `check`
  # app's eval refusal, and the `needsGuestNix` fence on `cachePushRoots`),
  # and tests/default.nix asserts the injected value is the one an image
  # binds. No other package path reads it, which is why the standalone
  # surface evaluates everything except images and that consumer class.
  nixPackage,
  # The flake's own source (`self`), carrying `.outPath` (a `-source` store
  # path with string context, so it roots into a closure like `nixpkgs`) and
  # `.narHash`. Only the flake scope sees these, so they are plumbed down to
  # `lib/image` for the guest `index` registry pin. Defaulted `null` so a bare
  # `import ./lib` (no flake) still evaluates; `lib/image` guards on it.
  self ? null,
}: let
  inherit (nixpkgs) lib;

  system = "x86_64-linux";
  viewRoot =
    if self == null
    then paths.root
    else self.outPath;

  viewSource = name:
    builtins.path {
      path = viewRoot + "/views/${name}";
      name = "${name}-source";
    };
  btop-src = viewSource "btop";
  clippy-src = viewSource "clippy";
  codex-src = viewSource "codex";
  curl-src = viewSource "curl";
  ghosttySource = viewSource "ghostty";
  git-src = viewSource "git";
  home-manager-src = viewSource "home-manager";
  jj-src = viewSource "jj";
  launchkSource = viewSource "launchk";
  mesa-src = viewSource "mesa";
  nix-src = viewSource "nix";
  nix-fast-build-src = viewSource "nix-fast-build";
  nushell-src = viewSource "nushell";
  rnix-0-12-src = viewSource "rnix-0-12";
  rnix-0-14-src = viewSource "rnix-0-14";
  starship-src = viewSource "starship";

  # Registry-driven package discovery, exposed as a factory over any packages
  # root so a downstream consumer (ix) discovers its own `packages/<name>/
  # {package.nix,default.nix}` tree with index's `package.nix`-marker walker
  # (`packages/registry.nix`) rather than re-forking it. index's own registry
  # below is one call of this factory.
  mkPackageRegistry = {root}:
    import (paths.packagesRoot + "/registry.nix") {
      inherit lib root;
      inherit (lists) findDuplicates;
    };
  # The generic registry-driven assembly loop (callPackage each entry, place it
  # at its `packageSet.attrPath`), the shared core `lib/packages.nix` uses for
  # index and a consumer reuses for its own registry + context. See
  # lib/mk-package-set.nix.
  mkPackageSet = import ./mk-package-set.nix {inherit lib;};
  packageRegistry = mkPackageRegistry {root = paths.packagesRoot;};
  packagePath = id: let
    entry = packageRegistry.byId.${id} or (throw "ix.lib: package registry has no `${id}` entry");
  in
    entry.path;

  # Shared ruff selector (ANN explicit-annotations + TID251 no-typing.cast),
  # imported once here and injected into every Python build gate so the policy
  # has a single source of truth. See lib/ruff-ann.nix.
  inherit
    (import ./ruff-ann.nix {
      inherit lib;
      ruffToml = paths.root + "/ruff.toml";
    })
    ruffAnnArgs
    ;

  inherit
    (import ./util/writers.nix {inherit lib ruffAnnArgs;})
    writePythonApplication
    writeNushellApplication
    writeBashApplication
    writeRustApplication
    writeProcessComposeApplication
    ;
  netCidr = import ./util/net-cidr.nix {inherit lib;};

  /**
  The assembled guest/fork Nix for a consumer that needs the CLIENT or its
  components -- the CI evaluator that links the fork's C++ libraries
  (packages/nix-eval-jobs), and the prebuilt-pin updaters that must prefetch
  with blake3 in the ix dialect (lib/util/pins.nix and friends) -- refused
  with the injection story when this instantiation has none. `consumer`
  names the call site so the refusal reads as who needed it, not a bare
  null error.

  Distinct from `packageSetFor`'s `nix-ix` on purpose: that stays the
  un-assembled RECIPE, because ix overrides it with the jj tree ABI archive
  to BUILD the value this returns. Consumers read here, the assembler reads
  there; giving both jobs the one name `repoPackages.nix-ix` is how the
  required gate found nix-eval-jobs throwing "jjTree was not supplied" from
  five frames down (required-ci-checks -> ci-shell-tools-floor-check ->
  github-actions-shell-tools -> nix-eval-jobs -> nix-fetchers).
  */
  nixPackageFor = consumer:
    if nixPackage == null
    then
      throw ''
        ${consumer}: no guest Nix was injected (`ix.nixPackage`).

        This consumer needs the assembled fork client (nix-ix with the jj
        tree ABI archive), and index evaluated standalone has none: the
        archive's crate lives in the ix repository. ix instantiates this
        flake with the assembled client (`index.withNixPackage`); read this
        surface through ix's `index` flake output.
      ''
    else nixPackage;
  securityRoots = import ./security-roots.nix {inherit lib;};
  # Force `allowSubstitutes = true` on a trivial-builder derivation that must be
  # substitutable (darwin cross-lane eval-time IFD nodes). See its doc comment.
  evalTimeSubstitutable = import ./util/eval-time-substitutable.nix;
  publicArtifactsFor = pkgs: import ./util/public-artifacts.nix {inherit lib pkgs;};
  # Mirror-enabled packages (opt-in `mirror` attr in a package's package.nix):
  # id, repo-relative path, and mirror-repo coordinates for each package that
  # publishes a standalone read-only mirror. `nix eval --json
  # '.#lib.mirrorPackages'` is what the mirror-sync workflow iterates to drive
  # `mirror publish`. See packages/mirror.
  mirrorPackages =
    map (entry: {
      inherit (entry) id;
      path = "packages/${entry.relativePath}";
      inherit (entry.mirror) repo description topics;
      # The monorepo flake output attr (`nix run .#<attr>`) when the package
      # is flake-exposed, so the generated mirror README can print a real run
      # command instead of guessing.
      flakeAttr =
        if entry.flake != null
        then entry.flake.attrName
        else null;
    })
    packageRegistry.mirrorEntries;
  secretRefs = import ./util/secret-refs.nix {inherit lib;};
  selfVersionFor = self: import ./util/self-version.nix {inherit lib self;};
  checks = import ./checks.nix {inherit lib;};

  /**
  Repo-local nixpkgs overlay.

  Exposes the few repo-owned packages that NixOS modules expect to find
  as `pkgs.<name>`. Flake-output-only packages live in `packageSetFor`
  instead so they don't leak into the nixpkgs namespace inside images.
  */
  overlay = import ./overlay.nix {
    inherit
      lib
      packageRegistry
      buildIxRustTool
      cargoUnitFor
      clippy-src
      rustWorkspaceFor
      writeNushellApplication
      writePythonApplication
      ;
    # Pure cross-cutting helpers (deepMerge, writers, ...) so overlay packages
    # that take an `ix` argument resolve it the same way flake-output packages
    # do. Defined below in this recursive `let`; threaded lazily.
    ix = sharedHelpers;
  };
  overlays = [overlay];

  /**
  nixpkgs instance with the repo overlay applied, evaluated for
  `x86_64-linux`. Use this when the image build needs `pkgs` directly.
  */
  pkgs = import nixpkgs {
    inherit system overlays;
    config = {};
  };

  # Auto-discovered NixOS module registry.
  nixosModules = discoverModules {root = paths.modules;};

  # Portable user-service layer (launchd + systemd from one spec). Lives
  # outside `modules/` on purpose: it is a home-manager module, not a NixOS
  # module, so it must not be swept into `nixosModules` above. Exposed to
  # consumers as `homeModules.portable-services` from the flake.
  portableServices = import ./services/portable-services.nix {inherit lib deepMerge;};

  # Eval-provenance walker (whence, #2413): map an evaluated home-manager /
  # nix-darwin configuration's deployed files back to their defining nix
  # sites via `definitionsWithLocations` + per-key `unsafeGetAttrPos`.
  # Consumed by modules/home/provenance.nix and modules/darwin/provenance.nix
  # and exposed so downstream tooling can render manifests for other configs.
  provenance = import ./provenance.nix {inherit lib;};

  # Flat list of module paths from the auto-discovered registry under
  # `modules/`. Pulled in unconditionally so every option is in scope; each
  # module stays inert until its `enable` flag is set.
  moduleList = lib.collect builtins.isPath nixosModules;

  bunLockFor = pkgs:
    import ./build/bun-lock.nix {
      inherit lib pkgs;
    };
  buildJsSite = import ./build/js-site.nix {
    inherit bunLockFor errors;
  };
  buildSvelteSite = import ./build/svelte-site.nix {
    inherit
      bunLockFor
      errors
      paths
      writeNushellApplication
      ;
  };
  buildNpmVitest = import ./build/npm-vitest.nix;
  buildZigPackage = import ./build/zig-package.nix {};
  buildLibghosttyVt = import ./build/libghostty-vt.nix {inherit lib writeNushellApplication;};
  uvLockFor = pkgs:
    import ./build/uv-lock.nix {
      inherit lib pkgs;
    };
  buildUvApplication = import ./build/uv-application.nix {inherit uvLockFor ruffAnnArgs;};
  # Shared Elixir quality lane (compile -Werror + format + credo --strict + test),
  # injected with the single-source-of-truth strict Credo config so every Elixir
  # gate enforces the same policy. The Elixir counterpart of buildUvApplication.
  buildElixirCheck = import ./build/elixir-check.nix {credoConfig = ./elixir/credo.exs;};
  # hex for a given elixir, carrying the darwin sandbox allowance Mix needs
  # from 1.19 on. See lib/build/elixir-hex.nix.
  elixirHex = args: import ./build/elixir-hex.nix args;
  buildPyStrictCheck = import ./build/py-strict-check.nix {inherit lib;};
  buildGradleFatJar = import ./build/gradle-fat-jar.nix {inherit lib;};
  wrapPackage = import ./build/wrap-package.nix {inherit lib;};
  # Markdown document rendering with JSON-encoded YAML frontmatter. Used by
  # typed wrappers that generate small `.md` files with parseable metadata.
  markdown = import ./util/markdown.nix {inherit lib;};
  skills = import ./skills.nix {inherit lib paths;};
  users = import ./users.nix {inherit lib paths;};
  agents = import ./agents.nix {inherit lib markdown;};
  hermes = import ./hermes {};
  claudePlugin = import ./claude-plugin.nix {inherit lib skills;};
  # Shared JetBrains Islands palette (both variants), the single source of truth
  # for syntax color across the repo: the code-highlight crate embeds this JSON
  # for the search `-c` output, and the base profile generates its
  # Neovim colorscheme from the same data through this value.
  islandsTheme = lib.importJSON (paths.packagesRoot + "/code-highlight/src/islands-theme.json");
  # Repo-default JVM major: imported once here (single source of truth) and
  # threaded into `languages.java`, which re-exports it as
  # `ix.languages.java.defaultJvmVersion` for modules/examples that pin the JDK.
  defaultJvmVersion = import ./languages/jvm-defaults.nix;
  languages = {
    cpp = import ./languages/cpp.nix {inherit errors;};
    dhall = import ./languages/dhall.nix {};
    elixir = import ./languages/elixir.nix {inherit errors;};
    erlang = import ./languages/erlang.nix {inherit errors;};
    futhark = import ./languages/futhark.nix {};
    gleam = import ./languages/gleam.nix {};
    go = import ./languages/go.nix {inherit errors;};
    haskell = import ./languages/haskell.nix {inherit errors;};
    idris = import ./languages/idris.nix {};
    java = import ./languages/java {inherit errors lib defaultJvmVersion;};
    javascript = import ./languages/javascript.nix {inherit errors;};
    kotlin = import ./languages/kotlin.nix {inherit errors;};
    ocaml = import ./languages/ocaml.nix {inherit errors;};
    python = import ./languages/python.nix {inherit errors;};
    rust = import ./languages/rust.nix {inherit errors rust-overlay;};
    scala = import ./languages/scala.nix {inherit errors;};
    zig = import ./languages/zig.nix {inherit errors;};
  };
  inherit
    (import ./rust/tooling.nix {
      inherit
        lib
        packagePath
        languages
        writePythonApplication
        rustWorkspaceFor
        clippy-src
        lists
        pins
        evalTimeSubstitutable
        ;
      repoRoot = paths.root;
    })
    buildIxRustTool
    cargoUnitFor
    buildRustPackage
    repoRustToolchainFor
    ;
  cargoUnit = cargoUnitFor pkgs;
  cargoUnitExternal = import ./rust/external.nix {repoRoot = paths.root;};
  # Patch the vendored rnix inside a rust tool so it lexes underscore digit
  # separators in nix numeric literals; the alejandra/statix/deadnix package
  # dirs under packages/nix/ consume this. See its doc comment.
  rnixDigitSeparators = import ./util/rnix-digit-separators {
    rnix012Src = rnix-0-12-src;
    rnix014Src = rnix-0-14-src;
  };
  goUnitFor = pkgs:
    import ./build/go-unit.nix {
      inherit lib pkgs;
      inherit (languages) go;
    };
  goUnit = goUnitFor pkgs;
  # Per-TU content-addressed Linux kernel builds (kbuild-unit, #3411): the
  # kbuild analog of cargoUnitFor. Stage 1 harvests a monolithic kbuild's
  # .cmd files into a plan; stage 2 renders one derivation per unit.
  kernelUnitFor = pkgs:
    import ./kernel/kbuild-unit.nix {
      inherit lib pkgs;
      nixKbuildUnit = buildIxRustTool pkgs (packagePath "nix-kbuild-unit");
      writeBashApplication = writeBashApplication pkgs;
    };

  systemdHardening = import ./services/systemd-hardening.nix;

  /**
  TigerBeetle's documented deployment contract (memory floor, reference
  `--cache-grid`, `CAP_IPC_LOCK`) as data, so consumers running a replica
  (ix billing-ledger) size units from the vendor doc instead of re-deriving
  it. See [`lib/services/tigerbeetle.nix`](lib/services/tigerbeetle.nix).
  */
  tigerbeetle = import ./services/tigerbeetle.nix;

  /**
  Helpers that throw with a fixable error message instead of a deep-eval
  crash. See [`lib/util/errors.nix`](lib/util/errors.nix) for the full surface:
  `assertEnum`, `requireArg`, `requireAttr`.
  */
  errors = import ./util/errors.nix {inherit lib;};

  /**
  Recursive attrset merge with two collision policies (`strict` throws,
  `rhs` wins) plus an N-ary `strictList`. Single sanctioned replacement
  for hand-rolled deep-merge and the patterns the `no-recursive-update`
  rule flags. See [`lib/util/deep-merge.nix`](lib/util/deep-merge.nix).
  */
  deepMerge = import ./util/deep-merge.nix {inherit lib;};

  /**
  Utilities for option values that are later joined under a runtime
  directory.

  `isSafe` accepts relative paths with ordinary segments and rejects empty,
  absolute, `.`, `..`, and repeated-slash forms. Use `isSafeName` for values
  that become one directory entry rather than a nested path. `shellPath` and
  `shellParent` return shell snippets for joining a root expression such as
  `$out` with a validated relative path.
  */
  relativePath = import ./util/relative-path.nix {inherit lib;};

  /**
  List helpers not covered by `nixpkgs.lib`: `findDuplicates` (repeated
  elements) and `findDuplicatesBy` (elements colliding under a key function).
  See [`lib/util/lists.nix`](lib/util/lists.nix).
  */
  lists = import ./util/lists.nix {inherit lib;};

  /**
  General attrset helpers beyond `nixpkgs.lib`: `flattenToDotted` collapses a
  nested attrset to a flat one keyed by dotted paths (a config tree ->
  `key.path=value` flags or dotted env names). See
  [`lib/util/attrs.nix`](lib/util/attrs.nix).
  */
  attrs = import ./util/attrs.nix {inherit lib;};

  /**
  Build efx plan IR (`efx_ir::Plan` JSON) from Nix — the terranix
  replacement. `plan` / `effect` / `lit` / `ref` construct effects natively;
  `fromTerranix` translates a terranix-shaped `resource.<type>.<name>` config
  into effects, turning terraform interpolation strings into first-class efx
  references. Feed `builtins.toJSON (efx.plan ...)` to `efx plan/apply --ir`.
  See [`lib/util/efx.nix`](lib/util/efx.nix) and
  [`packages/efx/README.md`](packages/efx/README.md).
  */
  efx = import ./util/efx.nix {inherit lib lists;};

  /**
  TOML value encoding. `scalar` renders one Nix scalar as the TOML literal a
  `key = value` pair expects (codex `--config a.b=1` flags). Scalars only;
  for whole TOML files use `pkgs.formats.toml`. See
  [`lib/util/toml.nix`](lib/util/toml.nix).
  */
  toml = import ./util/toml.nix {inherit lib;};

  /**
  Read a package's pinned hashes/digests from a sibling `pins.json` instead
  of inlining `hash = "sha256-..."` in the `.nix`. `loadPins ./pins.json`
  returns the validated `{ name = { hash; ... }; }` map; `loadPin ./pins.json
  "src"` returns one named entry. The JSON is the single source of truth an
  updater rewrites, so a bump touches one data file. See
  [`lib/util/pins.nix`](lib/util/pins.nix).
  */
  pins = import ./util/pins.nix {inherit lib;};

  /**
  Wrap a comment-capable `pkgs.formats` generator (keyValue, ini, toml, yaml)
  so each generated file opens with a `# generated by <file>:<line>`
  provenance header; the position comes from `builtins.unsafeGetAttrPos` or
  an explicit `{ file, line }` pair. See
  [`lib/util/format-provenance.nix`](lib/util/format-provenance.nix).
  */
  formatProvenance = import ./util/format-provenance.nix {inherit lib;};

  /**
  Single source of truth for the MCP servers baked into the agent wrappers.
  Define a server once in a neutral shape and render it to each tool's native
  config with `mcp.toClaudeJson` (Claude Code's `mcpServers` JSON) and
  `mcp.toCodexEntries` (dotted `mcp_servers.*` codex `-c` flags) and
  `mcp.toCursorJson` (cursor-agent's `mcp.json` object), so `index`
  is declared in one place rather than copied into both wrappers. See
  [`lib/util/mcp.nix`](lib/util/mcp.nix).
  */
  mcp = import ./util/mcp.nix {inherit lib;};

  /**
  Drop the `meta.license` marker on a vendored proprietary binary, so the
  per-system flake package set (evaluated without `allowUnfree`) can build a
  wrapper around it. Shared by the vendored-agent wrappers (claude-code,
  cursor-cli); see [`lib/util/vendored-unfree.nix`](lib/util/vendored-unfree.nix)
  for the full rationale.
  */
  allowVendoredUnfree = import ./util/vendored-unfree.nix {};

  mkMinecraftLoader = import ./minecraft/loader.nix;

  /**
  Declare a continuous-benchmark suite against the `indexbench` CLI.

  `mkBenchSuite pkgs { name; indexbench; macros ? []; allocCheck ? null; runs ? 10; }`
  returns `{ app; check ? }`:

  - `app` is a `nix run`-able wrapper that runs the suite's macro commands
    through `indexbench run`, recording timing, peak RSS, and any `@bench`
    custom metrics, and exiting non-zero on a regression. Belongs in
    `apps.bench` / the perf job, never in `checks` (timing and RSS are not
    reproducible in the Nix sandbox).
  - `check`, present only when `allocCheck = { bench; budgets; }` is set, is a
    `nix flake check` derivation that runs the bench once through
    `indexbench assert` and fails if a metric exceeds its budget. Allocation
    counts are reproducible, so this path is a real, hermetic CI gate.

  See [`lib/util/bench.nix`](lib/util/bench.nix) for the argument shape.
  */
  mkBenchSuite = import ./util/bench.nix {
    inherit lib writeNushellApplication;
  };

  /**
  Repo-owned Minecraft helpers exposed through `specialArgs.ix` and the
  flake's `lib` output.

  - `nbt`: typed NBT-tag constructors. Plain Nix scalars (attrset, list,
    string, bool, int, float) round-trip to compound, list, string, byte,
    int/long, and double tags. These constructors are the escape hatch for
    Minecraft's narrower tag types: bytes, shorts, floats, typed numeric
    arrays, and named roots.
  - `dimensionType`: vanilla dimension-type JSON snapshots plus a `withBase`
    merge helper. Lets `services.minecraft.datapacks.<n>.dimensionTypes.<dim>`
    set `base = "minecraft:overworld"` and override only the height knobs
    (or any other field) instead of restating the whole schema. See
    [`lib/minecraft/dimension-type.nix`](lib/minecraft/dimension-type.nix).
  */
  minecraft = {
    nbt = import ./minecraft/nbt.nix;
    dimensionType = import ./minecraft/dimension-type.nix {inherit lib deepMerge;};
  };

  /**
  Build a `pkgs.formats`-style generator for Minecraft NBT data.

  Arguments:
  - `pkgs`: package set used to build the encoder and output derivation.
  - `format`: `snbt` for readable stringified NBT or `nbt` for binary NBT.
  - `flavor`: binary NBT compression flavor: `uncompressed`, `gzip`, or
    `zlib`. Ignored for `snbt`.

  Returns an attrset with `type` and `generate`, matching `pkgs.formats.*`.
  */
  mkMinecraftNbtFormat = import ./minecraft/nbt-format.nix {
    inherit lib buildIxRustTool packagePath;
  };

  /**
  Build the `minecraft-sync-managed` wrapper for a Minecraft service.

  The wrapper passes the mutable data directory, managed `/etc/minecraft`
  roots, datapack worlds, reload settings, and RCON settings to the Rust
  sync tool. The tool then syncs ordinary managed files and datapacks, and
  reconciles `whitelist.json` and `ops.json` against the live server files
  by UUID.
  */
  mkMinecraftSyncManaged = args:
    import ./minecraft/sync-managed.nix (
      {
        package = buildIxRustTool pkgs (packagePath "minecraft-sync-managed");
        inherit writeNushellApplication;
      }
      // args
    );

  /**
  Pinned artifact catalogs surfaced to images and presets by name.
  Presets must consume entries through this set (or one of the module
  options it seeds) rather than inlining URLs and hashes.
  */
  artifacts = import ./util/artifacts.nix {inherit lib pkgs paths;};

  /**
  Flake-output-only repo packages, callPackage-style.

  These are derivations that flake consumers can reach as
  `packages.<system>.<name>`, but that we don't want to inject into the
  nixpkgs namespace inside an image's evaluation. Each entry takes the
  standard `pkgs` it should build against and the cross-cutting
  `specialArgs.ix` bundle.
  */
  packageSetFor = import ./packages.nix {
    inherit
      lib
      packageRegistry
      ixSpecialArgs
      cargoUnitFor
      goUnitFor
      rustWorkspaceFor
      clippy-src
      ;
  };

  /**
  Shared Rust workspace source and unit graph for repo-owned crates.

  The root Cargo.toml and Cargo.lock are the source of truth for IDEs,
  dependency versions, and package builds. The filtered source keeps the Nix
  closure to Rust workspace inputs instead of the full repository.

  `rustWorkspaceFor pkgs` returns `{ root; src; cargoLock; units; ghosttyLibDir; }` for the
  caller's package set. The default `rustWorkspace` uses the repo's
  `x86_64-linux` package set for image and module evaluation.
  */
  # Bound here rather than inline in `rustWorkspaceFor` below because two
  # consumers need it: that workspace builder, and `sharedHelpers`, which is
  # the `ix` module argument `lib/dev/profiles.nix` builds the dev toolchain
  # through.
  rustToolchainFor = languages.rust.toolchain;

  rustWorkspaceFor = import ./rust/workspace.nix {
    inherit
      lib
      paths
      packageRegistry
      cargoUnitFor
      buildSvelteSite
      buildLibghosttyVt
      writeBashApplication
      macosSdk
      appleSdkToolchain
      pins
      ;
    ghosttySrc = ghosttySource;
    inherit rustToolchainFor;
    # The ix rustc fork for the native unit graph (workspace.nix's
    # nativeForkToolchain). Built straight off its registry path with the
    # minimal `ix` closure the package reads (pins + languages + pkgs), the
    # same bootstrap shape tooling.nix uses for llm-clippy: resolving it
    # through the assembled package set would let the overlay that builds
    # rustc-ix recurse into itself, and making it the tooling.nix default
    # would move `defaultToolchainId` under every existing prebuilt-unit
    # assertion.
    forkRustToolchainFor = forkPkgs:
      forkPkgs.callPackage (packagePath "rustc-ix") {
        ix = {
          pkgs = forkPkgs;
          inherit languages pins;
        };
      };
  };
  rustWorkspace = rustWorkspaceFor pkgs;

  /**
  Host-language build glue for unibind-annotated crates
  (`unibind.build { crate; targets; }`): generated stubs, the merged python
  site tree, the strict type gate, the importable module, and the wheel, all
  from the crate's cdylib in the shared workspace graph. Bound per package
  set like `rustWorkspaceFor`; the default binds the repo's x86_64-linux set.
  See [packages/unibind/nix](packages/unibind/nix).
  */
  unibindFor = unibindPkgs:
    import (paths.packagesRoot + "/unibind/nix/build.nix") {
      inherit lib packageRegistry buildPyStrictCheck;
      pkgs = unibindPkgs;
      rustWorkspace = rustWorkspaceFor unibindPkgs;
      cargoUnit = cargoUnitFor unibindPkgs;
      rustToolchain = languages.rust.toolchain unibindPkgs;
      wheelBuilder = paths.root + "/lib/build/pyo3-wheel.py";
    };
  unibind = unibindFor pkgs;

  /**
  Pinned macOS SDK used to cross-compile Rust to Darwin from Linux. A
  function `{ pkgs }: derivation`; override it to supply your own SDK.
  See [`lib/darwin/macos-sdk.nix`](lib/darwin/macos-sdk.nix).
  */
  macosSdk = import ./darwin/macos-sdk.nix {inherit pins;};

  /**
  Linux -> Darwin nixpkgs cross scope for C/C++ closures that build through
  upstream nixpkgs packaging (`nix-ix`), with the linux-unbuildable apple
  toolchain pieces shimmed by llvm equivalents and SDK-lifted stubs.
  `pkgs: target: package set`.
  See [`lib/darwin/nixpkgs-cross.nix`](lib/darwin/nixpkgs-cross.nix).
  */
  darwinCrossPkgs = import ./darwin/nixpkgs-cross.nix {inherit macosSdk pins;};

  /**
  Verified Mac App Store `name -> numeric ID` catalog for nix-darwin
  `homebrew.masApps`. Select entries with `lib.getAttrs [names] ix.masApps`
  so a typo is an eval error instead of a zap-uninstall.
  See [`lib/darwin/mas-apps.nix`](lib/darwin/mas-apps.nix).
  */
  masApps = import ./darwin/mas-apps.nix;

  /**
  Shared git policy lists: `astMergeAttributes` (one `<glob> merge=ast-merge`
  line per supported language) and `globalIgnores` (editor/build/agent
  droppings for `core.excludesfile`). Render with `lib.concatLines`.
  See [`lib/util/git-defaults.nix`](lib/util/git-defaults.nix).
  */
  gitDefaults = import ./util/git-defaults.nix;

  /**
  Reusable "do not overlap" wrapper for scheduled agents (launchd or manual
  runs): a non-blocking per-name flock(2) via /usr/bin/perl (macOS ships no
  flock(1)); if the previous run still holds the lock the new fire exits 0
  silently, and the lock always releases on exit including crash/kill.
  `withLockFor pkgs` returns `{ package, wrap }` where
  `wrap label args = ["<store>/bin/with-lock" label "--"] ++ args`, ready to
  splice into a launchd agent's ProgramArguments.
  See [`lib/util/with-lock.sh`](lib/util/with-lock.sh).
  */
  withLockFor = pkgs: let
    bin = writeBashApplication pkgs {
      name = "with-lock";
      runtimeInputs = [pkgs.coreutils];
      text = builtins.readFile ./util/with-lock.sh;
    };
  in {
    package = bin;
    wrap = label: args:
      [
        "${bin}/bin/with-lock"
        label
        "--"
      ]
      ++ args;
  };

  /**
  zig + macOS SDK cross toolchain. `{ appleSdk, lib, pkgs, target }` returns
  `{ env, runtimeInputs, rustcArgsForPlatform }` consumed by
  `rustWorkspace.unitsFor`. See [`lib/darwin/apple-sdk-toolchain.nix`](lib/darwin/apple-sdk-toolchain.nix).
  */
  appleSdkToolchain = import ./darwin/apple-sdk-toolchain.nix;

  /**
  GHC as a Linux-hosted cross compiler targeting Darwin, built with the
  apple-sdk toolchain above; args are the exact nixpkgs deps it needs (see
  the file) plus `{ target, toolchain }`, returning the compiler derivation. See [`lib/darwin/cross-ghc.nix`](lib/darwin/cross-ghc.nix).
  */
  crossGhc = import ./darwin/cross-ghc.nix {
    ghcSrc = builtins.path {
      path = paths.root + "/views/ghc-cross";
      name = "ghc-cross-view";
    };
  };

  /**
  Setup-based builder that compiles a Haskell package plus its library
  closure with `crossGhc`, reusing the pinned nixpkgs haskellPackages for
  sources and the dependency graph. `{ crossGhc, haskellPackages, lib, llvmPackages, stdenv }`
  returns `{ build }`. See [`lib/darwin/cross-haskell.nix`](lib/darwin/cross-haskell.nix).
  */
  crossHaskell = import ./darwin/cross-haskell.nix;

  # Single source of truth for the ix public binary cache identity (URL + the
  # `ix-workspace:` trusted key that verifies its narinfos). See ./cache.nix.
  cache = import ./cache.nix;
  kdl = import ./formats/kdl.nix {inherit home-manager;};

  # Import-seam gate for fork-syntax islands; frozen syntax by design, see
  # the file header.
  evaluatorGate = import ./evaluator-gate.nix;

  /**
  Helper surface shared by both the per-module `specialArgs.ix`
  (`ixSpecialArgs`) and the public `index.lib` (`ixReturn`). Listed once
  here so a new shared helper reaches both surfaces from a single edit;
  each consumer splices its own extras on top with `//`.
  */
  sharedHelpers = {
    inherit (import ./util/endpoint.nix {inherit lib;}) endpoint endpointOf;
    inherit
      agents
      allowVendoredUnfree
      artifacts
      attrs
      buildElixirCheck
      buildGradleFatJar
      buildJsSite
      buildLibghosttyVt
      buildNpmVitest
      buildPyStrictCheck
      buildSvelteSite
      buildUvApplication
      elixirHex
      buildZigPackage
      cache
      cargoUnit
      checks
      claudePlugin
      deepMerge
      discoverTree
      efx
      evalTimeSubstitutable
      evaluatorGate
      formatProvenance
      gitDefaults
      goUnit
      hermes
      kernelUnitFor
      languages
      kdl
      lists
      masApps
      mcp
      minecraft
      mirrorPackages
      mkBenchSuite
      mkMinecraftLoader
      mkMinecraftNbtFormat
      wrapPackage
      mkMinecraftSyncManaged
      netCidr
      paths
      pins
      provenance
      publicArtifactsFor
      relativePath
      repoRustToolchainFor
      rnixDigitSeparators
      ruffAnnArgs
      rustToolchainFor
      rustWorkspace
      rustWorkspaceFor
      secretRefs
      securityRoots
      selfVersionFor
      skills
      systemdHardening
      tigerbeetle
      toml
      unibind
      unibindFor
      users
      withLockFor
      writeBashApplication
      writeNushellApplication
      writeProcessComposeApplication
      writePythonApplication
      writeRustApplication
      ;
    btopSrc = btop-src;
    home-managerSrc = home-manager-src;
    gitSrc = git-src;
    starshipSrc = starship-src;
    jjSrc = jj-src;
    nushell = nushell-src;
    nushellSrc = nushell-src;
    codexSrc = codex-src;
    curlSrc = curl-src;
    clippySrc = clippy-src;
    nixSrc = nix-src;
    nix-fast-buildSrc = nix-fast-build-src;
    nix-derivationSrc = viewSource "nix-derivation";
    rnix-0-12Src = rnix-0-12-src;
    rnix-0-14Src = rnix-0-14-src;
    drgnSrc = drgn-src;
    perftestSrc = perftest-src;
    fffSrc = fff-src;
    nuJupyterKernelSrc = nu-jupyter-kernel-src;
    launchkSrc = launchkSource;
    nixNinjaSrc = nix-ninja-src;
    snixSrc = snix-src;
    ghosttySrc = ghosttySource;
    mesaSrc = mesa-src;
    # Pinned toolchain evaluation context for the prebuilt public-SDK rlib:
    # the exact nixpkgs + rust-overlay sources whose evaluation reproduces the
    # toolchain id recorded in the artifact's manifest. Consumed only by
    # packages/sdk/rust/build.nix; see the input comments in flake.nix.
    sdkPrebuiltNixpkgsSrc = sdk-prebuilt-nixpkgs;
    sdkPrebuiltRustOverlaySrc = sdk-prebuilt-rust-overlay;
  };

  /**
  Cross-cutting helpers handed to every module through `specialArgs.ix`.
  Keep this surface small and stable: anything here is part of the
  cross-module contract.
  */
  ixSpecialArgs =
    sharedHelpers
    // {
      inherit buildRustPackage islandsTheme nixPackage nixPackageFor;
      packages = packageSetFor pkgs;
      # The flox CLI as built by flox's own flake (own nixpkgs pin, so the
      # derivation is exactly what cache.flox.dev serves). Consumed by
      # modules/profiles/base, which wraps it (metrics default) and ships it
      # in the consensus baseline.
      floxPackage = flox.packages.${system}.flox;
    };

  inherit
    (import ./image {
      inherit
        self
        lib
        nixpkgs
        rust-overlay
        paths
        system
        home-manager
        overlays
        ixSpecialArgs
        moduleList
        writeNushellApplication
        packageSetFor
        portableServices
        ;
    })
    evalImageConfig
    mkImage
    mkFleetFor
    mkFleet
    mkVmFor
    mkVm
    mkDevFor
    mkDev
    ;

  /**
  Import a `.ix` (JavaScript-syntax Nix) module: the in-eval `builtins.wasm`
  conversion through the `ix2nix-wasm` package output. That output is a
  derivation, so importing a `.ix` file IS import-from-derivation and needs
  either a warm `cache.ix.dev` or a builder that can produce the wasm32
  artifact. Requires an
  evaluator with `wasm-builtin` (`ix eval` / `ix apply`, or nix-ix with the
  feature in `extra-experimental-features`); on anything else importing a
  `.ix` file throws with those instructions. CI bootstraps the nix-ix client
  (.github/actions/bootstrap-patched-nix), so repo evals qualify. Once nix
  has proper parallel IFD we hope to drop the wasm converter and run the
  native ix2nix binary instead.

  `importIx`/`importIxWasm` and the `*For` variants are one implementation:
  the conversion no longer depends on a host package set, but the four names
  stay because scaffolded flakes and repo callers consume all of them
  (#4125).
  */
  /**
  The `.ix` converter, built rather than committed (2026-07-25).

  `lib/ix2nix.wasm` used to be a checked-in artifact so that importing a `.ix`
  module realized nothing mid-eval. That bought offline, builder-free evals at
  the cost of a binary in the tree that silently goes stale: the freshness gate
  caught it, nothing fixed it, and `flake-check` sat red on main until someone
  ran `ix2nix-wasm-regen` by hand.

  Now the converter is the `ix2nix-wasm` package output, so importing a `.ix`
  module is IFD. It cannot drift from the crate source, because it IS the crate
  source. The trade is deliberate and is a real cost, not a free win: an eval
  that touches a `.ix` file now needs that wasm32 output, and the artifact is
  built on x86_64-linux, so a Mac evaluating `.ix` modules needs a remote
  builder or a warm `cache.ix.dev`. That reverses #4125/#4127, which introduced
  the committed artifact to keep `ix init` scaffold evals off the wasm32 graph.

  Always built on x86_64-linux, whatever the host system asks for, which is
  why `importIxWasmFor` still ignores its argument exactly as it did when the
  artifact was committed. The pin is load-bearing, not a cache optimization:
  the wasm32 output is NOT bit-identical across build hosts, because the
  native toolchain's store path feeds `-C metadata` and so the symbol hashes
  differ per host. Without the pin a Mac would build its own converter, and
  every `.ix` module would convert through different bytes there than in CI.
  The committed artifact was pinned to the x86_64-linux build for that same
  reason; see the note on the retired `fresh` gate in packages/ix2nix/wasm.

  Callers that already hand in their own converter -- scaffolded flakes, the
  e2e -- are unaffected: `converter` was always a parameter of import-ix.nix.
  */
  importIxWasmFor = _hostSystem: importIxWasm;

  importIxWasm = import (paths.root + "/packages/ix2nix/import-ix.nix") {
    # `<out>/lib/ix2nix.wasm`, not the output root: `builtins.wasm` wants the
    # object itself and reading the directory fails with "read of 96 bytes: Is
    # a directory". Same path the package's own e2e passes.
    converter = "${ix2nixWasmFor "x86_64-linux"}/lib/ix2nix.wasm";
  };

  importIxFor = importIxWasmFor;

  importIx = importIxWasm;

  /**
  VM templates (RFC 0042 / ix#9242): renders a `default.ix` config's
  `templates` + `instances` exports into VMs. Needs only `lib` and `errors` --
  a template calls `index.lib.mkVm` itself, so nothing in there wants the
  image machinery or a host system, which is also why it is not curried over
  one the way `mkVmFor`/`mkDevFor` are.

  Not to be confused with the flake's own `templates` output (`ix dev init`
  scaffolds, RFC 0007): that one scaffolds a `default.ix`, this one
  instantiates the templates inside one.
  */
  templates = import ./templates.nix {inherit lib errors;};

  inherit
    (import ./discovery.nix {
      inherit
        lib
        paths
        importIxFor
        mkFleetFor
        mkVmFor
        mkDevFor
        ixReturn
        ;
    })
    discoverTree
    discoverModules
    exampleFleetsFor
    ;

  # Self-reference (let-bindings are mutually recursive): `exampleFleetsFor`
  # passes `ixReturn` back into examples as `index.lib`. Forced only when
  # an example actually reads from it.
  ixReturn =
    sharedHelpers
    // {
      inherit
        appleSdkToolchain
        bunLockFor
        crossGhc
        crossHaskell
        cargoUnitFor
        cargoUnitExternal
        darwinCrossPkgs
        discoverModules
        errors
        evalImageConfig
        exampleFleetsFor
        goUnitFor
        importIx
        importIxFor
        importIxWasm
        importIxWasmFor
        kernelUnitFor
        macosSdk
        mkDev
        mkDevFor
        mkFleet
        mkFleetFor
        mkImage
        mkPackageRegistry
        mkPackageSet
        mkVm
        mkVmFor
        nixPackage
        nixPackageFor
        nixosModules
        overlay
        overlays
        packageSetFor
        pkgs
        portableServices
        system
        templates
        uvLockFor
        ;

      /**
      Nous Research's Hermes agent flake. Examples consume
      `index.lib.hermesAgent.nixosModules.default` to add the
      `services.hermes-agent.*` option surface to an image, plus
      `index.lib.hermesAgent.overlays.default` if they want the
      `hermes-agent` package available at module-eval time.
      */
      hermesAgent = hermes-agent;
    };
in
  ixReturn
