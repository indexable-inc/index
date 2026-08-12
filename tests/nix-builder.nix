{
  lib,
  nixpkgs,
  paths,
  pkgs,
}: let
  microvmStub = {
    config,
    lib,
    ...
  }: {
    options.microvm = {
      storeOnDisk = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
      storeDiskType = lib.mkOption {
        type = lib.types.enum ["erofs" "squashfs"];
        default = "squashfs";
      };
      registerClosure = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
      writableStoreOverlay = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
      };
      volumes = lib.mkOption {
        type = lib.types.listOf (lib.types.submodule {
          options = {
            autoCreate = lib.mkOption {
              type = lib.types.bool;
              default = true;
            };
            image = lib.mkOption {type = lib.types.str;};
            mountPoint = lib.mkOption {type = lib.types.str;};
            fsType = lib.mkOption {
              type = lib.types.str;
              default = "ext4";
            };
            size = lib.mkOption {type = lib.types.ints.positive;};
          };
        });
        default = [];
      };
      shares = lib.mkOption {
        type = lib.types.listOf (lib.types.submodule {
          options = {
            source = lib.mkOption {type = lib.types.str;};
            mountPoint = lib.mkOption {type = lib.types.str;};
            tag = lib.mkOption {type = lib.types.str;};
          };
        });
        default = [];
      };
    };

    config = lib.mkIf config.microvm.storeOnDisk {
      fileSystems = {
        "/nix" = {
          device = "/dev/vdb";
          fsType = "xfs";
        };
        "/nix/store" = {
          device = "/dev/disk/by-label/nix-store";
          fsType = config.microvm.storeDiskType;
        };
      };
      boot.postBootCommands = lib.mkIf config.microvm.registerClosure "register-closure";
    };
  };

  evalWith = extraModule: storage:
    (nixpkgs.lib.nixosSystem {
      system = pkgs.stdenv.hostPlatform.system;
      modules = [
        microvmStub
        (paths.modules + "/services/nix-builder")
        {
          services.nix-builder = {
            enable = true;
            storage =
              storage
              // {
                image = "cache.img";
                size = 4096;
              };
          };
          microvm.volumes = [
            {
              image = "home.img";
              mountPoint = "/home";
              size = 1024;
            }
          ];
          system.stateVersion = "26.05";
        }
        extraModule
      ];
    }).config;
  eval = evalWith {};

  config = eval {
    minFree = 1024;
    maxFree = 2048;
  };
  invalidPressure = eval {
    minFree = 4096;
    maxFree = 2048;
  };
  invalidShare =
    evalWith {
      microvm.shares = [
        {
          source = "/var/lib/shared-nix";
          mountPoint = "/nix";
          tag = "nix";
        }
      ];
    } {
      minFree = 1024;
      maxFree = 2048;
    };
  missingMicrovm =
    (nixpkgs.lib.nixosSystem {
      system = pkgs.stdenv.hostPlatform.system;
      modules = [
        (paths.modules + "/services/nix-builder")
        {
          services.nix-builder.enable = true;
          system.stateVersion = "26.05";
        }
      ];
    }).config;
  seedService = config.boot.initrd.systemd.services.seed-nix-store;
  inherit (seedService) script;
  assertions = [
    {
      assertion = map (volume: volume.mountPoint) config.microvm.volumes == ["/nix" "/home"];
      message = "the builder store must retain the first data-volume slot";
    }
    {
      assertion =
        builtins.head config.microvm.volumes
        == {
          autoCreate = true;
          fsType = "xfs";
          image = "cache.img";
          mountPoint = "/nix";
          size = 4096;
        };
      message = "the module must declare the configured auto-created XFS volume";
    }
    {
      assertion = config.microvm.storeOnDisk && config.microvm.storeDiskType == "erofs";
      message = "the immutable microvm store disk must use EROFS";
    }
    {
      assertion = config.microvm.registerClosure && config.microvm.writableStoreOverlay == null;
      message = "the persistent store must register the boot closure without an overlay";
    }
    {
      assertion = config.fileSystems."/nix".autoFormat;
      message = "the persistent XFS volume must format on first boot";
    }
    {
      assertion = config.fileSystems."/nix/.boot-store".device == "/dev/disk/by-label/nix-store";
      message = "the immutable boot store must mount beside the native store";
    }
    {
      assertion = !builtins.hasAttr "/nix/store" config.fileSystems;
      message = "the EROFS store mount must stay disabled so /nix/store is a native XFS directory";
    }
    {
      assertion = lib.all (needle: lib.hasInfix needle script) [
        "regInfo="
        ".store-seed-staging"
        "sync -f"
        "mv -T"
        ".store-seed-old"
        ".store-seed"
      ];
      message = "the initrd seed must stage, sync, replace, and generation-mark the boot closure";
    }
    {
      assertion =
        seedService.requiredBy
        == [
          "initrd-find-nixos-closure.service"
          "initrd.target"
        ]
        && seedService.before
        == [
          "initrd-find-nixos-closure.service"
          "initrd-switch-root.target"
        ]
        && seedService.unitConfig.RequiresMountsFor
        == [
          "/sysroot/nix"
          "/sysroot/nix/.boot-store"
        ];
      message = "the seed must run after both filesystems mount and before closure discovery and switch-root";
    }
    {
      assertion =
        config.nix.settings.build-dir
        == "/nix/nix-build"
        && config.nix.settings.max-free == 2048
        && config.nix.settings.min-free == 1024
        && config.nix.settings.sync-before-registering;
      message = "the Nix daemon must build on XFS with pressure GC and crash-safe registration";
    }
    {
      assertion = !config.nix.gc.automatic;
      message = "periodic GC must stay disabled for the persistent builder cache";
    }
    {
      assertion =
        lib.elem "post-boot.service" config.systemd.services.nix-daemon.after
        && lib.elem "post-boot.service" config.systemd.services.nix-daemon.requires;
      message = "the Nix daemon must wait for closure registration";
    }
    {
      assertion = lib.any (item: !item.assertion) invalidPressure.assertions;
      message = "invalid pressure GC thresholds must fail a NixOS assertion";
    }
    {
      assertion =
        lib.any (
          item: !item.assertion && lib.hasInfix "shares mounted at /nix" item.message
        )
        invalidShare.assertions;
      message = "a microvm share mounted at /nix must fail a NixOS assertion";
    }
    {
      assertion =
        lib.any (
          item: !item.assertion && lib.hasInfix "requires the microvm.nix" item.message
        )
        missingMicrovm.assertions;
      message = "enabling the builder without microvm.nix must produce a clear assertion";
    }
  ];
  failures = map (item: item.message) (lib.filter (item: !item.assertion) assertions);
in
  assert lib.assertMsg (failures == []) (
    "ix-test-nix-builder:\n  " + lib.concatStringsSep "\n  " failures
  );
    pkgs.runCommand "ix-test-nix-builder" {__structuredAttrs = true;} ''
      mkdir -p "$out"
    ''
