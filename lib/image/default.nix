{
  lib,
  nixpkgs,
  rust-overlay,
  paths,
  system,
  home-manager,
  overlays,
  ixSpecialArgs,
  moduleList,
  writeNushellApplication,
  packageSetFor,
  portableServices,
  # The index flake's own `self`, for the guest `index` registry pin (see the
  # `nix.registry.index` module below). `null` when `lib` is imported without a
  # flake; the pin is then omitted.
  self ? null,
}: let
  # The `nix.registry.index.to` construction, shared with the
  # `image-registry-pin` check (tests/default.nix). See its doc comment; the
  # module exposes its exclusion list alongside `pin` so that check can assert
  # against the same list this consumer builds with.
  registryPin = (import ./registry-pin.nix {inherit lib;}).pin;
  /**
  One nixpkgs instance shared by every image evaluation. `lib.nixosSystem`
  otherwise instantiates a fresh nixpkgs PER node, and a consumer that
  evaluates many images in one evaluation (the ix fleet's regional-status
  canary evaluates every example fleet x 2 regions inside one NixOS host
  config) pays that instantiation 20-30 times over: 656M thunks / 5.5min
  of nix CPU for one host, and an OOM-killed deploy under nox (ix
  ENG-2728/ENG-2729). The parameters fold in exactly what the per-node
  modules used to set: the repo overlay (previously a `nixpkgs.overlays`
  module) and the platform config from `platform.nix`.

  Unfree packages enter images only by explicit name here, never by
  flipping `allowUnfree`. Every image shares this one instance via
  `nixpkgs.pkgs`, and the nixpkgs module then ignores a per-image
  `nixpkgs.config` (setting one even fails an assertion), so an image's
  unfree exception has to be added to this predicate, not to the image.
    - `yourkit-java`: the opt-in `ix.languages.java.yourkit` profiler agent
      an operator turns on for performance work.
      Refs: https://www.yourkit.com/docs/java/help/agent.jsp
    - `claude-code`: Anthropic's agent CLI imported by the dev base module;
      ships under commercial terms.
    - `nomad`: HashiCorp relicensed Nomad to BUSL 1.1 at 1.6. The
      `examples/nomad/cluster` fleet runs it as a demo scheduler, squarely
      inside the license's non-competing production grant.
      Refs: https://www.hashicorp.com/license-faq
    - `minecraft-server`: Mojang's EULA-bound server jar. The SDK's
      declarative example (ix packages/sdk/examples/nixos) boots it via
      `services.minecraft-server` with `eula = true` in the same config,
      so acceptance is explicit where the exception is used.
      Refs: https://www.minecraft.net/eula
  The predicate keeps every other unfree (Oracle JDK, Adobe runtimes,
  NVIDIA blobs) failing at eval until the platform allows it explicitly.
  */
  imagePkgs = import nixpkgs {
    inherit system overlays;
    config = {
      allowUnfreePredicate = pkg:
        builtins.elem (lib.getName pkg) [
          "yourkit-java"
          "claude-code"
          "minecraft-server"
          "nomad"
        ];
    };
  };

  /**
  Declarative-but-writable files for guest users: the `mutable.files` home
  module (modules/home/mutable-files.nix), wired into every guest's Home
  Manager the way lib/home-modules.nix wires it for workstations.

  A guest mounts /nix/store read-only, so Home Manager's default store-symlink
  deployment leaves every file it manages unwritable inside the VM -- fatal for
  the shell rc files that tools like rustup self-install into. The base profile
  routes root's rc files through this module; the block in
  modules/profiles/base/default.nix has the incident.

  `_file` keeps the module's own source in `definitionsWithLocations` (the
  #3938 wrapper lib/home-modules.nix documents); the module's own `key` still
  dedups this instance against any other a consumer combines.
  */
  mutableFilesHomeModule = {
    _file = paths.modules + "/home/mutable-files.nix";
    imports = [
      (import (paths.modules + "/home/mutable-files.nix") {
        # The module resolves `index-delta` through a `system -> package set`
        # function. Every image evaluates against the single `imagePkgs`
        # instance above, and lib/default.nix fixes that instance's system, so
        # there is exactly one answer here. Assert it rather than discard the
        # argument, so a future per-system guest fails at this line instead of
        # silently baking the wrong architecture's binary.
        indexPackages = requestedSystem:
          assert lib.assertMsg (requestedSystem == system)
          "guest mutable.files: index-delta requested for ${requestedSystem}, but images evaluate against ${system} (lib/default.nix pins it); thread a real per-system package set through lib/image first";
            packageSetFor imagePkgs;
        portableServicesModule = portableServices.homeModule;
      })
    ];
  };

  /**
  Locked flake-input sources baked into the base image but NOT reachable
  through a `nix.registry.*` pin, so `system.extraDependencies` roots them
  into the system closure (and `includeNixDB` in oci-layer.nix registers
  everything in that closure as valid). Single source of truth: the module
  below sets `system.extraDependencies` to this list, and the
  `base-image-nix-db` check (tests/default.nix) reads it back off the
  evaluated `system.extraDependencies` to assert each path ships valid — the
  registry-derived projection there cannot catch these, precisely because
  they are not registry pins.

  Measurement-driven exception to the "don't bake the flake inputs" hold
  (index #1748/#1815): on the current base image the FIRST `nix run index#jq`
  in a fresh ix VM took 2m44.8s, its log showing `unpacking
  'github:oxalica/rust-overlay/107c334f...' into the Git cache` — evaluating
  index's flake under `nix run` forces the `rust-overlay` input source, which
  the image does not ship, so nix fetches and unpacks it through VCFS. Baking
  it drops that to the warm ~2.6s. rust-overlay is ~19M; home-manager and
  hermes-agent stay unbaked until a measurement justifies each.

  `.outPath` is the ORIGINAL `-source` store path with string context, so it
  roots into the closure once (no duplicate copy — the #1748 trap); the path
  must be the LOCKED input so it matches what index's `flake.lock` narHash
  resolves to during in-guest eval.
  */
  extraBakedSources = [rust-overlay.outPath];

  /**
  Run the platform config, OCI packaging, base profile, the full module
  registry, and the caller's `modules` through `lib.nixosSystem`, then
  return the evaluated `config`. This is the evaluation path every
  image build and every eval test goes through, so a test exercising it
  catches the same regressions a real build would.

  Arguments:
  - `modules`: list of additional modules layered on top of the base.
  */
  evalImageConfig = {modules ? []}:
    (lib.nixosSystem {
      inherit system;
      specialArgs.ix = ixSpecialArgs;
      modules =
        [
          {nixpkgs.pkgs = imagePkgs;}
          {
            # Pin the system flake registry so `nix shell nixpkgs#foo` resolves
            # against the nixpkgs bundled in the image instead of fetching a
            # fresh tarball from GitHub on every invocation (~40 MB download,
            # 100k files extracted, 20+ minutes on VCFS).
            #
            # `narHash` locks the pin. Without it nix treats the `path:` input
            # as mutable and re-hashes AND re-copies the whole ~45k-file tree
            # into /nix/store on every eval; through the guest's virtiofs/VCFS
            # store that is ~3 minutes per `nix eval`/`nix run`, ~1 s locked
            # (measured in an `ix new` VM, 2026-07-02). `outPath` (a string)
            # rather than the path value also keeps `toJSON` from copying a
            # duplicate nixpkgs tree into the image closure. Lives here, not
            # platform.nix, because only this scope sees the flake input's
            # `narHash`.
            nix.registry.nixpkgs.to = {
              type = "path";
              path = nixpkgs.outPath;
              inherit (nixpkgs) narHash;
            };
          }
          {
            # Root the extra baked sources (see `extraBakedSources` above) into
            # the system closure so `includeNixDB` registers them as valid and
            # an in-guest `nix run index#...` finds rust-overlay already present
            # instead of unpacking it through VCFS (measured 2m44.8s -> ~2.6s).
            system.extraDependencies = extraBakedSources;
          }
        ]
        ++ lib.optional (self != null) {
          # Same treatment for the `index` flake itself, so an in-guest
          # `nix run index#<pkg>` (and any flake declaring `index` as an input
          # at this locked rev) resolves against the source baked in the image
          # instead of fetching it from GitHub. The base image's nix store DB
          # (`includeNixDB`, oci-layer.nix) registers this `-source` path as
          # valid — nix then treats the locked, narHash-matched reference as
          # already present and never re-fetches or re-ingests it, the same
          # property nixpkgs got in ix#6043/#1748/#1749/#1815.
          #
          # Whichever path the pin ends up naming carries string context, so it
          # roots into the image closure once (no duplicate copy, the #1748
          # trap). Only this flake scope sees `self`, so it is plumbed down
          # from `flake.nix`. Which path that is, and whether the pin keeps
          # `narHash`, depends on the shape `self` arrives in: its own
          # `-source` store path when index is consumed as its own flake, a
          # copy of `sourceRoot` when index is a subdirectory of ix and
          # `self.outPath` is therefore a subpath nix cannot short-circuit on
          # (ix#9290). ./registry-pin.nix has the full account; the
          # `image-registry-pin` check holds all three shapes.
          nix.registry.index.to = registryPin {
            inherit self;
            sourceRoot = paths.root;
          };
        }
        ++ [
          ./platform.nix
          # Keeps the system bus off the `Requires=` end of a mount unit,
          # so a switch that removes one does not stop the bus it is
          # issuing its own jobs over (ENG-11080). Its own file because
          # the VM test that proves it cannot import platform.nix.
          (paths.modules + "/system/dbus-survives-mount-removal")
          # Restores /tmp to 1777 on every activation. The image root has it
          # 0555 and ix's injected PID 1 mounts a tmpfs over it, so the mode
          # is only exposed when that mount is retired -- which is the switch
          # the module above made survivable (ENG-11080).
          (paths.modules + "/system/tmp-stays-writable")
          ./oci-layer.nix
          ./cas-layer.nix
          # Home Manager as a NixOS module. Per-tool XDG config (Nushell,
          # atuin, zoxide, ...) is configured under
          # `home-manager.users.root` in the base profile; this module
          # exposes the option set and shares the system pkgs.
          home-manager.nixosModules.home-manager
          {
            home-manager = {
              useGlobalPkgs = true;
              useUserPackages = true;
              # Activation renames existing user files with this extension
              # instead of failing, so an operator who hand-edited a config
              # sees the conflict rather than losing the file.
              backupFileExtension = "hm-backup";
              # Every guest user gets `mutable.files`: a read-only store mount
              # makes plain `home.file` the wrong deployment for anything an
              # in-guest tool edits. See `mutableFilesHomeModule` above.
              sharedModules = [mutableFilesHomeModule];
            };
          }
        ]
        ++ moduleList
        ++ modules;
    }).config;

  /**
  Build one self-contained OCI archive from a list of NixOS modules.

  Each image is independent: ix does not stack images at runtime, it
  runs one. Returns the OCI-archive derivation, which nothing boots from
  since ix retired the OCI ingest pipeline (ENG-6044 phase 7, ix#6930):
  the bootable artifact is the CAS manifest `ix.build.casImage` builds
  from `passthru.toplevel`, and `ix image push-manifest` publishes that.
  Use this as a `packages.<system>.<name>` output.
  */
  mkImage = args: (evalImageConfig args).ix.build.ociImage;

  # Shared bootstrap OCI reference used to materialize missing fleet nodes.
  # The archive is built and published outside the default flake checks.
  bootstrapImage = {
    name = "ix/test-cluster-bootstrap";
    tag = "zstd-tools-2026-05-12";
  };

  /**
  Build a fleet plan helper for a given host system. Returns a function
  that takes a fleet spec and produces the plan/commands tooling consumes.
  `mkFleet` is the default-system shortcut.
  */
  mkFleetFor = hostSystem: let
    hostPkgs = nixpkgs.legacyPackages."${hostSystem}";
  in
    import ./fleet.nix {
      inherit
        lib
        evalImageConfig
        writeNushellApplication
        bootstrapImage
        ;
      pkgs = hostPkgs;
      ixFleet = (packageSetFor hostPkgs).ix-fleet;
    };

  mkFleet = mkFleetFor system;

  /**
  Build one VM from a list of NixOS modules (ix#8306: the product surface
  is a single VM, so this is the seam `default.ix` configs call). A thin
  one-node wrapper over the same evaluator `mkFleet` uses -- the result
  keeps the exact shape tooling already consumes (`nixosConfigurations.<name>`,
  `planValue`, the lifecycle wrappers), so a flake inherits
  `nixosConfigurations` from it and a bare `ix apply` converges those nodes.

  The result is NOT a NixOS configuration and must not be bound to a flake's
  `ix.default`: that output is the single-VM seam `ix init` scaffolds, and
  `ix apply` builds `ix.default.config.system.build.toplevel` from it. A fleet
  result has no `config`, so the binding fails the apply on a missing
  attribute. Bind `(mkVm { ... }).nixosConfigurations.<name>` if you want the
  single-VM path; `tests/ix-default-is-a-vm.nix` refuses the other spelling.

  Arguments:
  - `modules`: list of NixOS modules defining the VM.
  - `name`: the `nixosConfigurations` key and the VM's default
    hostname/image name.
  - `deployment`: per-VM deployment options, the same keys the fleet
    evaluator checks (`region`, `ipv4`, `secrets`, `recreateOnUp`, ...).
  - `nodes`: peer VMs evaluated by their own `mkVm` calls. Pass the
    peers' `nixosConfigurations` and this VM's modules see them (plus
    the VM itself) as the `nodes` module argument, so cross-VM
    references like `ix.endpointOf nodes.<peer> "<listener>"` keep
    working without any fleet grouping.
  */
  mkVmFor = hostSystem: {
    modules,
    name ? "default",
    deployment ? {},
    nodes ? {},
  }:
    mkFleetFor hostSystem {
      peers = nodes;
      nodes.${name} = {inherit modules deployment;};
    };

  mkVm = mkVmFor system;

  # Dev-fleet layer over `mkFleet` (RFC 0007): consumes the forkable `ix.nix`
  # spec. Curried like `mkFleetFor` so example/flake eval can target a host
  # system.
  inherit
    (import ./dev.nix {
      inherit
        lib
        paths
        mkFleetFor
        evalImageConfig
        ;
    })
    mkDevFor
    ;
  mkDev = mkDevFor system;
in {
  inherit
    evalImageConfig
    mkImage
    bootstrapImage
    mkFleetFor
    mkFleet
    mkVmFor
    mkVm
    mkDevFor
    mkDev
    ;
}
