{
  lib,
  pkgs,
  nixCargoUnit,
  rust,
}: let
  inherit
    (builtins)
    attrNames
    elem
    filter
    genericClosure
    hasAttr
    head
    isList
    isString
    length
    removeAttrs
    replaceStrings
    toString
    ;

  inherit (lib) escapeShellArg;

  # The toolchain id baked into every unit hash for the default toolchain.
  # Exposed so callers of `mkPrebuiltLibraryUnit` can record and assert the id a
  # prebuilt rlib was compiled with without reconstructing it by hand. The id
  # rule itself lives at the toolchain owner (`rust.toolchainId`).
  defaultToolchainId = rust.toolchainId rust.defaultRustToolchain;

  # Apply the rustflags a normal `cargo build` reads from `.cargo/config.toml`,
  # which cargoUnit otherwise ignores (it assembles rustc args itself instead of
  # going through cargo). Parsing the config here is the only route: cargo's
  # `cargo build --unit-graph` does NOT carry rustflags (each unit records only
  # dependencies/features/mode/pkg_id/platform/profile/target), because cargo
  # resolves config rustflags at compile time and applies them when it invokes
  # rustc, which cargoUnit bypasses by invoking rustc per unit from the graph. So
  # there is nothing in the graph to pick up automatically; we read the config.
  # Returns the rustc args for a target triple following cargo precedence:
  # `target.<triple>.rustflags` wins outright over `build.rustflags` (cargo does
  # not merge the two). Flags may be a TOML array or a single whitespace-
  # separated string. `cfg(...)` target sections and the `[env]` table are NOT
  # honored (cargo evaluates those against the full target cfg set, which this
  # static parse does not reproduce). A `configPath` that does not exist yields
  # no flags, so callers may pass the path unconditionally.
  rustflagsFromCargoConfig = configPath: platform: let
    config = lib.importTOML configPath;
    normalize = flags:
      if builtins.isList flags
      then flags
      else filter (flag: flag != "") (lib.splitString " " flags);
    chosen = config.target.${platform}.rustflags or config.build.rustflags or null;
  in
    # Lazy: the `&&` short-circuits, so `config` (hence `importTOML`) is only
    # forced when the file exists and carries rustflags.
    if builtins.pathExists configPath && chosen != null
    then normalize chosen
    else [];

  emptyTestPolicy = {
    skip = [];
    testThreads = null;
  };

  testPolicyFields = attrNames emptyTestPolicy;

  normalizeTestPolicy = packageName: rawPolicy: let
    unknownFields = filter (field: !(elem field testPolicyFields)) (attrNames rawPolicy);
    policy = emptyTestPolicy // rawPolicy;
    nonStringSkips = filter (testName: !(isString testName)) policy.skip;
  in
    assert lib.assertMsg (unknownFields == [])
    "cargoUnit.buildWorkspace testPolicyByPackage.${packageName} has unknown fields: ${lib.concatStringsSep ", " unknownFields}";
    assert lib.assertMsg (
      isList policy.skip && nonStringSkips == []
    ) "cargoUnit.buildWorkspace testPolicyByPackage.${packageName}.skip must be a list of strings";
    assert lib.assertMsg (policy.testThreads == null || isString policy.testThreads)
    "cargoUnit.buildWorkspace testPolicyByPackage.${packageName}.testThreads must be null or a string"; policy;

  libtestArgsForTestPolicy = policy:
    lib.concatMap (testName: [
      "--skip"
      testName
    ])
    policy.skip
    ++ lib.optionals (policy.testThreads != null) [
      "--test-threads"
      policy.testThreads
    ];

  nextestFilterForTestPolicy = policy:
    lib.optionalString (policy.skip != [])
    "-E ${escapeShellArg "not (${lib.concatMapStringsSep " | " (testName: "test(~${testName})") policy.skip})"}";

  /**
  Plan a Rust workspace as one Nix derivation per Cargo rustc unit.

  `helpers` exposes the planner source, graph and renderer before generated
  units are imported. `workspace` is the normal buildWorkspace result.
  Derivation-produced sources must be resident before helper identities can
  be evaluated; planning does not remove that earlier source dependency.

  Each generated unit gets a scoped source input by default. Workspace crates
  receive their own package root, and registry/git crates receive their own
  vendored package directory. A source edit in `crates/api` does not change
  the Nix input for `crates/worker`, `itoa`, or `ryu`; a `Cargo.lock` update
  for one transitive crate leaves unrelated vendored crate derivations alone.
  Git dependency `outputHashes` are keyed by the exact `Cargo.lock` source
  string, including the locked rev, so multi-package git repos share one
  tree hash without losing package identity.
  The planning stage is manifest-scoped the same way: the whole-workspace
  `cargo --unit-graph` IFD runs in a content-stubbed tree (manifests
  verbatim, every other source file an empty stub at its exact path), so a
  source body edit re-runs only the cheap render IFD (whose include-scan
  reads real contents), never the workspace-wide cargo resolve; adding or
  removing files, or editing a manifest, re-plans (#3900).
  Pass `workspaceRoot = ./.` for local workspaces so `src` can stay a filtered
  build input while package scopes are carved from the real checkout root.
  Rendering fails when a unit path cannot be tied back to `src` or `vendorDir`.
  Pass `cargoTargets = [ [ "--workspace" ] [ "--workspace" "--tests" ] ]`
  to expose roots from several Cargo executions through one generated graph.
  Roots are consumed lazily: `binaries.<name>`, `libraries.<name>`, and
  `targetSets.<set>.*` each reference one rustc unit derivation, so selecting
  a subset of roots (say the native cdylibs out of a graph that also plans a
  wasm target) never builds the other entries' units. A second buildWorkspace
  call that only narrows `cargoTargets` yields byte-identical root
  derivations (pinned by a tests/default.nix assertion) and adds a unit-graph
  plus render IFD; create a separate workspace only when unit identity
  changes (profile, policy, rustToolchain, env, extraRustcArgs). `env` and
  `extraRustcArgs` fold into every unit, so values or native-library flags for
  one crate bust or perturb the whole dependency closure; scope them with
  `packageBuildEnv.<package> = { ... }` or
  `packageRustcArgs.<package> = [ ... ]` instead.
  Every unit compiles with `--remap-path-prefix` over its own source and over
  the toolchain's `rust-src`, unconditionally: `file!()` expands to an absolute
  path, which here is a store path, so without it every `unwrap` location in a
  dependency pins that dependency's source into the runtime closure of the
  linked binary and a 23 MB executable retains 2.5 GiB. Panic messages and
  backtraces name `/build/<crate>-<version>` and `/rustc` in place of the store
  path; `include_str!` and `env!("CARGO_MANIFEST_DIR")` read the real
  filesystem and are unaffected, so a crate that deliberately embeds a store
  path keeps it. Top-level
  `binaries`/`libraries` dedupe by Cargo target name and the first
  `cargoTargets` entry wins, so when one crate roots under several entries,
  select through `targetSets.<set>` instead. Per-case discovery is the
  exception to per-root laziness: `tests.<target>.cases` uses a shared
  manifest IFD that builds every test binary in the graph, and
  `doctests.<target>.cases` uses a shared doctest manifest covering every
  doctest target.
  Include `--benches` or `--bench <name>` to expose `[[bench]]` roots under
  `benchmarks` and `benchmarkPlan`. Tango benches can compare previous and
  next artifacts with `next.compareTangoBenchmarks { baseline = previous; }`,
  where `previous` is another generated workspace or a `benchmarkPlan` path.
  Test graphs also expose `coverageReport` and `makeCoverageReport`; build the
  workspace with `extraRustcArgs = [ "-Cinstrument-coverage" ]` and consume the
  generated `$out/lcov.info`. The selected Rust toolchain must provide matching
  `llvm-cov` and `llvm-profdata`, or callers must pass explicit tool paths to
  `makeCoverageReport`.

  `cargoConfigRustflags = true` applies the rustflags a normal `cargo build`
  would read from `<workspaceRoot>/.cargo/config.toml` (cargoUnit otherwise
  ignores cargo's config). Flags are resolved per target triple with cargo
  precedence (`target.<triple>.rustflags` over `build.rustflags`); `cfg(...)`
  target sections and the `[env]` table are not honored. Default off.

  Returns the generated attrset with `sourceAudit`, `units`, `roots`, `checkedRoots`,
  `packages`, `binaries`, `libraries`, `benchmarks`, `coverageReport`, `default`,
  `policyChecks`, plus the intermediate `plannerSource`, `unitGraphJson`,
  `unitsNix`, and `vendorDir` derivations for inspection (`unitGraphJson`'s
  paths are templated: `@workspaceRoot@` for the planner stub, which stands in
  for `src`, and `@vendorRoot@` for the vendor dir. The render stage fills both
  in. Templating is what keeps this metadata file's closure its own size rather
  than the vendor dir's).

  `testDiscovery` selects how the shared test manifest IFD learns #[test]
  names: `"binary"` (default) builds and runs every test binary with
  `--list --format terse`; `"dump-test-names"` compiles each harness test
  target with the ix rustc fork's `-Zdump-test-names` flag
  (rust-lang/rust#50297), which stops before codegen and linking, so a
  cold-store discovery never links a test binary. Option-driven, not
  feature-detected: probing the toolchain for the flag would itself cost an
  eval-time IFD, which is the exact cost this mode exists to remove, and a
  mis-selected upstream toolchain fails loudly ("unknown unstable option")
  rather than silently. Select it together with a fork toolchain
  (`rustToolchain = <rustc-ix package>`). Both modes produce byte-identical
  manifest files (pinned by the fork-discovery fixtures in
  tests/default.nix), and the choice never perturbs build-unit identity:
  only the manifest and its discovery inputs differ.

  `testPolicyByPackage.<package>` accepts structured test-runner policy:
  `{ skip = [ "case_name" ]; testThreads = "1"; }`. `buildWorkspace` renders
  it to libtest args for cargo-unit's per-test runner and to cargo-nextest
  filters for `testChecksByTarget`. Callers should pass policy data, not
  runner-specific argv.

  `nextestNoTestsByTarget.<target>` accepts "pass" or "fail"; omitted
  targets explicitly default to "pass". Required suites should select "fail".
  `wrapNextestTarget` is a target-name -> derivation -> derivation function
  applied once before runner aliases and the aggregate capture the map.

  `rust.resolveArgs` resolves the shared bundle (context, policy, linker,
  effects, checks) once; the two IFD stages and the unit import below read the
  once-resolved values (configScript, toolchainId, cargoLockPath, render flags,
  mold/clippy args, workspace checks) straight off it. The remaining knobs
  (`profile`, `target`, `contentAddressed`, `cargoTargets`,
  `extraUnits`/`extraLibraries`, the `test*` forwarding) each have a single
  reader and are read from raw args at that use site.
  */
  planWorkspace = rawArgs: let
    resolved = rust.resolveArgs rawArgs;
    inherit
      (resolved)
      context
      effects
      policy
      checks
      ;
    # A flat view of the resolved context for the field readers below; the
    # once-resolved values (configScript, toolchainId, cargoLockPath, render
    # flags, mold args, clippy args, checks) are read straight off the bundle.
    args =
      context
      // {
        inherit policy;
        inherit (resolved) cargoArgs;
      };
    inherit (args) vendorDir vendorSources;

    workspaceRoot =
      rawArgs.workspaceRoot or (throw ''
        cargoUnit.buildWorkspace requires workspaceRoot = ./path/to/workspace.
        Use workspaceRoot for the real checkout root that package-shaped sources can be carved from.
        Fetched or patched sources pass workspaceRoot = src.
      '');

    # The list of cargo invocations to plan: the graph builder and the
    # target-set naming both consume it, and it must be non-empty.
    cargoTargets = let
      targets = rawArgs.cargoTargets or [args.cargoArgs];
    in
      if targets == []
      then throw "cargoUnit.buildWorkspace requires at least one cargoTargets entry"
      else targets;

    explicitExtraUnits = rawArgs.extraUnits or {};
    extraLibraries = rawArgs.extraLibraries or {};
    testPolicyByPackage = lib.mapAttrs normalizeTestPolicy (rawArgs.testPolicyByPackage or {});
    testArgsFromPolicyByPackage =
      lib.mapAttrs (
        _packageName: libtestArgsForTestPolicy
      )
      testPolicyByPackage;
    explicitTestArgsByPackage = rawArgs.testArgsByPackage or {};
    testArgPolicyOverlap = filter (packageName: hasAttr packageName explicitTestArgsByPackage) (
      attrNames testArgsFromPolicyByPackage
    );
    testArgsByPackage = assert lib.assertMsg (testArgPolicyOverlap == [])
    "cargoUnit.buildWorkspace received both testPolicyByPackage and testArgsByPackage for: ${lib.concatStringsSep ", " testArgPolicyOverlap}";
      testArgsFromPolicyByPackage // explicitTestArgsByPackage;
    packageTestInputs = rawArgs.packageTestInputs or {};
    packageTestEnv = rawArgs.packageTestEnv or {};
    testRunPrelude = rawArgs.testRunPrelude or "";
    # Validated here (not only in the rendered file) so a typo fails at the
    # buildWorkspace call site with the caller's context, before any IFD.
    testDiscovery = let
      value = rawArgs.testDiscovery or "binary";
    in
      assert lib.assertMsg (elem value ["binary" "dump-test-names"])
      "cargoUnit.buildWorkspace testDiscovery must be \"binary\" or \"dump-test-names\", got ${toString value}"; value;

    # Every per-package table below is consumed in the rendered units file as
    # `<table>.${packageName} or <empty>`, so a key that names no package in
    # the graph is silently dropped: the value never reaches a unit and the
    # build still succeeds, which is how a `packageBuildEnv` scoping fix can
    # be a complete no-op with zero signal (ENG-10675 -- and the whole point
    # of `packageBuildEnv` is that the workspace-wide fallback is gone, so
    # there is nothing left to make the miss visible). Default-deny on the
    # keys instead.
    #
    # The universe is every package name in the lock, which covers workspace
    # members and vendored dependencies alike (`packageBuildEnv.libsqlite3-sys`
    # is a vendored crate). It is a superset of the names the renderer actually
    # tags units with -- a lock entry gated behind a cfg the graph never
    # resolves is accepted here -- because the exact set only exists after the
    # render IFD, and a typo is what this catches. Reading the lock is a plain
    # `importTOML` of a path, so it costs no IFD and fails in seconds.
    cargoLockPackageNames = lib.unique (
      map (package: package.name) (lib.importTOML context.cargoLockPath).package
    );
    packageBuildEnv = rawArgs.packageBuildEnv or {};
    packageRustcArgs = rawArgs.packageRustcArgs or {};
    unknownPackageKeyProblems = label: table:
      map (
        packageName: "${label}.${packageName} is not a package in Cargo.lock"
      ) (filter (packageName: !(elem packageName cargoLockPackageNames)) (attrNames table));
    packageTableProblems =
      unknownPackageKeyProblems "packageBuildEnv" packageBuildEnv
      ++ unknownPackageKeyProblems "packageRustcArgs" packageRustcArgs
      ++ unknownPackageKeyProblems "packageTestInputs" packageTestInputs
      ++ unknownPackageKeyProblems "packageTestEnv" packageTestEnv;

    # Every injected unit plus everything reachable from one through
    # `passthru.depUnits` (recorded by `mkPrebuiltLibraryUnit`), deduplicated
    # by derivation. A recorded dep whose unit key the caller explicitly
    # pinned in `extraUnits` is pruned BEFORE descending: the pinned
    # derivation (already a closure root) is the selected unit for that key,
    # and the discarded dep's own subtree must not auto-inject units or
    # raise conflicts on behalf of an artifact the graph never links.
    # Walking by drvPath rather than unitKey keeps two distinct derivations
    # that claim the same unpinned key visible to the conflict guard below
    # instead of silently dropping one of them.
    injectedUnitClosure = map (item: item.unit) (genericClosure {
      startSet =
        lib.mapAttrsToList (_: unit: {
          key = unit.drvPath;
          inherit unit;
        })
        explicitExtraUnits;
      operator = item:
        map
        (dep: {
          key = dep.drvPath;
          unit = dep;
        })
        (
          filter (dep: !(hasAttr (dep.passthru.unitKey or "") explicitExtraUnits)) (
            item.unit.passthru.depUnits or []
          )
        );
    });

    # The closure grouped by recorded unit key. Injected units without a
    # `passthru.unitKey` (arbitrary caller-owned derivations) record no key
    # and never participate in auto-injection.
    injectedUnitsByKey = lib.groupBy (unit: unit.passthru.unitKey) (
      filter (unit: unit ? passthru.unitKey) injectedUnitClosure
    );

    # Transitive deps of the injected prebuilts, auto-injected under their own
    # recorded unit keys so a caller injects only the root unit (ENG-2166).
    # An explicit `extraUnits` entry wins the merge below, so a caller can
    # deliberately pin one dep key to a different artifact.
    autoInjectedDepUnits = lib.mapAttrs (_: head) (
      lib.filterAttrs (key: _: !(hasAttr key explicitExtraUnits)) injectedUnitsByKey
    );

    extraUnits = autoInjectedDepUnits // explicitExtraUnits;

    # Planner input for the first IFD stage. Cargo's planning phase
    # (`cargo build --unit-graph`) resolves the workspace from manifest
    # CONTENTS (Cargo.toml, Cargo.lock, the in-tree cargo config) plus target
    # discovery by file EXISTENCE (src/lib.rs, src/main.rs, src/bin/*.rs,
    # tests/, benches/, build.rs); it never reads a target file's contents and
    # runs no build script or proc macro. So the planner runs in a stub tree:
    # manifests verbatim, every other file an empty stub at its exact relative
    # path. The stub derivation's inputs are the manifest slice and the
    # relative path list alone, so a source BODY edit changes neither input
    # and the whole-workspace cargo resolve never re-runs; adding or removing
    # files, or touching a manifest, re-plans, correctly (#3900). Relative
    # paths must match `src` exactly: the render stage below maps the planned
    # unit paths back onto the real tree.
    #
    # Symlinks are stubbed as empty regular files like everything else; a
    # source with a symlinked manifest or member directory would mis-plan, and
    # none of the callers has one (cargo fails loud on the unreadable
    # manifest if one appears).
    plannerSource = let
      srcRoot = toString args.src;
      # Every file in `src`, as workspace-relative paths (readDir's attr
      # order, so deterministic). Eval-time: a derivation-produced `src` is
      # realized here, which the units import below forces anyway.
      filesUnder = dir:
        lib.concatLists (
          lib.mapAttrsToList (
            name: type:
              if type == "directory"
              then map (child: "${name}/${child}") (filesUnder "${dir}/${name}")
              else [name]
          ) (builtins.readDir dir)
        );
      # The files cargo's planner reads by content: package manifests, the
      # lockfile, and the workspace-level cargo config (cargo reads the
      # config chain from cwd upward plus $CARGO_HOME, and configScript owns
      # the latter).
      plannerReadsContent = relPath:
        elem (baseNameOf relPath) [
          "Cargo.toml"
          "Cargo.lock"
        ]
        || relPath == ".cargo/config.toml"
        || relPath == ".cargo/config";
      # The manifest slice re-ingested from `src`: manifest files plus the
      # full directory skeleton (kept directories cost nothing and keep the
      # filter one predicate). Changes only when a manifest changes or the
      # tree's shape does.
      manifestTree = builtins.path {
        name = "cargo-unit-planner-manifests";
        path = args.src;
        filter = path: type:
          type == "directory" || plannerReadsContent (lib.removePrefix (srcRoot + "/") path);
      };
      fileListFile = builtins.toFile "cargo-unit-planner-file-list" (
        lib.concatLines (filesUnder srcRoot)
      );
    in
      pkgs.runCommand "cargo-unit-planner-src"
      {
        manifests = manifestTree;
        fileList = fileListFile;
      }
      ''
        cp -r "$manifests" "$out"
        chmod -R u+w "$out"
        while IFS= read -r relPath; do
          if [ ! -e "$out/$relPath" ]; then
            mkdir -p "$out/$(dirname "$relPath")"
            : > "$out/$relPath"
          fi
        done < "$fileList"
      '';

    # First IFD stage: emit Cargo's `--unit-graph` JSON for the vendored
    # workspace, one cargo invocation per `cargoTargets` entry merged into one
    # graph. Separate derivation from the render so both are independently
    # inspectable on the workspace output. Runs in the content-stubbed
    # `plannerSource`, never `src`, so the paths in the emitted graph carry
    # the stub's store prefix (the render stage rewrites them).
    unitGraphJson = let
      profile = rawArgs.profile or "release";
      target = rawArgs.target or null;

      profileArgs =
        {
          release = ["--release"];
          dev = [];
        }
            ."${profile}" or [
          "--profile"
          profile
        ];
      renderTarget = cargoTarget:
        lib.escapeShellArgs (
          [
            "build"
            "--unit-graph"
            "-Z"
            "unstable-options"
          ]
          ++ profileArgs
          ++ lib.optionals (target != null) [
            "--target"
            target
          ]
          ++ cargoTarget
          ++ [
            "--frozen"
            "--offline"
          ]
        );
      unitGraphFile = targetIndex: "$TMPDIR/unit-graph-${toString targetIndex}.json";

      inherit (context) configScript;
    in
      pkgs.runCommand "cargo-unit-graph.json"
      (
        {
          nativeBuildInputs =
            [
              args.rustToolchain
              pkgs.cacert
              nixCargoUnit
            ]
            ++ args.nativeBuildInputs;
          SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
          # Cargo still gates `--unit-graph` behind `-Z unstable-options`.
          # This keeps the input graph generation local to the IFD planner
          # derivation instead of requiring a flake-wide Rust overlay.
          RUSTC_BOOTSTRAP = "1";
        }
        // args.env
      )
      ''
        ${configScript}

        cd ${plannerSource}

        pids=
        ${lib.concatStringsSep "\n" (
          lib.imap0 (targetIndex: targetArgs: ''
            (
              export CARGO_TARGET_DIR="$TMPDIR/cargo-target-${toString targetIndex}"
              cargo ${renderTarget targetArgs} > "${unitGraphFile targetIndex}"
            ) &
            pids="$pids $!"
          '')
          cargoTargets
        )}

        for pid in $pids; do
          wait "$pid"
        done

        nix-cargo-unit merge ${lib.concatStringsSep " " (lib.genList unitGraphFile (length cargoTargets))} > "$TMPDIR/merged.json"

        # Template the two store prefixes cargo bakes into every `src_path` out
        # of the emitted graph, so this 500 KB metadata file does not declare a
        # runtime reference on the 600 MB vendor dir. Nix derives an output's
        # references by scanning its bytes for store hashes, so the only way to
        # drop the reference is for the text to leave the file; the render stage
        # substitutes both placeholders back before feeding the graph to
        # `nix-cargo-unit render`, which still wants absolute paths. Neither
        # placeholder can collide with real content: cargo emits filesystem
        # paths here, and `@` is not a path prefix cargo ever produces.
        sed \
          -e 's|${plannerSource}|@workspaceRoot@|g' \
          -e 's|${args.vendorDir}|@vendorRoot@|g' \
          "$TMPDIR/merged.json" > "$out"
      '';

    # The workspace's toolchain id, handed to the renderer and baked into
    # every from-source unit hash. A prebuilt unit must have been compiled
    # with this exact toolchain, or its hash (hence its key) would not
    # match. `mkPrebuiltLibraryUnit` asserts against its own `rustToolchain`
    # arg; the injection guards below cross-check against this id, the one
    # the graph really used. Sourced from the resolved context so the id is
    # derived once at the resolution boundary, not re-spelled here.
    workspaceToolchainId = context.toolchainId;

    # Second IFD stage: render `units.nix` from the unit graph above.
    unitsNix = let
      contentAddressed = rawArgs.contentAddressed or true;

      extraFlags = lib.optional contentAddressed "--content-addressed" ++ effects.renderFlags;

      # `src` as the store path this derivation actually receives as an input,
      # and the only spelling of it used below. Interpolating a path value and
      # `toString`-ing one disagree whenever `src` is a bare path rather than a
      # derivation: a flake's `src = ./.` interpolates to a fresh store copy of
      # the tree but stringifies to the flake source itself, and a plain
      # directory stringifies to a location outside the store entirely.
      # Spelling it both ways named one tree in the graph rewrite and a
      # different one as the root to slice against, so every local unit came
      # out "outside workspace root" (#4239). Interpolation is the correct one
      # of the two: the renderer both strips this prefix off the rewritten
      # graph paths and reads the tree behind it to include-scan file
      # contents, and only the interpolated form is a declared, readable
      # input of this derivation.
      srcRoot = "${args.src}";
    in
      pkgs.runCommand "cargo-units.nix"
      {
        nativeBuildInputs = [nixCargoUnit];
        cargoLockForRender = context.cargoLockPath;
      }
      ''
        # Fill in the two roots the planner templated out. `@workspaceRoot@`
        # becomes the real `src`: the graph was planned in the content-stubbed
        # `plannerSource`, and the renderer slices unit paths from, and
        # include-scans the contents of, the tree the units actually compile
        # from. Planning is content-independent, so the filled graph is
        # byte-identical to one planned in `src` directly. `@vendorRoot@`
        # becomes the vendor dir this derivation already depends on for the
        # include scan, so nothing is reachable here that was not before --
        # only the metadata file's own reference went away. `|` and `&` never
        # appear in a store path.
        sed \
          -e 's|@workspaceRoot@|${srcRoot}|g' \
          -e 's|@vendorRoot@|${args.vendorDir}|g' \
          ${unitGraphJson} > "$TMPDIR/unit-graph.json"

        nix-cargo-unit render \
          --workspace-root ${escapeShellArg srcRoot} \
          --vendor-root ${escapeShellArg args.vendorDir} \
          --toolchain-id ${escapeShellArg workspaceToolchainId} \
          ${lib.escapeShellArgs extraFlags} \
          --cargo-lock "$cargoLockForRender" \
          < "$TMPDIR/unit-graph.json" \
          > "$out"
      '';

    # Drop fortify for a dev-profile graph, and only for one.
    #
    # The property: glibc refuses to fortify below `-O`. `features.h` answers
    # `-D_FORTIFY_SOURCE` at `-O0` with `#warning _FORTIFY_SOURCE requires
    # compiling with optimization (-O)`, nixpkgs enables `fortify`/`fortify3`
    # by default, and a C dependency whose configure probes run under `-Werror`
    # therefore cannot configure at all in a dev-profile build. Measured on
    # x86_64-linux, same source, only the optimisation level differing:
    #
    #     == -O0 (dev profile CFLAGS) ==
    #     features.h:435:4: error: #warning _FORTIFY_SOURCE requires compiling
    #       with optimization (-O) [-Werror=cpp]
    #     cc1: all warnings being treated as errors
    #     O0 FAILED
    #     == -O3 (release profile CFLAGS) ==
    #     O3 OK
    #
    # The instance that found this was tikv-jemalloc-sys, whose two
    # `strerror_r` probes both failed and produced `configure: error: cannot
    # determine return type of strerror_r` -- an error naming neither
    # optimisation nor hardening, in a build that is green on every release
    # profile. hyperion's `bedwars-dev-boot-e2e` and `smash-dev-boot-e2e` were
    # red on that for a day. jemalloc is the instance; autoconf probing under
    # `-Werror` is the class, so this belongs here and not in one Cargo.toml.
    #
    # What is given up: fortify is a real hardening measure, and this turns it
    # off for the C dependencies of dev- and test-profile builds. They exist for
    # `debug_assertions`, tests and boot gates. Do not widen this to release,
    # where the artifact does ship and where `-O` satisfies glibc anyway so
    # there is nothing to fix. If you are reading this because a hardening
    # audit flagged it, that is the reason; the alternative is forcing an
    # optimisation level on dev builds, which papers over the conflict at the
    # wrong layer and leaves the next C dependency to rediscover it.
    #
    # If glibc ever stops warning below `-O`, `tests/dev-profile-fortify.nix`
    # is the guard: it builds a C dependency whose configure probes under
    # `-Werror` and fails if this seam has become unnecessary or insufficient.
    unitHardeningDisable =
      if builtins.elem (rawArgs.profile or "release") ["dev" "test"]
      then ["fortify" "fortify3"]
      else [];

    perUnitClippyEnabled = args.policy.clippy.enable;
    # Workspace-level policy checks: audit + machete only. Clippy is NOT here;
    # it runs per unit in the renderer (`clippyByPackage`), so a whole-workspace
    # `cargo clippy` would duplicate it and make one source edit invalidate every
    # crate's clippy. `workspaceChecks` omits it by construction (no suppression).
    # A workspace has no single crate name; name the checks explicitly.
    extraPolicyChecksFromRust = checks.workspace (rawArgs.pname or "cargo-unit-workspace");
    # Import the rendered units.nix with a given prebuilt-injection seam. The
    # generated (pre-seam) set is obtained by importing with empty seam args,
    # so the injection guards below can compare against the real generated keys
    # without a second IFD (the import is memoized; only the function call
    # differs). See mkPrebuiltLibraryUnit.
    importUnits = rustToolchain: seam: let
      # The renderer passes `null` for host units (build scripts, proc-macros)
      # that have no `--target`; resolve that to the host triple before handing
      # it to the policy hook, which deliberately rejects a non-triple platform.
      extraRustcArgsForPlatform = platform: let
        resolvedPlatform =
          if platform == null
          then pkgs.stdenv.hostPlatform.config
          else platform;
      in
        effects.rustcArgsForPlatform resolvedPlatform
        ++ (rawArgs.extraRustcArgsForPlatform or (_platform: [])) platform
        # Opt-in: apply `.cargo/config.toml` rustflags (per target triple,
        # cargo precedence) so consumers do not hand-copy them into
        # `extraRustcArgs`. Appended last so explicit caller args still win.
        ++ lib.optionals (rawArgs.cargoConfigRustflags or false) (
          rustflagsFromCargoConfig (workspaceRoot + "/.cargo/config.toml") resolvedPlatform
        );
      extraLinkRustcArgsForPlatform = platform: let
        resolvedPlatform =
          if platform == null
          then pkgs.stdenv.hostPlatform.config
          else platform;
      in
        effects.linkRustcArgsForPlatform resolvedPlatform
        ++ (rawArgs.extraLinkRustcArgsForPlatform or (_platform: [])) platform;
    in
      import unitsNix (
        {
          inherit pkgs vendorDir vendorSources;
          inherit (args) src;
          inherit rustToolchain;
          extraRustcArgs = rawArgs.extraRustcArgs or [];
          inherit workspaceRoot;
          # Scanner for the opt-in panic-freedom policy. The rendered check
          # asserts this is non-null when `policy.denyPanics` is set.
          cargoUnit = nixCargoUnit;
          extraNativeBuildInputs = args.nativeBuildInputs ++ effects.linkerNativeInputs;
          # `clippy-driver` ships in the clippy package; `rustToolchain` only
          # guarantees rustc + cargo. Adding the resolved clippy package keeps
          # version drift impossible because the toolchain pins the rustc that
          # `clippy-driver` links against.
          extraClippyNativeBuildInputs = lib.optional perUnitClippyEnabled args.policy.clippy.package;
          extraEnv = args.env;
          inherit
            testRunPrelude
            testArgsByPackage
            packageTestInputs
            packageTestEnv
            testDiscovery
            ;
          inherit packageBuildEnv packageRustcArgs;
          inherit extraRustcArgsForPlatform extraLinkRustcArgsForPlatform;
          # Manifest-derived flags come first so per-call `policy.clippy`
          # entries land later in argv and can override them. Cargo's
          # `[lints.clippy]` resolution is the load-bearing source for most
          # workspaces; `policy.clippy.deniedLints` stays as an escape hatch
          # for callers without a Cargo.toml policy.
          extraClippyLintArgs =
            rust.clippyLintFlagsFromManifest (args.src + "/Cargo.toml") ++ effects.clippyLintArgs;
          clippyEnabled = perUnitClippyEnabled;
          extraPolicyChecks = extraPolicyChecksFromRust;
          inherit unitHardeningDisable;
        }
        // seam
      );

    # The from-source units / libraries, before any prebuilt injection. Used
    # only to validate the injection keys; never built unless referenced.
    generatedView = importUnits args.rustToolchain {
      extraUnits = {};
      extraLibraries = {};
    };
    generatedUnitKeys = attrNames generatedView.units;
    generatedLibraryKeys = attrNames generatedView.libraries;

    # C1: a prebuilt injection must OVERRIDE a unit/library the graph already
    # references. A key that is absent silently builds from source, defeating
    # the feature with zero signal, so fail loud and name the offending key.
    # Returns a list of human-readable problem strings (empty when valid).
    injectionKeyProblems = label: injected: validKeys: let
      unknown = filter (key: !(elem key validKeys)) (attrNames injected);
    in
      lib.optional (unknown != []) ''
        ${label} key(s) not present in the generated graph: ${lib.concatStringsSep ", " unknown}
        A prebuilt injection must override a unit the workspace already references; a
        missing key would silently build from source. Available ${label} keys:
          ${lib.concatStringsSep "\n  " validKeys}'';

    # C2: each injected unit must carry the workspace's actual toolchain id.
    # `mkPrebuiltLibraryUnit` records it in passthru; non-prebuilt injections
    # without that passthru are not checked (callers own those).
    injectionToolchainProblems = label: injected: let
      mismatched =
        lib.filterAttrs (
          _: unit: (unit.passthru.toolchainId or workspaceToolchainId) != workspaceToolchainId
        )
        injected;
      render = key: unit: "${key} (compiled with ${unit.passthru.toolchainId or "?"})";
    in
      lib.optional (mismatched != {}) ''
        ${label} compiled with a toolchain other than this workspace's (${workspaceToolchainId}):
          ${lib.concatStringsSep "\n  " (lib.mapAttrsToList render mismatched)}
        A prebuilt rlib only links against, and only hashes to the same unit key as,
        the toolchain that produced it. Thread the workspace's rustToolchain into
        mkPrebuiltLibraryUnit.'';

    # C3: when an explicitly injected unit records its own unit key, the
    # caller's chosen attr key must agree with it. The artifact names inside
    # the unit embed that key's hash, and auto-injection keys the unit's deps
    # by `passthru.unitKey`, so a disagreement would inject one derivation
    # under two keys.
    injectionUnitKeyMismatchProblems = let
      mismatched = lib.filterAttrs (key: unit: (unit.passthru.unitKey or key) != key) explicitExtraUnits;
      render = key: unit: "${key} (the unit's own passthru.unitKey is ${unit.passthru.unitKey})";
    in
      lib.optional (mismatched != {}) ''
        extraUnits key(s) that disagree with the injected unit's recorded unitKey:
          ${lib.concatStringsSep "\n  " (lib.mapAttrsToList render mismatched)}
        A prebuilt unit must be injected under its `passthru.unitKey`; the rlib and
        extern-path inside it are named for that key's hash.'';

    # C4: two recorded prebuilts claiming one unit key with different
    # derivations is ambiguous, and whichever the graph linked would be a
    # silent choice. An explicit `extraUnits` entry for the key resolves the
    # ambiguity (it wins the merge), so only unpinned keys are problems.
    depUnitConflictProblems = let
      conflicts =
        lib.filterAttrs (
          key: unitDrvs: length unitDrvs > 1 && !(hasAttr key explicitExtraUnits)
        )
        injectedUnitsByKey;
      render = key: unitDrvs: "${key}:\n    ${lib.concatMapStringsSep "\n    " (unit: unit.drvPath) unitDrvs}";
    in
      lib.optional (conflicts != {}) ''
        conflicting prebuilt derivations recorded for the same dependency unit key:
          ${lib.concatStringsSep "\n  " (lib.mapAttrsToList render conflicts)}
        Two injected prebuilt units recorded different derivations for one transitive
        dep (`passthru.depUnits`). Pin the key in extraUnits explicitly to choose one.'';

    # All prebuilt-injection guard problems, gathered so a single assert can
    # report every offending key at once (and so the assert keeps its
    # `lib.assertMsg` shape, per the no-bare-assert lint).
    injectionProblems =
      injectionKeyProblems "extraUnits" explicitExtraUnits generatedUnitKeys
      ++ injectionKeyProblems "extraUnits (auto-injected depUnits)" autoInjectedDepUnits generatedUnitKeys
      ++ injectionKeyProblems "extraLibraries" extraLibraries generatedLibraryKeys
      ++ injectionToolchainProblems "extraUnits" extraUnits
      ++ injectionToolchainProblems "extraLibraries" extraLibraries
      ++ injectionUnitKeyMismatchProblems
      ++ depUnitConflictProblems;

    units = assert lib.assertMsg (packageTableProblems == []) ''
      cargoUnit.buildWorkspace: per-package table names package(s) that are not
      in Cargo.lock:
      ${lib.concatMapStringsSep "\n" (problem: "  - ${problem}") packageTableProblems}
      A per-package table is looked up by Cargo package name, so an unknown key
      is silently ignored and its value reaches no unit at all (ENG-10675).
      Check the spelling against the crate's own `[package] name`, which need
      not match the directory or the package registry id.
    '';
    assert lib.assertMsg (injectionProblems == []) (
      "cargoUnit.buildWorkspace: invalid prebuilt-unit injection:\n"
      + lib.concatStringsSep "\n" injectionProblems
    );
      importUnits args.rustToolchain {inherit extraUnits extraLibraries;};
    # Clippy is a rustc_private binary tied to its pinned nightly. Import a
    # separate graph with that exact toolchain so every dependency rlib it reads
    # was produced by the same rustc ABI. The normal build and test graph keeps
    # the repository toolchain and its prebuilt injections.
    clippyUnits =
      if perUnitClippyEnabled
      then
        importUnits args.policy.clippy.package.toolchain {
          extraUnits = {};
          extraLibraries = {};
        }
      else {};
    # `policy.clippy.packages = null` gates every package; a list gates only
    # those. See the option in policy.nix for why the boundary lives there.
    clippyPackages = args.policy.clippy.packages;
    # An allowlist entry that matches no package is a silent no-op, and the
    # thing it silently disables is the gate the allowlist exists to guarantee.
    # A rename or a typo would otherwise turn a lint gate off without turning
    # anything red. Listing the real names matters as much as naming the
    # offender, because two spellings are in play: this attrset is keyed by
    # cargo PACKAGE name (`jj-vfs`), while the unit keys next door carry the
    # lib TARGET name (`jj_vfs-0.43.0-<hash>`). "jj_vfs is not a package"
    # is useless without the list that shows the hyphen.
    unknownClippyPackages =
      lib.subtractLists (attrNames clippyUnits.clippyByPackage)
      (lib.optionals (clippyPackages != null) clippyPackages);
    gatedClippyByPackage = assert lib.assertMsg (unknownClippyPackages == []) ''
      cargoUnit.buildWorkspace: policy.clippy.packages names ${toString (length unknownClippyPackages)} package(s) this workspace does not build: ${lib.concatStringsSep ", " unknownClippyPackages}
      available: ${lib.concatStringsSep ", " (attrNames clippyUnits.clippyByPackage)}
    '';
      if clippyPackages == null
      then clippyUnits.clippyByPackage
      else lib.filterAttrs (name: _: elem name clippyPackages) clippyUnits.clippyByPackage;

    workspaceUnits =
      units
      // lib.optionalAttrs perUnitClippyEnabled {
        clippyByPackage = gatedClippyByPackage;
      };

    targetSetNames = let
      targetCount = length cargoTargets;
    in
      if rawArgs ? cargoTargetNames
      then let
        names = rawArgs.cargoTargetNames;
      in
        assert lib.assertMsg (
          length names == targetCount
        ) "cargoUnit.buildWorkspace requires cargoTargetNames to match cargoTargets length"; names
      else lib.genList toString targetCount;
    namedTargetSets = lib.listToAttrs (
      lib.zipListsWith lib.nameValuePair targetSetNames units.targetSets
    );
    packageTestEnvForPackage = packageName: packageTestEnv.${packageName} or {};
    nextestTargetTriple = rawArgs.target or pkgs.stdenv.hostPlatform.config;
    nextestRustLibDir = "${args.rustToolchain}/lib/rustlib/${nextestTargetTriple}/lib";
    nextestConfigFile = pkgs.writeText "cargo-unit-nextest.toml" ''
      [profile.default]
      retries = 0
      slow-timeout = { period = "${rawArgs.nextestPerTestTimeout or "120s"}", terminate-after = 1 }
    '';
    nextestNonInteractiveEnv = {
      # #1597: Nix builders can attach cargo-nextest to a pseudo-terminal
      # without carrying the usual CI environment. Force the plain reporter
      # path so progress redraws cannot stall dispatcher handoffs.
      NEXTEST_HIDE_PROGRESS_BAR = "true";
      NEXTEST_NO_INPUT_HANDLER = "true";
      NEXTEST_SHOW_PROGRESS = "none";
    };
    nextestNoTestsByTarget =
      lib.mapAttrs (
        targetName: value:
          assert lib.assertMsg (elem value ["pass" "fail"])
          "cargoUnit.buildWorkspace nextestNoTestsByTarget.${targetName} must be pass or fail"; value
      )
      (rawArgs.nextestNoTestsByTarget or {});
    wrapNextestTarget = rawArgs.wrapNextestTarget or (_targetName: drv: drv);
    mkNextestForTarget = targetName: entry: let
      inherit (entry) packageName;
      packageEnv = packageTestEnvForPackage packageName;
      packagePolicy = testPolicyByPackage.${packageName} or emptyTestPolicy;
      testBinary = entry.binary;
    in
      pkgs.runCommand "cargo-unit-nextest-${targetName}"
      (
        packageEnv
        // nextestNonInteractiveEnv
        // {
          __structuredAttrs = true;
          strictDeps = true;
          nativeBuildInputs =
            [
              pkgs.cargo-nextest
              pkgs.coreutils
              nixCargoUnit
            ]
            ++ (packageTestInputs.${packageName} or []);
        }
      )
      ''
        ${testRunPrelude}

        workspace_root="$TMPDIR/nextest-ws"
        mkdir -p "$workspace_root/src" "$workspace_root/target"
        cat > "$workspace_root/Cargo.toml" <<EOF
        [package]
        name = ${escapeShellArg packageName}
        version = "0.0.0"
        edition = ${escapeShellArg entry.edition}
        [lib]
        EOF
        : > "$workspace_root/src/lib.rs"

        test_binary="$(readlink -f ${escapeShellArg testBinary})"
        if [ ! -x "$test_binary" ]; then
          echo >&2 "error: cargo-unit test binary missing or not executable: $test_binary"
          exit 1
        fi

        nix-cargo-unit nextest-metadata \
          --workspace-root "$workspace_root" \
          --target-name ${escapeShellArg targetName} \
          --package-name ${escapeShellArg packageName} \
          --edition ${escapeShellArg entry.edition} \
          --test-binary "$test_binary" \
          --target-triple ${escapeShellArg nextestTargetTriple} \
          --rust-libdir ${escapeShellArg nextestRustLibDir} \
          --cargo-metadata "$workspace_root/cargo-metadata.json" \
          --binaries-metadata "$workspace_root/binaries-metadata.json"

        ${lib.concatStringsSep "\n" (map (name: "export ${name}") (attrNames packageEnv))}
        ${lib.concatStringsSep "\n" (map (name: "export ${name}") (attrNames nextestNonInteractiveEnv))}

        cargo-nextest nextest run \
          --config-file ${nextestConfigFile} \
          --cargo-metadata "$workspace_root/cargo-metadata.json" \
          --binaries-metadata "$workspace_root/binaries-metadata.json" \
          --workspace-remap "$workspace_root" \
          --no-fail-fast \
          --no-tests=${nextestNoTestsByTarget.${targetName} or "pass"} \
          --test-threads ${
          if packagePolicy.testThreads != null
          then packagePolicy.testThreads
          else ''"''${NIX_BUILD_CORES:-1}"''
        } \
          ${nextestFilterForTestPolicy packagePolicy}

        mkdir -p "$out"
        echo "ran ${targetName} test target" > "$out/result"
      '';
    nextestByTarget =
      lib.mapAttrs (
        targetName: entry: wrapNextestTarget targetName (mkNextestForTarget targetName entry)
      )
      (units.tests or {});
    # Whole-workspace nextest metadata for running the prebuilt test binaries
    # OUTSIDE nix, on a machine that never compiled them:
    #   cargo nextest run \
    #     --binaries-metadata <nextestExport>/binaries-metadata.json \
    #     --cargo-metadata <nextestExport>/cargo-metadata.json \
    #     --workspace-remap <real checkout root>
    # nextest derives each test's cwd from the cargo-metadata manifest dirs
    # remapped onto the real checkout, and its list phase executes each binary
    # once. Interpolating every target's binary store path (the JSON below plus
    # the executability probe) pins the whole suite into this export's closure,
    # the same mechanism testPlan uses, so substituting the export substitutes
    # every test binary. The renderer refuses an empty binary list, so a
    # workspace without test targets has no buildable export rather than a
    # vacuously green one.
    nextestExportBinaries = (pkgs.formats.json {}).generate "cargo-unit-nextest-export-binaries.json" (
      map (target: {
        # The raw cargo target name, not the workspace-global attr key:
        # nextest binary ids are package-scoped already.
        target-name = target.targetName;
        package-name = target.packageName;
        package-version = target.packageVersion;
        package-root = target.packageRoot;
        inherit (target) kind edition;
        binary-path = target.binary;
      }) (units.testTargets or [])
    );
    nextestExport =
      pkgs.runCommand "cargo-unit-nextest-export"
      {
        __structuredAttrs = true;
        strictDeps = true;
        nativeBuildInputs = [
          pkgs.coreutils
          nixCargoUnit
        ];
      }
      ''
        set -euo pipefail

        workspace_root="$TMPDIR/nextest-ws"
        mkdir -p "$workspace_root" "$out"
        : > "$out/test-binaries"
        ${lib.concatMapStrings (target: ''
          test_binary="$(readlink -f ${escapeShellArg target.binary})"
          if [ ! -x "$test_binary" ]; then
            echo >&2 "error: cargo-unit test binary missing or not executable: $test_binary"
            exit 1
          fi
          printf '%s\n' "$test_binary" >> "$out/test-binaries"
        '') (units.testTargets or [])}
        sort -u -o "$out/test-binaries" "$out/test-binaries"

        nix-cargo-unit nextest-metadata-workspace \
          --workspace-root "$workspace_root" \
          --binaries ${nextestExportBinaries} \
          --target-triple ${escapeShellArg nextestTargetTriple} \
          --rust-libdir ${escapeShellArg nextestRustLibDir} \
          --cargo-metadata "$out/cargo-metadata.json" \
          --binaries-metadata "$out/binaries-metadata.json"
      '';
    libtestByTarget = lib.mapAttrs (_targetName: target: target.all) (units.tests or {});
    testChecksByTarget =
      if args.policy.tests.useNextest
      then nextestByTarget
      else libtestByTarget;
    testChecksAll =
      pkgs.runCommand "cargo-unit-test-targets"
      {
        __structuredAttrs = true;
        strictDeps = true;
        deps = builtins.attrValues testChecksByTarget;
      }
      ''
        set -euo pipefail
        target_names=(${lib.escapeShellArgs (attrNames testChecksByTarget)})
        mkdir -p "$out"
        printf '%s\n' "''${target_names[@]}" > "$out/test-targets"
        echo "ran ''${#target_names[@]} cargo-unit test targets" > "$out/result"
      '';
  in {
    # These derivations precede the generated units import. Keep them outside
    # the workspace attrset merge so callers can batch metadata builds first.
    helpers = {inherit plannerSource unitGraphJson unitsNix;};
    workspace =
      workspaceUnits
      // {
        inherit
          plannerSource
          unitGraphJson
          unitsNix
          vendorDir
          testPolicyByPackage
          nextestByTarget
          nextestExport
          testChecksByTarget
          testChecksAll
          ;
        cargoConfigScript = context.configScript;
        targetSets = namedTargetSets;
        inherit (args) policy;
      };
  };

  buildWorkspace = args: (planWorkspace args).workspace;

  # One lookup for every selector that picks a root out of a workspace: fail
  # with the calling selector's name and the full set of available keys, so a
  # typo'd target name reads as a menu instead of a bare missing-attribute
  # error.
  rootOrThrow = caller: kind: roots: name:
    roots.${name}
      or (throw "${caller}: no ${kind} `${name}` in workspace; available: ${lib.concatStringsSep ", " (attrNames roots)}");

  /**
  Select one binary target from a generated workspace graph.

  `meta` is merged onto the selected binary derivation (the same way
  `selectRootWithTests` applies its `meta`), so a caller can set
  `meta.mainProgram` and other fields. Without this, `meta` passed to
  `buildBinary` lands in the `buildWorkspace` arg set and is dropped, so
  `lib.getExe` on the result warns and only guesses the binary name.
  */
  buildBinary = {
    binary,
    meta ? {},
    ...
  } @ args: let
    workspace = buildWorkspace (
      removeAttrs args [
        "binary"
        "meta"
      ]
    );
    root = rootOrThrow "buildBinary" "binary" (workspace.binaries or {}) binary;
  in
    root.overrideAttrs (old: {
      meta = (old.meta or {}) // meta;
    });

  /**
  Pick a binary out of a pre-built `buildWorkspace` plus its test
  derivations, ready for `passthru.tests` consumption.

  Test and doctest targets are every generated target owned by `packageName`.
  Each discovered test case becomes its own derivation by default;
  `<target>-all` remains available for callers that need the full harness as a
  single compatibility check.

  Use this when the caller has one shared workspace (`ix.rustWorkspace.units`)
  so all repo-owned crates ride the same unit graph. Use `buildBinary` when
  a crate needs its own workspace (different policy, fetched source, etc).
  */
  selectBinaryWithTests = workspace: {
    binary,
    packageName ? binary,
    includeTestCases ? true,
    meta ? {},
    passthru ? {},
  }:
    selectRootWithTests workspace {
      rootDrv = rootOrThrow "selectBinaryWithTests" "binary" (workspace.binaries or {}) binary;
      inherit
        packageName
        includeTestCases
        meta
        passthru
        ;
      defaultTestTargets = [binary];
    };

  /**
  Pick a library target from a pre-built `buildWorkspace` plus its test and
  doctest derivations, ready for `passthru.tests` consumption.

  The library version of `selectBinaryWithTests`, for crates that ship a
  `lib` target rather than a binary. `library` is the crate's library unit
  key (Cargo's underscored name, e.g. `ix_vt`); `packageName` is the Cargo
  package name used to look up test targets (e.g. `ix-vt`).
  */
  selectLibraryWithTests = workspace: {
    library,
    packageName,
    includeTestCases ? true,
    meta ? {},
    passthru ? {},
  }:
    selectRootWithTests workspace {
      rootDrv = rootOrThrow "selectLibraryWithTests" "library" (workspace.libraries or {}) library;
      inherit
        packageName
        includeTestCases
        meta
        passthru
        ;
      defaultTestTargets = [packageName];
    };

  # Shared core for `selectBinaryWithTests` / `selectLibraryWithTests`: take a
  # selected root derivation and assemble its `passthru.tests` from the shared
  # workspace's test/doctest targets and policy checks.
  selectRootWithTests = workspace: {
    rootDrv,
    packageName,
    defaultTestTargets,
    includeTestCases ? true,
    meta ? {},
    passthru ? {},
  }: let
    uncheckedRoot = rootDrv.passthru.unchecked or rootDrv;
    namesForPackage = attrName: fallback:
      if hasAttr attrName workspace && hasAttr packageName workspace.${attrName}
      then workspace.${attrName}.${packageName}
      else fallback;
    selectedTestTargets = namesForPackage "testTargetNamesByPackage" defaultTestTargets;
    selectedDoctestTargets = namesForPackage "doctestTargetNamesByPackage" [];
    flattenAllTargets = prefix: targetNames: targets:
      lib.mapAttrs' (targetName: target: lib.nameValuePair "${prefix}${targetName}-all" target.all) (
        lib.getAttrs (filter (name: targets ? ${name}) targetNames) targets
      );
    flattenCaseTargets = prefix: targetNames: targets:
      lib.concatMapAttrs (
        targetName: target:
          lib.mapAttrs' (
            case: drv:
              lib.nameValuePair "${prefix}${targetName}-${lib.replaceStrings ["::"] ["-"] case}" drv
          ) (target.cases or {})
      ) (lib.getAttrs (filter (name: targets ? ${name}) targetNames) targets);
    # Per-crate policy gates. Each crate gets its own clippy and
    # unused-crate-dependency check (referencing only its own units) instead of
    # the workspace-wide aggregates, so editing one crate rebuilds only its own
    # checks. cargoAudit is lockfile-scoped (one Cargo.lock) and is exposed once
    # at the workspace level rather than aliased onto every crate.
    # `buildWorkspace` always sets `policy`, so the policy flags are present.
    # The per-package maps come from the nix-cargo-unit renderer and are
    # genuinely absent when it emitted none, so those stay guarded.
    policyChecks =
      lib.optionalAttrs (
        workspace.policy.clippy.enable && (workspace.clippyByPackage or {}) ? ${packageName}
      ) {clippy = workspace.clippyByPackage.${packageName};}
      // lib.optionalAttrs (
        workspace.policy.denyUnusedCrateDependencies
        && (workspace.unusedCrateDependenciesByPackage or {}) ? ${packageName}
      ) {unusedCrateDependencies = workspace.unusedCrateDependenciesByPackage.${packageName};};
    testCases =
      flattenCaseTargets "" selectedTestTargets (workspace.tests or {})
      // flattenCaseTargets "doctest-" selectedDoctestTargets (workspace.doctests or {});
    tests =
      {
        package = uncheckedRoot;
      }
      // flattenAllTargets "" selectedTestTargets (workspace.tests or {})
      // flattenAllTargets "doctest-" selectedDoctestTargets (workspace.doctests or {})
      // lib.optionalAttrs includeTestCases testCases;
  in
    rootDrv
    // {
      meta = (rootDrv.meta or {}) // meta;
      passthru =
        (rootDrv.passthru or {})
        // passthru
        // {
          tests = (rootDrv.passthru.tests or {}) // policyChecks // (passthru.tests or {}) // tests;
          inherit policyChecks;
          inherit (workspace) policy;
        };
    };

  /**
  Select several binary targets from one workspace unit graph.

  Use `cargoTargets` on `buildWorkspace` when the same import should expose
  roots from several Cargo executions, such as build and test graphs.
  */
  buildBinaries = {binaries, ...} @ args: let
    workspace = buildWorkspace (removeAttrs args ["binaries"]);
  in
    lib.genAttrs binaries (rootOrThrow "buildBinaries" "binary" (workspace.binaries or {}));

  /**
  Build a library unit derivation from already-compiled artifacts instead of
  from source.

  The result is contract-identical to a library unit the renderer would emit
  (`packages/nix-cargo-unit/src/render.rs`, install phase): `$out` carries
  `$out/lib/lib<name>-<hash>.rlib`, the matching `.rmeta`, and
  `$out/nix-support/extern-path` holding the absolute path to the `.rlib`.
  A downstream unit therefore consumes it exactly like a from-source unit:
  `-L dependency=$out/lib` and `--extern <crate>=$(cat $out/nix-support/extern-path)`,
  plus a second `--extern <crate>=<sibling .rmeta>` when the consuming
  workspace compiles with `policy.compiler.embedMetadata = false` (the
  staged `.rmeta` is what makes a thin prebuilt rlib consumable there).

  Pass the produced derivation through `buildWorkspace`'s `extraUnits` (keyed by
  `"<name>-<version>-<hash>"`). Because a unit's `<hash>` hashes package
  identity, target, edition, crate-types, features, profile, dependency
  identities, and the toolchain id, but never the source bytes
  (`model.rs:612-672`, `hash.rs:18-26`), a metadata-faithful stub crate yields
  the same `<hash>` as the real prebuilt, so injecting this unit links a
  downstream crate against a prebuilt rlib with no source present.

  Scope: Rust library crate types only -- `rlib`, `dylib`, or both. A `cdylib`
  or `staticlib` is out of scope because its consumer is a C linker or an FFI
  host rather than a `--extern`, so an injected one would produce a unit no
  downstream crate can reference; a `proc-macro` is out of scope because the
  compiler loads it, so it has to match the host toolchain and not the target.

  Trust boundary: an injected prebuilt unit BYPASSES every per-unit policy gate
  (clippy, `--deny-panics`, unused-crate-dependencies) because those gates run
  on from-source compile units, not on a copied artifact. Inject only trusted
  artifacts (e.g. a first-party SDK rlib fetched from your own R2).

  `extraLibraries` is usually unnecessary: `buildWorkspace`'s `libraries` set
  derives from `units`, and a downstream crate links via `units.<key>`, so
  overriding `extraUnits.<key>` already routes the link through the prebuilt.
  Reach for `extraLibraries` only to make `workspace.libraries.<name>` itself
  point at the prebuilt (e.g. for `selectLibraryWithTests`).

  Arguments:
  - `pname`: the library unit's Cargo target name (the leading component of the
    unit key), which for a default `lib` target is the underscored crate name
    (e.g. package `my-lib` has target `my_lib`). Any dashes are mapped to
    underscores for the on-disk artifact names, matching the renderer.
  - `version`: the crate version, used only to build the unit key the caller
    injects under.
  - `hash`: the source-independent unit hash. Must equal the `<hash>` the
    renderer computes for the metadata-faithful stub the downstream graph sees,
    or the downstream `--extern`/`-L` references will not resolve to this unit.
  - `rlib`: path to the compiled `.rlib` artifact, or `null` for a crate whose
    only library crate type is `dylib` (`dylib` must then be given).
  - `rmeta`: path to the compiled `.rmeta` artifact.
  - `dylib`: optional path to the compiled shared library (`.so`, `.dylib` or
    `.dll`) for a crate whose crate types include `dylib`. Required whenever the
    consuming graph's unit declares `dylib`: rustc picks between an rlib and a
    dylib per consumer, so a consumer of a dylib unit is passed both artifacts,
    and a prebuilt that omits this one is silently linked statically -- which,
    for the crates a dylib is used for, means several copies of a crate's
    process-global state in one process.
  - `toolchainId`: the toolchain id the prebuilt was compiled with. Asserted
    equal to `baseNameOf (toString rustToolchain)` so a toolchain mismatch
    fails at eval, never at link time. Also recorded in `passthru.toolchainId`
    so `buildWorkspace` can cross-check it against the workspace's actual
    toolchain at injection time.
  - `rustToolchain`: optional; defaults to `rust.defaultRustToolchain`. Used
    only for the toolchain-id assertion. A caller whose `buildWorkspace` uses a
    non-default toolchain MUST thread that same `rustToolchain` here, or the
    workspace-side cross-check in `buildWorkspace` will reject the injection.
  - `depUnits`: this prebuilt's own dependency unit derivations, each built
    with `mkPrebuiltLibraryUnit` (each entry must carry `passthru.unitKey`).
    Direct deps that each record their own `depUnits`, or a flattened
    transitive list, inject identically. Defaults to `[ ]` (a leaf library).
    `buildWorkspace` walks `passthru.depUnits` transitively and auto-injects
    every recorded unit into the consuming graph under its own
    `passthru.unitKey`, so the caller injects only the root unit. Each
    auto-injected key must name a unit the consumer's graph already
    references (the C1 guard), which holds exactly when the consumer's
    manifest pins the dependency closure the prebuilt was compiled against:
    the unit hash folds in dependency hashes recursively, so a root key match
    implies every dep key matches. An explicit `extraUnits` entry for a dep
    key overrides the recorded derivation. The deps are also recorded to
    `$out/nix-support/dependency-units` for provenance.
  */
  mkPrebuiltLibraryUnit = {
    # The Cargo library TARGET name (the renderer's unit-key/rlib component),
    # not a stdenv derivation name; named `pname` so a `version` sibling does
    # not read as a `name = "<pname>-<version>"` restatement.
    pname,
    version,
    hash,
    rlib,
    rmeta,
    dylib ? null,
    toolchainId,
    rustToolchain ? rust.defaultRustToolchain,
    depUnits ? [],
  }: let
    expectedToolchainId = rust.toolchainId rustToolchain;
    # The renderer underscores the Cargo target name for on-disk artifacts
    # (`render.rs:1376`). Mirror that exactly so the rlib filename and the
    # `extern-path` contents match what a from-source unit would produce.
    libName = replaceStrings ["-"] ["_"] pname;
    # Keep the caller's extension: a consumer resolves this file through the
    # `DT_NEEDED`/install name recorded in it, which names the platform's own
    # spelling.
    dylibExtension =
      if dylib == null
      then ""
      else lib.head (filter (suffix: lib.hasSuffix suffix (toString dylib)) [".so" ".dylib" ".dll"]);
    rlibPath = "$out/lib/lib${libName}-${hash}.rlib";
    dylibPath = "$out/lib/lib${libName}-${hash}${dylibExtension}";
    externPaths = lib.optional (rlib != null) rlibPath ++ lib.optional (dylib != null) dylibPath;
    preferredExternPath = lib.head externPaths;
  in
    assert lib.assertMsg (toolchainId == expectedToolchainId) ''
      cargoUnit.mkPrebuiltLibraryUnit: toolchainId mismatch for `${pname}`.
        prebuilt was compiled with: ${toolchainId}
        this workspace's toolchain: ${expectedToolchainId}
      A prebuilt rlib/rmeta only links against the toolchain that produced it.
    '';
    # The artifact set this builder can express is rlib, rmeta and dylib: the
    # three a Rust library unit publishes for another Rust crate to link.
    # `cdylib` and `staticlib` are deliberately still refused -- their consumer
    # is a C linker or an FFI host, not a `--extern`, so injecting one here
    # would produce a unit no downstream crate can reference. A proc-macro is
    # refused for the same reason plus a different one: it is loaded by the
    # compiler, so it must match the host toolchain rather than the target.
    assert lib.assertMsg (rlib != null || dylib != null) ''
      cargoUnit.mkPrebuiltLibraryUnit: `${pname}` must provide `rlib`, `dylib`, or both.
    '';
    assert lib.assertMsg (rlib == null || lib.hasSuffix ".rlib" (toString rlib)) ''
      cargoUnit.mkPrebuiltLibraryUnit: `rlib` for `${pname}` must be a .rlib path; got ${toString rlib}.
      Only rlib and dylib libraries are supported (not cdylib/staticlib/proc-macro).
    '';
    assert lib.assertMsg (
      dylib == null || lib.any (suffix: lib.hasSuffix suffix (toString dylib)) [".so" ".dylib" ".dll"]
    ) ''
      cargoUnit.mkPrebuiltLibraryUnit: `dylib` for `${pname}` must be a .so/.dylib/.dll path; got ${toString dylib}.
    '';
    assert lib.assertMsg (lib.hasSuffix ".rmeta" (toString rmeta)) ''
      cargoUnit.mkPrebuiltLibraryUnit: `rmeta` for `${pname}` must be a .rmeta path; got ${toString rmeta}.
    '';
    # Auto-injection keys each dep by its `passthru.unitKey`, so an entry
    # without one could never be wired into a consuming graph. Reject it at
    # construction, naming the offender, instead of at injection time.
    assert lib.assertMsg (filter (dep: !(dep ? passthru.unitKey)) depUnits == []) ''
      cargoUnit.mkPrebuiltLibraryUnit: depUnits for `${pname}` must be prebuilt unit
      derivations carrying `passthru.unitKey` (build them with mkPrebuiltLibraryUnit); got:
        ${lib.concatMapStringsSep "\n  " (dep: dep.name or "<non-derivation>") (
        filter (dep: !(dep ? passthru.unitKey)) depUnits
      )}
    '';
      pkgs.runCommand "cargo-unit-prebuilt-${pname}-${version}-${hash}"
      {
        # Surfaced for callers/tests that want to confirm the injected key
        # without reconstructing the format string. `depUnits` is what
        # `buildWorkspace` walks to auto-inject this unit's transitive deps.
        passthru = {
          unitKey = "${pname}-${version}-${hash}";
          libraryName = libName;
          inherit
            pname
            version
            hash
            toolchainId
            depUnits
            ;
        };
      }
      ''
        mkdir -p "$out/lib" "$out/nix-support"
        ${lib.optionalString (rlib != null) ''
          cp ${lib.escapeShellArg (toString rlib)} "${rlibPath}"
        ''}
        ${lib.optionalString (dylib != null) ''
          cp ${lib.escapeShellArg (toString dylib)} "${dylibPath}"
        ''}
        cp ${lib.escapeShellArg (toString rmeta)} "$out/lib/lib${libName}-${hash}.rmeta"
        # Same artifact priority as the renderer's install phase: the rlib is
        # the single preferred artifact, and `extern-paths` carries every
        # linkable one in the same order, because only rustc can pick between
        # them. When the consuming workspace runs with
        # `policy.compiler.embedMetadata = false` (the default), its units
        # add a second `--extern` for the sibling .rmeta staged above, so a
        # thin prebuilt rlib (produced by a workspace with the same policy)
        # still supplies full metadata to dependents.
        printf '%s\n' "${preferredExternPath}" > "$out/nix-support/extern-path"
        ${lib.optionalString (dylib != null) ''
          printf '%s\n' ${lib.concatMapStringsSep " " (path: ''"${path}"'') externPaths} > "$out/nix-support/extern-paths"
        ''}
        ${lib.concatMapStringsSep "\n" (
            dep: ''printf '%s\n' ${lib.escapeShellArg (toString dep)} >> "$out/nix-support/dependency-units"''
          )
          depUnits}
      '';
in {
  inherit
    buildBinary
    buildBinaries
    buildWorkspace
    planWorkspace
    selectBinaryWithTests
    selectLibraryWithTests
    defaultToolchainId
    mkPrebuiltLibraryUnit
    ;
  # Named partial policies (e.g. `policyPresets.pureBuild`) for callers that build
  # pure artifacts and want to reference one name instead of re-spelling the gates.
  inherit (rust) policyPresets;
}
