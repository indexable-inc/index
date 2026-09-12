{
  lib,
  ix,
  pkgs,
  makeBinaryWrapper,
  installShellFiles,
  ripgrep,
  bubblewrap,
  runCommand,
  git,
  symlinkJoin,
  formats,
  binName ? "codex",
  # Shell globs the (claude-only) worktree-guard protects, threaded into the
  # shared hook module so both wrappers feed it the same inputs. Unused in the
  # codex render (worktree-guard is claude-only), kept only for parity.
  primaryCheckouts ? [
    "/home/*/index"
    "/home/*/ix"
  ],
  # Andrew-only local startup context: cached notes and ~/Projects inventory.
  # Disabled for the shared wrapper because those hooks print workstation-local
  # context that is not meaningful for other users.
  personalStartupContext ? false,
  # Consumer-supplied SessionStart context commands
  # ({ package, exeName ? null, args ? [], timeout ? 10 }); same seam as the
  # claude-code wrapper (index#3849).
  extraSessionStart ? [],
  # Sibling repo packages from the flake package set (threaded by
  # lib/packages.nix), used to locate the `ix-mcp-ex` entrypoint for the baked
  # `index` MCP server. `{ }` in the overlay package set, where the `mcp-ex`
  # sibling is out of scope, so the wrapper bakes no MCP server there (the same
  # fallback the claude-code wrapper uses).
  repoPackages ? {},
  # The Codex view carries the "route channel notifications into chat" patch.
  codexSrc ? ix.codexSrc,
  # Rule names dropped from the default house prompt. Only affects the computed
  # `systemPrompt` default below; ignored when `systemPrompt` is passed
  # explicitly.
  omitRules ? [],
  # Topic names dropped from the baked house prompt (prompt `omitTopics`).
  omitTopics ? [],
  # Forced config: codex `-c key=value` overrides applied on EVERY invocation.
  # `-c` is codex's highest-precedence layer (above ~/.codex/config.toml), so use
  # this ONLY for wrapper INVARIANTS the user must not silently lose. The one we
  # bake: turn off the startup update check, since the store binary is read-only
  # and the wrapper owns the version pin, so the check only ever costs a network
  # round-trip it can never act on. Shared house policy also lands here when it
  # must outrank mutable user config, such as disabling superseded shell tools.
  # Broader sandbox and approval posture stays in the user's config or Codex's
  # managed requirements layer.
  forcedSettings ? {
    check_for_update_on_startup = false;
  },
  # Soft defaults: codex `-c key=value` flags injected ONLY when the user's
  # config.toml does not already configure that exact dotted-key path, so an
  # explicit user value always wins. Detection is per-leaf (exact TOML path
  # lookup via the compiled Rust launcher, not substring grep): a config that
  # sets `features.multi_agent_v2.enabled` keeps only that key out of the
  # wrapper defaults, while sibling keys (like max_concurrent_threads_per_session)
  # are still injected if unset. A user's own later `-c` still wins over both.
  #
  # Default: a much higher multi-agent fan-out than stock. Run the v2 runtime
  # (stock default 4 = root + 3 subagents); 16 => root + 15 concurrent subagents.
  # v2 REJECTS `agents.max_threads` ("cannot be set when multi_agent_v2 is
  # enabled"), so the cap lives under the v2 feature; only `agents.max_depth` is
  # still read under v2 (3 => parent -> child -> grandchild -> great-grandchild).
  settings ? {
    features.multi_agent_v2 = {
      enabled = true;
      max_concurrent_threads_per_session = 16;
    };
    agents.max_depth = 3;
    # multi_agent_v2 is an under-development feature, so enabling it above
    # makes Codex print an unstable-features warning on every startup. The
    # wrapper opts into the feature deliberately, so it silences its own
    # warning; a user who sets this key keeps their value.
    suppress_unstable_features_warning = true;
  },
  # MCP servers. Local executable paths are forced because the generation
  # owns them. Remote servers stay soft so user config can override them.
  mcpServers ?
    (import (ix.paths.packagesRoot + "/agent/common.nix") {
      inherit lib ix;
      # Cross (Linux->Darwin) lane: repoPackages are host-native builds, so
      # baking `index` would point mcp_servers.index.command at a Linux
      # ix-mcp-ex the Mac cannot exec. No Darwin ix-mcp-ex exists in this
      # eval. Bake no kernel there: use the exa-only overlay fallback and keep
      # Codex's native shell tools enabled.
      repoPackages =
        if ix.cross.isCross or false
        then builtins.removeAttrs repoPackages ["mcp-ex"]
        else repoPackages;
      promptOmitRules = omitRules;
      promptOmitTopics = omitTopics;
    }).defaultServers,
  # The house model/base instructions Codex should run with. This becomes a
  # store-backed `model_instructions_file` soft default. Null bakes no default.
  systemPrompt ?
    (import (ix.paths.packagesRoot + "/agent/common.nix") {
      inherit lib ix repoPackages;
      promptOmitRules = omitRules;
      promptOmitTopics = omitTopics;
    }).systemPromptFor
    "codex",
  # Existing prompt file to use instead of materializing `systemPrompt`.
  # Overrides `systemPrompt` when non-null.
  modelInstructionsFile ? null,
}: let
  # Cross signal from the RFC 0009 lane (lib/per-system.nix `crossIxFor`): on a
  # Linux build host this ix carries `cross = { isCross; target; targetSystem;
  # pkgs; }` (`pkgs` = the target's nixpkgs cross scope; the `pkgs` argument of
  # THIS file stays the build host's) and codex-rs is cross-compiled to Darwin.
  # `null`/absent on a native build.
  crossTarget = ix.cross.target or null;
  isCross = ix.cross.isCross or false;

  effectiveModelInstructionsFile =
    if modelInstructionsFile != null
    then modelInstructionsFile
    else if systemPrompt != null
    then builtins.toFile "codex-system-prompt.txt" systemPrompt
    else null;

  # The compiled Rust launcher (packages/config-launch): reads IX_LAUNCH_SPEC
  # (a baked JSON file describing the target binary, config path, forced flags,
  # and soft defaults), performs per-key TOML presence detection against the
  # user's config.toml, then exec's the target preserving argv0.
  launcher = ix.rustWorkspace.units.binaries.config-launch;
  entriesOf = flat:
    lib.mapAttrsToList (key: v: {
      inherit key;
      value = ix.toml.scalar v;
    })
    flat;

  # Gates fold in the native tools each baked MCP server supersedes. The native
  # shell remains available with or without the kernel.
  sharedPermissions = import (ix.paths.packagesRoot + "/agent/policy/permissions.nix") {
    inherit lib;
    indexKernelBaked = mcpServers ? index;
    exaSearchBaked = mcpServers ? exa;
  };
  effectiveForcedSettings =
    forcedSettings
    // sharedPermissions.codex.forcedSettings
    // {
      features =
        (forcedSettings.features or {}) // (sharedPermissions.codex.forcedSettings.features or {});
    };
  localMcpServers = lib.filterAttrs (_: server: server ? command) mcpServers;
  remoteMcpServers = lib.filterAttrs (_: server: !(server ? command)) mcpServers;
  # Dirs prepended to PATH at launch (the old `--prefix PATH :`): the pinned
  # ripgrep codex shells out to, and bubblewrap for its Linux sandbox. Passed to
  # the launcher as `path_prepend` (it joins them ahead of the caller's PATH).
  # Native builds only. Under the Linux->Darwin cross lane (lib/per-system.nix
  # `buildCrossPackage`) this `pkgs` is the x86_64-linux BUILD host's scope:
  # `ripgrep`/`bubblewrap` are ELF and `hostPlatform.isLinux` is true, so
  # prepending them shadows the Mac's own rg with one that cannot exec (shipped
  # once, HM gen 738, 2026-09-03). A Darwin ripgrep through `ix.cross.pkgs`
  # costs 49 derivations including a cross rustc (dry-run, nixpkgs
  # 46db2e09e1d3), so the cross spec prepends nothing and codex resolves rg on
  # the caller's PATH, as it did before the launcher.
  pathPrepend = lib.optionals (!isCross) (map (p: "${lib.getBin p}/bin") (
    [ripgrep] ++ lib.optional pkgs.stdenv.hostPlatform.isLinux bubblewrap
  ));
  specValue = {
    target = lib.getExe codexWithNotifications;
    path_prepend = pathPrepend;
    config_dir_env = "CODEX_HOME";
    config_dir_default = "~/.codex";
    config_file = "config.toml";
    forced =
      entriesOf (ix.attrs.flattenToDotted effectiveForcedSettings)
      ++ ix.mcp.toCodexEntries localMcpServers;
    soft =
      entriesOf (
        ix.attrs.flattenToDotted (
          lib.optionalAttrs (effectiveModelInstructionsFile != null) {
            model_instructions_file = toString effectiveModelInstructionsFile;
          }
          // settings
        )
      )
      ++ ix.mcp.toCodexEntries remoteMcpServers;
  };
  spec = (formats.json {}).generate "codex-launch-spec.json" specValue;

  # Codex reads hooks from config, not from launch flags, so expose the rendered
  # shared hook policy for home-manager or managed requirements consumers.
  # `isCross` makes the runner a portable sh shim around the cross-compiled
  # claude-hooks (see hook-runner.nix), so the hooksJson a Mac installs from
  # the aliased package carries no build-host ELF wrappers or Linux tool paths.
  hookRunner = import (ix.paths.packagesRoot + "/agent/policy/hook-runner.nix") {
    inherit
      lib
      runCommand
      makeBinaryWrapper
      ix
      git
      primaryCheckouts
      repoPackages
      isCross
      ;
  };
  hooksJson = (formats.json {}).generate "codex-hooks.json" {
    hooks =
      (import (ix.paths.packagesRoot + "/agent/policy/hooks.nix") {
        inherit
          lib
          hookRunner
          primaryCheckouts
          personalStartupContext
          extraSessionStart
          ;
      }).codex;
  };
  # The codex-rs workspace version. The fork tracks upstream loosely and the
  # index wrapper owns the real version pin, so this stays fixed at codex-rs's
  # own `workspace.package.version`.
  version = "0.0.0";

  # codex-rs built on the per-unit Rust DAG (see ./rust.nix). This is the deep
  # change: instead of `rustPlatform.buildRustPackage`, codex now rides the same
  # cargoUnit machinery as index's own crates, so it cross-compiles to Darwin
  # from Linux and a Codex view update only rebuilds the crates that changed.
  codexRust = import ./rust.nix {
    inherit lib pkgs ix codexSrc binName;
    target = crossTarget;
  };
  codexBinary = codexRust.binary;
  codexHostBinary = codexRust.hostBinary;

  # The raw codex binary and `codex-code-mode-host` side by side in ONE bin/,
  # plus shell completions. Side by side is load-bearing: codex resolves the
  # host as a sibling of `current_exe()` (install-context
  # `code_mode_host_program`), and on Linux `current_exe()` is /proc/self/exe,
  # the REAL file, so a wrapper or symlink elsewhere would point the lookup
  # back at codex's own unit output where no host lives. Hence both are copied
  # (cargoUnit roots are one derivation per binary, there is no shared output
  # to link) and no wrapper sits between the user and the real codex here; the
  # PATH tools codex shells out to ride the launch spec's `path_prepend` on
  # native builds (a cross spec prepends nothing, see `pathPrepend`).
  # Completions run the binary, so they are gated on the build host being able
  # to execute it (skipped when cross).
  codexWithNotifications =
    runCommand "codex-${version}" {
      inherit version;
      nativeBuildInputs = lib.optional (!isCross) installShellFiles;
      meta = {
        description = "OpenAI Codex CLI";
        homepage = "https://github.com/openai/codex";
        changelog = "https://github.com/openai/codex/commits/main";
        license = lib.licenses.asl20;
        mainProgram = binName;
        platforms = lib.platforms.unix;
      };
    } ''
      # shell
      mkdir -p "$out/bin"
      cp ${codexBinary}/bin/${binName} "$out/bin/${binName}"
      cp ${codexHostBinary}/bin/codex-code-mode-host "$out/bin/codex-code-mode-host"
      chmod 0755 "$out/bin/${binName}" "$out/bin/codex-code-mode-host"
      ${lib.optionalString (!isCross && pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform) ''
        installShellCompletion --cmd ${binName} \
          --bash <("$out/bin/${binName}" completion bash) \
          --fish <("$out/bin/${binName}" completion fish) \
          --zsh <("$out/bin/${binName}" completion zsh)
      ''}
    '';
in
  # These baked defaults also reach the Codex GUI app's remote-SSH sessions, not
  # just terminal use. The desktop app does NOT ship its own binary to the remote
  # (unlike VS Code Remote SSH): it bootstraps the host through the remote user's
  # login shell and runs `codex app-server` from the remote PATH (then connects via
  # `codex app-server proxy`). So whenever THIS wrapper is the `codex` first on the
  # remote's login-shell PATH, it intercepts that `app-server` launch and injects
  # the same `-c` flags, and every GUI/phone session against that host inherits the
  # defaults. Caveats: the wrapper must win the remote *login* shell PATH (the probe
  # uses `$SHELL -lc`, which skips ~/.bashrc/~/.zshrc), and a stale already-running
  # `codex app-server` is reused without re-injecting, so kill it once after a bump.
  symlinkJoin {
    name = "codex-${codexWithNotifications.version}";
    paths = [codexWithNotifications];
    # symlinkJoin links the whole codex output (libexec, completions, ...); we only
    # replace the entrypoint with our wrapper so the baked defaults ride every
    # invocation while everything else stays pristine.
    nativeBuildInputs = lib.optional (!isCross) makeBinaryWrapper;
    postBuild =
      if isCross
      then ''
        # Cross lane: a portable #!/bin/sh shim, since makeBinaryWrapper's
        # compiled wrapper is a build-host (Linux ELF) binary dead on the Mac.
        # `exec -a "$0"` reproduces makeBinaryWrapper --inherit-argv0 so
        # config-launch sees the name codex was invoked as (it reads argv0 and
        # re-execs the target with it); macOS /bin/sh supports `exec -a`.
        rm -f $out/bin/${binName}
        cp ${
          pkgs.writeText "codex-cross-launch" ''
            #!/bin/sh
            export IX_LAUNCH_SPEC=${spec}
            exec -a "$0" ${launcher}/bin/config-launch "$@"
          ''
        } $out/bin/${binName}
        chmod 0755 $out/bin/${binName}
      ''
      else ''
        # shell
        rm -f $out/bin/${binName}
        makeBinaryWrapper ${launcher}/bin/config-launch $out/bin/${binName} \
          --inherit-argv0 \
          --set IX_LAUNCH_SPEC ${spec}
      '';
    # The codex hooks.json rendered from the shared declaration list, for a
    # consumer to deliver to `~/.codex/hooks.json` (see the `hooksJson` comment).
    passthru = {
      inherit hooksJson spec specValue;
      # codex-rs rides its OWN cargoUnit workspace (a second buildWorkspace,
      # not the shared crossWorkspace), so its unit-graph IFD artifacts are
      # invisible to per-system.nix's crossIfdRoots. Expose them so the cross
      # lane can publish them as explicit push roots; without that a Darwin
      # consumer's eval of the cross codex re-vendors/re-renders this graph and
      # hits the #1890 substitute-or-nothing trap on x86_64-linux drvs it
      # cannot build (RFC 0009, same rationale as crossIfdRoots).
      workspaceIfdRoots = {
        inherit (codexRust.workspace) unitsNix unitGraphJson vendorDir;
      };
      modelInstructionsFile = effectiveModelInstructionsFile;
      permissions = sharedPermissions.codex;
    };
    meta =
      codexWithNotifications.meta
      // {
        description = "${codexWithNotifications.meta.description or "OpenAI Codex CLI"} (index wrapper with baked defaults)";
        mainProgram = binName;
      };
  }
