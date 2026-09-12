# Build the raw EFI-bootable aarch64 NixOS disk for the panes seamless-windows
# guest (index#1686): a headless Wayland compositor (`panes-compositor`)
# exporting each toplevel over vsock, plus one systemd-nspawn container per app
# from ./apps.nix. Boot it with `vmkit boot-linux --disk <out> --gpu`.
#
# Assembled with systemd-repart (the image/repart module imported by
# ./nixos.nix), which runs in the build sandbox with no qemu/kvm VM, so it
# builds on any plain aarch64-linux builder, same as
# packages/chrome-vm-image. `path` is `pkgs.path` from the package autoArgs.
{
  lib,
  path,
  repoPackages,
  ix,
  # Writer for `passthru.updateScript`, bound only on the flake-package path
  # (lib/packages.nix); the overlay path leaves it null so `pkgs.*` carries no
  # updater. Same nullable-writer pattern as vector-bin.
  updateScriptWriter ? null,
  # Public ssh key authorized for root in the guest, enabling the ssh
  # switch-in-place loop (README, "Iterating on the guest"). Deliberately null
  # by default: the image is repo-built and cacheable, so any default key
  # would ship a static root credential to everyone. With null, sshd still
  # runs but nothing can log in; bake your own key via
  # `panes-guest-image.override { sshAuthorizedKey = ...; }`.
  sshAuthorizedKey ? null,
}: let
  nixos = import "${path}/nixos/lib/eval-config.nix" {
    system = "aarch64-linux";
    modules = [
      ./nixos.nix
      # Inject the repo-built compositor through the package set so the guest
      # module reads it as `pkgs.panes-compositor` (same pattern as
      # packages/vz-linux-guest); its meta lives in one place,
      # packages/panes/compositor. Minecraft's LWJGL natives ride the same
      # overlay because their pins (./pins.json, loaded through `ix`) live
      # outside apps.nix (see ./lwjgl-natives.nix).
      {
        nixpkgs.overlays = [
          (final: prev: {
            inherit (repoPackages) panes-compositor panes-audio;
            # ./pins.json carries only the lwjgl-* jar pins now (mesa is a
            # de-forked patch series, not a pin; see the `mesa` override below
            # and index#1894), but keep the prefix filter as a guard so an
            # unrelated future pin cannot leak into lwjgl-natives.nix, which
            # asserts one shared LWJGL version across its pins.
            lwjgl-natives-linux-arm64 = final.callPackage ./lwjgl-natives.nix {
              pins = lib.filterAttrs (name: _: lib.hasPrefix "lwjgl" name) (ix.pins.loadPins ./pins.json);
            };
            # The checked bash writer, threaded through the overlay because
            # `ix` is not in module scope; apps.nix uses it for the Minecraft
            # launch wrapper instead of the unchecked writeShellScript.
            writeBashApplication = ix.writeBashApplication final;
            # Venus (virtio-gpu Vulkan) with the driver-side external-semaphore
            # delta (index#1742): MoltenVK hosts never support SYNC_FD semaphore
            # import, so stock mesa masks VK_KHR_synchronization2 (clamping the
            # device to Vulkan 1.2) and VK_KHR_swapchain. Only `src` is swapped;
            # the patched tree is upstream tag mesa-26.1.2 plus the venus
            # commits, so nixpkgs' recipe and patches still apply.
            #
            # The Mesa view carries the Venus delta on its upstream anchor.
            # `hardware.graphics.enable` in ./nixos.nix consumes
            # `pkgs.mesa`, so this override is what /run/opengl-driver (and the
            # container ICDs) get.
            mesa = prev.mesa.overrideAttrs (old: {
              # The pinned base is upstream mesa-26.1.2, so the patched tree's
              # version equals nixpkgs mesa only when both track 26.1.2. A
              # nixpkgs mesa bump ahead of the pin can also stop the nixpkgs
              # patch set applying against the pinned tree; the assert catches
              # the version skew, the build failure catches the patch skew.
              src = assert lib.assertMsg (old.version == "26.1.2")
              "panes-guest-image: the Mesa view is 26.1.2 but nixpkgs mesa is ${old.version}; update the view to the matching tag and boot-validate the panes guest on a linux GPU host";
                ix.mesaSrc;
            });
          })
        ];
        # The builder-chosen root login key (see the sshAuthorizedKey package
        # arg above); an empty list leaves sshd running with no way in.
        users.users.root.openssh.authorizedKeys.keys =
          lib.optional (
            sshAuthorizedKey != null
          )
          sshAuthorizedKey;
      }
    ];
  };
  # Mechanically re-pins the ./pins.json jar hashes from their URLs
  # (`nix run .#update`); bumping the LWJGL version is the human edit.
  updateScript = ix.pins.mkOptionalUpdater {
    writeNushellApplication = updateScriptWriter;
    nix = ix.nixPackageFor "packages/panes/guest-image: updateScript";
    pname = "panes-guest-image";
    relPath = "packages/panes/guest-image/pins.json";
  };
in
  # Expose the raw disk directly as the package output (the repart module
  # produces it at `${system.build.image}/${image.filePath}`).
  nixos.pkgs.runCommand "panes-guest.raw"
  {
    __structuredAttrs = true;
    passthru =
      {
        # The system closure alone, for the ssh switch-in-place loop (README,
        # "Iterating on the guest"): build
        # `.#packages.aarch64-linux.panes-guest-image.toplevel`, `nix copy` it
        # into the running guest, activate with its switch-to-configuration.
        # Skips the disk assembly entirely.
        toplevel = nixos.config.system.build.toplevel;
      }
      // lib.optionalAttrs (updateScript != null) {inherit updateScript;};
  }
  ''
    cp --sparse=always "${nixos.config.system.build.image}/${nixos.config.image.filePath}" "$out"
  ''
