{
  lib,
  paths,
  packageRegistry,
  cargoUnitFor,
  buildSvelteSite,
  buildLibghosttyVt,
  ghosttySrc,
  writeBashApplication,
  # Cross-compilation leaves, threaded in so `unitsFor { target }` can build a
  # second unit graph for a non-host triple without `workspace.nix` having
  # to reach back into the assembled `ix` surface.
  rustToolchainFor,
  # `pkgs -> the ix rustc fork toolchain` (packages/rustc-ix), for the NATIVE
  # unit graph on x86_64-linux (see nativeForkToolchain below). Threaded as
  # its own formal beside `rustToolchainFor`, never made the tooling-wide
  # default in lib/rust/tooling.nix: that default backs every
  # `ix.buildRustPackage` and the prebuilt-unit toolchainId assertions
  # (cargo-unit.nix's `defaultToolchainId`), and resolving the fork through
  # the assembled package set there would let the overlay that builds
  # rustc-ix recurse into itself.
  forkRustToolchainFor,
  appleSdkToolchain,
  macosSdk,
  # The shared pins reader (lib/util/pins.nix), threaded down from
  # lib/default.nix so the libkrun-efi 1.19.3 pins load from the sibling
  # pins.json without a cross-directory `../` import (no-parent-path).
  pins,
}: workspacePkgs: let
  inherit (paths) root;

  # libghostty-vt built for the workspace's package set, from the checked
  # Ghostty view (the fork's C-API additions such as the per-cell
  # hyperlink URI are part of the surface ix-vt binds, so the unpatched
  # upstream base would fail the link). ix-vt-sys links this dylib, so the
  # unit graph needs both the build-script env (so the build script emits the
  # link directives) and a workspace-wide `-L` search path (a build script's
  # own link-search does not propagate to the final per-unit link in this
  # graph; see the alsa note below for the same shape). The dylib dir is also
  # a runtime input for the ix-vt tests, which dlopen it.
  libghosttyVt = buildLibghosttyVt workspacePkgs {
    ghosttySource = ghosttySrc;
  };
  ghosttyLibDir = "${libghosttyVt}/lib";

  # The dashboard's single-page UI (Svelte/Vite, one self-contained index.html).
  # `dashboard-core`'s build script embeds it at compile time via
  # `IX_DASHBOARD_SITE_HTML` below, so the generated bundle is built by nix
  # rather than committed to the repo. Only `dashboard-core` reads the env var,
  # and it is handed over through `packageBuildEnv` rather than the
  # workspace-wide `env` so the site stays out of every other unit's build
  # closure.
  dashboardSiteRoot = root + "/packages/dashboard/dashboard-core/site";
  dashboardSite = buildSvelteSite workspacePkgs {
    sourceRoot = dashboardSiteRoot;
    serve.enable = false;
    # The islands palette (single source of truth, owned by code-highlight)
    # lives outside the filtered site source; hand the build that one file so
    # vite's `$islands-theme` alias resolves (see the site's vite.config.js).
    preBuild = "export IX_ISLANDS_THEME=${
      root + "/packages/code-highlight/src/islands-theme.json"
    }";
  };
  dashboardSiteHtml = "${dashboardSite}/share/dashboard-site/index.html";
  src = let
    rustPackageFiles = packagePath:
      lib.fileset.unions [
        (packagePath + "/Cargo.toml")
        (packagePath + "/src")
        (lib.fileset.maybeMissing (packagePath + "/benches"))
        (lib.fileset.maybeMissing (packagePath + "/build.rs"))
        (lib.fileset.maybeMissing (packagePath + "/tests"))
        (lib.fileset.maybeMissing (packagePath + "/templates"))
      ];
  in
    lib.fileset.toSource {
      inherit root;
      fileset = lib.fileset.unions (
        [
          (root + "/Cargo.toml")
          (root + "/Cargo.lock")
          (rustPackageFiles (paths.modules + "/services/resource-monitor/stats-writer"))
          (rustPackageFiles (paths.modules + "/services/sandboxed-agent/egress-check"))
          (rustPackageFiles (paths.modules + "/services/sandboxed-agent/launch"))
          (rustPackageFiles (paths.modules + "/services/sandboxed-agent/proxy"))
        ]
        ++ map (entry: rustPackageFiles entry.path) packageRegistry.rustWorkspaceEntries
      );
    };
  cargoLock = root + "/Cargo.lock";

  # `vmkit` links libkrun for its Linux-guest backend, a different libkrun per
  # host. nixpkgs only provides `libkrun-efi` when the *build host* is
  # aarch64-darwin (it is not cross-buildable from Linux), and classic `libkrun`
  # only on a Linux host, so gate on the build host, NOT the target: a
  # Linux->darwin cross build (the `cross-darwin-smoke` check) must never force
  # `workspacePkgs.libkrun-efi`, which would refuse to evaluate on the Linux host.
  # When neither gate holds, `vmkit`'s build script omits the link env, so the
  # crate compiles without the libkrun backend (see its `build.rs`/`linuxkrun.rs`).
  buildHostIsAarch64Darwin =
    workspacePkgs.stdenv.hostPlatform.isDarwin && workspacePkgs.stdenv.hostPlatform.isAarch64;
  buildHostIsLinux = workspacePkgs.stdenv.hostPlatform.isLinux;

  # macOS host: libkrun-efi lib dir + the OVMF firmware blob it embeds (the latter
  # lives in the libkrun source tree). `vmkit`'s build script embeds the firmware
  # via `KRUN_EFI_FIRMWARE` and links `-lkrun`; the search path/rpath are injected
  # below because a build script's link-search does not reach the final unit link.
  # Only referenced under `buildHostIsAarch64Darwin`, so non-darwin hosts never
  # force the (host-only) package.
  #
  # nixpkgs pins libkrun-efi 1.18.0, whose vsock `from_tx_virtq_head` accepts
  # only the exact two-descriptor hdr+data TX layout; the combined or split
  # descriptor chains modern guest kernels emit are silently dropped mid
  # SOCK_STREAM (upstream containers/libkrun#535/#579), which desyncs the panes
  # guest->host frame stream (#1719). 1.19.3's packet.rs rewrite handles those
  # chains, so rebuild the same nixpkgs expression against the 1.19.3 source
  # until the nixpkgs pin catches up (nixpkgs master already carries this exact
  # bump; both hashes below match its pkgs/by-name/li/libkrun-efi). Version
  # deltas the override must carry: the upstream repo moved orgs
  # (containers -> libkrun), the guest init the vendored `init_blob` crate
  # embeds moved from `init/` to `src/init_blob/init/`, and the EFI firmware
  # moved from `edk2/` to `src/vmm/edk2/`.
  libkrunEfiSrcPin = pins.loadPin ./pins.json "libkrun-efi-src";
  libkrunEfiSrc = workspacePkgs.fetchFromGitHub {
    inherit (libkrunEfiSrcPin) owner repo hash;
    tag = "v${libkrunEfiSrcPin.version}";
  };
  # Same recipe as the `initBinary` in nixpkgs' libkrun-efi expression, rebuilt
  # here because the pinned 1.18.0 one compiles from `init/` in the old source.
  libkrunEfiInit = workspacePkgs.pkgsCross.aarch64-multiplatform.pkgsStatic.stdenv.mkDerivation {
    pname = "libkrun-init";
    inherit (libkrunEfiSrcPin) version;
    src = libkrunEfiSrc;

    dontConfigure = true;

    # Upstream ships no tests for the static init blob.
    doCheck = false;

    buildPhase = ''
      # shell
      runHook preBuild
      cd src/init_blob/init
      $CC -O2 -static -Wall -o init init.c dhcp.c
      runHook postBuild
    '';

    installPhase = ''
      # shell
      runHook preInstall
      install -D init $out/init
      runHook postInstall
    '';
  };
  libkrunEfi = workspacePkgs.libkrun-efi.overrideAttrs (old: {
    inherit (libkrunEfiSrcPin) version;
    src = libkrunEfiSrc;
    cargoDeps = workspacePkgs.rustPlatform.fetchCargoVendor {
      src = libkrunEfiSrc;
      inherit (pins.loadPin ./pins.json "libkrun-efi-cargo-vendor") hash;
    };
    env =
      (old.env or {})
      // {
        KRUN_INIT_BINARY_PATH = "${libkrunEfiInit}/init";
      };
  });
  libkrunEfiLibDir = "${libkrunEfi}/lib";
  krunEfiFirmware = "${libkrunEfi.src}/src/vmm/edk2/KRUN_EFI.silent.fd";

  # Linux host: classic KVM libkrun (no firmware). It boots a rootfs over virtiofs
  # under its bundled libkrunfw kernel, so the core path needs no block/net
  # feature; GPU, block, and net are enabled for parity with the macOS path and so
  # `--gpu` (and future disk boots) work. nixpkgs installs the shared lib into
  # `lib64` and force-links `-lkrunfw` with an rpath, so libkrun.so resolves
  # libkrunfw itself at runtime: only libkrun's own lib dir must reach our binary's
  # rpath. Only referenced under `buildHostIsLinux`, so darwin hosts never force it.
  libkrunLinux = workspacePkgs.libkrun.override {
    withBlk = true;
    withNet = true;
    withGpu = true;
  };
  libkrunLinuxLibDir = "${libkrunLinux}/lib64";

  # pyo3 extension modules must leave Python symbols undefined so the host
  # interpreter resolves them when it loads the module; macOS refuses
  # undefined symbols in a dylib without `-undefined dynamic_lookup`, while
  # Linux allows them. Injected below for every registry `pyExtension`
  # package, replacing the per-crate build.rs copies of these two flags.
  pyExtensionLinkArgs = [
    "-C"
    "link-arg=-undefined"
    "-C"
    "link-arg=dynamic_lookup"
  ];

  # One workspace-wide unit graph for every repo-owned Rust crate. Each
  # crate's `default.nix` picks its binary and test targets out of the native
  # graph via `ix.cargoUnit.selectBinaryWithTests`, so the unit graph + vendor
  # closure get generated once instead of per crate. `nix-cargo-unit` itself
  # stays on the bootstrap path (it's what builds this graph). `target != null`
  # produces a separate cross graph used only to emit binaries. The `src`
  # fileset above spans every crate, but a source body edit re-runs only the
  # render IFD: cargo-unit plans the graph against a manifest-scoped stub of
  # `src`, so the whole-workspace cargo resolve re-runs only when a manifest
  # or the file set changes (lib/rust/cargo-unit.nix `plannerSource`, #3900).
  # The native graph's toolchain on x86_64-linux: the ix rustc fork
  # (packages/rustc-ix), for the same two capabilities the ix workspace
  # already defaults to (ix repo, lib/workspace-cargo-unit.nix): rmeta
  # byte-stability, which policy.compiler.rmetaStability's auto defaults
  # turn on for any fork-toolchain graph so a comment edit stops cascading
  # past the first crate whose output converges, and `-Zdump-test-names`
  # discovery, which compiles test targets without codegen or linking inside
  # the eval-blocking manifest IFD. Off x86_64-linux the fork package does
  # not exist (index builds it for the CI architecture only) and the native
  # graph keeps the repo-pin default, exactly as before; an explicit system
  # gate rather than a probe, so a broken registry entry on x86_64-linux
  # fails loud instead of quietly falling back. The cross graphs below keep
  # their own rust-overlay toolchain untouched (they need cross targets the
  # fork does not carry).
  nativeForkToolchain =
    if workspacePkgs.stdenv.hostPlatform.system == "x86_64-linux"
    then forkRustToolchainFor workspacePkgs
    else null;

  mkUnits = {target ? null}: let
    # `cargo` cfg-excludes platform-gated deps per target, so an Apple-Silicon
    # or Intel macOS unit graph never sees `alsa-sys`; gate the ALSA plumbing on
    # the *target* OS rather than the build host so a Linux→macOS cross build
    # does not drag Linux audio inputs into a Darwin graph.
    targetIsLinux =
      if target == null
      then workspacePkgs.stdenv.hostPlatform.isLinux
      else lib.hasInfix "-linux-" target;
    # Same target-OS gate for the pyo3 link-arg injection below: the darwin
    # *target* decides whether the cdylib link needs `dynamic_lookup`.
    targetIsDarwin =
      if target == null
      then workspacePkgs.stdenv.hostPlatform.isDarwin
      else lib.hasSuffix "-apple-darwin" target;
    targetSystem =
      if target == null
      then workspacePkgs.stdenv.hostPlatform.system
      else if lib.hasSuffix "-apple-darwin" target
      then
        if lib.hasPrefix "aarch64-" target
        then "aarch64-darwin"
        else "x86_64-darwin"
      else if lib.hasPrefix "aarch64-" target
      then "aarch64-linux"
      else "x86_64-linux";
    excludedWorkspaceMembers =
      lib.filter (
        entry: !(builtins.elem entry (packageRegistry.rustWorkspaceEntriesFor targetSystem))
      )
      packageRegistry.rustWorkspaceEntries;
    cargoWorkspaceExcludes =
      lib.concatMap (entry: [
        "--exclude"
        entry.id
      ])
      excludedWorkspaceMembers;
    # Crates that must not share cargo's workspace-wide feature resolution
    # (registry `isolatedFeatures`): the Python consumers unify e.g.
    # unibind-runtime's `py` feature across a `--workspace` resolve, which
    # would pull pyo3's `#[used]` constructors into a Node addon cdylib that
    # then fails to dlopen (undefined Python symbols). Each such crate roots
    # its own `-p` cargo invocation, so its dependency features resolve from
    # its own manifest alone; nix-cargo-unit merges the graphs, and the
    # `--exclude` below keeps the crate out of the workspace resolve so its
    # roots exist exactly once.
    isolatedFeatureMembers =
      lib.filter (
        entry: builtins.elem entry (packageRegistry.rustWorkspaceEntriesFor targetSystem)
      )
      packageRegistry.isolatedFeatureEntries;
    isolatedFeatureExcludes =
      lib.concatMap (entry: [
        "--exclude"
        entry.id
      ])
      isolatedFeatureMembers;
    isolatedFeatureTargets = map (entry: ["-p" entry.id]) isolatedFeatureMembers;
    isolatedFeatureTargetNames = map (entry: "isolated-${entry.id}") isolatedFeatureMembers;
    # A build script's `rustc-link-search` does not reach the final per-unit link
    # in this graph, so a linked native lib's directory is added to the link search
    # here directly, plus an rpath entry so the resulting binary resolves the shared
    # object at runtime without `LD_LIBRARY_PATH` (the `-L` alone only covers link
    # time). Harmless for crates that never reference the lib: they keep no
    # DT_NEEDED/load command for it.
    linkSearchWithRpath = dir: [
      "-L"
      "native=${dir}"
      "-C"
      "link-arg=-Wl,-rpath,${dir}"
    ];
    # The Apple cross toolchain (zig cc + macOS SDK), or null for host/musl/Linux
    # targets that build with the ordinary linker.
    appleToolchain =
      if target != null && lib.hasSuffix "-apple-darwin" target
      then
        appleSdkToolchain {
          appleSdk = macosSdk {pkgs = workspacePkgs;};
          inherit lib target writeBashApplication;
          pkgs = workspacePkgs;
        }
      else null;
    isCross = target != null;
    cargoUnit = cargoUnitFor workspacePkgs;
    # ── Workspace-wide build env, and the guard that keeps it narrow ────────
    # Every entry lands in all ~2.4k units of the graph (see the `env = ` note
    # below). Nothing this repo builds belongs here; put it in
    # `packageBuildEnv.<cargo-package>` instead.
    workspaceWideEnv =
      lib.optionalAttrs (appleToolchain != null) appleToolchain.env;
    # Default-deny over the store paths reachable from `workspaceWideEnv`.
    # Adding a workspace-wide env var that carries a store path, or repointing
    # an existing one at a different derivation, fails eval until the author
    # either scopes it with `packageBuildEnv.<package>` (almost always the right
    # answer) or records the derivation below with a reason. Keyed by
    # `parseDrvName` name with the store hash stripped, so a version bump or a
    # rebuild of an allowed dependency is inert and only a genuinely new
    # dependency trips the guard.
    # The only entries this needs today, enumerated by running the guard with
    # an empty allowlist against all four graphs (aarch64-darwin native,
    # x86_64-linux native, and the two Linux->Darwin cross targets). Both
    # native graphs report zero store paths; everything below comes from the
    # cross graphs.
    allowedWorkspaceWideEnvDeps = lib.optionalAttrs (appleToolchain != null) (
      # `appleSdkToolchain`'s CC/CXX/AR/RANLIB/LINKER wrappers and its CMake
      # toolchain file: on a cross-darwin graph these ARE the C toolchain, so
      # every unit that compiles C goes through them and there is nowhere
      # narrower to put them. The tool list is spelled out rather than
      # derived from the toolchain, so a new wrapper still has to be approved
      # here. They move only when the wrapper text changes.
      lib.genAttrs (
        map (tool: "apple-sdk-${tool}-${target}") [
          "ar"
          "cc"
          "cxx"
          "linker"
          "ranlib"
        ]
        ++ ["apple-sdk-toolchain-${target}.cmake"]
      ) (_name: "appleToolchain: cross-darwin C toolchain wrapper")
      // {
        # SDKROOT and the `-isysroot` in CFLAGS/CXXFLAGS. The version is part
        # of the derivation name, so `parseDrvName` cannot strip it and an SDK
        # pin bump WILL trip this guard. That is the intended trade: bumping
        # the SDK re-hashes every unit in the cross graph anyway, so it is
        # worth one line of review here (lib/darwin/macos-sdk.nix).
        "MacOSX15.4.sdk" = "appleToolchain: SDKROOT and -isysroot";
      }
    );
    # `getContext` keys are store paths (`…-name.drv` for a derivation
    # reference, `…-name` for a plain source path); strip the 32-char hash and
    # the `.drv` suffix, then drop the version so bumps do not trip the guard.
    storeDepName = storePath: let
      base = baseNameOf storePath;
    in
      (builtins.parseDrvName (
        lib.removeSuffix ".drv" (builtins.substring 33 (builtins.stringLength base) base)
      ))
      .name;
    # A value is a string, a path, or a derivation (cargo-unit stringifies all
    # three into the unit's env), and every one of those can carry a store
    # path. Taking the context of the *stringified* value rather than only of
    # `isString` values is what makes `SDKROOT = <derivation>` visible: filter
    # on `isString` and a derivation-valued attr sails straight past the guard.
    storeDepsOf = value:
      map storeDepName (
        builtins.attrNames (
          builtins.getContext (
            if builtins.isAttrs value && !(lib.isDerivation value)
            then ""
            else builtins.toString value
          )
        )
      );
    workspaceWideEnvProblems = lib.unique (
      lib.concatLists (
        lib.mapAttrsToList (
          envName: value:
            map (dep: "${envName} -> ${dep}") (
              builtins.filter (dep: !(allowedWorkspaceWideEnvDeps ? ${dep})) (storeDepsOf value)
            )
        )
        workspaceWideEnv
      )
    );
    # Hung off `cargoUnit` rather than written as `assert …; buildWorkspace {…}`
    # so that reaching `buildWorkspace` forces it. The two are equally eager;
    # this spelling just keeps the 200-line argument attrset below out of the
    # assert's body, and so out of the diff.
    guardedCargoUnit = assert lib.assertMsg (workspaceWideEnvProblems == []) ''
      rust workspace: workspace-wide `env` gained a store-path dependency that
      is not on the allowlist:
      ${lib.concatMapStringsSep "\n" (entry: "  - ${entry}") workspaceWideEnvProblems}
      cargo-unit folds `env` into every unit, so this pins that path into all
      ~2.4k unit derivations and every rebuild of it re-hashes the whole graph
      (ENG-10672). Move the variable to `packageBuildEnv.<cargo-package>` so
      only its readers are invalidated, or, if it genuinely has to be
      workspace-wide, add the derivation to `allowedWorkspaceWideEnvDeps`
      above with a reason.
    ''; cargoUnit;
  in
    guardedCargoUnit.buildWorkspace (
      {
        pname = "ix-rust-workspace${lib.optionalString isCross "-${target}"}";
        inherit src;
        cargoLock.lockFile = cargoLock;
        workspaceRoot = root;
        cargoArgs = ["--workspace"] ++ cargoWorkspaceExcludes ++ isolatedFeatureExcludes;
        # Cross test/bench binaries can't execute on the build host, so a cross
        # graph builds only the `--workspace` root set; the native graph keeps
        # the test and bench roots for `passthru.tests`. Isolated-feature
        # crates root their own `-p` entries (see isolatedFeatureMembers).
        cargoTargets =
          [
            (["--workspace"] ++ cargoWorkspaceExcludes ++ isolatedFeatureExcludes)
          ]
          ++ isolatedFeatureTargets
          ++ lib.optionals (!isCross) [
            (
              [
                "--workspace"
                "--tests"
              ]
              ++ cargoWorkspaceExcludes
              ++ isolatedFeatureExcludes
            )
            (
              [
                "--workspace"
                "--benches"
              ]
              ++ cargoWorkspaceExcludes
              ++ isolatedFeatureExcludes
            )
          ];
        cargoTargetNames =
          [
            "build"
          ]
          ++ isolatedFeatureTargetNames
          ++ lib.optionals (!isCross) [
            "test"
            "bench"
          ];
        packageTestInputs = {
          tui = [workspacePkgs.vim];
          # ix-vt's tests dlopen the libghostty-vt dylib at runtime; make its lib
          # dir available so the loader resolves `@rpath`/`-l ghostty-vt`.
          ix-vt = [libghosttyVt];
          # clone-cli's diff-gate integration tests build temp git repos and run
          # `git` (directly and via the `clone` binary's diff gate). The test
          # sandbox has no git otherwise, so the tests panic spawning it.
          clone-cli = [workspacePkgs.git];
          # efx's cloudflare executors shell out to curl (pointed at a local
          # stub via CLOUDFLARE_API_BASE in the integration tests). The test
          # sandbox has no curl otherwise, so the executors fail spawning it.
          efx = [workspacePkgs.curl];
          # mirror's generator reads the package's commit history with `git
          # log` and its tests build a scratch monorepo to commit into. The
          # test sandbox has no git otherwise, so the tests panic spawning it.
          mirror = [workspacePkgs.git];
          # nix-web-monitor's switch dirty-tree-guard tests build scratch git
          # repos and run `git status` against them. The test sandbox has no
          # git otherwise, so the tests panic spawning it.
          nix-web-monitor = [workspacePkgs.git];
          # tree-sync's file set comes from `git ls-files`, and its tests build
          # real repositories, worktrees and submodules to prove the .git-is-a-
          # file cases work. The remote tests run the generated far-end commands
          # (`mkdir -p`, `tar -x`, `find -printf`, `xargs rm`) through a stand-in
          # for ssh, so the sandbox needs those too.
          tree-sync = [
            workspacePkgs.git
            workspacePkgs.gnutar
            workspacePkgs.findutils
            workspacePkgs.coreutils
          ];
        };
        # `rodio` (packages/minecraft/sound) pulls `cpal`/`alsa-sys`, whose build
        # script needs ALSA's pkg-config metadata to link `libasound` on Linux.
        #
        # `pkg-config` + `PKG_CONFIG_PATH` let `alsa-sys`'s build script find ALSA
        # and emit `link-lib=asound`. That `-lasound` propagates to the final
        # `minecraft-sound` link, but the build script's `link-search` path does
        # not, so the linker reports `cannot find -lasound`. Add ALSA's lib dir to
        # every unit's rustc link search directly so the final binary link resolves
        # it. Harmless for crates that never reference `libasound`.
        nativeBuildInputs =
          lib.optional targetIsLinux workspacePkgs.pkg-config
          ++ lib.optionals (appleToolchain != null) appleToolchain.runtimeInputs;
        # rusqlite's `session` feature (pulled in by `sqlmerge`, and unified
        # across the workspace by cargo's feature resolution) switches
        # libsqlite3-sys to `buildtime_bindgen`: the pregenerated bundled
        # bindings do not cover the sqlite3session_* API, so its build script
        # runs bindgen, which dlopens libclang at runtime and parses the
        # bundled sqlite3 headers. No sandbox has a libclang on the default
        # dlopen search path -- macOS was assumed to leak one in from the
        # system, but it does not, and the build script panicked with `Unable
        # to find libclang` on aarch64-darwin (#4272) -- so hand it nixpkgs'
        # libclang plus the include dirs clang would otherwise get from its
        # driver, which bindgen (a bare libclang caller, not a driver) never
        # sees. Gated on the *build host*, not the target: build scripts are
        # compiled and executed for the machine doing the building, so it is
        # that machine's libclang and header layout that a cross graph needs
        # too. Scoped per-package so the env does not invalidate every other
        # unit in the dependency closure (see cargo-unit's buildWorkspace docs).
        # Everything in this table reaches only its package's own compile and
        # build-script-run units; anything moved out of it and into
        # workspace-wide `env` below re-hashes all ~2.4k units in the graph on
        # every rebuild of its value (ENG-10672), which the guard at the top of
        # this `let` now refuses.
        packageBuildEnv =
          {
            libsqlite3-sys = {
              LIBCLANG_PATH = "${workspacePkgs.llvmPackages.libclang.lib}/lib";
              BINDGEN_EXTRA_CLANG_ARGS = lib.concatStringsSep " " (
                [
                  "-isystem"
                  "${workspacePkgs.llvmPackages.libclang.lib}/lib/clang/${
                    lib.versions.major workspacePkgs.llvmPackages.libclang.version
                  }/include"
                ]
                # glibc's headers live in a separate `dev` output that only the
                # cc wrapper knows about. Darwin's come from the SDK sysroot
                # libclang is already configured with, so it needs no equivalent.
                ++ lib.optionals buildHostIsLinux [
                  "-isystem"
                  "${workspacePkgs.stdenv.cc.libc.dev}/include"
                ]
              );
            };
          }
          # `alsa-sys` only exists in a Linux-target graph (cargo cfg-excludes
          # it elsewhere), and its build script is the only
          # `pkg_config::probe_library` caller this workspace feeds: the value
          # holds ALSA's `.pc` file and nothing else, so no other crate can
          # resolve anything through it. The final `minecraft-sound` link still
          # needs ALSA's lib dir, which `extraLinkRustcArgsForPlatform`
          # supplies on link units only.
          // lib.optionalAttrs targetIsLinux {
            alsa-sys.PKG_CONFIG_PATH = "${workspacePkgs.alsa-lib.dev}/lib/pkgconfig";
          }
          // {
            # ix-vt-sys's build script reads this to emit the libghostty-vt link
            # search path (packages/tui/vt/ix-vt-sys/build.rs); it is the only
            # reader in the tree.
            ix-vt-sys.IX_VT_GHOSTTY_LIB_DIR = ghosttyLibDir;
            # dashboard-core's build script reads this to embed the dashboard
            # page. Scoped per-package: workspace-wide it made the Svelte/Vite
            # site (and its npm install) a build input of EVERY unit in the
            # graph, so anything that had to build an ix binary from source --
            # an `ix apply` guest, most of all -- ran `npm install` for a
            # SvelteKit app first. That is what ran a 14 GB guest root out of
            # space on `dashboard-site-node-modules` (ENG-10488).
            dashboard-core.IX_DASHBOARD_SITE_HTML = dashboardSiteHtml;
            # Both are read by packages/vmkit/build.rs and nothing else. The
            # firmware path is re-emitted as `cargo:rustc-env`, so vmkit's own
            # compile unit resolves the `include_bytes!(env!(...))` in
            # linuxkrun.rs -- `packageBuildEnv` covers a package's compile units
            # as well as its build-script-run unit, so the store path stays an
            # input of both. The two gates are mutually exclusive (darwin build
            # host vs linux build host), so at most one key is populated.
            vmkit =
              lib.optionalAttrs buildHostIsAarch64Darwin {
                KRUN_EFI_FIRMWARE = krunEfiFirmware;
              }
              // lib.optionalAttrs (buildHostIsLinux && !isCross) {
                VMKIT_LINK_LIBKRUN = "1";
              };
          };
        # Per-package rustc args for pyo3 extension-module cdylibs (registry
        # `pyExtension`). Scoped per package so the relaxed link cannot mask
        # genuine undefined-symbol errors elsewhere in the workspace. rustc
        # only forwards `-C link-arg` to actual link units, so the args are
        # inert for the package's rlib/rmeta compiles.
        packageRustcArgs = lib.optionalAttrs targetIsDarwin (
          lib.genAttrs (map (entry: entry.id) packageRegistry.pyExtensionEntries) (
            _id: pyExtensionLinkArgs
          )
        );
        # cargo-unit folds `env` into EVERY unit in the graph, vendored
        # crates.io dependencies included, so a value carrying a store path
        # makes all ~2.4k unit derivations depend on that path and every
        # rebuild of it re-hashes the whole graph. Keep this table free of
        # anything the repo rebuilds often; `packageBuildEnv` above is where
        # those go. The `workspaceWideEnvProblems` guard at the top of this
        # `let` is default-deny over exactly this attrset.
        env = workspaceWideEnv;
        # Build scripts emit native `-l` flags that propagate to downstream final
        # links, but their `rustc-link-search` paths do not cross cargo-unit's
        # per-unit derivation boundary. Keep native search/rpath args on final
        # link units only, so pure dependency rlibs remain independent of these
        # host native libraries.
        extraLinkRustcArgsForPlatform = _platform:
          linkSearchWithRpath ghosttyLibDir
          ++ lib.optionals targetIsLinux (
            [
              "-L"
              "native=${workspacePkgs.alsa-lib}/lib"
            ]
            # smithay's wayland_frontend (panes-compositor) links libxkbcommon:
            # the `-lxkbcommon` flag reaches the final link but, as with alsa
            # above, the emitting crate's link-search path does not. The rpath
            # keeps the guest binary loadable without LD_LIBRARY_PATH.
            ++ linkSearchWithRpath "${workspacePkgs.libxkbcommon}/lib"
          )
          ++ lib.optionals buildHostIsAarch64Darwin (linkSearchWithRpath libkrunEfiLibDir)
          ++ lib.optionals (buildHostIsLinux && !isCross) (linkSearchWithRpath libkrunLinuxLibDir);
        # The native graph runs every policy check once across the whole
        # workspace (selected package outputs expose these as explicit tests).
        # A cross graph is a pure build artifact, so it skips policy to avoid
        # re-running clippy/audit/machete that the native graph already covers.
        # `embedMetadata = true` because this graph pins a stable toolchain
        # and `-Zembed-metadata=no` is nightly-only: leaving the default
        # sends a `-Z` flag to a rustc that exits 1 on it (ENG-12992). The cost
        # is a fatter rlib on a graph nothing links against twice.
        policy =
          if isCross
          then cargoUnit.policyPresets.pureBuild // {compiler.embedMetadata = true;}
          else {
            denyUnusedCrateDependencies = true;
            cargoAudit.enable = true;
            # cargo-machete is redundant with the per-crate
            # unused_crate_dependencies (rustc) gate, which is compile-based and
            # more precise than machete's heuristic scan, and machete only ran
            # as one whole-workspace pass. Rely on the per-crate check instead.
            cargoMachete.enable = false;
            clippy.enable = true;
          };
      }
      # The fork default for the native graph (see nativeForkToolchain
      # above). Conditional so the non-x86_64-linux native graph's args stay
      # byte-identical to the pre-flip ones; discovery flips with the
      # toolchain because `-Zdump-test-names` only exists there, mirroring
      # the ix workspace's lane. This re-keys every native unit once
      # (toolchainId salts every unit hash).
      // lib.optionalAttrs (!isCross && nativeForkToolchain != null) {
        rustToolchain = nativeForkToolchain;
        testDiscovery = "dump-test-names";
      }
      // lib.optionalAttrs isCross {
        inherit target;
        # Input-address the cross graph (the native graph keeps cargo-unit's
        # `contentAddressed = true` default). A floating-CA output has no
        # eval-time path, so substituting it requires the substituter's
        # `/realisations` build trace to map drv hash -> output path -- and
        # cache.ix.dev (atticd behind ncps) serves narinfos only, 404ing that
        # endpoint. A Mac evaluating a Darwin cross alias therefore planned a
        # local x86_64-linux cross build it cannot run (#1755). Input-addressed
        # drvs carry concrete out paths at eval, so the Mac substitutes via
        # plain narinfo, and cache-push's probe can skip already-pushed cross
        # roots instead of always re-realising them.
        contentAddressed = false;
        # rust-overlay toolchain carrying the cross target's `rust-std`. The
        # native graph keeps `cargo-unit`'s default (nixpkgs cargo + rustc).
        rustToolchain = rustToolchainFor workspacePkgs {
          channel = "stable";
          version = "latest";
          targets = [target];
        };
        extraRustcArgsForPlatform =
          if appleToolchain != null
          then appleToolchain.rustcArgsForPlatform
          else (_platform: []);
      }
    );

  units = mkUnits {};
in {
  inherit
    root
    src
    cargoLock
    units
    dashboardSite
    # The devshell mirrors this into IX_VT_GHOSTTY_LIB_DIR (and darwin's
    # DYLD_FALLBACK_LIBRARY_PATH) so plain `cargo test -p tui -p ix-vt`
    # links and dlopens the same libghostty-vt the unit graph uses (#3118).
    ghosttyLibDir
    ;

  /**
  Build a cross-compiled unit graph for a non-host `target` triple.

  `target` is a Rust target triple. `aarch64-apple-darwin` /
  `x86_64-apple-darwin` build through the zig + macOS SDK toolchain (see
  [`lib/darwin/apple-sdk-toolchain.nix`](lib/darwin/apple-sdk-toolchain.nix)); other triples
  (e.g. `x86_64-unknown-linux-musl`) build with the ordinary linker and only
  need a toolchain carrying the target `rust-std`. Returns the same shape as
  `units`; select a binary with `ix.cargoUnit.selectBinaryWithTests` or
  `workspace.binaries.<name>`.
  */
  unitsFor = {target}:
    mkUnits {inherit target;};
}
