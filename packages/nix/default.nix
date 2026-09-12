{
  ix,
  lib,
  updateScriptWriter ? null,
  # The prebuilt jj tree ABI: a prefix holding `lib/libjj_tree.a` and
  # `include/jj_tree.h`, built from the ix crate `jj-tree-abi`. The fork's
  # libfetchers links its jj fetcher against it
  # (`src/libfetchers/meson.options`, option `jj-tree-prefix`), so a nix
  # built without it has no jj fetcher at all.
  #
  # `null` by DEFAULT AND FATAL WHEN FORCED, rather than a required formal.
  # The crate lives in the ix repository, outside this flake's source root
  # (`index.url = "path:./index"`), so nothing here can build it and only a
  # consumer can supply it. A required formal would throw during the
  # registry's package-set evaluation, which would also destroy the
  # `.override` seam that consumer needs -- the override re-calls this
  # function, and you cannot call `.override` on a value that throws. So it
  # is accepted as null and refused at the point of use, which leaves the
  # attribute evaluable, the override usable, and a build without it loud.
  jjTree ? null,
  # The ix jj client, on PATH as `jj` for the fork's functional tests
  # (`tests/functional/common/functions.sh`, `requireJj`, which fails rather
  # than skips). Same null-and-fatal contract, for the same reason: it is
  # `packages/jj-ix` in the ix repository, and that package name is what the
  # argument below is spelled after. It reaches the suite through
  # `nativeBuildInputs` below, not through a package.nix argument: see there.
  jjIx ? null,
}:
# The Nix view is surfaced as `ix.nixSrc` and built through nixpkgs' own
# modular nix packaging so the result is a protocol-compatible drop-in for the
# 2.34.7 daemon the fleet runs.
#
# The source is used as it comes. There is no in-repo series and no `./patches`
# directory. An earlier revision of this file described one, from a de-forking
# attempt that was reverted, and the description outlived the mechanism.
#
# So the fork's delta is not enumerated here, deliberately: it is the commits
# between the upstream anchor and the view tip. A list here would be a second
# copy that drifts. What the delta currently carries, in one line each: the
# GC-roots client-interrupt daemon crash fix; treating an inaccessible default
# lookup-path entry as absent (the macOS sandbox denies the host's
# root-channels dir with EPERM, which aborted the C API unit tests and the
# recursive-nix functional test on any darwin host that has one, while clean CI
# builders lack the path and so carry the stock drvs green); the
# `build-status-dir` build-observability series, where behind an experimental
# feature of that name every active build or substitution goal writes a JSON
# status file under `<nixStateDir>/status/`, readable daemonlessly via `nix
# store builds [--json]`; the lazy trees backport (NixOS/nix#15711 and its
# post-merge fixes) behind an off-by-default `lazy-trees` eval setting
# (indexable-inc/index#3645, and see indexable-inc/index#4297 for why no host
# sets it); `builtins.wasm` behind `wasm-builtin`, implemented by the Rust
# evaluator (rust/nix-eval-rs/src/wasm.rs; the C++ side registers the name and
# refuses the call) with deterministic execution so eval stays bit-identical
# across the mixed fleet (indexable-inc/index#3997); lazy git ref
# resolution so rev-pinned `builtins.fetchGit` inputs evaluate without network
# once cached (indexable-inc/index#4028); a jj working-copy fetcher; and a
# Rust evaluator with incremental evaluation and a shared runtime. The C++
# evaluator and its parallel executor have been removed. The fork's
# `codex/flake-check-eval-cache` branch (draft PR
# indexable-inc/nix#1) is deliberately excluded: self-declared WIP, untested,
# incomplete.
let
  # Read `pkgs` from `ix` rather than a `pkgs` callPackage formal: a `src`/`pkgs`
  # formal is fragile against `callPackage` auto-binding, and the rest of the
  # nix/* packages read `pkgs` off their argument the same way.
  inherit (ix) pkgs;

  # An argument this flake cannot supply for itself. Deferred rather than
  # asserted at eval time: see the `jjTree` formal above for why the
  # difference matters to the consumer's `.override`.
  fromIx = name: value:
    if value != null
    then value
    else
      throw ''
        packages/nix: `${name}` was not supplied.

        This nix links the jj tree ABI out of the ix repository, which is
        outside this flake's source root, so it has to be handed in. This
        package is not a flake output (`flake = false` in package.nix): it is
        reached through index.lib's `packageSetFor`, and the one consumer
        that supplies both arguments is ix's `nixPackageBySystem`
        (nix/flake/outputs/workspace.nix), which overrides this package with

            jjTree = <ix>.packages.<system>.jj-tree-abi;
            jjIx   = <ix>.packages.<system>.jj-ix;

        Every ix consumer of the fork reads that binding; nothing evaluates
        this package un-overridden on purpose. Standalone (index without
        ix), supply `jjTree` and `jjIx` when calling `packageSetFor`.

        There is deliberately no fallback. A nix built without the ABI has
        no jj fetcher, and shipping one silently would turn a configure-time
        error into a runtime "unsupported input type".
      '';

  # Cross lane (RFC 0009, #3585): when the registry cross lane instantiates
  # this package (`cross = true` in package.nix), swap the component scope to
  # the Linux -> Darwin nixpkgs cross scope so the whole modular C++ closure
  # builds on the linux fleet and a Mac only substitutes the fork daemon.
  # Everything modular below reads `componentPkgs`; `pkgs` stays the native
  # package set for build-platform helpers (provenance JSON, update script,
  # test tooling). See lib/darwin/nixpkgs-cross.nix for the scope.
  isCross = ix.cross.isCross or false;
  componentPkgs =
    if isCross
    then ix.cross.pkgs
    else pkgs;
  # `file -bL` prints the architecture in Mach-O spelling.
  machoArch =
    if lib.hasPrefix "aarch64-" (ix.cross.target or "")
    then "arm64"
    else "x86_64";

  bootstrapLockPath = ix.paths.root + "/.github/actions/bootstrap-patched-nix/lock.json";
  bootstrapLock = lib.importJSON bootstrapLockPath;
  updateScriptArgs = {
    name = "nix-ix-bootstrap-lock-update";
    runtimeInputs = [
      pkgs.git
    ];
    meta.description = "Record the checked Nix view bootstrap output";
    text = ''
      # nu
      const lock_path = ".github/actions/bootstrap-patched-nix/lock.json"
      const source_path = "views/nix"

      def main [] {
        let current = (open $lock_path)
        let actual_system = (
          ^nix --extra-experimental-features nix-command eval --raw --expr builtins.currentSystem
          | str trim
        )
        if $actual_system != $current.system {
          error make {msg: $"bootstrap lock requires ($current.system), found ($actual_system)"}
        }
        let source_tree = (^git rev-parse $"HEAD:($source_path)" | str trim)
        let output_path = (
          ^nix build --no-link --print-out-paths $"path:./($source_path)#nix-cli"
          | str trim
        )
        {outputPath: $output_path, sourceTree: $source_tree, system: $actual_system}
        | to json --indent 2
        | save --force $lock_path
        print $"updated ($lock_path) to ($source_tree)"
      }
    '';
  };

  # nixpkgs builds `nixVersions.nix_2_34` as
  # `(nixComponents_2_34.overrideSource fetchedSrc).appendPatches patches_common`
  # then takes `.nix-everything` (pkgs/tools/package-management/nix/default.nix).
  # The pinned rev IS the tag 2.34.7 nixpkgs itself fetches (byte-identical
  # narHash), so swapping the fetched source for our patched tree via the same
  # modular `overrideSource` handle rebuilds every component from the patched
  # tree while keeping nixpkgs' interdependency scope and build wiring intact.
  base = componentPkgs.nixVersions.nixComponents_2_34;
  upstreamVersion = lib.removeSuffix "\n" (builtins.readFile (ix.nixSrc + "/.version"));

  # curl 8.21.0 started consuming the public curl_multi_wakeup() eventfd from
  # inside curl_multi_perform(). That loses a wakeup for callers which perform
  # before polling, and libstore's file-transfer worker is exactly such a
  # caller: it can then sleep for its full 10-second idle timeout. Upstream
  # fixed it by giving the threaded resolver a separate internal wakeup pair
  # (https://github.com/curl/curl/issues/22272).
  #
  # This source override belongs here instead of on `pkgs.curl`. Curl reaches
  # GHC, rustc, cargo and the whole python package set as a build input via
  # git-minimal (ghc -> sphinx -> pytest-xdist -> execnet -> hatch-vcs -> git-minimal ->
  # curl), so overriding it globally rehashes most of nixpkgs and detaches the
  # tree from cache.nixos.org. Measured on 2026-07-25 against nixpkgs
  # e2587cae: arrow-cpp substitutes as a 28.5 MiB download and souffle as
  # 2.8 MiB, and both were being compiled from source here for this one patch.
  # `modular/src/libstore/package.nix` is the only nix component that takes
  # curl as an input, so scoping it to that component keeps the fix where the
  # stall happens and leaves everything else matching the binary cache.
  #
  # The view is a Git tree, so it lacks the generated files in Curl's release
  # tarball. Regenerating them adds an autotools step to this scoped build.
  # Drop this override once nixpkgs ships a Curl containing
  # 009fd378e8f01c97ebe67a14a41a06d56430f3df. The version assertion makes a
  # nixpkgs Curl bump fail visibly instead of silently carrying a stale fork.
  curlForNixStore = assert lib.assertMsg (componentPkgs.curl.version == "8.21.0")
  "remove the Curl source override: expected nixpkgs Curl 8.21.0, got ${componentPkgs.curl.version}";
    componentPkgs.curl.overrideAttrs (old: {
      src = ix.curlSrc;
      nativeBuildInputs = (old.nativeBuildInputs or []) ++ [componentPkgs.autoreconfHook];
      postPatch = ''
        # shell
        patchShebangs scripts
      '';
      preConfigure = ''
        # shell
        substituteInPlace ./config.guess --replace-fail /usr/bin/uname uname
      '';
    });

  # The source is the complete Nix view tree. The view history carries the
  # changes on top of upstream 2.34.7, so the only remaining build-time
  # patches are nixpkgs' own (`patchesCommon`).
  #
  # Components report a stable fork version. Exact source provenance belongs
  # to the aggregate artifact and its installed ix-provenance.json, so a Rust
  # edit does not rename and rebuild every unrelated C++ component.
  patchedNix = let
    cppSource = builtins.path {
      path = ix.nixSrc;
      name = "nix-source";
      filter = path: _type: let
        relative = lib.removePrefix "${ix.nixSrc}/" path;
      in
        relative != "rust" && !lib.hasPrefix "rust/" relative;
    };
    # nixpkgs' modular components derive sourceRoot from `patchedSrc.name`.
    # Keep all C++ headers, build support and embedded data. The Rust runtime
    # is supplied independently by prefix and never built from this source.
    # overrideSource otherwise includes the complete tree in every component,
    # defeating the runtime's separate source boundary even with stable versions.
    patchedSrc = {
      name = "nix-source";
      outPath = cppSource;
    };
    # nixpkgs deliberately omits filesets from its vendored component recipes.
    # Keep those recipes and their dependency scope, but snapshot each complete
    # fork component directory so additions and deletions enter its own source.
    # Cross-directory inputs are explicit; component .version/build-support
    # symlinks keep their original relative targets inside each snapshot.
    componentExtraRoots =
      (lib.genAttrs [
        "src/libutil"
        "src/libutil-c"
        "src/libutil-test-support"
        "src/libutil-tests"
        "src/libstore"
        "src/libstore-c"
        "src/libstore-test-support"
        "src/libstore-tests"
        "src/libfetchers"
        "src/libfetchers-c"
        "src/libfetchers-tests"
        "src/libexpr"
        "src/libexpr-c"
        "src/libexpr-test-support"
        "src/libexpr-tests"
        "src/libflake"
        "src/libflake-c"
        "src/libflake-tests"
        "src/libmain"
        "src/libmain-c"
        "src/libcmd"
        "src/nswrapper"
        "src/perl"
      ] (_: []))
      // {
        "src/nix" = [
          "scripts"
          "misc"
          "doc/manual/generate-manpage.nix"
          "doc/manual/generate-settings.nix"
          "doc/manual/generate-store-info.nix"
          "doc/manual/utils.nix"
          "doc/manual/source/store/types/index.md.in"
          "doc/manual/source/command-ref/files/profiles.md"
        ];
        "src/libstore-tests" = ["tests/functional/derivation"];
        "tests/functional" = ["scripts/nix-profile.sh.in"];
        "doc/manual" = manualFixtureRoots;
        "src/internal-api-docs" = ["src"];
        "src/external-api-docs" = [
          "src/libutil-c"
          "src/libexpr-c"
          "src/libflake-c"
          "src/libstore-c"
        ];
        "src/json-schema-checks" = manualFixtureRoots ++ ["doc/manual/source/protocols/json/schema"];
      };
    manualFixtureRoots = [
      "src/libutil-tests/data/memory-source-accessor"
      "src/libutil-tests/data/hash"
      "src/libstore-tests/data/content-address"
      "src/libstore-tests/data/store-path"
      "src/libstore-tests/data/realisation"
      "src/libstore-tests/data/derivation"
      "src/libstore-tests/data/derived-path"
      "src/libstore-tests/data/path-info"
      "src/libstore-tests/data/nar-info"
      "src/libstore-tests/data/build-result"
      "src/libstore-tests/data/dummy-store"
      "tests/functional/derivation"
    ];
    componentSource = relative: let
      roots = [relative ".version" "nix-meson-build-support"] ++ componentExtraRoots.${relative};
    in
      assert lib.assertMsg (builtins.hasAttr relative componentExtraRoots)
      "packages/nix: no source boundary declared for component ${relative}";
      assert lib.assertMsg (builtins.pathExists (ix.nixSrc + "/${relative}/meson.build"))
      "packages/nix: component ${relative} has no Meson source root";
      assert lib.assertMsg (builtins.all (entry: builtins.pathExists (ix.nixSrc + "/${entry}")) roots)
      "packages/nix: component ${relative} has a missing source dependency"; {
        name = "nix-source";
        outPath = builtins.path {
          path = ix.nixSrc;
          name = "nix-source";
          filter = path: _type: let
            part = lib.removePrefix "${ix.nixSrc}/" path;
          in
            path
            == toString ix.nixSrc
            || builtins.any
            (entry: part == entry || lib.hasPrefix "${entry}/" part || lib.hasPrefix "${part}/" entry)
            roots;
        };
      };
    withComponentSource = _: previous: let
      prefix = "${patchedSrc.name}/";
      relative = lib.removePrefix "./" (lib.removePrefix prefix previous.sourceRoot);
    in
      assert lib.assertMsg (lib.hasPrefix prefix previous.sourceRoot)
      "packages/nix: unexpected component sourceRoot ${previous.sourceRoot}"; {
        version = componentVersion;
        src = componentSource relative;
        sourceRoot = "nix-source/${relative}";
      };
    source = {
      version = upstreamVersion;
      storePath = builtins.unsafeDiscardStringContext (toString ix.nixSrc);
    };
    commonPatches = [];
    sourceDigest = builtins.hashString "sha256" (builtins.toJSON {
      inherit (source) storePath;
      inherit commonPatches;
    });
    shortHash = builtins.substring 0 20 sourceDigest;
    version = "${upstreamVersion}+ix.h${builtins.substring 0 8 sourceDigest}";
    componentVersion = "${upstreamVersion}+ix";
    provenance = {
      schema = 3;
      algorithm = "sha256";
      inherit commonPatches sourceDigest source version;
    };
    provenanceJson = (pkgs.formats.json {}).generate "nix-ix-provenance.json" provenance;

    nixHostRs = import ./rust-host-runtime.nix {
      inherit ix lib componentPkgs;
    };
    nixEvalRs = import ./rust-runtime.nix {
      inherit ix lib componentPkgs;
    };
    # The Rust evaluator's meson flags, in one place. Every `nix-ix` links the
    # Rust evaluator: `builtins.wasm` (the `.ix` converter) exists only there,
    # so a nix-ix without it cannot import `.ix` at all. The cost is one crate
    # derivation (`nixEvalRs` above), shared by both C++ consumers.
    withRustEval = component:
      component.overrideAttrs (old: {
        mesonFlags =
          (old.mesonFlags or [])
          ++ ["-Drust-eval-prefix=${nixEvalRs}"];
      });
    patchedComponents = ((base.overrideSource patchedSrc).overrideAllMesonComponents
      withComponentSource)
      .overrideScope (final: prev: {
      # See `curlForNixStore` above: libstore owns the file-transfer
      # worker the curl regression stalls, and it is the only component that
      # takes Curl, so the source view is scoped to it instead of `pkgs.curl`.
      nix-store = (prev.nix-store.override {curl = curlForNixStore;}).overrideAttrs (old: {
        mesonFlags = (old.mesonFlags or []) ++ ["-Drust-host-prefix=${nixHostRs}"];
      });
      # Stable component versions do not identify host behavior. The immutable
      # CLI output captures its complete host dependency closure for cache keys.
      nix-cli = prev.nix-cli.overrideAttrs (old: {
        mesonFlags = (old.mesonFlags or []) ++ ["-Dhost-build-identity=${builtins.placeholder "out"}"];
      });
      # Both consumers use exactly the same runtime path and allocator/state.
      nix-flake = withRustEval prev.nix-flake;
      # The remaining flake C API exposes settings and reference parsing. Its
      # inherited recipe still propagates the deleted expression C API.
      nix-flake-c = prev.nix-flake-c.overrideAttrs (old: {
        propagatedBuildInputs =
          lib.filter
          (input: !(builtins.elem (lib.getName input) ["nix-expr" "nix-expr-c"]))
          old.propagatedBuildInputs;
      });
      nix-flake-tests = prev.nix-flake-tests.overrideAttrs (old: {
        buildInputs =
          lib.filter (input: lib.getName input != "nix-expr-test-support") old.buildInputs
          ++ [final.nix-store-test-support];
      });
      # nixpkgs owns these component recipes; source overrides do not remove
      # its parser generators, old TOML parser, or deleted REPL dependencies.
      nix-expr = prev.nix-expr.overrideAttrs (old: {
        nativeBuildInputs =
          lib.filter
          (input: !(builtins.elem (lib.getName input) ["bison" "flex" "cmake"]))
          old.nativeBuildInputs;
        buildInputs = lib.filter (input: lib.getName input != "toml11") old.buildInputs;
      });
      nix-cmd = withRustEval (prev.nix-cmd.overrideAttrs (old: {
        buildInputs =
          lib.filter
          (input: !(builtins.elem (lib.getName input) ["editline" "readline"]))
          old.buildInputs;
        mesonFlags = lib.filter (flag: !lib.hasPrefix "-Dreadline-flavor=" flag) old.mesonFlags;
      }));

      # The jj fetcher lives in libfetchers, so this is the component that
      # links the jj archive. The Rust evaluator is a separate shared runtime
      # (above). Everything above libfetchers (libexpr, libflake, the CLI,
      # the daemon) picks it up transitively, which is why there is exactly
      # one seam here.
      #
      # `overrideAttrs`, not `override`, and this is the ONLY author of the
      # flag. nixpkgs' modular packaging owns the component lambdas: it
      # vendors its own copy of every `package.nix` under
      # `pkgs/tools/package-management/nix/modular/`, and `overrideSource`
      # swaps `src` alone. The libfetchers lambda actually called here is
      # nixpkgs' (formals: lib, mkMesonLibrary, nix-util, nix-store,
      # nlohmann_json, libgit2, version), so `.override { jjTree = ...; }`
      # died at eval with "function 'anonymous lambda' called with unexpected
      # argument 'jjTree'". What does cross the `overrideSource` boundary is
      # the SOURCE, and the source's meson is what reads the flag
      # (`src/libfetchers/meson.options`, `meson.build`, which errors when the
      # prefix is empty) -- the same seam `-Drust-eval-prefix` uses above. The
      # fork's own `src/libfetchers/package.nix` therefore declares no
      # `jjTree` and builds no flag: one option, one author, here.
      #
      # The archive needs no `buildInputs` entry. Interpolating the store path
      # into `mesonFlags` carries its string context, which is what makes it
      # an input of this derivation, and meson reads
      # `<prefix>/lib/libjj_tree.a` and `<prefix>/include` by absolute path.
      # `buildInputs` would additionally splice it, which is wrong under
      # `isCross`: the consumer hands in an archive already built for the host
      # platform.
      nix-fetchers = prev.nix-fetchers.overrideAttrs (old: {
        mesonFlags =
          (old.mesonFlags or [])
          ++ ["-Djj-tree-prefix=${fromIx "jjTree" jjTree}"];
      });

      # The fork's functional suite drives a real jj client: `requireJj`
      # (`tests/functional/common/functions.sh`) fails rather than skips, and
      # the three jj tree-identity tests in `passthru.tests` below run through
      # this derivation. nixpkgs owns this lambda too and its formals have no
      # `jj-ix`, so the client is appended to the input list nixpkgs' copy
      # does declare rather than passed as an argument.
      #
      # `old.nativeBuildInputs` with no `or []` on purpose: nixpkgs' copy
      # always sets it -- that is where `git` and `mercurial` come from -- so a
      # nixpkgs refactor that renames the attribute fails here loudly instead
      # of quietly building a test closure with neither git nor jj.
      #
      # `packages/jj-ix` installs `bin/jj` (`lib/jj-client-satellite.nix`,
      # `installedName = "jj"`), which is the name `requireJj` probes. It has
      # to be the ix client: the tests exercise ix-local stores that upstream
      # jujutsu cannot open.
      #
      # `unixtools.script` and `zstd` for the same reason: the fork's own
      # tests/functional/package.nix declares them (binary-cache.sh rewrites
      # a NAR with the compressor the cache used, zstd by default), but that
      # lambda is not the one this path calls, and nixpkgs' copy has neither.
      # Without them binary-cache.sh dies with exit 127 (`zstd: command not
      # found`) rather than skipping: the tool is a declared dependency of
      # the suite, not an optional one.
      nix-functional-tests = prev.nix-functional-tests.overrideAttrs (old: {
        nativeBuildInputs =
          old.nativeBuildInputs
          ++ [
            (fromIx "jjIx" jjIx)
            pkgs.unixtools.script
            pkgs.python3
            pkgs.zstd
          ];
      });
    });

    # The aggregate `nix` package (daemon + client + libs), the same attribute
    # `nixVersions.nix_2_34` exposes.
    nixEverything = patchedComponents.nix-everything;
  in
    nixEverything.overrideAttrs (old: {
      inherit version;
      passthru =
        (old.passthru or {})
        // {
          # The patched modular component set, for tools that must link the
          # same patched libexpr this daemon-compatible client uses
          # (packages/nix-eval-jobs: the CI evaluator has to parse the
          # same language the client does, underscore digit separators
          # included).
          components = patchedComponents;
          # The crate derivation itself, for `nixEvalRsClippy` below (which is
          # this derivation with its build phase swapped) and for building the
          # evaluator alone.
          # The aggregate inherits a component version in passthru, which
          # otherwise shadows its own derivation version for consumers.
          inherit nixEvalRs nixHostRs provenance componentVersion version;
        };
      # The aggregate's `doCheck = true` gates the build on `checkInputs`: the
      # five component unit-test runners plus the entire upstream functional
      # suite. Those dominate a cold build of this closure and re-validate
      # nothing per consumer rebuild: the source arrives pre-patched from the
      # fork repo, the series carries its own
      # upstream-style functional test inside the patched tree, and the `smoke`
      # passthru below executes the linked binary. With them on, the cache-push
      # darwin lane (3-core hosted mac) blew its 4 h job budget cold-building
      # this package and froze `cache-ready` (run 28772327218, index#1967).
      doCheck = false;
      # The cross build cannot execute its result (the `smoke` passthru is
      # native-only below), so assert the container format in-build: a
      # mislinked binary (ELF, wrong arch) fails on the linux builder instead
      # of on the first Mac that substitutes it.
      nativeBuildInputs = (old.nativeBuildInputs or []) ++ lib.optional isCross pkgs.file;
      installPhase =
        (old.installPhase or "")
        + ''
          # shell
          install -Dm444 ${provenanceJson} "$out/share/nix/ix-provenance.json"
        ''
        + lib.optionalString isCross ''
          # shell
          format=$(file -bL "$out/bin/nix")
          # file(1) orders arch and kind differently across versions
          # ("Mach-O 64-bit arm64 executable" vs "... executable arm64").
          case $format in
          "Mach-O 64-bit ${machoArch} executable"* | "Mach-O 64-bit executable ${machoArch}"*) ;;
          *)
            echo "expected a Mach-O 64-bit ${machoArch} executable, got: $format" >&2
            exit 1
            ;;
          esac
        '';
      meta =
        (old.meta or {})
        // {
          description = "NixOS/nix ${upstreamVersion} from the index jj view (h${shortHash})";
          mainProgram = "nix";
        };
    });

  package = patchedNix;

  # Test 2414 calls wakeup, perform, then poll. Curl 8.21.0 consumed the public
  # wakeup during perform and left poll asleep until the idle timeout.
  curlMultiWakeup = curlForNixStore.overrideAttrs (_: {
    doCheck = true;
    checkTarget = "test";
    checkFlags = ["TFLAGS=2414"];
  });

  # The override's real risk is that the whole modular C++ tree still links and
  # the installed binary and provenance file agree with the eval-time identity.
  # `--version` exits without touching a store or daemon, so it is safe here.
  smoke =
    pkgs.runCommand "nix-ix-smoke"
    {
      nativeBuildInputs = [
        package
        pkgs.jq
      ];
      strictDeps = true;
    }
    ''
      expected=${lib.escapeShellArg "nix (Nix) ${package.componentVersion}"}
      actual=$(nix --version)
      if [[ "$actual" != "$expected" ]]; then
        echo "nix --version disagrees with the compiled component version" >&2
        printf 'expected: %s\nactual:   %s\n' "$expected" "$actual" >&2
        exit 1
      fi

      if ! jq -e \
        --arg version ${lib.escapeShellArg package.version} \
        --arg sourceDigest ${lib.escapeShellArg package.provenance.sourceDigest} \
        --arg sourceStorePath ${lib.escapeShellArg package.provenance.source.storePath} \
        '.schema == 3 and .algorithm == "sha256" and .version == $version and .sourceDigest == $sourceDigest and .source.storePath == $sourceStorePath' \
        ${package}/share/nix/ix-provenance.json >/dev/null; then
        echo "installed provenance disagrees with the package identity" >&2
        cat ${package}/share/nix/ix-provenance.json >&2
        exit 1
      fi

      mkdir -p "$out"
    '';

  focusedFunctionalTest = {
    name,
    testDaemon ? null,
  }: let
    # One component scope; every build links the Rust evaluator, so a test
    # whose claims include a Rust arm runs that arm here with nothing to skip
    # on.
    tests = package.components.nix-functional-tests.override (
      lib.optionalAttrs (testDaemon != null) {test-daemon = testDaemon;}
    );
  in
    tests.overrideAttrs (old: {
      mesonCheckFlags = (old.mesonCheckFlags or []) ++ [name];
    });

  # These protocol regressions create their own disposable stores. Their
  # focused owner needs the real CLI and Python, not the unrelated jj client
  # required by the full Meson suite that also registers the same scripts.
  focusedPythonTest = {
    name,
    script,
  }:
    pkgs.runCommand name {
      NIX_CONFIG = "experimental-features = nix-command ca-derivations\nmin-free = 0\nmax-free = 0";
      NIX_USER_CONF_FILES = "/dev/null";
    } ''
      # shell
      export NIX_CONF_DIR="$TMPDIR/empty-nix-conf"
      ${lib.getExe pkgs.python3} ${ix.nixSrc + "/tests/functional/${script}"} ${package.components.nix-cli}/bin/nix
      mkdir "$out"
    '';

  remoteRetainedFixture =
    pkgs.runCommandCC "nix-remote-retained-fixture" {
      nativeBuildInputs = [pkgs.pkg-config];
      buildInputs = [package.components.nix-store];
    } ''
      # shell
      mkdir -p "$out/bin"
      $CXX -std=c++23 ${ix.nixSrc + "/tests/functional/remote-retained-fixture.cc"} \
        $(pkg-config --cflags --libs nix-store) -o "$out/bin/remote-retained-fixture"
    '';
  remoteRetainedMapping = pkgs.runCommand "nix-remote-retained-mapping" {} ''
    # shell
    ${lib.getExe pkgs.python3} ${ix.nixSrc + "/tests/functional/remote-retained-mapping.py"} \
      ${package.components.nix-cli}/bin/nix ${remoteRetainedFixture}/bin/remote-retained-fixture
    mkdir "$out"
  '';

  # The whole `rust-eval` meson suite as ONE check: every `rust-eval-*.sh`
  # functional test, which tests/functional/meson.build puts in that suite by
  # name. One derivation and no roster, because the roster was the defect: three
  # tests were registered here by hand while a dozen others were meson tests no
  # CI job ran, which is the E1 finding again -- a control that exists and never
  # executes reads as coverage. `focusedFunctionalTest` leaves the derivation
  # named `nix-functional-tests`, which every other focused check also carries,
  # so this one has its own pname and is addressable on its own.
  #
  # Direct result, filesystem, and persistent-cache contracts.
  rustEvalTests = package.components.nix-functional-tests.overrideAttrs (old: {
    pname = "nix-rust-eval-tests";
    mesonCheckFlags = (old.mesonCheckFlags or []) ++ ["--suite" "rust-eval"];

    # Pinned, not inherited. `postCheck` runs only inside `checkPhase`, and
    # stdenv skips `checkPhase` when `doCheck` is unset -- so a future
    # build-budget trim setting `doCheck = false` (as another derivation in
    # this file already does) would make the guard below unreachable and
    # take this lane green without running the required tests.
    doCheck = true;

    # A SKIPPED test leaves this derivation GREEN. `skipTest` exits 77,
    # meson's default exitcode protocol reads 77 as SKIP, and `meson test`
    # still exits 0 -- so if any test in the suite ever starts skipping,
    # the check goes green and the lane measures nothing for ever. That is exactly the failure class
    # this lane exists to kill, one level up. So assert the outcome the lane
    # is promoted on: every test in the suite RAN, and every one said OK.
    #
    # Read meson's STRUCTURED log, not the human one. `testlog.txt` embeds
    # the whole build environment, and that includes this postCheck's own
    # source, so a text pattern there can match itself and pass vacuously.
    # `testlog.json` is one JSON object per test and is selected on fields.
    # The expected count comes from meson's own listing of the suite, never
    # from a number kept by hand here.
    #
    # Fails closed: a missing log, a count that disagrees with the listing,
    # or any result other than OK is a failure rather than a pass.
    postCheck = ''
      # shell
      testlogs=()
      while IFS= read -r tl; do testlogs+=("$tl"); done < <(
        find . -path '*meson-logs/testlog.json' -print | sort)
      if [[ ''${#testlogs[@]} -ne 1 ]]; then
        echo "Rust evaluator tests: expected exactly one meson testlog.json, found ''${#testlogs[@]}" >&2
        exit 1
      fi
      builddir=$(dirname "$(dirname "''${testlogs[0]}")")
      registered=$(meson test -C "$builddir" --suite rust-eval --list | grep -c .)
      if [[ "$registered" -lt 1 ]]; then
        echo "Rust evaluator tests: meson lists no tests in the rust-eval suite" >&2
        exit 1
      fi
      ran=$(jq -r '.name' "''${testlogs[0]}" | grep -c .)
      notOk=$(jq -r 'select(.result != "OK") | "\(.name): \(.result)"' "''${testlogs[0]}")
      if [[ "$ran" -ne "$registered" ]]; then
        echo "Rust evaluator tests: the rust-eval suite lists $registered tests but $ran ran:" >&2
        jq -r '.name' "''${testlogs[0]}" >&2
        exit 1
      fi
      if [[ -n "$notOk" ]]; then
        echo "Rust evaluator tests: tests that did not report OK:" >&2
        echo "$notOk" >&2
        echo "A SKIP here means this build lost the rust evaluator, and without" >&2
        echo "this check the derivation would have succeeded without running the required tests." >&2
        exit 1
      fi
      # These migrations require their live command boundary fixtures even
      # if a future Meson edit accidentally drops a registration.
      for required in rust-eval-registry rust-eval-build-scheduler rust-eval-import-cache; do
        if ! jq -s -e --arg required "$required" \
          '[.[] | select((.name | split(" - ") | last | sub("^nix-functional-tests:"; "")) == $required)] | length == 1 and .[0].result == "OK"' \
          "''${testlogs[0]}" >/dev/null; then
          echo "Rust evaluator tests: mandatory test $required did not run exactly once successfully" >&2
          exit 1
        fi
      done
      echo "Rust evaluator tests: $ran of $registered rust-eval tests OK"
    '';
  });

  hostValueTests = package.components.nix-expr-tests.tests.run;

  contentAddressBoundaries = focusedFunctionalTest {name = "blake3";};
  deferredStoreWrites = focusedPythonTest {
    name = "nix-deferred-store-writes";
    script = "deferred-store-writes.py";
  };
  remoteAdmissionProgress = focusedPythonTest {
    name = "nix-remote-admission-progress";
    script = "remote-admission-progress.py";
  };
  legacySshGc = focusedPythonTest {
    name = "nix-legacy-ssh-gc";
    script = "legacy-ssh-gc.py";
  };
  realisationSignatures = focusedFunctionalTest {name = "signatures";};

  autoGcInterrupt = focusedFunctionalTest {name = "gc-auto";};
  # `libfetchers: resolve git refs lazily and refresh the cached HEAD`
  # regression coverage: a cached rev-pinned fetchGit input must evaluate
  # without remote git subprocesses, and the cached HEAD must refresh on a
  # successful network lookup (indexable-inc/index#4028).
  fetchGitHeadCache = focusedFunctionalTest {name = "fetchGit-head-cache";};
  daemonSignal = focusedFunctionalTest {
    name = "daemon-signal";
    testDaemon = package.components.nix-cli;
  };
  buildStatus = focusedFunctionalTest {name = "build-status";};
  # Patch 0024 regression coverage: the upstream relative-paths lock file
  # test now asserts sparse child-lock semantics (stale copied nodes refresh
  # from the child's own flake.lock; in-sync locks stay byte-identical).
  sparseLocks = focusedFunctionalTest {name = "relative-paths-lockfile";};
  # The lock-file WRITE path (`InputScheme::putFile`) per source kind. jj
  # needs no commit step, because a snapshot gives every working-copy state a
  # revision; a git working tree needs one, and since the fetchers stopped
  # serving mutable trees that difference decides whether `nix flake update`
  # can write at all.
  lockFileWrites = focusedFunctionalTest {name = "lock-file-writes";};
  # The jj tree-identity suite (`tests/functional/jj-tree/`), the coverage for
  # the fetcher that links `libjj_tree.a`: `identity` that a store path is
  # derived from the blake3 tree id with no file reads, `lock` that a jj input
  # locks as `treeHash` and never `narHash`, `relative` that `path:./sub`
  # inside a jj parent resolves to the parent's SUBTREE object rather than
  # acquiring an identity of its own, and `filtered` that `builtins.path` on
  # a jj-served directory is addressed by the FILTERED tree's id (no file
  # read, lazily mounted, unmoved by edits outside the kept files). Meson
  # names a test after its script (`fs.replace_suffix`), and each of these
  # four basenames is registered exactly once across the whole functional
  # suite, so the bare name selects one test.
  jjTreeIdentity = focusedFunctionalTest {name = "identity";};
  jjTreeLock = focusedFunctionalTest {name = "lock";};
  jjTreeRelative = focusedFunctionalTest {name = "relative";};
  # `filtered` also asserts that the Rust evaluator's road lands on the same
  # store paths, and refuses to run without that evaluator (every nix-ix links
  # it, so a refusal here is a broken build, not a missing option).
  jjTreeFiltered = focusedFunctionalTest {name = "filtered";};
  # `fix(libstore): don't abort when an output path becomes valid mid-build`
  # regression coverage: a local-overlay store whose LOWER store gains an
  # input-addressed output while the overlay is still building that very
  # derivation must keep the registered path and carry on, not abort the
  # process on `assert(newInfo.ca)`. That is the shape of the ephemeral-upper
  # CI lane (ix#8445), where concurrent jobs publish into the shared durable
  # store the others build against.
  overlayLowerGainsOutput = focusedFunctionalTest {name = "lower-gains-output";};
  # `don't let Darwin discard a fast-exiting builder's log` regression
  # coverage: a builder that writes to stderr and exits at once, while other
  # jobs are starting, must still have its output in the failure message and in
  # `nix log`. On macOS it did not -- XNU flushes a pseudoterminal's output
  # queue about 0.6s after the last slave fd closes, and nix only polls once it
  # has finished starting every runnable child (ENG-11172). This runs on linux
  # too, where it asserts the invariant the darwin fix restores.
  buildLogFastExit = focusedFunctionalTest {name = "build-log-fast-exit";};
  # `libstore: Bit-reproducibly fix darwin Mach-O page hashes after rewriting`
  # regression coverage: after `RewritingSink` mutates bytes the linker had
  # already covered with ad-hoc page hashes, the rewritten binary must still
  # execute, verify under codesign, and keep its `linker-signed` flag rather
  # than being re-signed. The test expects `--check` itself to fail (LC_UUID is
  # still a stale content hash, index#4336) and inspects the `.check` binary.
  machoRewrite = focusedFunctionalTest {name = "macho-rewrite";};
  # Separate per-unit checks cover the default workspace and runtime-only
  # no-default-features configuration without unifying `perf` back on.
  nixEvalRsClippy = package.nixEvalRs.checks.clippy;
  nixEvalRsTests = package.nixEvalRs.checks.tests;
  nixEvalRsDocs = package.nixEvalRs.checks.docs;
  nixHostRsTests = package.nixHostRs.checks.tests;
  nixHostRsDocs = package.nixHostRs.checks.docs;
  nixHostRsClippy = package.nixHostRs.checks.clippy;
  rustRuntimeOwnership = package.nixHostRs.checks.ownership;
  rustRuntimeSourceIsolation = package.nixHostRs.checks.sourceIsolation;
  rustRuntimeUnitIsolation = package.nixHostRs.checks.unitIsolation;
in
  package.overrideAttrs (old: {
    passthru =
      (old.passthru or {})
      // {
        inherit bootstrapLock;
        # Execution tests are native-only: the cross package's binary cannot
        # run on the linux build host (its format is asserted in-build
        # instead), and the check catalog collects tests from the native
        # `repoPackages` entry only.
        tests =
          (old.passthru.tests or old.tests or {})
          // lib.optionalAttrs (!isCross) {
            inherit deferredStoreWrites remoteRetainedMapping contentAddressBoundaries remoteAdmissionProgress legacySshGc realisationSignatures autoGcInterrupt buildLogFastExit buildStatus curlMultiWakeup daemonSignal fetchGitHeadCache jjTreeFiltered jjTreeIdentity jjTreeLock jjTreeRelative lockFileWrites machoRewrite hostValueTests nixEvalRsClippy nixEvalRsTests nixEvalRsDocs nixHostRsTests nixHostRsDocs nixHostRsClippy rustRuntimeOwnership rustRuntimeSourceIsolation rustRuntimeUnitIsolation overlayLowerGainsOutput rustEvalTests smoke sparseLocks;
          };
      }
      // lib.optionalAttrs (updateScriptWriter != null) {
        updateScript = updateScriptWriter updateScriptArgs;
      };
  })
