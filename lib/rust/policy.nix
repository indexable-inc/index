# The Rust build policy: the quality/correctness gates applied to a build
# (unused-dep denial, panic-freedom, cargo-audit, cargo-machete, clippy, tests)
# and the linker choice, plus their consequences (rustc args, native inputs,
# lint flags) and the workspace/crate policy-check derivations. Owns the default
# policy and the caller-merge. The check builders run cargo in the vendored tree,
# so the vendor module's `vendorConfigScript` / `cargoLockFile` are threaded in.
{
  lib,
  pkgs,
  clippyPackage,
  vendorConfigScript,
  cargoLockFile,
  # The shared pins reader (lib/util/pins.nix), threaded down from
  # lib/default.nix so the advisory-db rev+hash load from the sibling pins.json
  # without a cross-directory `../` import (no-parent-path).
  pins,
}: let
  inherit
    (builtins)
    filter
    removeAttrs
    ;

  inherit (lib) any;

  toFlagSequence = flag:
    lib.concatMap (arg: [
      flag
      arg
    ]);

  nonEmpty = l: l != [];

  # The policy schema, declared once as module options so the defaults, the
  # caller-merge, and typo rejection (no `freeformType`, so an unknown key throws)
  # all come from one declaration. `clippy.denyWarnings` is a write-only knob: the
  # resolver post-filters `deniedLints` with it and drops it from the result.
  policyModule = {
    options = {
      denyUnusedCrateDependencies = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Fail a unit whose declared crate dependencies are unused (rustc gate).";
      };
      # Opt-in: scans each unit's objects for functions that can reach a panic.
      # Off by default because it is a best-effort gate, not a soundness proof.
      denyPanics = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Scan each unit's objects for functions that can reach a panic (best-effort).";
      };
      cargoAudit = {
        # On by default: an offline, lockfile-only runCommand (`cargo-audit audit
        # --file Cargo.lock --no-fetch --stale`) decoupled from compilation, so it
        # re-runs only when the lockfile or DB changes. Opt out on pure-build graphs.
        enable = lib.mkOption {
          type = lib.types.bool;
          default = true;
          description = "Run the offline, lockfile-only cargo-audit check.";
        };
        db = lib.mkOption {
          type = lib.types.package;
          # The rev + SRI pin lives in the sibling pins.json (repo policy: no
          # inline hash literals); bump by editing the rev there and re-pinning.
          default = pkgs.fetchFromGitHub {
            inherit
              (pins.loadPin ./pins.json "advisory-db")
              owner
              repo
              rev
              hash
              ;
          };
          description = "The advisory database cargo-audit checks against.";
        };
        deny = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [];
          description = "Advisory ids/warning kinds to escalate to errors.";
        };
        ignore = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [];
          description = "Advisory ids to ignore.";
        };
      };
      cargoMachete = {
        enable = lib.mkOption {
          type = lib.types.bool;
          default = true;
          description = "Run cargo-machete to find unused dependencies across the workspace.";
        };
        extraArgs = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [];
          description = "Extra arguments passed to cargo-machete.";
        };
      };
      clippy = {
        enable = lib.mkOption {
          type = lib.types.bool;
          default = true;
          description = "Run clippy (per unit in a workspace, whole-crate otherwise).";
        };
        package = lib.mkOption {
          type = lib.types.package;
          default = clippyPackage;
          description = "The clippy package providing clippy-driver.";
        };
        packages = lib.mkOption {
          type = lib.types.nullOr (lib.types.listOf lib.types.str);
          default = null;
          example = ["jj-vfs"];
          description = ''
            Cargo package names to gate, or null for every package in the
            workspace. A list is for a workspace we only partly own: a vendored
            upstream tree with our crates grafted in answers to its own CI for
            its own code, and to our lint policy for ours.

            Naming them here rather than filtering wherever the checks get
            wired keeps "we gate what we own" next to the decision. A caller
            that later wires `clippyByPackage` wholesale then inherits the
            boundary instead of silently adopting the whole tree.

            These are cargo PACKAGE names, so `jj-vfs`, not the `jj_vfs`
            of the unit keys, which carry the lib target name instead.

            An entry naming no package in the workspace is refused rather than
            ignored, because the failure of a silent no-op is that the gate
            this list exists to guarantee quietly stops running. The refusal
            lists the workspace's real package names, since the usual mistake
            is the spelling rather than the crate.
          '';
        };
        cargoArgs = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = ["--all-targets"];
          description = "Target-selection args for the whole-crate `cargo clippy`.";
        };
        deniedLints = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [];
          description = "Lints denied via `-D` (escape hatch; prefer Cargo.toml `[lints]`).";
        };
        allowedLints = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [];
          description = "Lints allowed via `-A`.";
        };
        denyWarnings = lib.mkOption {
          type = lib.types.bool;
          default = true;
          description = "When false, drop `warnings` from deniedLints so a warning does not fail the build.";
        };
      };
      tests = {
        enable = lib.mkOption {
          type = lib.types.bool;
          default = true;
          description = "Run the crate's tests as part of the build.";
        };
        useNextest = lib.mkOption {
          type = lib.types.bool;
          default = true;
          description = "Use cargo-nextest for parallel test execution.";
        };
      };
      # Compile-scope rustc knobs for the cargo-unit engine only (the
      # `buildPackage` cargo path never sees them, so a caller-supplied stable
      # toolchain there cannot trip over a `-Z` flag). Every flag here salts
      # unit identity: flipping one rebuilds the whole Rust workspace in CI.
      #
      # Candidates from the nightly-2026-05-27 `-Z help` sweep that were
      # REJECTED, with the evidence, so nobody re-proposes them blind:
      #
      # - `-Zthreads=N` (parallel frontend): real wall-time win on a large
      #   frontend (vcfs dev-profile lib unit: real 5.4 s at threads=1, 3.4 s
      #   at 4, 2.8 s at 8; tokio release lib 4.1 s to 3.0 s at 4), but
      #   byte-NONDETERMINISTIC on this nightly: threads=4 produced 3+
      #   distinct rlib/rmeta outputs in 5 vcfs runs and 5 distinct in 5
      #   tokio runs; threads=8 produced 5 distinct outputs in 5 vcfs runs
      #   (sizes wobbling by ~1.7 kB run to run). In a content-addressed store that defeats early cutoff
      #   and triggers spurious downstream rebuilds, which disqualifies the
      #   flag regardless of the wall-time win. Byte-diff evidence for a
      #   possible compiler-side fix: of the 258 rlib members only lib.rmeta
      #   (diffs from header offset 0x8, then 1-2 byte diffs clustered around
      #   0x2ef6a and on, total size delta ~1.7 kB) and exactly one codegen
      #   unit object (cgu.219; same size, identical symbol table, diffs
      #   confined to the embedded .llvmbc bitcode: six 3-4 byte ranges near
      #   0x10c8 and one 26-byte range at 0x3001) varied; all machine-code
      #   sections were byte-identical. A link-dominated bin unit
      #   (orchestrator, opt-level 3, 248 MB output) showed no variance in 3
      #   runs and no wall-time win (50 s either way). No option is exposed:
      #   a knob that poisons the CA store is a footgun; re-evaluate against
      #   a future toolchain bump as a measurement, not a config flip.
      # - `-Zshare-generics=off`: on this nightly the default is ON at
      #   opt-level 0/1 and OFF at opt-level 2/3 (verified by symbol
      #   inspection: at opt 0 a dependent imports the upstream instantiation,
      #   with =off it defines a local copy). Forcing =off at the dev profile
      #   grew the vcfs rlib from 69,848,774 B to 72,369,374 B with no
      #   wall-time win (5.4 s vs 5.7 s), and it buys no cache independence:
      #   the upstream unit is already a nix input of the dependent, so the
      #   coupling it removes is one nix already tracks.
      # - `--remap-path-scope` (stable on this nightly): nothing to widen.
      #   The engine's `--remap-path-prefix` already applies to every scope by
      #   default; measured rlib+rmeta of a workspace unit contain zero
      #   /nix/store strings, and 5 from-scratch rebuilds of the same unit
      #   were byte-identical. The one store-path leak was the dep-info .d
      #   file, which rustc exempts from remapping; fixed in the renderer by
      #   not installing it (see render.rs install phase), not by scoping.
      # - `-Zlocation-detail=none`: changes runtime behavior (panic locations
      #   lose file/line fleet-wide), and measured no reproducibility win to
      #   pay for it: with debuginfo=0 and both `-Zlocation-detail=none` and
      #   `-Zincremental-ignore-spans=yes`, a dependent's rlib still changed
      #   when a dependency's lines shifted (the dependency SVH is
      #   span-sensitive on this nightly and is baked into dependent
      #   metadata). Reject as default; revisit only with fleet consent to
      #   degraded panic messages.
      # - `-Zincremental-ignore-spans=yes`: no observable effect for us. The
      #   engine never uses `-Cincremental`, and the flag did not make the
      #   rmeta span-insensitive on this nightly (comment-shift test above).
      compiler = {
        embedMetadata = lib.mkOption {
          type = lib.types.bool;
          default = false;
          description = ''
            Embed full crate metadata in rlibs (`-Zembed-metadata=no` when
            false, the RFC 3763 cargo `-Zno-embed-metadata` scheme).

            The engine already emits a separate full .rmeta next to every
            lib rlib (`--emit dep-info,metadata,link`), so the embedded copy
            is pure duplication. When this is off, dependents pass the crate
            twice, `--extern name=<rlib>` plus `--extern name=<sibling
            .rmeta>` (cargo's -Zno-embed-metadata scheme; rustc does not
            fall back to `-L` search for an --extern-pinned crate), so
            compiles read the full .rmeta and links consume the thin rlib.
            Off by default on measurement (nightly-2026-05-27, x86_64-linux):
            tokio release-profile rlib 22,697,770 B to 13,263,482 B and vcfs
            dev-profile rlib 69,848,774 B to 58,345,910 B, .rmeta unchanged
            and compile wall time flat, with every configuration built 5
            times and no byte variance observed. Verified off: dependent
            compiles, bin links, and rustdoc/doctest invocations all resolve,
            and a dependent compiled with the dual extern is byte-identical
            to one compiled against the same full rlib alone. Proc-macros and
            dylib crate types keep embedded metadata (the renderer exempts
            them; their dylib is the metadata carrier).
          '';
        };
        # The ix rustc fork's rmeta byte-stability flags
        # (indexable-inc/rustc PR #2, pinned at packages/rustc-ix). They make
        # a lib unit's .rmeta byte-identical across non-interface edits, which
        # is what lets content-addressed early cutoff stop an edit's rebuild
        # cascade at the first crate whose output converges. Fork-only: the
        # default nightly hard-errors on them ("unknown unstable option"), so
        # resolve.nix refuses an EXPLICIT selection at eval unless the graph's
        # toolchain records a fork rev (packages/rustc-ix passthru.forkRev).
        #
        # Each option defaults to AUTO (null / "auto"): the trio is ON, in
        # the cutoff mode, for exactly the graphs whose toolchain is the
        # fork, and off everywhere else. Auto rather than plain true because
        # the flags cannot even parse on an upstream toolchain, and plain
        # false would re-ask every fork-toolchain caller to restate the
        # engine's whole reason for carrying the fork.
        #
        # The auto set is the FULL trio with stripSpans = "all", on evidence:
        # the fork PR's per-edit-class table and the local reproduction
        # (chain-a fixture crate, opt 0, debuginfo 0: a comment-line
        # insertion leaves the rmeta byte-identical with the trio, and
        # differs without) show only the full trio stabilizes general comment
        # edits and whole-line shifts; each flag alone leaves residual churn
        # (isolation runs in the fork PR: with spans and src-hash handled but
        # contentSvh off, exactly the 16 SVH bytes still differ). stripSpans
        # was auto-OFF between #10170 and the 42daae19928e pin, while the
        # fork rev of the day erased hygiene along with span locations and
        # ICEd every reader of an affected .rmeta; resolve.nix, where the
        # auto values resolve, carries that history and the re-flip evidence.
        #
        # What the auto default trades away, for a graph that ships debugged
        # binaries rather than CI checks: dependents' "defined here"
        # diagnostic notes into fork-built crates, dependent-side debuginfo
        # declaration attribution for functions inlined from them (both from
        # stripSpans = "all"; "non-exported" costs nothing in DWARF but only
        # holds byte-position-preserving edits identical), and cross-crate
        # diagnostic source snippets degraded to file:line:col (from
        # normalizeSrcHash; the zeroed hash never verifies, so dependents
        # never quote stale source). A fork-toolchain graph that wants those
        # back sets the options to their off values explicitly.
        rmetaStability = {
          contentSvh = lib.mkOption {
            type = lib.types.nullOr lib.types.bool;
            default = null;
            description = ''
              Derive the SVH embedded in crate metadata from the encoded
              metadata bytes (`-Zrmeta-content-svh`), so it is stable exactly
              when the rest of the metadata is. Trades away nothing
              user-visible (the loader's link-time SVH equality check keeps
              working); rejected by the fork with `-Cincremental`, which this
              engine never passes. null (default) = auto: on exactly when the
              graph's toolchain is the ix rustc fork.
            '';
          };
          stripSpans = lib.mkOption {
            type = lib.types.enum ["auto" "off" "non-exported" "all"];
            default = "auto";
            description = ''
              Replace spans with dummies when encoding crate metadata
              (`-Zrmeta-strip-spans=<mode>`), so encoded bytes do not depend
              on source positions. "non-exported" preserves spans in exported
              MIR bodies and costs nothing in dependents' debuginfo, but only
              byte-position-preserving edits stay identical. "all" is the
              cutoff mode: general comment edits and line shifts stay
              identical, at the cost of dependents' "defined here" diagnostic
              notes into this crate and, for functions inlined from this
              crate, dependent-side debuginfo declaration attribution (the
              fork PR documents both, with DWARF verification either way).
              "auto" (default) resolves to "off" on every toolchain, the ix
              rustc fork included: the pinned fork rev writes .rmeta under
              this flag that ICEs the crates which read it (resolve.nix
              carries the reproduction). Setting "all" or "non-exported"
              explicitly still works and is still fork-guarded; that is how
              the cutoff check keeps the capability under test.
            '';
          };
          normalizeSrcHash = lib.mkOption {
            type = lib.types.nullOr lib.types.bool;
            default = null;
            description = ''
              Zero the per-file source content hashes in crate metadata
              (`-Zrmeta-normalize-src-hash`), the 16 bytes of MD5 that
              otherwise churn on any file edit even at debuginfo 0. Dependents
              degrade cross-crate diagnostic snippets into this crate to
              file:line:col (the zeroed hash never verifies, so they never
              quote stale source). null (default) = auto: on exactly when the
              graph's toolchain is the ix rustc fork.
            '';
          };
        };
      };
      linker = {
        useMold = lib.mkOption {
          type = lib.types.bool;
          default = pkgs.stdenv.hostPlatform.isLinux;
          description = ''
            Link with mold on Linux.

            Kept over lld and wild on measurement rather than reputation: all
            three are within 3% on wall time for our largest binary, and the
            linker is only 1.3 to 1.8 GB of a 15 GB link step that rustc
            dominates. The numbers and the revisit condition are in
            `packages/clang-mold-musl/default.nix`, which is where the musl
            target actually selects a linker.
          '';
        };
        useLld = lib.mkOption {
          type = lib.types.bool;
          default = pkgs.stdenv.hostPlatform.isDarwin;
          description = "Link with lld on macOS (the default cctools ld64 is single-threaded and slow).";
        };
        buildId = lib.mkOption {
          type = lib.types.bool;
          default = true;
          description = ''
            Emit a GNU build-id note into every ELF this policy links.

            The 20-byte `.note.gnu.build-id` is the join key every symbol
            consumer looks an address up by: the debuginfod protocol, a
            separate `.debug` file, `coredumpctl debug`, a continuous
            profiler's symbol cache, and Antithesis's coverage symbolization.
            Neither rustc nor mold emits one unless asked, so without this a
            linked binary is unsymbolizable by anything that does not already
            know its store path. Measured absent on every ix fleet binary
            before this option existed (ix#8936).

            `sha1` over the linked output rather than `uuid`, so the note is a
            function of the bytes and a reproducible build keeps a reproducible
            build-id. 20 bytes is also the length every consumer is actually
            tested against; mold computes sha256 and truncates either way, so
            the shorter note is not the slower one.
          '';
        };
      };
    };
  };

  # Named partial policies for recurring intents, so callers reference one name
  # instead of re-spelling the same field set. Resolved against the schema like
  # any caller policy. `pureBuild` turns off every gate: for a pure build artifact
  # (a cross graph, a prebuilt-injection graph) where the native graph already
  # ran clippy/audit/machete/unused-dep over the same sources.
  policyPresets = {
    pureBuild = {
      denyUnusedCrateDependencies = false;
      cargoAudit.enable = false;
      cargoMachete.enable = false;
      clippy.enable = false;
    };
  };

  # Resolve a caller's partial policy against the schema: defaults, merge, and
  # typo rejection come from `evalModules`. `denyWarnings` is applied here by
  # post-filtering `deniedLints` (and then dropped, so it carries no effect of its
  # own); `_module` is stripped so the result is a plain policy record matching
  # the historical shape.
  resolvePolicy = userPolicy: let
    evaluated =
      (lib.evalModules {
        modules = [
          policyModule
          {config = userPolicy;}
        ];
      }).config;
    deniedLints =
      if evaluated.clippy.denyWarnings
      then evaluated.clippy.deniedLints
      else filter (lint: lint != "warnings") evaluated.clippy.deniedLints;
  in
    removeAttrs evaluated ["_module"]
    // {
      clippy =
        removeAttrs evaluated.clippy ["denyWarnings"]
        // {
          inherit deniedLints;
        };
    };

  # `platform` is a rust target triple (e.g. `x86_64-unknown-linux-gnu`); the fast
  # linker is per-OS, so each branch is gated on the triple: mold for a `-linux-`
  # triple, lld for an `-apple-darwin` triple. Host builds pass
  # `pkgs.stdenv.hostPlatform.config` rather than a sentinel, so the tests run on a
  # real triple and a non-triple argument fails loudly instead of defaulting.
  #
  # The lld branch borrows the `-B${pkgs.lld}/bin -fuse-ld=lld` incantation from the
  # Linux->darwin cross toolchain in `lib/darwin/apple-sdk-toolchain.nix` (the `-B`
  # makes the clang driver resolve `ld64.lld`), but applies only to a *native* darwin
  # link: it is additionally gated on a darwin build host. The cross toolchain already
  # injects `-fuse-ld=lld` via `CARGO_TARGET_<T>_LINKER`, so without this host gate a
  # future darwin-host darwin-cross would stack the flag on that wrapper.
  rustcArgsForPolicyForPlatform = policy: platform:
    lib.optionals (policy.linker.useMold && lib.hasInfix "-linux-" platform) [
      "-C"
      "link-arg=-fuse-ld=mold"
    ]
    # Gated on the ELF-producing triple rather than on the linker choice: both
    # mold and bfd/lld accept `--build-id` and neither emits the note by
    # default, so the flag belongs to the platform, not to `useMold`.
    ++ lib.optionals (policy.linker.buildId && lib.hasInfix "-linux-" platform) [
      "-C"
      "link-arg=-Wl,--build-id=sha1"
    ]
    ++ lib.optionals
    (policy.linker.useLld && pkgs.stdenv.hostPlatform.isDarwin && lib.hasInfix "-apple-darwin" platform)
    [
      "-C"
      "link-arg=-fuse-ld=lld"
      "-C"
      "link-arg=-B${pkgs.lld}/bin"
    ];

  nativeBuildInputsForPolicy = policy: lib.optional policy.linker.useMold pkgs.mold ++ lib.optional policy.linker.useLld pkgs.lld;

  clippyLintArgs = policy:
    toFlagSequence "-D" policy.clippy.deniedLints ++ toFlagSequence "-A" policy.clippy.allowedLints;

  # Cargo only emits `[lints.clippy]` into the unit graph's `lint_rustflags`
  # when invoked as `cargo clippy`, not `cargo build`. Parse the workspace
  # manifest and emit the equivalent `-D|-W|-A clippy::<lint>` flags so
  # per-unit clippy sees the workspace lint policy.
  clippyLintFlagsFromManifest = manifestPath: let
    # `clippy::cargo` group lints invoke `cargo` to read workspace metadata.
    # Per-unit clippy runs in a sandboxed build directory without a discoverable
    # Cargo.toml (the unit's source closure is package-shaped), so those lints
    # error out with "could not find Cargo.toml". Skip them here; a future
    # workspace-level cargo-clippy check is the right home.
    cargoGroupClippyLints = [
      "cargo"
      "cargo_common_metadata"
      "multiple_crate_versions"
      "negative_feature_names"
      "redundant_feature_names"
      "wildcard_dependencies"
    ];

    manifest = lib.importTOML manifestPath;

    raw = manifest.workspace.lints.clippy or manifest.lints.clippy or {};

    filtered = removeAttrs raw cargoGroupClippyLints;

    entryFor = name: value: {
      inherit name;
      level = value.level or value;
      priority = value.priority or 0;
    };

    entries = lib.mapAttrsToList entryFor filtered;

    sortedEntries = lib.sortOn (v: v.priority) entries;

    levelFlags = {
      deny = "-D";
      forbid = "-D";
      warn = "-W";
      allow = "-A";
    };

    entryFlags = entry: let
      inherit (entry) level;
      levelFlag =
        levelFlags."${level}"
              or (throw "cargoUnit: unknown clippy lint level '${level}' in ${manifestPath}");
    in [
      levelFlag
      "clippy::${entry.name}"
    ];
  in
    lib.concatMap entryFlags sortedEntries;

  # The three policy-check derivations for an already-normalized `args` + crate
  # name. clippy also needs to know whether the caller set `clippy.cargoArgs` (a
  # fact the policy merge flattens away), so the owner threads it through. Built
  # lazily and gated by the `crateChecks` / `workspaceChecks` wrappers below, so a
  # check the caller's altitude does not select is never forced.
  checkDerivations = {
    args,
    pname,
    clippyCargoArgsSet ? false,
  }: let
    configScript = vendorConfigScript {
      inherit (args) cargoExtraConfig cargoLock vendorDir;
    };

    cargoAuditCheck = let
      inherit (args.policy) cargoAudit;
      lockFile = cargoLockFile args.cargoLock;

      auditFlags = toFlagSequence "--deny" cargoAudit.deny ++ toFlagSequence "--ignore" cargoAudit.ignore;
    in
      pkgs.runCommand "${pname}-cargo-audit"
      {
        nativeBuildInputs = [pkgs.cargo-audit];
        # Stage the lockfile through a derivation input so its store path
        # is realized in every builder's sandbox, not just the one that
        # evaluated the expression.
        inherit lockFile;
      }
      ''
        export CARGO_HOME="$TMPDIR/cargo-home"
        mkdir -p "$CARGO_HOME"
        cp "$lockFile" "$TMPDIR/Cargo.lock"
        cd "$TMPDIR"

        cargo-audit audit \
          --file Cargo.lock \
          --db ${lib.escapeShellArg cargoAudit.db} \
          --no-fetch \
          --stale \
          ${lib.escapeShellArgs auditFlags}

        mkdir -p "$out"
      '';

    cargoMacheteCheck =
      pkgs.runCommand "${pname}-cargo-machete"
      (
        {
          nativeBuildInputs =
            [
              args.rustToolchain
              pkgs.cacert
              pkgs.cargo-machete
            ]
            ++ args.nativeBuildInputs;
          SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
          CARGO_NET_OFFLINE = "true";
        }
        // args.env
      )
      ''
        ${configScript}

        cd ${args.src}

        cargo-machete \
          --with-metadata --skip-target-dir \
          ${lib.escapeShellArgs args.policy.cargoMachete.extraArgs} \
          .

        mkdir -p "$out"
      '';
    cargoClippyCheck = let
      # If the caller already picks targets via `cargoArgs` (e.g.
      # `--all-targets`) and didn't override `clippy.cargoArgs`, drop the
      # policy default so we don't double up.
      cargoTargetSelectors = [
        "--all-targets"
        "--lib"
        "--bin"
        "--bins"
        "--example"
        "--examples"
        "--test"
        "--tests"
        "--bench"
        "--benches"
      ];

      lacksTarget = lib.mutuallyExclusive args.cargoArgs cargoTargetSelectors;

      hasLintPolicy = any nonEmpty [
        args.policy.clippy.deniedLints
        args.policy.clippy.allowedLints
      ];

      clippyArgs =
        args.cargoArgs
        ++ lib.optionals (lacksTarget || clippyCargoArgsSet) args.policy.clippy.cargoArgs
        ++ lib.optional hasLintPolicy "--"
        ++ clippyLintArgs args.policy;

      rustFlags = lib.concatStringsSep " " (
        rustcArgsForPolicyForPlatform args.policy pkgs.stdenv.hostPlatform.config
      );
    in
      pkgs.runCommand "${pname}-cargo-clippy"
      (
        {
          nativeBuildInputs =
            [
              args.rustToolchain
              pkgs.cacert
              args.policy.clippy.package
              pkgs.stdenv.cc
            ]
            ++ args.nativeBuildInputs
            ++ nativeBuildInputsForPolicy args.policy;
          SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        }
        // args.env
      )
      (
        ''
          ${configScript}

          export CARGO_TARGET_DIR="$TMPDIR/cargo-target"

        ''
        + (lib.optionalString (rustFlags != "")
          /*
          bash
          */
          ''
            export RUSTFLAGS="''${RUSTFLAGS:+$RUSTFLAGS }"${lib.escapeShellArg rustFlags}
          '')
        +
        /*
        bash
        */
        ''

          cd ${args.src}

          cargo clippy \
            --frozen --offline \
            ${lib.escapeShellArgs clippyArgs}

          mkdir -p "$out"
        ''
      );
  in {
    inherit cargoAuditCheck cargoMacheteCheck cargoClippyCheck;
  };

  # The per-crate gate set: clippy runs as a whole-crate `cargo clippy`. Each
  # check is gated on its enable flag and stays lazy.
  crateChecks = {
    args,
    pname,
    clippyCargoArgsSet ? false,
  }: let
    checks = checkDerivations {inherit args pname clippyCargoArgsSet;};
  in
    lib.optionalAttrs args.policy.cargoAudit.enable {cargoAudit = checks.cargoAuditCheck;}
    // lib.optionalAttrs args.policy.cargoMachete.enable {cargoMachete = checks.cargoMacheteCheck;}
    // lib.optionalAttrs args.policy.clippy.enable {cargoClippy = checks.cargoClippyCheck;};

  # The workspace gate set: audit + machete only. A workspace runs clippy per
  # unit in the renderer (`clippyByPackage`), so a whole-workspace `cargo clippy`
  # is deliberately absent here rather than suppressed after the fact.
  workspaceChecks = {
    args,
    pname,
  }: let
    checks = checkDerivations {inherit args pname;};
  in
    lib.optionalAttrs args.policy.cargoAudit.enable {cargoAudit = checks.cargoAuditCheck;}
    // lib.optionalAttrs args.policy.cargoMachete.enable {cargoMachete = checks.cargoMacheteCheck;};
in {
  inherit
    resolvePolicy
    policyPresets
    rustcArgsForPolicyForPlatform
    nativeBuildInputsForPolicy
    clippyLintArgs
    clippyLintFlagsFromManifest
    crateChecks
    workspaceChecks
    ;
}
