# Platform policy for the ix default runner template: substrate facts and
# GitHub-hosted-image parity only. No language toolchains: jobs install via
# rustup/mise/setup-* through nix-ld, and the install rides the lineage's
# seed snapshot HOME, so it is paid once per rev roll, not per job.
{
  lib,
  pkgs,
  ...
}: {
  system.stateVersion = "25.05";

  # Substitute through ix's public pull-through cache. Must live in the
  # image: module.nix pins trusted-users to root, so job code cannot add a
  # substituter at runtime.
  nix.settings = {
    substituters = [
      "https://cache.ix.dev"
      "https://cache.nixos.org/"
    ];
    trusted-public-keys = [
      "ix-workspace:JuAaeOPfR3GL3nUICpEz/88/+S3BzGF3L6bPYFy0GwI="
      "hil-stor-2:UYyDQcJ/iepiePK/ptHRqR2t98okIpsfOVqE0Pm5CwY="
      "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
    ];
    # Reactive headroom on top of the module's weekly GC: seed lineages
    # carry the store across fork generations.
    min-free = 50 * 1024 * 1024 * 1024;
    max-free = 100 * 1024 * 1024 * 1024;
    # Upstream default caches a MISSING narinfo for 3600s, snapshot-carried
    # into every fork; 60s is the ix fleet pin.
    narinfo-cache-negative-ttl = 60;
  };

  # Prefer IPv4: some regions' guests hold global v6 whose upstream gateway
  # does not answer NDP, so AAAA-bearing destinations die EHOSTUNREACH
  # before client fallback. Remove once v6 delivery lands (ENG-10881 class).
  environment.etc."gai.conf".text = ''
    precedence ::ffff:0:0/96 100
  '';

  services.ix-runner = {
    # Ubuntu build-essential parity: what GitHub's hosted images preinstall
    # and no language installer provides on a NixOS guest.
    extraPackages = [
      pkgs.gcc
      pkgs.gnumake
      pkgs.cmake
      pkgs.pkg-config
      pkgs.python3
      pkgs.perl # Ubuntu ships it; autotools/openssl-sys configure scripts run it
      pkgs.openssl
      pkgs.git-lfs
      pkgs.glibc.bin # mise-action probes `ldd` to pick its binary
    ];

    # Parallelism honesty on 64-vCPU elastic guests: tools sizing off the
    # ceiling oversubscribe the host / OOM the pre-inflation guest. Workflow
    # env overrides unit env, so per-repo tuning always wins. mkDefault so a
    # pool layering this file can override in nix too.
    jobEnvironment = lib.mapAttrs (_: lib.mkDefault) {
      CARGO_BUILD_JOBS = "16";
      NEXTEST_TEST_THREADS = "16";
      RUST_TEST_THREADS = "16";
      VITEST_MAX_WORKERS = "4";
      MISE_NODE_COMPILE = "0"; # mise would source-build on NixOS; prebuilts run via nix-ld
      MISE_PYTHON_COMPILE = "0";
    };
  };
}
