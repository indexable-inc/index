# Vendored from github.com/indexable-inc/ix-runners @ 768d545 (module.nix).
# The first-class ix CI control plane (crates/ci/runners) writes the JIT
# credential this module's path unit watches; /var/lib/ix-runner/jitconfig
# is a cross-repo contract.
#
# One GitHub Actions runner, one job, one ix VM.
#
# MECHANISM ONLY, maintained by ix. Repo policy - toolchain packages, job
# env - lives in the customer's importing config. The ix-platform
# workarounds below each carry a short why and an issue link on this
# repository; workaround code is deleted centrally as platform fixes land,
# and customers pick that up as a rev bump of their flake input.
#
# Design: the machine exists for exactly one job. The reconcile (this
# repository's action) creates it - restored from its lineage's seed
# snapshot, or cold from this very configuration - and writes a single-job
# JIT runner credential to /var/lib/ix-runner/jitconfig. The path unit below
# is watching; the service consumes the file (a spent credential must never
# ride into a seed snapshot), runs the one job, and exits. The reconcile
# then deletes the machine, or snapshots it first when the job was a green
# default-branch run - which is how the NEXT machine of this lineage boots
# with this one's warm HOME already in place.
#
# There is deliberately no registration token, no slot, no repair path and
# no co-tenant isolation here: one job per machine makes the machine
# boundary the isolation, and anything unhealthy is deleted, not healed.
{
  config,
  lib,
  pkgs,
  ...
}: let
  inherit
    (lib)
    makeOverridable
    mkEnableOption
    mkIf
    mkOption
    types
    ;
  cfg = config.services.ix-runner;

  # Where the reconcile writes the credential, and the path unit watches.
  jitconfigPath = "/var/lib/ix-runner/jitconfig";

  # Everything FHS-assuming job steps expect on PATH. Jobs inherit the
  # runner UNIT's PATH - on image-booted template guests
  # /run/current-system/sw/bin is empty (ix platform, see https://github.com/indexable-inc/ix-runners/issues/1), so
  # systemPackages alone never reaches job steps.
  baseUserland = [
    pkgs.bashInteractive
    pkgs.coreutils
    pkgs.git
    pkgs.gnutar
    pkgs.gzip
    pkgs.findutils
    pkgs.gnugrep
    pkgs.gnused
    pkgs.gawk
    pkgs.which
    pkgs.file
    pkgs.curl
    pkgs.wget
    pkgs.jq
    pkgs.rsync
    pkgs.unzip
    pkgs.zip
    pkgs.zstd
    pkgs.xz
    pkgs.bzip2
  ];

  # nix-ld exports these through environment.sessionVariables, which rides
  # PAM and therefore never reaches a system unit; without them the loader
  # falls back to its compiled-in /run/current-system/sw path, the very tree
  # https://github.com/indexable-inc/ix-runners/issues/1 says is unreliable at unit start. Same values, resolved to the
  # store.
  nixLdEnvironment = {
    NIX_LD = "${config.system.path}/share/nix-ld/lib/ld.so";
    NIX_LD_LIBRARY_PATH = "${config.system.path}/share/nix-ld/lib";
  };

  # GitHub's runner hardcodes node20 as its internal runtime; nixpkgs ships
  # node24 only (node20 is EOL), which breaks every hashFiles() expression.
  # Serve node24 under the node20 name inside a copied package: a copy keeps
  # the binary-cache hit (an override would rebuild the runner from source),
  # and the base-path rewrite makes the copy the runner's root so it finds
  # the shim.
  runnerPackage = makeOverridable (
    {nodeRuntimes ? ["node24"]}: let
      base = pkgs.github-runner.override {inherit nodeRuntimes;};
    in
      pkgs.runCommand "github-runner-with-node20"
      {
        # A local copy of an already-substituted closure: shipping it through
        # a remote builder and a cache costs more than the `cp` it replaces.
        preferLocalBuild = true;
        allowSubstitutes = false;
      }
      ''
        cp -a ${base} $out
        chmod -R u+w $out
        # Every reference, not just $out/bin: one missed path sends the
        # runner back into ${base}, where lib/externals/node20 does not exist.
        grep -rlIZ -- "${base}" $out | xargs -0r sed -i "s|${base}|$out|g" \
          || { echo "rewriting ${base} references failed (nothing matched, or sed did)" >&2; exit 1; }
        if grep -rqI -- "${base}" $out; then
          echo "github-runner copy still references ${base}" >&2
          exit 1
        fi
        ln -sfn ${pkgs.nodejs_24} $out/lib/externals/node20
        # A nixpkgs layout change must fail the build here, not at hashFiles().
        test -x $out/lib/externals/node20/bin/node \
          || { echo "no node20 shim at $out/lib/externals/node20/bin/node" >&2; exit 1; }
      ''
  ) {};
in {
  options.services.ix-runner = {
    enable = mkEnableOption "the single-job GitHub Actions runner on this ix VM";

    extraPackages = mkOption {
      type = types.listOf types.package;
      default = [];
      description = "Toolchain packages on the job's PATH, on top of the base userland.";
    };

    jobEnvironment = mkOption {
      type = types.attrsOf types.str;
      default = {};
      example = {
        CARGO_INCREMENTAL = "0";
      };
      description = "Extra environment variables for the runner, and so for every job step.";
    };
  };

  config = mkIf cfg.enable {
    # The one tenant. A normal user with a persistent HOME: that HOME is the
    # warmth the seed snapshot carries - rustup, ~/.cargo registries, mise
    # toolchains, node_modules caches, playwright browsers.
    users.users.runner = {
      isNormalUser = true;
      home = "/home/runner";
      createHome = true;
      shell = pkgs.bashInteractive;
    };

    systemd = {
      tmpfiles.rules = [
        # Root-owned drop directory: the reconcile writes the credential here
        # over the platform's file API, and only the consume step below (root)
        # may move it. Job code must never be able to read a fresh one.
        "d /var/lib/ix-runner 0700 root root -"
      ];

      paths.ix-runner = {
        description = "watch for a single-job runner credential";
        wantedBy = ["multi-user.target"];
        pathConfig.PathExists = jitconfigPath;
      };

      services.ix-runner = {
        description = "GitHub Actions runner (single JIT job)";
        wants = ["network-online.target"];
        after = [
          "network.target"
          "network-online.target"
        ];

        environment =
          {
            HOME = "/home/runner";
            # Where the runner keeps _diag and its default _work folder; the
            # nixpkgs runner honors it, which is what lets the package run from
            # the read-only store.
            RUNNER_ROOT = "/var/lib/ix-runner-state";
          }
          // nixLdEnvironment
          // cfg.jobEnvironment;

        path = baseUserland ++ cfg.extraPackages ++ [config.nix.package];

        serviceConfig = {
          User = "runner";
          Group = "users";
          StateDirectory = "ix-runner-state";
          StateDirectoryMode = "0700";
          RuntimeDirectory = "ix-runner";
          WorkingDirectory = "/var/lib/ix-runner-state";

          # CONSUME the credential before anything runs: move it out of the
          # watched location so a spent blob can never ride into a seed
          # snapshot and trigger a ghost start on restore. Root ("+"): the
          # drop directory is deliberately unreadable to the runner user, and
          # the copy stays root-only, because systemd (not the runner) reads
          # it. Three commands, no shell: each must succeed for the next to
          # run, which is the `set -e` a script would have spelled. The sed
          # turns the blob into the one-line environment file ExecStart reads.
          ExecStartPre = [
            "+${pkgs.coreutils}/bin/install -m 0400 ${jitconfigPath} /run/ix-runner/jitconfig"
            "+${pkgs.gnused}/bin/sed -i 's/^/JITCONFIG=/' /run/ix-runner/jitconfig"
            "+${pkgs.coreutils}/bin/rm ${jitconfigPath}"
          ];
          # The dash is load-bearing: systemd loads environment files before
          # EVERY command of the unit, the first ExecStartPre included, and the
          # file does not exist until that command has run. Without it the
          # unit fails on "Failed to load environment files" before install
          # ever runs. By ExecStart the file exists, or install has already
          # failed the unit.
          EnvironmentFile = "-/run/ix-runner/jitconfig";
          # The blob rides argv, visible in-guest via /proc/PID/cmdline.
          # Accepted: the machine is single-tenant, and by the time job code
          # runs here the single-use credential is already spent.
          ExecStart = "${runnerPackage}/bin/Runner.Listener run --jitconfig \${JITCONFIG}";
          # One job, one life. The reconcile deletes the machine once the
          # registration disappears; nothing restarts here.
          Restart = "no";

          # The kernel OOM-killing one compiler process must not kill the
          # runner with it: the job must FAIL on GitHub, not hang as a zombie
          # the reconcile cannot distinguish from work.
          OOMPolicy = "continue";
          # Hosted-runner parity: jobs get 65536 there; systemd defaults 1024.
          LimitNOFILE = 1048576;
          UMask = "0022";
        };
      };

      # /tmp is a tmpfs sized off BOOT-time RAM (1.5G observed, 100% full
      # under real jobs) and closure tmpfs settings do not reach image boots -
      # a live remount does (ix platform, see https://github.com/indexable-inc/ix-runners/issues/2). 16G virtual is safe:
      # tmpfs pages only cost when used, and virtio-mem plugs RAM on demand.
      services.ix-runner-tmp-resize = {
        description = "resize the boot-path /tmp tmpfs";
        wantedBy = ["multi-user.target"];
        before = ["ix-runner.service"];
        unitConfig.ConditionPathIsMountPoint = "/tmp";
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${pkgs.util-linux}/bin/mount -o remount,size=16G /tmp";
        };
      };
    };

    # kernel.threads-max (and the RLIMIT_NPROC default derived from it) is
    # computed from BOOT-time RAM and never recomputed when virtio-mem grows
    # the guest; a cargo build seeing the elastic vCPU ceiling hits
    # pthread_create EAGAIN. Pin real numbers.
    boot.kernel.sysctl = {
      "kernel.threads-max" = 1048576;
      "kernel.pid_max" = 4194304;
    };

    # A job that outruns virtio-mem should swap and slow down, not hard-OOM.
    # Compressed swap also keeps resident (billed) RAM down.
    zramSwap.enable = true;

    # Foreign dynamically-linked FHS binaries (mise, rustup toolchains,
    # prebuilt node, cargo-binstall downloads, playwright browsers) run
    # unmodified via nix-ld + envfs.
    programs.nix-ld = {
      enable = true;
      # Deliberately GENEROUS. Every entry is one less loader failure for a
      # foreign FHS binary a job downloads at runtime - the reactive
      # add-a-lib-per-failure loop this replaces cost one fleet debugging
      # round per new binary. Baseline: the community nix-ld set
      # (wiki.nixos.org/wiki/Nix-ld) minus desktop one-offs, plus the dotnet
      # set (the Actions runner itself is dotnet), minus what the nixos
      # nix-ld module already concatenates in on its own. The cost is image
      # closure size only; the list dies when nixpkgs#354513 (nix-ld
      # resolves against the whole system closure) lands.
      libraries = [
        # Core toolchain and compression
        pkgs.libgcc
        pkgs.libxcrypt
        pkgs.libxcrypt-legacy
        pkgs.gmp
        pkgs.libelf
        # Crypto, TLS, network
        pkgs.expat
        pkgs.libgcrypt
        pkgs.libgpg-error
        pkgs.krb5
        # System plumbing
        pkgs.dbus
        pkgs.libcap
        pkgs.libusb1
        pkgs.fuse # AppImages
        pkgs.e2fsprogs
        pkgs.icu
        # dotnet-based tools (the Actions runner itself, omnisharp class)
        pkgs.lttng-ust
        pkgs.libsecret
        # X11
        pkgs.libx11
        pkgs.libxext
        pkgs.libxcomposite
        pkgs.libxdamage
        pkgs.libxfixes
        pkgs.libxrandr
        pkgs.libxcursor
        pkgs.libxi
        pkgs.libxinerama
        pkgs.libxrender
        pkgs.libxscrnsaver
        pkgs.libxtst
        pkgs.libxt
        pkgs.libxmu
        pkgs.libxft
        pkgs.libsm
        pkgs.libice
        pkgs.libxshmfence
        pkgs.libxxf86vm
        pkgs.libxcb
        pkgs.libxcb-util
        pkgs.libxcb-wm
        pkgs.libxcb-image
        pkgs.libxcb-keysyms
        pkgs.libxcb-render-util
        pkgs.libxcb-cursor
        pkgs.libxkbcommon
        # Graphics and rendering
        pkgs.libGL
        pkgs.libGLU
        pkgs.vulkan-loader
        pkgs.mesa
        # libgbm was split out of mesa in nixpkgs; chromium's headless shell
        # dlopens libgbm.so.1.
        pkgs.libgbm
        pkgs.libdrm
        pkgs.libva
        pkgs.libvdpau
        pkgs.pixman
        pkgs.libjpeg
        pkgs.libpng
        pkgs.libtiff
        pkgs.librsvg
        pkgs.fontconfig
        pkgs.freetype
        pkgs.harfbuzz
        pkgs.fribidi
        pkgs.gdk-pixbuf
        # GUI toolkits (electron, prebuilt GUI test tools)
        pkgs.glib
        pkgs.gtk3
        pkgs.pango
        pkgs.cairo
        pkgs.atk
        pkgs.at-spi2-atk
        pkgs.at-spi2-core
        pkgs.gsettings-desktop-schemas
        pkgs.libnotify
        # Playwright-downloaded chromium/firefox/webkit runtime set
        pkgs.nss
        pkgs.nspr
        pkgs.cups
        pkgs.alsa-lib
        # Audio and media
        pkgs.libpulseaudio
        pkgs.pipewire
        pkgs.flac
        pkgs.libvorbis
        pkgs.libogg
        pkgs.speex
        pkgs.libsamplerate
        pkgs.ffmpeg
      ];
    };
    services.envfs.enable = true;

    # For interactive `ix shell` debugging; job steps get their PATH from
    # the unit (see https://github.com/indexable-inc/ix-runners/issues/1), never from here.
    environment.systemPackages = baseUserland ++ cfg.extraPackages;

    nix.settings = {
      experimental-features = [
        "nix-command"
        "flakes"
      ];
      # trusted-users is root-equivalent (it can point the daemon at any
      # substituter or import any path). Job code has no business holding it
      # even on a single-job machine: the seed snapshot is taken from this
      # disk, and anything root-equivalent could poison what every later
      # fork of this lineage boots from.
      trusted-users = ["root"];
    };

    # The seed lineage inherits this disk across generations of forks, so
    # the store only ever grows between rev rolls; collect on the machines
    # that live long enough for it to fire.
    nix.gc = {
      automatic = true;
      dates = "weekly";
      options = "--delete-older-than 30d";
    };
  };
}
