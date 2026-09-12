{
  lib,
  nixComponents,
  # The jj client the functional suite drives (`requireJj` in
  # common/functions.sh fails rather than skips, and the fixtures use the ix
  # client's `init --repo` surface, which upstream jujutsu does not have and
  # whose ix-local stores upstream cannot open). Nothing in this tree can
  # build that client, and TODAY NOTHING SETS THIS ARGUMENT: the ix package
  # scope wires `packages/jj-ix` into the suite's nativeBuildInputs
  # (index/packages/nix/default.nix) but does not instantiate these VM tests
  # at all. When it adopts them it passes the client here (NixOS module
  # args / specialArgs); until then the jj-driving tests fail on this
  # runner, loudly and by design, in requireJj, whose NixOS message names
  # this argument.
  jjClient ? null,
  ...
}:

{
  # We rarely change the script in a way that benefits from type checking, so
  # we skip it to save time.
  skipTypeCheck = true;

  nodes.machine =
    { config, pkgs, ... }:
    {

      virtualisation.writableStore = true;
      system.extraDependencies = [
        config.nix.package.inputDerivation
      ];

      nix.settings.substituters = lib.mkForce [ ];

      environment.systemPackages =
        let
          run-test-suite = pkgs.writeShellApplication {
            name = "run-test-suite";
            runtimeInputs = [
              pkgs.meson
              pkgs.ninja
              pkgs.jq
              pkgs.git

              # Want to avoid `/run/current-system/sw/bin/bash` because we
              # want a store path. Likewise for coreutils.
              pkgs.bash
              pkgs.coreutils
            ]
            ++ lib.optional (jjClient != null) jjClient;
            text = ''
              set -x

              cat /proc/sys/fs/file-max
              ulimit -Hn
              ulimit -Sn

              cd ~

              cp -r ${nixComponents.nix-functional-tests.src} nix
              chmod -R +w nix

              chmod u+w nix/.version
              echo ${nixComponents.version} > nix/.version

              export isTestOnNixOS=1

              export NIX_REMOTE_=daemon
              export NIX_REMOTE=daemon

              export NIX_STORE=${builtins.storeDir}

              meson setup nix/tests/functional build
              cd build
              meson test -j1 --print-errorlogs
            '';
          };
        in
        [
          run-test-suite
          pkgs.git
        ];
    };
}
