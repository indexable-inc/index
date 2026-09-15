# Base runtime profile.
#
# Auto-enabled by `lib/image/oci-layer.nix`. The bar: the first ten minutes of
# a person or agent in a fresh VM must just work. That means the consensus
# baseline every competitor's default sandbox ships (VCS, downloaders,
# archivers, a Python and Node runtime, a C toolchain) plus the cross-cutting
# debugging and introspection CLI. Image-specific runtime dependencies still
# belong in the image or service that needs them; sealed appliances disable
# the profile to drop the interactive stack.
{
  config,
  ix,
  lib,
  pkgs,
  ...
}: let
  cfg = config.ix.profiles.base;

  # The guest Nix, refused rather than defaulted when the injection is absent.
  # Same shape and same reason as `fromIx` in packages/nix/default.nix, which
  # is where the throw this one replaces used to surface from: a null here is
  # a consumer that could not assemble the fork, and quietly substituting
  # nixpkgs' Nix would trade this eval error for `attribute 'wasm' missing`
  # inside a booted VM.
  requireNixPackage = value:
    if value != null
    then value
    else
      throw ''
        modules/profiles/base: no guest Nix was injected (`ix.nixPackage`).

        Every image built through `index.lib` runs this profile, and its
        `nix.package` is the evaluator `ix apply` needs (`builtins.wasm` for
        `.ix` modules, the jj fetcher for jj-addressed inputs). Assembling it
        needs the jj tree ABI archive from the ix repository, outside index's
        source root, so it is supplied at the flake boundary and reaches
        `import ./lib` as `nixPackage`:

            inputs.index.withNixPackage { nixPackage = <the assembled nix-ix>; imageShellModule = <the template shell module>; }

        ix does that once (nix/flake/outputs/workspace.nix `index`) and exposes
        the instantiated surface as its `index` flake output; a flake that
        applies an image (the examples, `ix init` scaffolds) reads `ix.index`
        rather than the bare index flake, which has no guest Nix by construction.

        There is deliberately no fallback. An image shipping nixpkgs' Nix
        boots, accepts the generated project, and then fails inside the guest
        on `attribute 'wasm' missing`, which is the late failure this profile
        exists to prevent.
      '';

  # Claude Code refuses bypass-permissions mode for the root user unless it is
  # told it is sandboxed (`getuid() === 0 && IS_SANDBOX !== "1"` exits with
  # "cannot be used with root/sudo privileges"), and guest sessions run as
  # root, so the bare `pkgs.claude-code` wrapper (which bakes
  # `--dangerously-skip-permissions`) would refuse to start in every VM. The
  # guest VM is precisely the sandbox that guard asks about, so bake
  # IS_SANDBOX=1 into the binary. Kept byte-identical to the wrapper in
  # `lib/dev/agents.nix` (the managed-settings policy owner for dev images) so
  # an image importing both modules dedupes to one `bin/claude` instead of
  # colliding. Named with the upstream version so `lib.getName` stays
  # "claude-code" for the image nixpkgs unfree allowlist.
  claude-code =
    pkgs.runCommand "claude-code-${pkgs.claude-code.version}"
    {nativeBuildInputs = [pkgs.makeWrapper];}
    ''
      makeWrapper ${pkgs.claude-code}/bin/claude "$out/bin/claude" --set IS_SANDBOX 1
    '';

  # This profile's own Neovim Lua, shipped as an ordinary Neovim plugin.
  #
  # `programs.neovim.configure.customLuaRC` is one string, so the natural way
  # to write this is to read every source file into it -- and then Neovim
  # reports every error against the generated file. When nixpkgs picked up the
  # nvim-treesitter `main` rewrite (0.10, which deleted
  # `nvim-treesitter.configs`), every VM opened with `init.lua:64: module
  # 'nvim-treesitter.configs' not found`: line 64 of a store path, which is
  # line 3 of nvim/plugins/treesitter.lua. A plugin directory is how Neovim
  # keeps a file's own name and line numbers in a stack trace, so the config
  # goes out as one.
  #
  # `after/plugin/` and not `plugin/`: Neovim sources a start package's own
  # `plugin/` scripts before any after-directory, and each file here calls
  # `require('<plugin>').setup{}` on a plugin whose `plugin/` scripts must have
  # run first. Measured order is customLuaRC -> plugin/ -> after/plugin/.
  #
  # agent.lua installs as the module `ix.agent` rather than being inlined. It
  # already returns a module table, and the other consumer of the same file
  # (users/andrewgazelka, as `lua/agent/init.lua`) already requires it as one;
  # the `(function() ... end)()` wrapper it used to need existed only because a
  # single concatenated string had nowhere to put a module.
  nvimConfig =
    pkgs.runCommandLocal "ix-nvim-config" {}
    ''
      mkdir -p "$out/after/plugin"
      cp ${./nvim/plugins}/*.lua "$out/after/plugin/"
      install -Dm444 ${./nvim/agent.lua} "$out/lua/ix/agent.lua"
      # zz- so it sorts after every plugin's own setup(): agent.setup()
      # registers keymaps that which-key picks up.
      echo "require('ix.agent').setup()" > "$out/after/plugin/zz-ix-agent.lua"
    '';

  # flox comes from its own flake (`ix.floxPackage`, not nixpkgs -- flox is
  # not packaged there) and is a pure CLI: no daemon, no boot units, nothing
  # runs until a user types `flox`, so a tenant who never uses it pays only
  # image bytes. Its bundled Nix inherits the guest's /etc/nix/nix.conf, so
  # flox installs substitute through `cache.ix.dev` like everything else (the
  # flox cache is an ncps upstream on the ix side). Metrics are opt-in on ix:
  # `--set-default` (not `--set`) so FLOX_DISABLE_METRICS=false or
  # `flox config` can still opt back in per user.
  flox =
    pkgs.runCommand "flox-${ix.floxPackage.version}"
    {nativeBuildInputs = [pkgs.makeWrapper];}
    ''
      makeWrapper ${ix.floxPackage}/bin/flox "$out/bin/flox" --set-default FLOX_DISABLE_METRICS true
    '';
in {
  imports = [ix.imageShellModule];

  options.ix.profiles.base = {
    enable = lib.mkEnableOption "base runtime tools";

    shellWorkspace = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Pre-create a writable workspace directory and auto-cd login
          shells into it. Disable for sealed appliances where there is
          no interactive workflow to land in.
        '';
      };

      directory = lib.mkOption {
        type = lib.types.str;
        default = "/work/ix";
        description = "Workspace directory entered by login shells.";
      };
    };
  };

  config = lib.mkIf config.ix.profiles.base.enable {
    ix.interactiveShell.enable = lib.mkDefault true;
    # `cache.ix.dev` is the ix pull-through cache for guest Nix operations. Put
    # it in the auto-enabled base profile so every image gets the same warm
    # binary-cache path unless an image explicitly disables the profile.
    #
    # The matching `ix-workspace:` key has to be trusted alongside it. The
    # guests keep `require-sigs` on, so without the key the daemon rejects every
    # `cache.ix.dev` narinfo as unsigned ("ignoring substitute ... not signed by
    # any of the keys in 'trusted-public-keys'") and silently rebuilds the whole
    # system closure from source, which then dies on the guest's no-namespace
    # sandbox and fails `ix apply`. Both fields come from the one cache-identity
    # source of truth (lib/cache.nix), exposed here as `ix.cache`.
    nix = {
      # `ix init` emits fleets whose evaluator is `ix2nix-wasm`, so every
      # managed builder and workload VM must provide IX's patched Nix. Leaving
      # this at nixpkgs' default makes `ix apply` accept the generated project,
      # create a builder, and then fail late with `attribute 'wasm' missing`.
      #
      # Injected, not `ix.packages.nix-ix`: assembling that package needs the
      # jj tree ABI archive, whose crate lives in the ix repository outside
      # index's source root, so the assembled value has to reach this module
      # from whoever holds the archive. `ix.nixPackage` is that binding (one
      # formal on lib/default.nix, supplied at flake.nix). Reaching into
      # `ix.packages` from here instead would put the choice inside a NixOS
      # module, where no consumer of `index.lib` can override it.
      package = requireNixPackage ix.nixPackage;

      settings = {
        substituters = lib.mkBefore [ix.cache.url];
        # `ix.cache.flox.publicKey` rides along because the base profile ships
        # flox and the pull-through preserves flox-origin signatures on
        # `cache.flox.dev` paths it re-serves. ncps also appends its own host
        # key (already trusted), so this is belt-and-braces for paths that
        # arrive with only the flox signature (e.g. `nix copy` between
        # machines). cache.flox.dev itself is deliberately NOT a guest
        # substituter: the guest keeps exactly one cache URL and the edge
        # fans out.
        trusted-public-keys = lib.mkAfter [ix.cache.publicKey ix.cache.flox.publicKey];

        # Rollback-journal mode for `/nix/var/nix/db/db.sqlite`, against nix's
        # own default of WAL.
        #
        # The reason this was added no longer exists. It compensated for a
        # guest whose writable layer was a virtiofs+FUSE mount with no DAX:
        # SQLite's WAL needs an mmap'd `*-shm` that is a coherent shared window
        # plus strict WAL/main fsync ordering on checkpoint, that layer gave
        # neither, and the DB degraded to `database disk image is malformed`
        # (indexable-inc/ix#6259). ix#7192 then deleted that layer -- VCFS root
        # serving is channel-only and VCFS carries no `fuse`/`fuser` dependency
        # at all -- and ix#6259 was closed NOT_PLANNED on 2026-07-25 because the
        # thing it described was gone. A guest today boots xfs on a virtio-blk
        # device, where WAL is safe and faster:
        #
        #   $ ix shell <vm> -- sh -lc 'grep " / " /proc/self/mountinfo; grep -c virtiofs /proc/self/mountinfo'
        #   26 1 254:0 / / rw,noatime - xfs /dev/root rw,...
        #   0
        #
        # It stays for now only because `Rootfs::Virtiofs` is still a live
        # variant in ix (legacy per-VM `config.json` recovery, and golden-capture
        # topology), so "no guest root is ever FUSE-backed" is not something the
        # tree currently proves. Retiring this needs that established, or better,
        # needs the mode derived from the filesystem the DB actually sits on
        # rather than assumed fleet-wide here. ENG-10689.
        use-sqlite-wal = false;
      };
    };

    # Install terminfo for every common terminal emulator so ncurses tools
    # (`clear`, `tmux`, `vim`, ...) work no matter what `$TERM` an operator's
    # client propagates in. Without this, shelling in from Ghostty fails with
    # `'xterm-ghostty': unknown terminal type.` because the guest only ships
    # ncurses' built-in set. Pulls only the small `terminfo` outputs (ghostty,
    # kitty, alacritty, wezterm, foot, ...), not the terminal binaries.
    environment = {
      enableAllTerminfo = true;
      # Native CLI installers use the XDG-standard per-user binary directory.
      # Make those binaries available on the next login without asking every
      # image user to edit a shell-specific startup file.
      localBinInPath = true;
    };

    # Cubic halves cwnd on any loss, so a residential last-mile at
    # 30 ms and a couple percent loss caps a single TCP flow far
    # below the path's real capacity. BBR models bottleneck bandwidth
    # and RTT from delivery-rate measurements and is largely loss-
    # insensitive, which matches every workload here that accepts
    # inbound from arbitrary internet endpoints (Minecraft players,
    # Xpra browser clients, repo fetches via `git-clone`). fq is the
    # qdisc BBR was designed to pace with; BBR without fq leaves
    # bandwidth on the table.
    #
    # If `tcp_bbr` is not present in the running kernel, the sysctl
    # write is a no-op and Cubic stays in place. Per-socket buffer
    # caps (`rmem_max`, `wmem_max`, `tcp_{r,w}mem`) are deliberately
    # left at kernel defaults: a 64 MiB per-socket ceiling is real
    # memory cost on small VMs with many accepted sockets, and the
    # default 4 MiB cap fits the BDP of every workload shipped here.
    boot.kernel.sysctl = {
      "net.ipv4.tcp_congestion_control" = "bbr";
      "net.core.default_qdisc" = "fq";
    };

    # Per-tool config for root lives in Home Manager (used here as a
    # NixOS module per AGENTS.md). Nushell's config.nu ships as a real
    # `.nu` file next to this module; HM writes it to the right XDG path
    # under /root/.config/nushell/ and follow-up tool integrations
    # (atuin, zoxide, direnv, fzf) hang off the same root user attrset.
    #
    # The system shell module owns the prompt so it is present before
    # Home Manager activation, including for users other than root.
    home-manager.users.root = {config, ...}: let
      # Home Manager owns the attribute names of the files it generates, and
      # they are not all guessable: zsh keys its rc files off `dotDir` (so
      # `./.zshrc` while dotDir is the home directory), nushell keys its three
      # off `configDir` (an absolute path), and fish's lives under
      # `xdg.configFile`. Each entry maps a Home Manager key to the target
      # `mutable.files` deploys it to.
      nushellRcFiles = [
        "config.nu"
        "env.nu"
        "login.nu"
      ];

      homeFileRcs =
        {
          ".profile" = ".profile";
          ".bashrc" = ".bashrc";
          ".bash_profile" = ".bash_profile";
          "./.zshenv" = ".zshenv";
          "./.zshrc" = ".zshrc";
          "./.zprofile" = ".zprofile";
        }
        // lib.genAttrs' nushellRcFiles (
          name:
            lib.nameValuePair
            "${config.programs.nushell.configDir}/${name}"
            ".config/nushell/${name}"
        );

      configFileRcs = {
        "fish/config.fish" = ".config/fish/config.fish";
      };

      # The declared base is exactly what the store symlink used to point at:
      # a `text` definition still populates `source` through `mkDefault`
      # (views/home-manager/modules/lib/file-type.nix), and disabling the entry
      # suppresses only the link, not the content.
      mutableRc = source: {
        inherit source;
        # Shell rc files have no structured format. Pin it instead of letting
        # detection decide, because the detected format is frozen into
        # index-delta's state the first time the file is seeded.
        format = "text";
        # The whole point: a local edit survives activation, and a base change
        # under drift queues in `index-delta status` instead of clobbering it.
        persistence = "durable";
        declaredAt = "modules/profiles/base/default.nix";
      };

      # Every rc file root could plausibly end up with, not just the ones Home
      # Manager generates today, so enabling another shell module (or a
      # `loginExtra` that makes `.zlogin` appear) trips the assertion below
      # rather than quietly shipping one read-only dotfile again.
      rcTargets =
        [
          ".profile"
          ".bashrc"
          ".bash_profile"
          ".bash_logout"
          ".zshenv"
          ".zshrc"
          ".zprofile"
          ".zlogin"
          ".zlogout"
          ".config/fish/config.fish"
        ]
        ++ map (name: ".config/nushell/${name}") nushellRcFiles;

      storeSymlinkedRc =
        lib.filter
        (file: file.enable && lib.elem file.target rcTargets)
        (lib.attrValues config.home.file);
    in {
      home.stateVersion = "25.11";

      # ROOT'S SHELL RC FILES SHIP AS WRITABLE REAL FILES, NOT STORE SYMLINKS.
      #
      # Home Manager deploys what it manages as a symlink into /nix/store, and
      # a guest mounts the store read-only -- `findmnt -T /root/.profile` in a
      # fresh VM reports `/dev/vda[/nix/store] xfs ro,...`. Anything that
      # installs itself by appending to a shell rc therefore dies on first run.
      # rustup, verbatim:
      #
      #   could not amend shell profile: '/root/.profile': could not write
      #   rcfile file: '/root/.profile': Read-only file system (os error 30)
      #
      # That is the bar at the top of this file failing inside the first ten
      # minutes. `mutable.files` (modules/home/mutable-files.nix, wired into
      # every guest by lib/image/default.nix) deploys the same declared content
      # as a plain writable file that `index-delta` seeds and tracks, with
      # `durable` persistence so an in-guest edit is never clobbered by a later
      # activation. Do not "clean this up" back into plain `home.file`: the
      # symlink IS the defect.
      home.file = lib.genAttrs (lib.attrNames homeFileRcs) (_: {enable = false;});
      xdg.configFile = lib.genAttrs (lib.attrNames configFileRcs) (_: {enable = false;});

      mutable.files =
        lib.mapAttrs' (
          key: target:
            lib.nameValuePair target (mutableRc config.home.file.${key}.source)
        )
        homeFileRcs
        // lib.mapAttrs' (
          key: target:
            lib.nameValuePair target (mutableRc config.xdg.configFile.${key}.source)
        )
        configFileRcs;

      # Two-sided guard on the block above: the keys are Home Manager's, so an
      # upstream rename would leave our `enable = false` pointing at nothing
      # and quietly restore a read-only symlink under a name we no longer
      # convert. Assert on the deployed shape instead of on our own key list,
      # which we define and so cannot fail.
      assertions = [
        {
          assertion = storeSymlinkedRc == [];
          message = "ix.profiles.base: root's ${lib.concatMapStringsSep ", " (file: file.target) storeSymlinkedRc} would deploy as a read-only /nix/store symlink, which breaks every tool that self-installs into a shell rc (see the EROFS comment in modules/profiles/base/default.nix). Route it through `mutable.files` by adding its Home Manager key to `homeFileRcs` or `configFileRcs` there.";
        }
      ];

      # Workaround for upstream nixpkgs#485682: `make-options-doc` strips
      # string context from `options.json` via `unsafeDiscardStringContext`,
      # which Nix flags as "references store path without a proper context"
      # every time the derivation is constructed. Home Manager's
      # `manual.manpages.enable` (default true) is the only thing in our
      # base closure that pulls `nixosOptionsDoc`, so toggling it off
      # silences seven warnings per `nix run .#health-checks` (one per
      # fleet node) until the upstream fix lands. Per-package `man tar`
      # etc. are unaffected (NixOS `documentation.man.enable` is left
      # alone). Re-enable per-image when an operator actually wants
      # `man home-configuration.nix` from inside the VM, e.g. a dev box
      # that's editing the source checkout in place.
      manual.manpages.enable = false;
      programs = {
        nushell = {
          enable = true;
          configFile.source = ./config.nu;
          loginFile.source = ./login.nu;
          # env.nu is tiny machine-owned glue: one line that surfaces
          # the workspace path from the shellWorkspace option into the
          # Nushell session so login.nu can read it. Generating it
          # inline keeps the workspace path in one Nix source of truth.
          envFile.text = ''
            $env.IX_WORKDIR = "${cfg.shellWorkspace.directory}"
          '';
        };
        # Let Home Manager own root's bash/zsh/fish rc files. Without this
        # the `enable*Integration` flags below (atuin, zoxide, direnv, fzf)
        # are inert for these shells: Home Manager only writes
        # the init snippets into a shell's rc when that shell's module is
        # enabled, so an operator who `chsh`-ed into bash or fish would land
        # at a bare prompt with none of the wiring. The NixOS
        # `programs.zsh`/`programs.fish` modules further down handle
        # system-wide registration; these are the per-user HM counterparts.
        bash.enable = true;
        zsh = {
          enable = true;
          # The system rc owns completion and key bindings for every user.
          enableCompletion = false;
          # Match Nushell's login.nu behavior for the platform-default shell.
          # Keep non-login interactive shells in their caller's directory.
          initContent = lib.mkIf cfg.shellWorkspace.enable (lib.mkBefore ''
            if [[ -o login && -d ${lib.escapeShellArg cfg.shellWorkspace.directory} ]]; then
              builtin cd -- ${lib.escapeShellArg cfg.shellWorkspace.directory}
            fi
          '');
        };
        fish.enable = true;
        # fish 4.8.0 (current nixpkgs nixos-unstable) removed
        # share/fish/tools/create_manpage_completions.py, which Home Manager's
        # default `generateCompletions = true` invokes to derive completions
        # from man pages. Every `<pkg>-fish-completions` derivation then fails
        # ("python: can't open file …create_manpage_completions.py"), failing
        # the whole system closure and breaking `ix apply` for every fleet. The
        # shell rc/integration wiring above is what we want from fish here;
        # man-page completions are an orthogonal nicety.
        # https://github.com/indexable-inc/index/issues/1632
        fish.generateCompletions = false;
        # btop resource monitor, configured natively through the Home
        # Manager module (no opaque btop.conf to hand-maintain) so every
        # VM opens btop with the same tuned layout: 500 ms refresh, just
        # the proc+cpu boxes, process tree, vim keys, Fahrenheit temps.
        # `color_theme = "gotham"` is a bare name, which btop resolves
        # against its bundled themes dir ($out/share/btop/themes, found
        # relative to the binary) at startup, so no absolute store path is
        # baked in. btop is also in environment.systemPackages below so
        # non-root users get the binary; both reference the same
        # derivation, so there is no second copy in the store.
        btop = {
          enable = true;
          settings = {
            color_theme = "gotham";
            theme_background = false;
            truecolor = true;
            force_tty = false;
            disable_presets = "Off";
            presets = "cpu:1:default,proc:0:default cpu:0:default,mem:0:default,net:0:default cpu:0:block,net:0:tty";
            vim_keys = true;
            disable_mouse = false;
            rounded_corners = true;
            terminal_sync = true;
            graph_symbol = "block";
            graph_symbol_cpu = "default";
            graph_symbol_gpu = "default";
            graph_symbol_mem = "default";
            graph_symbol_net = "default";
            graph_symbol_proc = "default";
            shown_boxes = "proc cpu";
            update_ms = 500;
            proc_sorting = "cpu lazy";
            proc_reversed = false;
            proc_tree = true;
            proc_colors = true;
            proc_gradient = true;
            proc_per_core = true;
            proc_mem_bytes = true;
            proc_cpu_graphs = true;
            proc_info_smaps = false;
            proc_left = false;
            proc_filter_kernel = false;
            proc_follow_detailed = true;
            proc_aggregate = true;
            keep_dead_proc_usage = false;
            cpu_graph_upper = "total";
            cpu_graph_lower = "total";
            show_gpu_info = "Auto";
            cpu_invert_lower = true;
            cpu_single_graph = false;
            cpu_bottom = false;
            show_uptime = true;
            show_cpu_watts = true;
            check_temp = true;
            cpu_sensor = "Auto";
            show_coretemp = true;
            cpu_core_map = "";
            temp_scale = "fahrenheit";
            base_10_sizes = false;
            show_cpu_freq = true;
            clock_format = "%X";
            background_update = true;
            custom_cpu_name = "";
            disks_filter = "/";
            mem_graphs = true;
            mem_below_net = false;
            zfs_arc_cached = true;
            show_swap = true;
            swap_disk = true;
            show_disks = false;
            only_physical = true;
            use_fstab = true;
            zfs_hide_datasets = false;
            disk_free_priv = false;
            show_io_stat = true;
            io_mode = false;
            io_graph_combined = false;
            io_graph_speeds = "";
            swap_upload_download = false;
            net_download = 100;
            net_upload = 100;
            net_auto = false;
            net_sync = true;
            net_iface = "";
            base_10_bitrate = "Auto";
            show_battery = true;
            selected_battery = "Auto";
            show_battery_watts = true;
            log_level = "WARNING";
            save_config_on_exit = false;
            nvml_measure_pcie_speeds = true;
            rsmi_measure_pcie_speeds = true;
            gpu_mirror_graph = true;
            shown_gpus = "nvidia amd intel apple";
            custom_gpu_name0 = "";
            custom_gpu_name1 = "";
            custom_gpu_name2 = "";
            custom_gpu_name3 = "";
            custom_gpu_name4 = "";
            custom_gpu_name5 = "";
          };
        };
        # SQLite-backed, searchable shell history that follows the
        # operator across bash/zsh/fish/nushell. Local-only by default;
        # sync to an atuin server only when the operator chooses to.
        atuin = {
          enable = true;
          enableNushellIntegration = true;
          enableBashIntegration = true;
          enableZshIntegration = true;
          enableFishIntegration = true;
        };
        # Frecency-ranked directory jumper: `z minecraft` jumps to the
        # most-used directory matching that fragment. SSH dev sessions
        # bounce between /etc, /var/log, /work/ix, and service data dirs
        # constantly; full paths get old fast.
        zoxide = {
          enable = true;
          enableNushellIntegration = true;
          enableBashIntegration = true;
          enableZshIntegration = true;
          enableFishIntegration = true;
        };
        # Per-directory environment loading. nix-direnv caches nix-shell
        # evaluation so cd'ing into a repo with a shell.nix or flake.nix
        # gets its environment without re-evaluating Nix every time.
        direnv = {
          enable = true;
          nix-direnv.enable = true;
          enableNushellIntegration = true;
          enableBashIntegration = true;
          enableZshIntegration = true;
          enableFishIntegration = true;
        };
        # Fuzzy finder for files, processes, branches, and anything else the
        # operator pipes into fzf. Atuin owns Ctrl-R history search above, so
        # leave fzf's overlapping history widget disabled.
        fzf = {
          enable = true;
          enableBashIntegration = true;
          enableZshIntegration = true;
          enableFishIntegration = true;
          historyWidget.command = "";
        };
        # AST-aware merge driver. Parses 30+ languages (Nix, Rust,
        # Python, TS/JS, Go, Java, ...) and resolves structural conflicts
        # that the line-based three-way merge would otherwise mark. The
        # HM module wires the driver, sets `* merge=mergiraf` globally,
        # and forces conflictStyle = diff3 (required by the driver).
        # `enableGitIntegration` is set explicitly because home-manager
        # plans to flip the default in a future release.
        mergiraf = {
          enable = true;
          enableGitIntegration = true;
        };
        # delta replaces git's pager for diffs and the interactive
        # add/checkout selection screens. Side-by-side rendering and
        # syntax highlighting make review usable over an SSH session.
        # `enableGitIntegration` is set explicitly because home-manager
        # deprecated the automatic-on-when-git-enabled behavior.
        delta = {
          enable = true;
          enableGitIntegration = true;
          options = {
            navigate = true;
            side-by-side = true;
            features = "interactive";
          };
        };
        # Generic git defaults for any operator working in an ix VM.
        # Identity (user.name/email), commit signing, GPG/SSH agent
        # paths, and per-host credential helpers stay out of here: those
        # are personal and belong in the operator's own dotfiles, not
        # baked into every image.
        git = {
          enable = true;
          # Route SQLite database files to the sqlmerge driver instead of
          # mergiraf's global `* merge=mergiraf`. gitattributes resolves each
          # attribute by the LAST matching line, and the mergiraf HM module
          # contributes its wildcard line at default order, so pin these after
          # it with mkAfter or the wildcard would win for .db files too.
          attributes = lib.mkAfter [
            "*.db merge=sqlite"
            "*.sqlite merge=sqlite"
            "*.sqlite3 merge=sqlite"
          ];
          settings = {
            alias = {
              # Compact log: subject + short hash, one per line.
              lg = "log --pretty=format:'%s %C(dim)%h%C(reset)'";
              # Pull rebase then push, the recovery move after a rejected push.
              sync = "!git pull --rebase && git push";
              # Delete local branches whose remote-tracking branch is gone.
              cleanup = "!git fetch --prune && git branch -vv | grep \": gone]\" | grep -v \"\\\\*\" | awk \"{print \\$1}\" | xargs -r git branch -d";
            };
            init = {
              defaultBranch = "main";
              # Git 3.0 flips new repos to SHA-256 objects; adopt ahead of the
              # default flip. GitHub started hosting sha256 repos in mid-2026
              # (actions/checkout and gh already handle them). The object
              # format is per-repo and fixed at init, so existing SHA-1 clones
              # are untouched.
              defaultObjectFormat = "sha256";
              # reftable (also the Git 3.0 direction) replaces the loose-file
              # + packed-refs backend: atomic multi-ref transactions and no
              # D/F or case-sensitivity ref-name collisions, which matters on
              # VMs that script branch churn. Per-repo and fixed at init,
              # like the object format.
              defaultRefFormat = "reftable";
            };
            # Refuse to operate on a bare repository unless it is named
            # explicitly (--git-dir / GIT_DIR). A cloned repo can embed a
            # bare repo with a malicious config in a subdirectory; without
            # this, any git command that walks into it executes that config's
            # hooks and aliases (the attack safe.bareRepository was added
            # for). Agents cd through untrusted checkouts constantly, so the
            # hardening default is worth the rare explicit --git-dir.
            safe.bareRepository = "explicit";
            pull.rebase = true;
            push = {
              autoSetupRemote = true;
              default = "simple";
              followTags = true;
            };
            fetch = {
              prune = true;
              # NOT writeCommitGraph. It writes a *split* commit-graph, adding
              # one small increment per fetch on top of the existing chain, and
              # nothing here ever compacts that chain: `maintenance.auto` is
              # false below (deliberately, see its comment), and
              # `maintenance.repo` covers only registered repos. So the chain
              # grows one file per fetch without bound, and a chain that grows
              # without bound eventually breaks.
              #
              # Observed 2026-07-25 on ~/.config/nix at five chained graphs:
              # every `git pull --rebase` died with `fatal: invalid commit
              # position. commit-graph is likely corrupt`, while `git fsck
              # --connectivity-only` passed, so the object store was fine and
              # only the cache was bad. Deleting the chain did not stick,
              # because the next fetch immediately wrote a fresh increment.
              # `git commit-graph write --reachable --split=replace` collapses
              # it back to one verified file and is the manual repair.
              #
              # `gc.writeCommitGraph` below still keeps a graph current, and gc
              # rewrites it whole rather than appending, so the read speedup
              # survives without the unbounded chain.
              negotiationAlgorithm = "skipping";
              parallel = 0;
            };
            rebase = {
              autoSquash = true;
              autoStash = true;
              updateRefs = true;
            };
            rerere = {
              enabled = true;
              autoupdate = true;
            };
            merge = {
              # mergiraf's HM module sets conflictStyle = "diff3" globally
              # because mergiraf needs classic diff3 markers to parse the
              # three-way merge output; we set ff/renormalize alongside.
              ff = "only";
              renormalize = true;
              # Three-way merge for SQLite database files (repo crate
              # `packages/sqlmerge`, via the session extension). Without a
              # driver a .db file is binary to git and every concurrent edit
              # is a full-file conflict. The `attributes` lines above route
              # *.db/*.sqlite/*.sqlite3 here; mergiraf keeps everything else.
              sqlite = {
                name = "SQLite three-way merge (sqlmerge)";
                driver = "${lib.getExe pkgs.sqlmerge} %O %A %B";
              };
            };
            diff = {
              algorithm = "histogram";
              statNameWidth = 500;
              statGraphWidth = 500;
            };
            log = {
              date = "relative";
              decorate = "auto";
            };
            blame.coloring = "highlightRecent";
            column.worktree = "auto";
            branch.sort = "-committerdate";
            tag.sort = "-version:refname";
            status.aheadBehind = true;
            advice = {
              statusHints = false;
              addEmptyPathspec = false;
            };
            core = {
              commitGraph = true;
              multiPackIndex = true;
              untrackedcache = true;
              preloadindex = true;
            };
            gc = {
              writeCommitGraph = true;
              auto = 256;
            };
            index = {
              threads = 0;
              version = 4;
            };
            checkout.workers = 0;
            feature.manyFiles = true;
            submodule = {
              recurse = true;
              fetchJobs = 16;
            };
            # Never let ordinary git commands spawn background `git
            # maintenance run --auto`: each auto run detaches an uncapped
            # `git repack --cruft`, and at agent-fleet commit/fetch volume
            # those stack without bound -- 1,033 of them piled up in ~12
            # minutes and swapped a 128 GB workstation (ix#8161, #4001).
            # Repos that want maintenance get scheduled runs via
            # `git maintenance register` (maintenance.repo) instead.
            maintenance.auto = false;
          };
        };
      };
    };

    # Ship every common operator shell so an SSH session can chsh into
    # whatever the operator already knows. bash is implicit in NixOS;
    # zsh and fish get their NixOS modules so /etc/shells registration
    # and system-wide completion paths are wired without per-image
    # setup. Zsh is the platform default user shell (see
    # the shared interactive shell module); Home Manager owns its root-user config
    # and the login-time workspace behavior above.
    # Every command-not-found points at the escape hatch: with cache.ix.dev
    # warm, `nix run nixpkgs#<tool>` materializes most tools in about a
    # second, but nothing surfaces that to someone staring at a bare
    # "command not found". One hint line per shell; no auto-run (executing
    # an unreviewed store path on a typo is not a default anyone wants).
    programs = let
      hint = tool: "hint: 'nix run nixpkgs#${tool}' runs it without installing (warm cache, about a second); 'nix shell nixpkgs#${tool}' puts it on PATH.";
    in {
      zsh = {
        interactiveShellInit = ''
          command_not_found_handler() {
            print -u2 "zsh: command not found: $1"
            print -u2 "${hint "$1"}"
            return 127
          }
        '';
      };
      fish = {
        enable = true;
        interactiveShellInit = ''
          function fish_command_not_found
            echo "fish: Unknown command: $argv[1]" >&2
            echo "${hint "$argv[1]"}" >&2
          end
        '';
      };
      bash.interactiveShellInit = ''
        command_not_found_handle() {
          echo "bash: $1: command not found" >&2
          echo "${hint "$1"}" >&2
          return 127
        }
      '';

      # nixpkgs' channel-DB command-not-found defines these same three
      # handler functions when enabled; its default is only false here
      # because a flake-pinned nixpkgs tarball lacks programs.sqlite.
      # Pin it off so the handlers above stay the single owner instead
      # of colliding by module-include order.
      command-not-found.enable = false;

      # git for every user, not just root: the curated per-user config
      # (delta, mergiraf, aliases) stays in Home Manager above, but the
      # binary itself must not depend on whose profile is on PATH --
      # DynamicUser services and any future non-root user get git too.
      # The system gitconfig also carries a fallback identity so the
      # first `git commit` in a fresh VM never dies with "unable to
      # auto-detect email address"; system scope is git's lowest
      # precedence, so any real identity (global or repo-local) wins.
      git = {
        enable = true;
        config = {
          user = {
            name = "ix";
            email = "root@ix.local";
          };
        };
      };

      # Neovim is wired through the NixOS module because the wrapper bakes
      # the curated config and its plugins into the binary itself, so no
      # per-user `~/.config/nvim/` has to exist for the operator to land in
      # the configured experience. (The colorschemes are the exception, and
      # have to be: `programs.neovim.runtime` writes them to /etc/xdg/nvim,
      # which unlike a packdir entry is on the runtimepath while init.lua is
      # still running -- and init.lua is where `:colorscheme` is called.)
      # (HM's neovim module on
      # release-25.05 and release-25.11 extends nixpkgs' plugin
      # submodule with a `runtime` attr that the wrapped binary's
      # submodule rejects; the `suppressIncompatibleConfig` cleanup
      # only landed on master, so until 26.05 ships it the HM path is
      # unusable for any plugin set anyway.) Operators can still drop
      # `~/.config/nvim/` overrides; the wrapper's config is the
      # system-wide XDG default they fall back to.
      #
      # defaultEditor wires EDITOR via environment.sessionVariables;
      # vi/vim aliases mean muscle memory from any other Unix box
      # lands on nvim. init.lua ships the base options (numbers, leader,
      # undo, soft wrap, ...) and is the only thing spliced into the
      # generated init.lua, because options have to be set before a plugin
      # loads. Everything else rides in `nvimConfig` as an ordinary plugin.
      # treesitter ships every grammar (cross-tenant dedup makes this
      # free, see AGENTS.md).
      neovim = {
        enable = true;
        defaultEditor = true;
        viAlias = true;
        vimAlias = true;
        configure = {
          customLuaRC = builtins.readFile ./nvim/init.lua;
          packages.ix.start =
            [
              nvimConfig
              pkgs.vimPlugins.nvim-treesitter.withAllGrammars
            ]
            ++ builtins.attrValues {
              inherit
                (pkgs.vimPlugins)
                plenary-nvim
                telescope-nvim
                gitsigns-nvim
                which-key-nvim
                oil-nvim
                ;
            };
        };
        # ix-islands colorscheme, generated from the shared islands palette so
        # the editor and the search `-c` highlighter never drift. Both
        # variants live in packages/code-highlight/src/islands-theme.json (the
        # single source of truth, exposed here as ix.islandsTheme), and
        # nvim/islands-body.lua holds the highlight-group wiring both variants
        # share. Faithful port of JetBrains Islands Dark/Light (see
        # andrewgazelka/vscode-islands for the VS Code variant); init.lua picks
        # the dark variant by default. Both are available via
        # `:colorscheme ix-islands-{dark,light}` at runtime.
        runtime = let
          body = builtins.readFile ./nvim/islands-body.lua;
          colorscheme = variant: slots: let
            colorTable = lib.concatMapAttrsStringSep "\n" (slot: hex: ''${slot} = "${hex}",'') slots;
          in
            pkgs.writeText "ix-islands-${variant}.lua" ''
              -- ix-islands-${variant}
              --
              -- Generated from packages/code-highlight/src/islands-theme.json
              -- and nvim/islands-body.lua. Edit those, not this file.
              vim.cmd("highlight clear")
              if vim.fn.exists("syntax_on") == 1 then
                vim.cmd("syntax reset")
              end
              vim.o.background = "${variant}"
              vim.g.colors_name = "ix-islands-${variant}"

              local c = {
              ${colorTable}
              }

              ${body}
            '';
        in {
          "colors/ix-islands-dark.lua".source = colorscheme "dark" ix.islandsTheme.dark;
          "colors/ix-islands-light.lua".source = colorscheme "light" ix.islandsTheme.light;
        };
      };
    };

    environment.variables = {
      # Neovim queries the terminal for its background colour (OSC 11) and
      # waits 100 ms for a DSR reply before giving up with
      # `E1568: Terminal did not respond to DSR request for 'background'
      # color`. The ix console and the browser terminal do not answer, so
      # every `nvim` in a VM pays the timeout and then prints the warning over
      # the first screen the operator sees. The query cannot be turned off
      # from Lua -- `:help 'ttyfast'` is explicit that "the queries are
      # performed early, before --cmd and user config, so `:set nottyfast` in
      # your config happens too late" -- and $NVIM_NOTTYFAST is the knob it
      # names instead. Nothing is lost: the two things those queries decide
      # are 'background', which the ix-islands colorscheme sets, and
      # 'termguicolors', which nvim/init.lua sets.
      NVIM_NOTTYFAST = "1";
    };

    environment.systemPackages =
      builtins.attrValues {
        inherit
          (pkgs)
          ast-grep
          bat
          bpftrace
          btop
          codex
          # dig/nslookup for DNS debugging; `host` alone (shipped via
          # NixOS defaults) answers "does it resolve" but not "from which
          # server, with which record details".
          dnsutils
          # Stack unwinder and ELF/DWARF inspector. `eu-stack` resolves
          # stripped binaries against separate debuginfo, `eu-readelf`
          # gives a saner view of section/note contents than `readelf`,
          # and `eu-unstrip` recombines a stripped binary with its
          # debug companion before feeding it to gdb/drgn/pahole.
          elfutils
          eza
          fd
          file
          # C toolchain: `pip install` of any package with a native
          # extension, node-gyp, and every "./configure && make" README
          # assume cc + make exist. 5 of 7 competitor default images ship
          # one; a VM that can't build a C extension fails the first
          # real Python or Node session.
          gcc
          gdb
          gnumake
          # gnutar, gzip, and zstd ride along so any VM switched once stays
          # switchable: the `ix apply` source upload streams a tarball through
          # `tar -x -I zstd` inside the guest, and these binaries are not
          # on NixOS' default system PATH.
          gnutar
          gzip
          # Alternative editors next to the default neovim. Helix is the
          # modern single-binary editor; micro is the nano-style fallback
          # for operators who want predictable bindings without modes.
          helix
          htop
          micro
          jq
          lldb
          lsof
          mgrep
          ncdu
          # nh wraps nixos-rebuild/home-manager/darwin-rebuild with a
          # build tree (via nom), pre-activation diffs (via dix), and
          # confirmation prompts. nix-output-monitor is shipped
          # separately so plain `nom nix build .#foo` works outside nh.
          # nix-tree is the interactive TUI for exploring a derivation's
          # dependency graph.
          nh
          nix-output-monitor
          nix-tree
          # Default language runtimes. python3 and node are the two
          # interpreters "run this script" instructions assume exist
          # (both ship in 5 of 7 competitor default sandboxes); uv is
          # the package/venv path for Python that doesn't fight the
          # read-only store the way bare pip does.
          nodejs
          python3
          uv
          # TLS/cert debugging (s_client, x509) plus the digest/keygen
          # one-liners every deploy doc reaches for.
          openssl
          # Walks DWARF/BTF type info to pretty-print kernel and userspace
          # structs out of core dumps, /proc/kcore, or VM RAM memfds. The
          # canonical tool for "I have raw memory and I need to know what
          # struct lives at this offset", which gdb/lldb both fumble.
          pahole
          # killall/pstree muscle memory from every other Unix box.
          psmisc
          # drgn complements pahole: pahole answers "what is the layout of
          # struct foo?", drgn lets you start from a typed root and walk
          # the live value graph in Python (dereference pointers, follow
          # intrusive lists, dump fields). Packaged in `packages/drgn/`
          # against the v0.2.0 upstream release until the open nixpkgs PR
          # (#446138) lands and the pin moves.
          drgn
          pv
          ripgrep
          # The `sqlite3` CLI: half of local app state (browsers, package
          # managers, our own atuin history) is a SQLite file, and
          # inspecting one without the CLI means writing a script.
          sqlite
          strace
          tcpdump
          # zellij (below) is the curated multiplexer, but tmux is the
          # one operators and agent recipes actually type; both are tiny.
          tmux
          tree
          # Complements ripgrep with what it lacks: boolean queries
          # (-%), fuzzy match (-Z), and searching inside archives and
          # compressed files (-z).
          ugrep
          # Half the internet distributes .zip; gnutar/zstd cover the
          # rest of the archive formats but reject zip, which turns a
          # plain `curl -LO <github archive>` into a dead end.
          unzip
          # wget rides along for the copy-paste commands that assume it;
          # curl comes from NixOS' core packages.
          wget
          # Hex dump/reverse for quick binary pokes without pulling
          # a full vim install (nvim's wrapper does not expose xxd).
          xxd
          # Pane and tab multiplexer for one session. Connection survival
          # across SSH drops is handled by ix itself (AGENTS.md "VM
          # assumptions"), so zellij is shipped for splits, not reattach.
          zellij
          zip
          zstd
          ;
      }
      # The two agent CLIs ride together: codex comes straight from pkgs (above),
      # claude needs the IS_SANDBOX wrapper from the let-block because guests run
      # as root. Both belong in the consensus baseline this profile ships - "the
      # first ten minutes of a person or agent in a fresh VM must just work"
      # includes the agents themselves. flox rides from the let-block too: it
      # is not in nixpkgs (own flake input) and gets the metrics-default
      # wrapper.
      ++ [claude-code flox];

    systemd.tmpfiles.rules =
      [
        # Belt and suspenders, not the mechanism. The guarantee is made at
        # capture: sealing an image refuses when a platform-made lock
        # artifact is present in it (ENG-12405), because a boot-side heal
        # can only enumerate the messes someone already found, while a
        # capture-side gate refuses the ones nobody has met yet. This rule
        # stays for images sealed before that gate existed, and for a lock
        # arriving by a route the gate's list does not yet name.
        #
        # What it heals: with `use-sqlite-wal = false` (above) nix opens
        # db.sqlite on SQLite's unix-dotfile VFS, whose lock is a real
        # directory entry (`db.sqlite.lock`) rather than a POSIX lock that
        # dies with its holder. A rootfs carrying one boots with the lock
        # still present and no process able to hold it, so every later nix
        # invocation spins forever in SQLITE_BUSY retries -- `ix apply`
        # then fails its 5s wasm-capability probe with "stream into guest
        # failed" (ix#8389). Removing it here ("!" = boot only, before
        # nix-daemon or any login shell can run nix) is always safe for
        # the same reason it is needed: after a fresh boot nobody holds it.
        #
        # The sibling db.sqlite-journal must stay, and the seal gate does
        # not refuse it either: a hot journal is how SQLite rolls back the
        # interrupted write on next open.
        "R! /nix/var/nix/db/db.sqlite.lock"
      ]
      # Pre-create the workspace at boot so login.nu can cd into it
      # without racing tmpfiles or relying on mkdir from the shell.
      ++ lib.optional cfg.shellWorkspace.enable
      "d ${cfg.shellWorkspace.directory} 0755 root root -";
  };
}
