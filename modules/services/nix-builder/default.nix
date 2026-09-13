{
  config,
  lib,
  options,
  ...
}: let
  inherit
    (lib)
    mkEnableOption
    mkIf
    mkOption
    types
    ;
  cfg = config.services.nix-builder;
  hasMicrovm = options ? microvm;
  nixRoot = "/nix";
  bootStore = "${nixRoot}/.boot-store";
  physicalStore = "${nixRoot}/store";
in {
  options.services.nix-builder = {
    enable = mkEnableOption "a persistent native Nix store for a microvm.nix builder";

    storage = {
      image = mkOption {
        type = types.str;
        default = "builder-store.img";
        description = ''
          Host path of the sparse disk image attached as the persistent builder
          store. Relative paths are resolved by the microvm.nix runner.
        '';
      };

      size = mkOption {
        type = types.ints.positive;
        default = 256 * 1024;
        description = "Persistent builder store image size in MiB.";
      };

      minFree = mkOption {
        type = types.ints.positive;
        default = 30 * 1024 * 1024 * 1024;
        description = "Free bytes below which the Nix daemon starts garbage collection.";
      };

      maxFree = mkOption {
        type = types.ints.positive;
        default = 60 * 1024 * 1024 * 1024;
        description = "Free bytes the Nix daemon targets during pressure garbage collection.";
      };
    };
  };

  config = lib.mkMerge [
    (mkIf cfg.enable {
      assertions = [
        {
          assertion = hasMicrovm;
          message = "services.nix-builder requires the microvm.nix NixOS module";
        }
        {
          assertion = cfg.storage.minFree < cfg.storage.maxFree;
          message = "services.nix-builder.storage.minFree must be less than maxFree";
        }
      ];
    })
    (lib.optionalAttrs hasMicrovm (mkIf cfg.enable {
      assertions = [
        {
          assertion =
            builtins.length (builtins.filter (volume: volume.mountPoint == nixRoot) config.microvm.volumes)
            == 1;
          message = "services.nix-builder must be the only microvm volume mounted at /nix";
        }
        {
          # microvm.nix folds share mounts over volume mounts, so a /nix share
          # would silently replace the persistent XFS store.
          assertion =
            builtins.all (share: share.mountPoint != nixRoot && share.mountPoint != physicalStore)
            (config.microvm.shares or []);
          message = "services.nix-builder does not allow microvm shares mounted at /nix or /nix/store";
        }
      ];

      microvm = {
        storeOnDisk = true;
        storeDiskType = "erofs";
        registerClosure = true;
        writableStoreOverlay = null;

        # Drive letters are positional in vfkit. Owning the first data-volume
        # slot keeps the persistent store bound to the same disk as consumers add
        # unrelated volumes.
        volumes = lib.mkBefore [
          {
            # The runner creates a sparse image on first start; the guest
            # formats it via fileSystems."/nix".autoFormat below.
            autoCreate = true;
            image = cfg.storage.image;
            mountPoint = nixRoot;
            fsType = "xfs";
            size = cfg.storage.size;
          }
        ];
      };

      fileSystems = {
        "${nixRoot}" = {
          autoFormat = true;
          neededForBoot = true;
        };
        "${bootStore}" = {
          device = "/dev/disk/by-label/nix-store";
          fsType = "erofs";
          neededForBoot = true;
          noCheck = true;
          options = ["ro"];
        };
        # microvm.nix unconditionally declares its EROFS disk at /nix/store. The
        # real store must remain an ordinary directory on XFS because a nested
        # bind mount disappears inside Nix build chroots after a VM restart.
        # astlog-ignore: no-mkforce
        "${nixRoot}/store".enable = lib.mkForce false;
      };

      boot.initrd.systemd = {
        enable = true;
        services.seed-nix-store = {
          description = "Seed the persistent Nix store from the immutable boot store";
          # nixos-init boots never create initrd-find-nixos-closure.service, so
          # the seed is also pulled in via initrd.target and ordered before
          # switch-root, which both boot flavours pass through.
          requiredBy = [
            "initrd-find-nixos-closure.service"
            "initrd.target"
          ];
          before = [
            "initrd-find-nixos-closure.service"
            "initrd-switch-root.target"
          ];
          unitConfig.RequiresMountsFor = [
            "/sysroot${nixRoot}"
            "/sysroot${bootStore}"
          ];
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
          };
          script = ''
            store=/sysroot${physicalStore}
            staging=/sysroot${nixRoot}/.store-seed-staging
            old=/sysroot${nixRoot}/.store-seed-old
            rm -rf "$staging" "$old"
            install -d -m 1775 "$store"
            install -d -m 0755 /sysroot${nixRoot}/var/nix/daemon-socket

            seedId=
            for parameter in $(</proc/cmdline); do
              case "$parameter" in
                regInfo=*) seedId="''${parameter#regInfo=}"; seedId="''${seedId%/registration}"; seedId="''${seedId##*/}" ;;
              esac
            done
            if [[ -z "$seedId" ]]; then
              echo "seed-nix-store: regInfo is missing from the kernel command line" >&2
              exit 1
            fi

            seedMarker=/sysroot${nixRoot}/.store-seed
            if [[ -e "$seedMarker" ]]; then
              markerId=$(<"$seedMarker")
              if [[ "$markerId" == "$seedId" ]]; then
                exit 0
              fi
            fi

            install -d -m 0755 "$staging" "$old"
            for source in /sysroot${bootStore}/*; do
              cp -a "$source" "$staging/''${source##*/}"
            done
            sync -f /sysroot${nixRoot}

            for staged in "$staging"/*; do
              name="''${staged##*/}"
              target="$store/$name"
              if [[ -e "$target" || -L "$target" ]]; then
                mv -T "$target" "$old/$name"
              fi
              mv -T "$staged" "$target"
            done
            rm -rf "$staging" "$old"
            sync -f /sysroot${nixRoot}

            printf '%s\n' "$seedId" >"$seedMarker.new"
            mv -T "$seedMarker.new" "$seedMarker"
            sync -f /sysroot${nixRoot}
          '';
        };
      };

      nix = {
        enable = true;
        gc.automatic = false;
        settings = {
          build-dir = "${nixRoot}/nix-build";
          min-free = cfg.storage.minFree;
          max-free = cfg.storage.maxFree;
          # The host runner may be terminated without a guest shutdown. Persist
          # every path before the database can report it valid across that crash.
          sync-before-registering = true;
        };
      };

      systemd = {
        services.nix-daemon = {
          enable = true;
          after = ["post-boot.service"];
          requires = ["post-boot.service"];
        };
        sockets.nix-daemon.enable = true;
        tmpfiles.rules = [
          "d ${physicalStore} 1775 root nixbld -"
          "D ${nixRoot}/nix-build 0755 root root -"
        ];
      };
    }))
  ];
}
