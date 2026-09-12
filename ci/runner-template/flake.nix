# The ix platform default runner template, as its own subflake: the runner
# control plane builds it as github:indexable-inc/index/<rev>?dir=ci/runner-template#ci-runner
# on customer machines, so its input closure must not drag in the root
# flake's internal-visibility inputs.
#
# Lock-bump cadence: deliberate, not automated. GitHub deprecates runner
# versions on its own schedule, so the nixpkgs pin (which carries
# pkgs.github-runner) must be bumped before the pinned runner falls off
# GitHub's support window - and every bump lands as a new template rev,
# which re-seeds all platform-default pools (see README.md).
{
  description = "ix platform default CI runner template";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = {nixpkgs, ...}: let
    runner = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./module.nix
        ./platform.nix
        {services.ix-runner.enable = true;}
      ];
    };
    # Toolchain-baked variant for baml-class pools: the same mechanism and
    # platform policy plus baml.nix (rustup, sccache, go, node, ruby, the
    # musl cross gcc and the openssl env baked into the image instead of
    # arriving via the seed snapshot's HOME). Heavier closure, so it is a
    # separate attr, not a default-template change.
    bamlRunner = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./module.nix
        ./platform.nix
        ./baml.nix
        {services.ix-runner.enable = true;}
      ];
    };
    # Family variant for flox-class pools: the same mechanism and platform
    # policy plus flox.nix (cache.flox.dev substitution, nix-daemon
    # max-jobs/cores sizing, 8-thread parallelism pins, and a thin
    # pre-devshell userland). Almost nothing is baked - flox CI runs inside
    # `nix develop` - but the substituter set and pins are flox policy, so
    # it is a separate attr, not a default-template change.
    floxRunner = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./module.nix
        ./platform.nix
        ./flox.nix
        {services.ix-runner.enable = true;}
      ];
    };
  in {
    nixosConfigurations = {
      ci-runner = runner;
      baml = bamlRunner;
      flox = floxRunner;
    };

    # A plain ix template leaves root-device and bootloader facts to the
    # platform's injected machine profile; extend as a container only for
    # the check, so the closure realizes without inventing guest hardware.
    checks.x86_64-linux = let
      asContainer = configuration:
        (configuration.extendModules {
          modules = [{boot.isContainer = true;}];
        }).config.system.build.toplevel;
    in {
      ci-runner-template = asContainer runner;
      baml-template = asContainer bamlRunner;
      flox-template = asContainer floxRunner;
    };
  };
}
