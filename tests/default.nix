# Eval tests. Module and example assertions live in `groups`; `eval` aggregates
# them along with the cross-module checks (fleet, helpers).
{
  nixpkgs,
  ix,
  paths,
  home-manager,
}: let
  inherit (nixpkgs) lib;
  inherit (ix) pkgs;
  fs = lib.fileset;
  repoPackages = ix.packageSetFor pkgs;
  # Fixture pins (rev + SRI hash pairs for the real-workspace snapshots and the
  # shared go-unit vendor hash) live in the sibling pins.json, never inline
  # (repo policy: no hash literals in tracked .nix).
  pins = ix.pins.loadPins ./pins.json;
  portableServicesTest = import ./portable-services.nix {inherit lib pkgs ix;};
  # Provenance walker + home module (whence, #2413): asserts the manifest of
  # a real home-manager eval links deployed paths to their defining sites,
  # so it takes the home-manager flake input rather than option stubs.
  provenanceTest = import ./provenance.nix {
    inherit
      lib
      pkgs
      ix
      paths
      home-manager
      ;
  };
  nixBuilderTest = import ./nix-builder.nix {
    inherit
      lib
      nixpkgs
      paths
      pkgs
      ;
  };
  # VM boot smoke test for the minecraft-blocks Paper plugin (ENG-2186). Not
  # part of the `eval` aggregate: it boots a qemu VM, so it is its own check
  # (`checks.<system>.minecraft-blocks-vm`).
  minecraftBlocksVmTest = import ./minecraft-blocks-vm.nix {
    inherit
      lib
      pkgs
      ix
      paths
      ;
  };
  # VM boot + protocol smoke test for the Minestom spleef example server. Same
  # deal: qemu VM, so its own check (`checks.<system>.minestom-spleef-vm`).
  minestomSpleefVmTest = import ./minestom-spleef-vm.nix {
    inherit
      lib
      pkgs
      ix
      paths
      ;
  };
  # The only test in the suite that runs a generation switch rather than
  # reading the generation it would build. Same deal again: qemu VM, so its
  # own check (`checks.<system>.switch-stops-a-mount-vm`).
  switchStopsAMountVmTest = import ./switch-stops-a-mount-vm.nix {
    inherit lib pkgs paths;
  };
  # Public Rust SDK: validates the prebuilt, R2-hosted ix-sdk-wire artifact
  # pins. The old end-to-end link proof needs a matching published rustc
  # dependency closure before it can be a reliable CI gate.
  sdkRust = import (paths.root + "/packages/sdk/rust/build.nix") {inherit lib pkgs ix;};
  packageRegistry = import (paths.packagesRoot + "/registry.nix") {
    inherit lib;
    root = paths.packagesRoot;
    inherit (ix.lists) findDuplicates;
  };
  missingPackageMetadata =
    map (
      dir: lib.removePrefix "${builtins.toString paths.packagesRoot}/" (builtins.toString dir)
    )
    packageRegistry.packageDirsWithoutMetadata;
  securityRootArgs = {
    attr = "packages.${pkgs.stdenv.hostPlatform.system}.hello";
    name = "hello";
    class = "distributed-cli";
    owner = "indexable-inc/index";
    environment = "none";
    exposure = "local";
    criticality = "low";
    slaHours = 168;
  };
  securityRoot = ix.securityRoots.mkRoot securityRootArgs;
  securityRootJson = builtins.fromJSON (
    builtins.unsafeDiscardStringContext (builtins.toJSON securityRoot)
  );
  invalidSecurityRootClass =
    builtins.tryEval (ix.securityRoots.mkRoot (securityRootArgs // {class = "package";})).class;
  invalidSecurityRootSla =
    builtins.tryEval (ix.securityRoots.mkRoot (securityRootArgs // {slaHours = 0;})).slaHours;
  # Example READMEs must document the one product entrypoint (ix#8306):
  # an explicit `ix apply` line, single- and multi-VM alike, with no stale
  # references to the pre-`default.ix` config name.
  applyReadmes = [
    "declared/groups"
    "dev/vm"
    "east-west/firewall"
    "hermes/agent"
    "hermes/api-server"
    "hermes/minecraft-operator"
    "hermes/telegram"
    "k8s/k3s"
    "kernel-build"
    "minecraft/blocks"
    "minecraft/crazy-terrain"
    "minecraft/factions"
    "minecraft/survival"
    "multi-client/file-sharing"
    "multi-vm/hello"
    "multi-vm/microservices"
    "multi-vm/switch-multi"
    "nixos/switch"
    "nomad/cluster"
    "observability/stack"
    "polyglot/dev"
    "python/daily-scraper"
    "ray/cluster"
    "s3/storage"
    "synced-github/auth"
  ];

  versions = import (paths.minecraftCatalogs + "/versions.nix") {
    inherit lib;
    inherit (ix) artifacts;
  };
  defaultMinecraftVersion = versions.default;
  defaultMinecraftModule = versions.${defaultMinecraftVersion};

  minecraftModule = {
    ix,
    lib,
    ...
  }: let
    commonCatalog = ix.artifacts.minecraft.modCatalogs.common;
  in {
    ix.image.name = "minecraft";

    services.minecraft = {
      enable = true;
      properties.motd = "ix-powered Minecraft";
      mods = lib.genAttrs (lib.attrNames commonCatalog) (_: {});
    };
  };

  minecraftBedrockModule = _: {
    ix.image.name = "minecraft-bedrock";

    services.minecraft-bedrock = {
      enable = true;
      settings = {
        server-name = "ix-powered Bedrock";
        max-players = 20;
      };
    };
  };

  remoteDesktopImageModule = {pkgs, ...}: {
    ix.image.name = "ix-remote-desktop";

    environment.systemPackages = [
      pkgs.xterm
      pkgs.firefox
    ];

    services.remote-desktop = {
      enable = true;
      openFirewall = true;
      allowUnauthenticated = true;
    };
  };

  rustToolchainFile = lib.importTOML (paths.root + "/rust-toolchain.toml");
  rustPinnedNightlyDate = lib.removePrefix "nightly-" rustToolchainFile.toolchain.channel;

  # Thin wrapper to keep call sites as plain lists; delegates to ix.evalImageConfig
  # so tests exercise the same evaluation path as production image builds.
  evalConfig = modules: ix.evalImageConfig {inherit modules;};
  # The portable fleet modules (services.ix-spark) take the
  # index lib as `indexLib` (not `ix`, which a host binds to its own specialArg).
  # In index's own eval the `ix` specialArg already IS the index lib, so re-expose
  # it under that name for those modules.
  withIndexLib = {ix, ...}: {_module.args.indexLib = ix;};
  plainPkgs = import nixpkgs {
    inherit (pkgs.stdenv.hostPlatform) system;
    config = {};
    overlays = [];
  };
  # Evaluates the JVM profile against a PLAIN nixpkgs package set (no repo
  # overlay) to prove it resolves its JDK from stock nixpkgs. `ix` is supplied as
  # a specialArg exactly as every real module eval does (lib/image/default.nix);
  # that is independent of the pkgs overlay, so the no-overlay contract holds
  # while the module reads `ix.languages.java.defaultJvmVersion` like its siblings.
  standaloneJvmProfile = lib.nixosSystem {
    inherit (pkgs.stdenv.hostPlatform) system;
    specialArgs.ix = ix;
    modules = [
      (paths.modules + "/profiles/jvm")
      {
        nixpkgs.pkgs = plainPkgs;
        system.stateVersion = "25.05";
        ix.profiles.jvm.enable = true;
      }
    ];
  };
  failedAssertionsFor = modules: let
    config = evalConfig modules;
  in
    builtins.filter (assertion: !assertion.assertion) config.assertions;
  samePorts = left: right: lib.sort (a: b: a < b) left == lib.sort (a: b: a < b) right;
  # ix guest sidecars are opened by the shared platform base config.
  baseFirewallTcpPorts = [5001];
  baseFirewallUdpPorts = [8443];
  googleOauthEnvVars = [
    "GOOGLE_OAUTH_CLIENT_ID"
    "GOOGLE_OAUTH_CLIENT_SECRET"
  ];
  sampleMcpServers = ix.mcp.defaultServers {
    indexCommand = "/bin/ix-mcp";
  };
  sampleCodexMcpEntries = ix.mcp.toCodexEntries sampleMcpServers;
  sampleClaudeMcpServers = ix.mcp.toClaudeJson sampleMcpServers;
  sampleCodexMcpEntriesWithoutIndex = ix.mcp.toCodexEntries (ix.mcp.defaultServers {});
  sampleCodexMcpEntry = key: lib.findFirst (entry: entry.key == key) null sampleCodexMcpEntries;
  sampleCodexMcpEntryWithoutIndex = key: lib.findFirst (entry: entry.key == key) null sampleCodexMcpEntriesWithoutIndex;
  agentCommon = import (paths.packagesRoot + "/agent/common.nix") {inherit lib ix repoPackages;};
  sampleClaudeSystemPrompt = agentCommon.systemPromptFor "claude";
  sampleCodexSystemPrompt = agentCommon.systemPromptFor "codex";
  sampleClaudeContextPrompt = agentCommon.contextFor "claude";
  sampleCodexContextPrompt = agentCommon.contextFor "codex";
  homeAgentPkgs = import nixpkgs {
    inherit (pkgs.stdenv.hostPlatform) system;
    config = {
      allowUnfreePredicate = pkg: lib.getName pkg == "claude-code";
    };
    overlays = [ix.overlay];
  };
  homeAgentIndexPackages = _: ix.packageSetFor homeAgentPkgs;
  homeAgentHome = home-manager.lib.homeManagerConfiguration {
    pkgs = homeAgentPkgs;
    modules = [
      (import (paths.packagesRoot + "/agent/home-manager/claude-code.nix") {
        indexPackages = homeAgentIndexPackages;
        promptModule = paths.packagesRoot + "/agent-prompt";
      })
      (import (paths.packagesRoot + "/agent/home-manager/codex.nix") {
        indexPackages = homeAgentIndexPackages;
        promptModule = paths.packagesRoot + "/agent-prompt";
      })
      {
        home = {
          username = "agent";
          homeDirectory = "/home/agent";
          stateVersion = "25.05";
        };
        programs.claude-code = {
          enable = true;
          systemPrompt.omitRules = ["reportToPlaybook"];
          houseContext.enable = true;
          personalStartupContext = true;
        };
        programs.codex = {
          enable = true;
          configDir = ".config/codex-test";
          defaults.agents.max_depth = 4;
          systemPrompt.omitRules = ["reportToPlaybook"];
        };
      }
    ];
  };
  homeAgentConfig = homeAgentHome.config;
  # index#3537: an explicit `package =` outprioritizes the modules'
  # mkDefault-ed wrapper, so the systemPrompt.omitRules fold can no longer
  # reach the shipped package; the modules must fail eval loudly instead of
  # silently shipping prompts that still carry the omitted rules. Home
  # Manager checks assertions eagerly on any config access, so tryEval
  # failing here is the guard tripping: the same fixpoint with the package
  # left defaulted (homeAgentConfig above) passes every assertion, making
  # the explicit package the only delta.
  homeAgentExplicitPackageFails = program:
    !(builtins.tryEval (
      builtins.seq
      (homeAgentHome.extendModules {
        modules = [{programs.${program}.package = homeAgentPkgs.hello;}];
      }).config
      true
    )).success;
  macosGuestsConfig =
    (lib.evalModules {
      specialArgs.pkgs = homeAgentPkgs;
      modules = [
        (import (paths.root + "/modules/home/macos-guests.nix") {
          inherit ix;
          indexPackages = homeAgentIndexPackages;
        })
        ({lib, ...}: {
          options = {
            assertions = lib.mkOption {
              type = lib.types.listOf lib.types.anything;
              default = [];
            };
            home = {
              homeDirectory = lib.mkOption {type = lib.types.str;};
              packages = lib.mkOption {
                type = lib.types.listOf lib.types.package;
                default = [];
              };
            };
            launchd.agents = lib.mkOption {
              type = lib.types.attrsOf (lib.types.submodule {
                options = {
                  enable = lib.mkOption {type = lib.types.bool;};
                  config = lib.mkOption {type = lib.types.attrsOf lib.types.anything;};
                };
              });
              default = {};
            };
          };
          config = {
            home.homeDirectory = "/Users/agent";
            macosGuests.test = {
              lifecycle.macAddress = "0e:c9:c7:6c:25:a8";
              ssh = {
                host = "192.168.64.6";
                user = "ix";
              };
            };
          };
        })
      ];
    }).config;
  macosGuestAgent = macosGuestsConfig.launchd.agents.test;

  minecraft = let
    config = evalConfig [
      minecraftModule
      defaultMinecraftModule
    ];
  in {
    inherit config;
    cfg = config.services.minecraft;
    service = let
      unit = config.systemd.services.minecraft;
    in {
      inherit unit;
      config = unit.serviceConfig;
    };

    paper = let
      config = evalConfig [
        minecraftModule
        versions."1.21.11-paper"
      ];
    in {
      inherit config;
      cfg = config.services.minecraft;
      service = let
        unit = config.systemd.services.minecraft;
      in {
        inherit unit;
        config = unit.serviceConfig;
      };
      managed = {
        serverFiles = config.environment.etc."minecraft/managed-server-files".source;
        dropins = config.environment.etc."minecraft/managed-dropins".source;
      };
    };

    rcon = let
      config = evalConfig [
        minecraftModule
        defaultMinecraftModule
        {
          services.minecraft.rcon.enable = true;
        }
      ];
    in {
      inherit config;
      cfg = config.services.minecraft;
      managed.serverFiles = config.environment.etc."minecraft/managed-server-files".source;

      openFirewall = let
        config = evalConfig [
          minecraftModule
          defaultMinecraftModule
          {
            services.minecraft.rcon = {
              enable = true;
              port = 25_576;
              openFirewall = true;
            };
          }
        ];
      in {
        inherit config;
        cfg = config.services.minecraft;
      };
    };

    worldBorder = let
      config = evalConfig [
        minecraftModule
        defaultMinecraftModule
        {
          services.minecraft.worldBorder = {
            enable = true;
            center = {
              x = 100;
              z = -50;
            };
            diameter = 8000;
          };
        }
      ];
      service = config.systemd.services.minecraft-world-border;
    in {
      inherit config service;
      cfg = config.services.minecraft;
    };

    paperPlugins = let
      config = evalConfig [
        minecraftModule
        versions."26.1.2-paper"
        {
          services.minecraft.plugins = {
            pvpindex-factions = {};
            simple-voice-chat.port = 24_455;
            terraformgenerator.worlds = [
              "factions"
              "factions_nether"
              "factions_the_end"
            ];
            worldedit = {};
          };
          services.minecraft.properties.level-name = "factions";
        }
      ];
    in {
      inherit config;
      cfg = config.services.minecraft;
    };

    nestedProperties = let
      config = evalConfig [
        minecraftModule
        defaultMinecraftModule
        {
          services.minecraft.properties = {
            query = {
              port = 25_565;
            };
            rcon = {
              port = 25_575;
            };
          };
        }
      ];
    in {
      inherit config;
      managed.serverFiles = config.environment.etc."minecraft/managed-server-files".source;
    };

    access = let
      json = pkgs.formats.json {};
      config = evalConfig [
        minecraftModule
        defaultMinecraftModule
        {
          services.minecraft = {
            whitelist.enable = true;
            players = {
              Alice = {
                uuid = "00000000-0000-0000-0000-000000000001";
                whitelist = true;
                operator = {
                  enable = true;
                  level = 3;
                  bypassesPlayerLimit = true;
                };
              };

              Bob = {
                uuid = "00000000-0000-0000-0000-000000000002";
                whitelist = true;
              };
            };
          };
        }
      ];
    in {
      inherit config;
      cfg = config.services.minecraft;
      fixtures = {
        whitelist = {
          current = json.generate "minecraft-whitelist-current.json" [
            {
              uuid = "00000000-0000-0000-0000-000000000001";
              name = "OldAlice";
            }
            {
              uuid = "00000000-0000-0000-0000-000000000003";
              name = "Manual";
            }
            {
              uuid = "00000000-0000-0000-0000-000000000004";
              name = "Removed";
            }
          ];

          previous = json.generate "minecraft-whitelist-previous.json" [
            {
              uuid = "00000000-0000-0000-0000-000000000001";
              name = "OldAlice";
            }
            {
              uuid = "00000000-0000-0000-0000-000000000004";
              name = "Removed";
            }
          ];
        };

        operators = {
          current = json.generate "minecraft-operators-current.json" [
            {
              uuid = "00000000-0000-0000-0000-000000000001";
              name = "OldAlice";
              level = 1;
              bypassesPlayerLimit = false;
            }
            {
              uuid = "00000000-0000-0000-0000-000000000005";
              name = "ManualOp";
              level = 4;
              bypassesPlayerLimit = false;
            }
            {
              uuid = "00000000-0000-0000-0000-000000000006";
              name = "RemovedOp";
              level = 4;
              bypassesPlayerLimit = false;
            }
          ];

          previous = json.generate "minecraft-operators-previous.json" [
            {
              uuid = "00000000-0000-0000-0000-000000000001";
              name = "OldAlice";
              level = 1;
              bypassesPlayerLimit = false;
            }
            {
              uuid = "00000000-0000-0000-0000-000000000006";
              name = "RemovedOp";
              level = 4;
              bypassesPlayerLimit = false;
            }
          ];
        };
      };
      service = let
        unit = config.systemd.services.minecraft;
      in {
        inherit unit;
        config = unit.serviceConfig;
      };
      managed = {
        access = config.environment.etc."minecraft/managed-access".source;
        serverFiles = config.environment.etc."minecraft/managed-server-files".source;
      };
      syncManaged = ix.mkMinecraftSyncManaged {
        inherit pkgs;
        inherit (config.services.minecraft) dropinDir;
        dataDir = "/build/minecraft-access-data";
        managedRoot = "/build/minecraft-managed-root";
        plugmanReloadEnabled = false;
        rconEnabled = false;
        ignoredPlugins = [];
        datapackWorlds = [];
        rconPort = config.services.minecraft.rcon.port;
        rconPasswordFile = "/build/minecraft-access-data/.ix-rcon-password";
        rconBroadcastToOps = false;
      };
    };

    nbt = let
      tags = ix.minecraft.nbt;
      config = evalConfig [
        minecraftModule
        defaultMinecraftModule
        {
          services.minecraft = {
            serverFiles = {
              "generated/example.snbt" = tags.compound {
                DataVersion = tags.int 4325;
                Enabled = tags.bool true;
                Health = tags.short 20;
                Angle = tags.float 0.5;
                Precise = tags.double 12.25;
                Flags = tags.byteArray [
                  1
                  0
                  (-1)
                ];
                Spawn = tags.compound {
                  Dimension = tags.string "minecraft:overworld";
                  Pos = tags.list [
                    (tags.double 1.5)
                    (tags.double 65.25)
                    (tags.double (-30.5))
                  ];
                };
              };

              "generated/example.nbt" = tags.root "ix" (
                tags.compound {
                  Name = tags.string "binary";
                  Values = tags.intArray [
                    1
                    2
                    3
                  ];
                }
              );

              "generated/example.nbt.gz" = tags.compound {
                Name = tags.string "compressed";
              };
            };

            configFiles."generated/client.snbt" = tags.compound {
              Side = tags.string "config";
            };
          };
        }
      ];
    in {
      inherit config;
      cfg = config.services.minecraft;
      managed = {
        config = config.environment.etc."minecraft/managed-config".source;
        serverFiles = config.environment.etc."minecraft/managed-server-files".source;
      };
    };

    datapacks = let
      config = evalConfig [
        minecraftModule
        defaultMinecraftModule
        {
          services.minecraft = {
            properties.level-name = "My World";
            datapacks.max-height.dimensionTypes.overworld = {
              min_y = -2032;
              height = 4064;
              logical_height = 4064;
            };
          };
        }
      ];
    in {
      inherit config;
      cfg = config.services.minecraft;
      service = let
        unit = config.systemd.services.minecraft;
      in {
        inherit unit;
        config = unit.serviceConfig;
      };
      managed.datapacks = config.environment.etc."minecraft/managed-datapacks".source;
      syncManaged = ix.mkMinecraftSyncManaged {
        inherit pkgs;
        inherit (config.services.minecraft) dropinDir;
        dataDir = "/build/minecraft-datapack-data";
        managedRoot = "/build/minecraft-datapack-managed-root";
        plugmanReloadEnabled = false;
        rconEnabled = false;
        ignoredPlugins = [];
        datapackWorlds = config.services.minecraft.datapacks.max-height.worlds;
        rconPort = config.services.minecraft.rcon.port;
        rconPasswordFile = "/build/minecraft-datapack-data/.ix-rcon-password";
        rconBroadcastToOps = false;
      };
    };
  };

  bedrock = let
    config = evalConfig [minecraftBedrockModule];
  in {
    inherit config;
    cfg = config.services.minecraft-bedrock;
    service = let
      unit = config.systemd.services.minecraft-bedrock;
    in {
      inherit unit;
      config = unit.serviceConfig;
    };
  };

  remoteDesktop = let
    config = evalConfig [remoteDesktopImageModule];
  in {
    inherit config;
    cfg = config.services.remote-desktop;
    service = let
      unit = config.systemd.services.remote-desktop;
    in {
      inherit unit;
      config = unit.serviceConfig;
    };
  };

  remoteDesktopModuleDefault = let
    config = evalConfig [
      {
        services.remote-desktop.enable = true;
      }
    ];
  in {
    inherit config;
    cfg = config.services.remote-desktop;
  };

  resourceMonitor = let
    config = evalConfig [
      {
        services.resource-monitor = {
          enable = true;
          runtimeDirectory = "/run/ix/resource-monitor";
        };
      }
    ];
    unit = config.systemd.services.resource-monitor;
  in {
    inherit config;
    cfg = config.services.resource-monitor;
    service = {
      inherit unit;
      config = unit.serviceConfig;
    };
  };

  developmentBase = let
    config = evalConfig [(paths.root + "/lib/dev/base")];
  in {
    inherit config;
    # Outer pkgs has no allowUnfree, so forcing pkgs.claude-code here would
    # throw at eval; use lib.getName over the rendered systemPackages list.
    packageNames = map lib.getName config.environment.systemPackages;
  };

  pythonAppClosureProbe = ix.writePythonApplication pkgs {
    name = "python-app-closure-probe";
    src = pkgs.writeText "python-app-closure-probe.py" ''
      # python
      print("python app source is in the runtime closure")
    '';
    # `check = false` already skips the checker, so the default flip cannot
    # reach this fixture; pin the checker anyway to make that intent explicit.
    pyChecker = "zuban";
    check = false;
  };

  processComposeApplication = ix.writeProcessComposeApplication pkgs {
    name = "process-compose-fixture";
    processes.hello.command = "true";
  };

  bashApplicationProbe = ix.writeBashApplication pkgs {
    name = "bash-application-probe";
    runtimeInputs = [pkgs.hello];
    text = ''
      hello
    '';
  };

  zigAppFixture = fs.toSource {
    root = ./fixtures/zig-app;
    fileset = fs.unions [
      ./fixtures/zig-app/build.zig
      ./fixtures/zig-app/build.zig.zon
      ./fixtures/zig-app/src
    ];
  };

  zigApplication = ix.buildZigPackage pkgs {
    pname = "zig-app-fixture";
    version = "0.1.0";
    src = zigAppFixture;
    zig = ix.languages.zig.toolchain pkgs {version = "0.14";};
    testSteps = {
      lib = "test-lib";
      exe = "test-exe";
    };
  };

  zigDepsFixture = fs.toSource {
    root = ./fixtures/zig-deps;
    fileset = fs.unions [
      ./fixtures/zig-deps/build.zig
      ./fixtures/zig-deps/build.zig.zon
      ./fixtures/zig-deps/src
    ];
  };

  zigDepsApplication = ix.buildZigPackage pkgs {
    pname = "zig-deps-fixture";
    version = "0.1.0";
    src = zigDepsFixture;
    zig = ix.languages.zig.toolchain pkgs {version = "0.14";};
    zigDepsHash = "sha256-2eURmY4iF5iG5CdYiI7cKbrT3ymqb9UFUxO22LmsZ9s=";
  };

  cargoUnitFixture = fs.toSource {
    root = ./fixtures/cargo-unit-hello;
    fileset = fs.unions [
      ./fixtures/cargo-unit-hello/benches
      ./fixtures/cargo-unit-hello/build.rs
      ./fixtures/cargo-unit-hello/Cargo.lock
      ./fixtures/cargo-unit-hello/Cargo.toml
      ./fixtures/cargo-unit-hello/src
    ];
  };

  cargoUnitWorkspace = ix.cargoUnit.buildWorkspace {
    src = cargoUnitFixture;
    workspaceRoot = ./fixtures/cargo-unit-hello;
    cargoTargetNames = [
      "build"
      "test"
      "bench"
    ];
    packageTestInputs.cargo-unit-hello = [pkgs.hello];
    packageTestEnv.cargo-unit-hello.CARGO_UNIT_FIXTURE_ENV = "ok";
    # Drive the packageBuildEnv -> build.rs -> rustc-env path: the build script
    # reads CARGO_UNIT_BUILD_ENV and re-exposes it; the fixture test compares the
    # baked value against this expected value (passed at test runtime).
    packageBuildEnv.cargo-unit-hello.CARGO_UNIT_BUILD_ENV = "build-ok";
    packageTestEnv.cargo-unit-hello.CARGO_UNIT_BUILD_ENV_EXPECTED = "build-ok";
    cargoTargets = [
      ["--workspace"]
      [
        "--workspace"
        "--tests"
      ]
      [
        "--workspace"
        "--benches"
      ]
    ];
  };

  # Same workspace narrowed to the build graph only. Root derivations are
  # per-unit, so this must yield byte-identical roots to lazily selecting from
  # the multi-target workspace above; the helpers assertion pins that equality,
  # which also proves a selected root's closure contains nothing from the
  # dropped target sets. Consumers should select roots lazily instead of
  # spinning up subset workspaces like this one (#716).
  cargoUnitSubsetWorkspace = ix.cargoUnit.buildWorkspace {
    src = cargoUnitFixture;
    workspaceRoot = ./fixtures/cargo-unit-hello;
    packageTestInputs.cargo-unit-hello = [pkgs.hello];
    packageTestEnv.cargo-unit-hello.CARGO_UNIT_FIXTURE_ENV = "ok";
    # Mirror cargoUnitWorkspace exactly except cargoTargets so the byte-identical
    # root assertion (a packageBuildEnv-tagged unit must narrow identically) holds.
    packageBuildEnv.cargo-unit-hello.CARGO_UNIT_BUILD_ENV = "build-ok";
    packageTestEnv.cargo-unit-hello.CARGO_UNIT_BUILD_ENV_EXPECTED = "build-ok";
    cargoTargets = [["--workspace"]];
  };

  cargoUnitCoverageRustToolchain = ix.languages.rust.toolchain pkgs {
    channel = "nightly";
    version = rustPinnedNightlyDate;
    components = [
      "cargo"
      "llvm-tools"
      "rust-std"
      "rustc"
    ];
  };

  cargoUnitCoverageWorkspace = ix.cargoUnit.buildWorkspace {
    pname = "cargo-unit-hello-coverage";
    src = cargoUnitFixture;
    workspaceRoot = ./fixtures/cargo-unit-hello;
    rustToolchain = cargoUnitCoverageRustToolchain;
    cargoArgs = [
      "--workspace"
      "--tests"
    ];
    profile = "dev";
    extraRustcArgs = ["-Cinstrument-coverage"];
    packageTestInputs.cargo-unit-hello = [pkgs.hello];
    packageTestEnv.cargo-unit-hello.CARGO_UNIT_FIXTURE_ENV = "ok";
    policy = {
      denyUnusedCrateDependencies = false;
      cargoAudit.enable = false;
      cargoMachete.enable = false;
      clippy.enable = false;
    };
  };

  cargoUnitHello = cargoUnitWorkspace.binaries.cargo-unit-hello;

  # Exercises `cargoConfigRustflags`: the fixture's crate compiles only when the
  # `--cfg cargo_config_ok` from its `.cargo/config.toml` ([build] rustflags) is
  # applied, so building this binary at all proves the option fed those flags to
  # rustc (a plain build would hit the crate's compile_error). See
  # tests/fixtures/cargo-unit-cargo-config.
  cargoUnitCargoConfigFixture = fs.toSource {
    root = ./fixtures/cargo-unit-cargo-config;
    fileset = fs.unions [
      ./fixtures/cargo-unit-cargo-config/.cargo
      ./fixtures/cargo-unit-cargo-config/Cargo.lock
      ./fixtures/cargo-unit-cargo-config/Cargo.toml
      ./fixtures/cargo-unit-cargo-config/src
    ];
  };
  cargoUnitCargoConfigWorkspace = ix.cargoUnit.buildWorkspace {
    src = cargoUnitCargoConfigFixture;
    workspaceRoot = ./fixtures/cargo-unit-cargo-config;
    cargoConfigRustflags = true;
    cargoArgs = ["--workspace"];
    policy = {
      denyUnusedCrateDependencies = false;
      cargoAudit.enable = false;
      cargoMachete.enable = false;
      clippy.enable = false;
    };
  };
  cargoUnitCargoConfig = cargoUnitCargoConfigWorkspace.binaries.cargo-unit-cargo-config;
  cargoUnitSelectedHello = ix.cargoUnit.selectBinaryWithTests cargoUnitWorkspace {
    binary = "cargo-unit-hello";
    packageName = "cargo-unit-hello";
  };
  cargoUnitTangoComparison = cargoUnitWorkspace.compareTangoBenchmarks {
    baseline = cargoUnitWorkspace;
    args = [
      "--time"
      "0.01"
      "--fail-threshold"
      "100000"
    ];
  };

  cargoUnitBinaries = {
    inherit
      (cargoUnitWorkspace.targetSets.build.binaries)
      cargo-unit-goodbye
      cargo-unit-hello
      ;
  };

  cargoUnitPolicyDisabledWorkspace = ix.cargoUnit.buildWorkspace {
    src = cargoUnitFixture;
    workspaceRoot = ./fixtures/cargo-unit-hello;
    policy = {
      denyUnusedCrateDependencies = false;
      cargoAudit.enable = false;
      cargoMachete.enable = false;
      clippy.enable = false;
    };
  };

  # Self-test for the prebuilt-library injection seam (mkPrebuiltLibraryUnit +
  # extraUnits / extraLibraries). The shape: build a leaf library crate normally
  # (the consumer's own source, `answer() = 42`), then inject a prebuilt unit
  # built from a metadata-identical VARIANT of that library (`answer() = 99`)
  # under the same source-independent unit key, and assert the downstream
  # consumer prints 99. Using a distinguishable variant is what makes the proof
  # real: a same-source rlib is byte-identical, so a runtime check could not tell
  # prebuilt from source; 99-vs-42 can only come from the injected prebuilt.
  # The fixture also has a chained pair (prebuilt-mid depends on prebuilt-lib,
  # prebuilt-chain-consumer depends only on prebuilt-mid) proving that a
  # prebuilt unit's recorded `depUnits` are auto-injected into the consuming
  # graph: the chain consumer links and prints 100 (variant 99 + 1) with only
  # the mid prebuilt passed to extraUnits.
  cargoUnitPrebuiltFixture = fs.toSource {
    root = ./fixtures/cargo-unit-prebuilt;
    fileset = fs.unions [
      ./fixtures/cargo-unit-prebuilt/Cargo.lock
      ./fixtures/cargo-unit-prebuilt/Cargo.toml
      ./fixtures/cargo-unit-prebuilt/crates
    ];
  };

  # A metadata-identical variant of the fixture whose library returns 99 instead
  # of 42. Same package name/version/edition/deps, so cargo-unit computes the
  # same unit key; only the function body (source bytes, which the key ignores)
  # differs. This stands in for "a prebuilt artifact compiled elsewhere".
  cargoUnitPrebuiltVariantSource = pkgs.runCommand "cargo-unit-prebuilt-variant-source" {} ''
    cp -R ${cargoUnitPrebuiltFixture}/. "$out"
    chmod -R u+w "$out"
    sed -i 's/^    42$/    99/' "$out/crates/prebuilt-lib/src/lib.rs"
    grep -q '99' "$out/crates/prebuilt-lib/src/lib.rs"
  '';

  cargoUnitPrebuiltPolicy = {
    denyUnusedCrateDependencies = false;
    cargoAudit.enable = false;
    cargoMachete.enable = false;
    clippy.enable = false;
  };

  # Shared args for the prebuilt-seam fixture workspaces.
  cargoUnitPrebuiltCommon = {
    workspaceRoot = ./fixtures/cargo-unit-prebuilt;
    cargoArgs = ["--workspace"];
    policy = cargoUnitPrebuiltPolicy;
  };

  # (a) The variant workspace, standing in for an out-of-tree prebuilt SDK
  # build. Its lib rlib (answer = 99) is what we inject.
  cargoUnitPrebuiltVariant = ix.cargoUnit.buildWorkspace (
    cargoUnitPrebuiltCommon
    // {
      pname = "cargo-unit-prebuilt-variant";
      src = cargoUnitPrebuiltVariantSource;
      workspaceRoot = cargoUnitPrebuiltVariantSource;
    }
  );

  # Find the single unit whose key starts with `<target-name>-<version>-` in a
  # workspace's unit set (mirrors `cargoUnitScopeUnit`; the attr name IS the
  # unit key) and split out the trailing hash. The crate names here have no
  # dashes and the version is a fixed literal, so stripping the prefix leaves
  # the hash. Exactly one match is asserted so a manifest or profile drift
  # fails here, not downstream.
  cargoUnitPrebuiltUnitByPrefix = workspace: prefix: let
    names = builtins.filter (lib.hasPrefix prefix) (builtins.attrNames workspace.units);
    key = builtins.head names;
  in
    assert lib.assertMsg (
      builtins.length names == 1
    ) "expected exactly one ${prefix}* unit, found ${lib.concatStringsSep ", " names}"; {
      inherit key;
      hash = lib.removePrefix prefix key;
      unit = workspace.units.${key};
    };

  cargoUnitPrebuiltVariantLib = cargoUnitPrebuiltUnitByPrefix cargoUnitPrebuiltVariant "prebuilt_lib-0.1.0-";
  cargoUnitPrebuiltVariantMid = cargoUnitPrebuiltUnitByPrefix cargoUnitPrebuiltVariant "prebuilt_mid-0.1.0-";

  # (b) Wrap the variant's rlib+rmeta as a prebuilt unit. The rlib/rmeta paths
  # are reconstructed from the known underscored name + hash, exactly as the
  # renderer wrote them (render.rs:1376-1392). The toolchain id matches the
  # default toolchain the variant compiled with, so the eval-time assertion in
  # `mkPrebuiltLibraryUnit` passes.
  cargoUnitPrebuiltLibUnit = ix.cargoUnit.mkPrebuiltLibraryUnit {
    # The Cargo library TARGET name, which is what the renderer uses for both
    # the unit key and the rlib filename (render.rs:1376, prepare_graph names).
    # The package is `prebuilt-lib`; its lib target is `prebuilt_lib`.
    pname = "prebuilt_lib";
    version = "0.1.0";
    inherit (cargoUnitPrebuiltVariantLib) hash;
    rlib = "${cargoUnitPrebuiltVariantLib.unit}/lib/libprebuilt_lib-${cargoUnitPrebuiltVariantLib.hash}.rlib";
    rmeta = "${cargoUnitPrebuiltVariantLib.unit}/lib/libprebuilt_lib-${cargoUnitPrebuiltVariantLib.hash}.rmeta";
    toolchainId = ix.cargoUnit.defaultToolchainId;
  };

  # The variant mid prebuilt, recording its leaf dep so buildWorkspace can
  # auto-inject it. The mid rlib embeds the VARIANT leaf's SVH, so linking it
  # in a consumer graph only works when the leaf prebuilt rides along; that is
  # the path the chain arm proves end to end (ENG-2166).
  cargoUnitPrebuiltMidUnit = ix.cargoUnit.mkPrebuiltLibraryUnit {
    pname = "prebuilt_mid";
    version = "0.1.0";
    inherit (cargoUnitPrebuiltVariantMid) hash;
    rlib = "${cargoUnitPrebuiltVariantMid.unit}/lib/libprebuilt_mid-${cargoUnitPrebuiltVariantMid.hash}.rlib";
    rmeta = "${cargoUnitPrebuiltVariantMid.unit}/lib/libprebuilt_mid-${cargoUnitPrebuiltVariantMid.hash}.rmeta";
    toolchainId = ix.cargoUnit.defaultToolchainId;
    depUnits = [cargoUnitPrebuiltLibUnit];
  };

  # Negative arm: a wrong toolchain id must fail at eval (not at link time).
  # `tryEval` should report `success = false`.
  cargoUnitPrebuiltToolchainMismatchEval = builtins.tryEval (
    builtins.seq
    (ix.cargoUnit.mkPrebuiltLibraryUnit {
      pname = "prebuilt_lib";
      version = "0.1.0";
      inherit (cargoUnitPrebuiltVariantLib) hash;
      rlib = "${cargoUnitPrebuiltVariantLib.unit}/lib/libprebuilt_lib-${cargoUnitPrebuiltVariantLib.hash}.rlib";
      rmeta = "${cargoUnitPrebuiltVariantLib.unit}/lib/libprebuilt_lib-${cargoUnitPrebuiltVariantLib.hash}.rmeta";
      toolchainId = "definitely-not-the-toolchain";
    }).drvPath
    true
  );

  # Negative arm: depUnits entries that carry no `passthru.unitKey` could never
  # be auto-injected; mkPrebuiltLibraryUnit must reject them at construction.
  cargoUnitPrebuiltBadDepEval = builtins.tryEval (
    builtins.seq
    (ix.cargoUnit.mkPrebuiltLibraryUnit {
      pname = "prebuilt_mid";
      version = "0.1.0";
      inherit (cargoUnitPrebuiltVariantMid) hash;
      rlib = "${cargoUnitPrebuiltVariantMid.unit}/lib/libprebuilt_mid-${cargoUnitPrebuiltVariantMid.hash}.rlib";
      rmeta = "${cargoUnitPrebuiltVariantMid.unit}/lib/libprebuilt_mid-${cargoUnitPrebuiltVariantMid.hash}.rmeta";
      toolchainId = ix.cargoUnit.defaultToolchainId;
      depUnits = [(pkgs.runCommand "not-a-prebuilt-unit" {} ''mkdir "$out"'')];
    }).drvPath
    true
  );

  # (c) Build the consumer workspace from its OWN source (lib answer = 42), but
  # inject the variant prebuilt unit (answer = 99) over the from-source lib unit.
  # The consumer links the injected prebuilt rlib; if it prints 99 it used the
  # prebuilt, if 42 it fell back to its own source. `extraLibraries` also
  # surfaces the prebuilt through `libraries`.
  cargoUnitPrebuiltInjected = ix.cargoUnit.buildWorkspace (
    cargoUnitPrebuiltCommon
    // {
      pname = "cargo-unit-prebuilt-injected";
      src = cargoUnitPrebuiltFixture;
      extraUnits = {
        ${cargoUnitPrebuiltVariantLib.key} = cargoUnitPrebuiltLibUnit;
      };
      extraLibraries = {
        prebuilt_lib = cargoUnitPrebuiltLibUnit;
      };
    }
  );

  cargoUnitPrebuiltConsumer = cargoUnitPrebuiltInjected.binaries.prebuilt-consumer;

  # (d) ENG-2166: inject ONLY the mid prebuilt; its recorded leaf dep must be
  # auto-injected for the chain consumer to link at all. The mid rlib references
  # the VARIANT leaf's SVH, which no from-source leaf build can satisfy, so a
  # successful link plus the 100 output proves the dep auto-injection worked.
  cargoUnitPrebuiltChainInjected = ix.cargoUnit.buildWorkspace (
    cargoUnitPrebuiltCommon
    // {
      pname = "cargo-unit-prebuilt-chain";
      src = cargoUnitPrebuiltFixture;
      extraUnits = {
        ${cargoUnitPrebuiltVariantMid.key} = cargoUnitPrebuiltMidUnit;
      };
    }
  );
  cargoUnitPrebuiltChainConsumer = cargoUnitPrebuiltChainInjected.binaries.prebuilt-chain-consumer;

  # The consumer workspace's OWN from-source lib unit key (no injection). Used to
  # prove the variant (different source) hashes to the same key, which is the
  # source-independence the whole swap relies on.
  cargoUnitPrebuiltPlain = ix.cargoUnit.buildWorkspace (
    cargoUnitPrebuiltCommon
    // {
      pname = "cargo-unit-prebuilt-plain";
      src = cargoUnitPrebuiltFixture;
    }
  );
  cargoUnitPrebuiltPlainLib = cargoUnitPrebuiltUnitByPrefix cargoUnitPrebuiltPlain "prebuilt_lib-0.1.0-";
  cargoUnitPrebuiltPlainMid = cargoUnitPrebuiltUnitByPrefix cargoUnitPrebuiltPlain "prebuilt_mid-0.1.0-";

  # A SECOND prebuilt for the same leaf unit key, wrapped from the PLAIN
  # workspace's artifacts (answer = 42): same unit key, different derivation.
  # Used by the explicit-override arm and the dep-conflict negative arm below.
  cargoUnitPrebuiltLibUnitFromPlain = ix.cargoUnit.mkPrebuiltLibraryUnit {
    pname = "prebuilt_lib";
    version = "0.1.0";
    inherit (cargoUnitPrebuiltPlainLib) hash;
    rlib = "${cargoUnitPrebuiltPlainLib.unit}/lib/libprebuilt_lib-${cargoUnitPrebuiltPlainLib.hash}.rlib";
    rmeta = "${cargoUnitPrebuiltPlainLib.unit}/lib/libprebuilt_lib-${cargoUnitPrebuiltPlainLib.hash}.rmeta";
    toolchainId = ix.cargoUnit.defaultToolchainId;
  };

  # A well-formed prebuilt whose unit key exists in NO graph. Recorded as a
  # dep of the overridden leaf below: if the closure traversal ever walks the
  # discarded leaf's subtree, this key gets auto-injected and the C1 guard
  # fails eval, so the override arm passing proves the prune.
  cargoUnitPrebuiltPhantomDep = ix.cargoUnit.mkPrebuiltLibraryUnit {
    pname = "phantom_dep";
    version = "0.0.1";
    hash = "0000000000000000";
    # Never built or linked; any .rlib/.rmeta-suffixed store path satisfies the
    # shape asserts.
    rlib = "${cargoUnitPrebuiltVariantLib.unit}/lib/libprebuilt_lib-${cargoUnitPrebuiltVariantLib.hash}.rlib";
    rmeta = "${cargoUnitPrebuiltVariantLib.unit}/lib/libprebuilt_lib-${cargoUnitPrebuiltVariantLib.hash}.rmeta";
    toolchainId = ix.cargoUnit.defaultToolchainId;
  };

  # The variant leaf again, but recording the phantom dep. This is the
  # derivation the override arm DISCARDS; its subtree must be pruned, not
  # walked.
  cargoUnitPrebuiltLibUnitWithPhantomDep = ix.cargoUnit.mkPrebuiltLibraryUnit {
    pname = "prebuilt_lib";
    version = "0.1.0";
    inherit (cargoUnitPrebuiltVariantLib) hash;
    rlib = "${cargoUnitPrebuiltVariantLib.unit}/lib/libprebuilt_lib-${cargoUnitPrebuiltVariantLib.hash}.rlib";
    rmeta = "${cargoUnitPrebuiltVariantLib.unit}/lib/libprebuilt_lib-${cargoUnitPrebuiltVariantLib.hash}.rmeta";
    toolchainId = ix.cargoUnit.defaultToolchainId;
    depUnits = [cargoUnitPrebuiltPhantomDep];
  };

  # Explicit override of an auto-injected dep: the caller pins the leaf key to
  # a different derivation than the one the mid prebuilt recorded. Eval-only:
  # the assertion below checks the graph routes the key to the explicit pin,
  # and the recorded (discarded) leaf carries a phantom dep that would fail C1
  # if the traversal walked the discarded subtree instead of pruning it.
  # Actually LINKING this combination would fail (the mid rlib references the
  # variant leaf's SVH, not the plain one's), which is exactly why replacing a
  # recorded dep must be an explicit caller choice and never a silent merge.
  cargoUnitPrebuiltChainOverride = ix.cargoUnit.buildWorkspace (
    cargoUnitPrebuiltCommon
    // {
      pname = "cargo-unit-prebuilt-chain-override";
      src = cargoUnitPrebuiltFixture;
      extraUnits = {
        ${cargoUnitPrebuiltVariantMid.key} = ix.cargoUnit.mkPrebuiltLibraryUnit {
          pname = "prebuilt_mid";
          version = "0.1.0";
          inherit (cargoUnitPrebuiltVariantMid) hash;
          rlib = "${cargoUnitPrebuiltVariantMid.unit}/lib/libprebuilt_mid-${cargoUnitPrebuiltVariantMid.hash}.rlib";
          rmeta = "${cargoUnitPrebuiltVariantMid.unit}/lib/libprebuilt_mid-${cargoUnitPrebuiltVariantMid.hash}.rmeta";
          toolchainId = ix.cargoUnit.defaultToolchainId;
          depUnits = [cargoUnitPrebuiltLibUnitWithPhantomDep];
        };
        ${cargoUnitPrebuiltVariantLib.key} = cargoUnitPrebuiltLibUnitFromPlain;
      };
    }
  );

  # C4 negative arm: one root recording two different derivations for the same
  # dep unit key, with no explicit pin to break the tie, must fail at eval.
  cargoUnitPrebuiltDepConflictEval = builtins.tryEval (
    builtins.seq (
      builtins.attrNames
      (ix.cargoUnit.buildWorkspace (
        cargoUnitPrebuiltCommon
        // {
          pname = "cargo-unit-prebuilt-dep-conflict";
          src = cargoUnitPrebuiltFixture;
          extraUnits = {
            ${cargoUnitPrebuiltVariantMid.key} = ix.cargoUnit.mkPrebuiltLibraryUnit {
              pname = "prebuilt_mid";
              version = "0.1.0";
              inherit (cargoUnitPrebuiltVariantMid) hash;
              rlib = "${cargoUnitPrebuiltVariantMid.unit}/lib/libprebuilt_mid-${cargoUnitPrebuiltVariantMid.hash}.rlib";
              rmeta = "${cargoUnitPrebuiltVariantMid.unit}/lib/libprebuilt_mid-${cargoUnitPrebuiltVariantMid.hash}.rmeta";
              toolchainId = ix.cargoUnit.defaultToolchainId;
              depUnits = [
                cargoUnitPrebuiltLibUnit
                cargoUnitPrebuiltLibUnitFromPlain
              ];
            };
          };
        }
      )).units
    )
    true
  );

  # C3 negative arm: injecting a prebuilt under a key that disagrees with its
  # own recorded unitKey must fail at eval. The mid's generated key exists in
  # the graph and the toolchain matches, so only the key-mismatch guard fires.
  cargoUnitPrebuiltKeyMismatchEval = builtins.tryEval (
    builtins.seq (
      builtins.attrNames
      (ix.cargoUnit.buildWorkspace (
        cargoUnitPrebuiltCommon
        // {
          pname = "cargo-unit-prebuilt-key-mismatch";
          src = cargoUnitPrebuiltFixture;
          extraUnits = {
            ${cargoUnitPrebuiltVariantMid.key} = cargoUnitPrebuiltLibUnit;
          };
        }
      )).units
    )
    true
  );

  # M1 / C1 negative arm: a mis-keyed injection (a key absent from the generated
  # graph) must now fail loud, not silently build from source. `tryEval` over the
  # workspace's unit-set attribute names should report `success = false`.
  cargoUnitPrebuiltMiskeyEval = builtins.tryEval (
    builtins.seq (
      builtins.attrNames
      (ix.cargoUnit.buildWorkspace (
        cargoUnitPrebuiltCommon
        // {
          pname = "cargo-unit-prebuilt-miskey";
          src = cargoUnitPrebuiltFixture;
          # Deliberately wrong key: not present in the generated unit set.
          extraUnits = {
            "prebuilt_lib-0.1.0-deadbeefdeadbeef" = cargoUnitPrebuiltLibUnit;
          };
        }
      )).units
    )
    true
  );

  goUnitFixture = fs.toSource {
    root = ./fixtures/go-unit-hello;
    fileset = fs.unions [
      ./fixtures/go-unit-hello/go.mod
      ./fixtures/go-unit-hello/go-modules.nix
      ./fixtures/go-unit-hello/go.sum
      ./fixtures/go-unit-hello/main.go
      ./fixtures/go-unit-hello/main_test.go
    ];
  };

  goUnitWorkspace = ix.goUnit.buildWorkspace {
    pname = "go-unit-hello";
    src = goUnitFixture;
    env.GOFLAGS = "-mod=readonly";
    packages = ["."];
  };

  goUnitNestedFixture = fs.toSource {
    root = ./fixtures/go-unit-nested;
    fileset = ./fixtures/go-unit-nested/module;
  };

  goUnitNestedWorkspace = ix.goUnit.buildWorkspace {
    pname = "go-unit-nested";
    src = goUnitNestedFixture;
    modRoot = "module";
    packages = ["."];
  };

  goUnitStdlibFixture = fs.toSource {
    root = ./fixtures/go-unit-stdlib;
    fileset = fs.unions [
      ./fixtures/go-unit-stdlib/go.mod
      ./fixtures/go-unit-stdlib/main.go
      ./fixtures/go-unit-stdlib/main_test.go
    ];
  };
  goUnitMissingGoSumFixture = fs.toSource {
    root = ./fixtures/go-unit-hello;
    fileset = fs.unions [
      ./fixtures/go-unit-hello/go.mod
      ./fixtures/go-unit-hello/main.go
      ./fixtures/go-unit-hello/main_test.go
    ];
  };
  goUnitRequireNoSpaceFixture = fs.toSource {
    root = ./fixtures/go-unit-require-nospace;
    fileset = fs.unions [
      ./fixtures/go-unit-require-nospace/go.mod
      ./fixtures/go-unit-require-nospace/main.go
    ];
  };

  goUnitStdlibWorkspace = ix.goUnit.buildWorkspace {
    pname = "go-unit-stdlib";
    src = goUnitStdlibFixture;
    vendorHash = null;
    packages = ["."];
  };

  goUnitDerivedStdlibSource = pkgs.runCommand "go-unit-stdlib-source" {} ''
    cp -R ${goUnitStdlibFixture}/. "$out"
  '';

  goUnitDerivedStdlibWorkspace = ix.goUnit.buildWorkspace {
    pname = "go-unit-stdlib-derived";
    src = goUnitDerivedStdlibSource;
    goMod = ./fixtures/go-unit-stdlib/go.mod;
    vendorHash = null;
    packages = ["."];
  };
  goUnitDerivedSource = pkgs.runCommand "go-unit-hello-source" {} ''
    cp -R ${goUnitFixture}/. "$out"
  '';
  goUnitDerivedWorkspaceWithVendorHashFile = ix.goUnit.buildWorkspace {
    pname = "go-unit-hello-derived";
    src = goUnitDerivedSource;
    goMod = ./fixtures/go-unit-hello/go.mod;
    goSum = ./fixtures/go-unit-hello/go.sum;
    vendorHashFile = ./fixtures/go-unit-hello/go-modules.nix;
    packages = ["."];
  };
  goUnitDerivedUnreadableNoSumEval = builtins.tryEval (
    builtins.attrNames
    (ix.goUnit.buildWorkspace {
      pname = "go-unit-hello-derived-no-sum";
      src = goUnitDerivedSource;
      vendorHash = null;
      packages = ["."];
    }).packages
  );
  goUnitDerivedMissingGoSumKeyEval = let
    workspace = ix.goUnit.buildWorkspace {
      pname = "go-unit-hello-derived-missing-go-sum";
      src = goUnitDerivedSource;
      goMod = ./fixtures/go-unit-hello/go.mod;
      vendorHashFile = ./fixtures/go-unit-hello/go-modules.nix;
      packages = ["."];
    };
  in
    builtins.tryEval workspace.default.drvPath;
  goUnitMissingGoModFixture = fs.toSource {
    root = ./fixtures/go-unit-hello;
    fileset = ./fixtures/go-unit-hello/main.go;
  };
  goUnitMissingGoModEval =
    builtins.tryEval
    (ix.goUnit.buildWorkspace {
      pname = "go-unit-missing-go-mod";
      src = goUnitMissingGoModFixture;
      vendorHash = null;
      packages = ["."];
    }).vendorHashKey;
  goUnitMissingGoModPackagesEval = builtins.tryEval (
    builtins.attrNames
    (ix.goUnit.buildWorkspace {
      pname = "go-unit-missing-go-mod";
      src = goUnitMissingGoModFixture;
      vendorHash = null;
      packages = ["."];
    }).packages
  );
  goUnitMissingGoSumEval = builtins.tryEval (
    builtins.attrNames
    (ix.goUnit.buildWorkspace {
      pname = "go-unit-missing-go-sum";
      src = goUnitMissingGoSumFixture;
      vendorHash = pins.go-unit-fixture-vendor.hash;
      packages = ["."];
    }).packages
  );
  goUnitMissingGoSumNoSumEval = builtins.tryEval (
    builtins.attrNames
    (ix.goUnit.buildWorkspace {
      pname = "go-unit-missing-go-sum-no-sum";
      src = goUnitMissingGoSumFixture;
      vendorHash = null;
      packages = ["."];
    }).packages
  );
  goUnitRequireNoSpaceNoSumEval = builtins.tryEval (
    builtins.attrNames
    (ix.goUnit.buildWorkspace {
      pname = "go-unit-require-nospace-no-sum";
      src = goUnitRequireNoSpaceFixture;
      vendorHash = null;
      packages = ["."];
    }).packages
  );
  goUnitMissingExplicitGoSumEval = builtins.tryEval (
    builtins.attrNames
    (ix.goUnit.buildWorkspace {
      pname = "go-unit-missing-explicit-go-sum";
      src = goUnitMissingGoSumFixture;
      goSum = goUnitMissingGoSumFixture + "/go.sum";
      vendorHash = pins.go-unit-fixture-vendor.hash;
      packages = ["."];
    }).packages
  );

  goUnitPackageCollisionEval =
    builtins.tryEval
    (ix.goUnit.buildWorkspace {
      pname = "go-unit-collision";
      src = goUnitFixture;
      packages = [
        "a.b"
        "a/b"
      ];
    }).packages;

  # `policy.clippy.packages`: null gates every package, a list gates only those.
  #
  # Deliberately the two-crate scope fixture (`scope-alpha`, `scope-bravo`)
  # rather than the one-crate hello fixture. With a single package, "filtered to
  # the one package" and "not filtered at all" are the same attrset, so the
  # assertion would pass against a filter that does nothing.
  #
  # These also pin the SPELLING. `clippyByPackage` is keyed by cargo PACKAGE
  # name (hyphens), not by the unit key (`scope_alpha-0.1.0-<hash>`, which
  # carries the lib target name), and reaching for the wrong namespace is the
  # mistake the refusal below exists to catch.
  cargoUnitClippyAllowlisted = ix.cargoUnit.buildWorkspace {
    src = cargoUnitScopeFixture;
    workspaceRoot = ./fixtures/cargo-unit-workspace-scope;
    policy.clippy.packages = ["scope-alpha"];
  };

  cargoUnitClippyUnfiltered = ix.cargoUnit.buildWorkspace {
    src = cargoUnitScopeFixture;
    workspaceRoot = ./fixtures/cargo-unit-workspace-scope;
  };

  # An allowlist entry matching no package must refuse rather than quietly gate
  # nothing, since the thing a silent no-op disables is the gate the allowlist
  # exists to guarantee. `scope_alpha` is the real shape of the mistake: an
  # underscored unit-key spelling where a package name belongs.
  cargoUnitClippyUnknownEval =
    builtins.tryEval
    (ix.cargoUnit.buildWorkspace {
      src = cargoUnitScopeFixture;
      workspaceRoot = ./fixtures/cargo-unit-workspace-scope;
      policy.clippy.packages = ["scope_alpha"];
    })
    .clippyByPackage;

  cargoUnitScopePolicy = {
    denyUnusedCrateDependencies = false;
    cargoAudit.enable = false;
    cargoMachete.enable = false;
    clippy.enable = false;
  };

  cargoUnitScopeFixture = fs.toSource {
    root = ./fixtures/cargo-unit-workspace-scope;
    fileset = fs.unions [
      ./fixtures/cargo-unit-workspace-scope/Cargo.lock
      ./fixtures/cargo-unit-workspace-scope/Cargo.toml
      ./fixtures/cargo-unit-workspace-scope/crates
    ];
  };

  # Derived from the base tree rather than checked in beside it. The two differ
  # by one crate's function bodies and are otherwise the same workspace by
  # design, so a second copy on disk is duplication the clone gate charges for
  # every time either side is edited, and a second place to forget when the
  # fixture grows a target.
  cargoUnitScopeAlphaChangedFixture = pkgs.runCommand "cargo-unit-workspace-scope-alpha-changed" {} ''
    cp -R ${cargoUnitScopeFixture}/. "$out"
    chmod -R u+w "$out"
    substituteInPlace "$out/crates/alpha/src/lib.rs" --replace-fail 'alpha:' 'alpha changed:'
  '';

  cargoUnitScopeLockChangedFixture = pkgs.runCommand "cargo-unit-workspace-scope-lock-changed" {} ''
    cp -R ${cargoUnitScopeFixture}/. "$out"
    chmod -R u+w "$out"
    cp ${./fixtures/cargo-unit-workspace-scope/Cargo.itoa-1.0.14.lock} "$out/Cargo.lock"
  '';

  # rust-src on top of the repo default (which omits it). rustc rewrites std's
  # virtual `/rustc/<commit>` source paths back to this component's real
  # directory whenever it is installed, so a toolchain carrying it is what makes
  # the toolchain half of the path remap observable in a linked binary. The
  # component itself is 19 MB and the rest of the aggregate is shared with the
  # default toolchain, so this costs a rebuild of five tiny fixture units.
  cargoUnitScopeRustToolchain = ix.repoRustToolchainFor pkgs {
    components = [
      "cargo"
      "rust-src"
      "rust-std"
      "rustc"
    ];
  };

  cargoUnitScopeWorkspace = {
    name,
    src,
    workspaceRoot ? ./fixtures/cargo-unit-workspace-scope,
  }:
    ix.cargoUnit.buildWorkspace {
      pname = "cargo-unit-workspace-scope-${name}";
      inherit src;
      inherit workspaceRoot;
      rustToolchain = cargoUnitScopeRustToolchain;
      cargoArgs = ["--workspace"];
      policy = cargoUnitScopePolicy;
    };

  cargoUnitScopeWorkspaces = {
    base = cargoUnitScopeWorkspace {
      name = "base";
      src = cargoUnitScopeFixture;
    };
    alphaChanged = cargoUnitScopeWorkspace {
      # Both, per the buildWorkspace docstring's rule for patched sources: local
      # package scopes are carved from `workspaceRoot`, not from `src`, so
      # leaving the root at the base tree silently rebuilds alpha from the
      # unpatched body and the "changed crate rebuilds" assertion goes green on
      # two identical inputs.
      name = "alpha-changed";
      src = cargoUnitScopeAlphaChangedFixture;
      workspaceRoot = cargoUnitScopeAlphaChangedFixture;
    };
    lockChanged = cargoUnitScopeWorkspace {
      name = "lock-changed";
      src = cargoUnitScopeLockChangedFixture;
    };
    # The shape the buildWorkspace docstring recommends and that every other
    # caller here avoided: `src` and `workspaceRoot` both the caller's own
    # directory, as a bare path rather than a pre-filtered derivation. A path
    # value interpolates to a different store path than it stringifies to, so
    # rendering used to name one tree in the unit-graph rewrite and the other
    # as the root to slice against, and refused every local unit as "outside
    # workspace root" (#4239). The fixture directory holds one file the
    # filtered `base` source drops (an alternate lockfile), which the planner
    # stubs and nothing reads, so the two must still render identically.
    pathSrc = cargoUnitScopeWorkspace {
      name = "path-src";
      src = ./fixtures/cargo-unit-workspace-scope;
    };
  };

  cargoUnitScopeUnit = workspace: prefix: let
    matches = lib.filterAttrs (name: _: lib.hasPrefix prefix name) workspace.units;
    names = builtins.attrNames matches;
  in
    assert lib.assertMsg (builtins.length names == 1)
    "expected exactly one cargo-unit unit with prefix ${prefix}, found ${lib.concatStringsSep ", " names}";
      matches.${builtins.head names};

  cargoUnitScope = {
    base = {
      alpha = cargoUnitScopeUnit cargoUnitScopeWorkspaces.base "scope_alpha-0.1.0-";
      bravo = cargoUnitScopeUnit cargoUnitScopeWorkspaces.base "scope_bravo-0.1.0-";
      itoa = cargoUnitScopeUnit cargoUnitScopeWorkspaces.base "itoa-1.0.18-";
      ryu = cargoUnitScopeUnit cargoUnitScopeWorkspaces.base "ryu-1.0.23-";
    };
    alphaChanged = {
      alpha = cargoUnitScopeUnit cargoUnitScopeWorkspaces.alphaChanged "scope_alpha-0.1.0-";
      bravo = cargoUnitScopeUnit cargoUnitScopeWorkspaces.alphaChanged "scope_bravo-0.1.0-";
      itoa = cargoUnitScopeUnit cargoUnitScopeWorkspaces.alphaChanged "itoa-1.0.18-";
      ryu = cargoUnitScopeUnit cargoUnitScopeWorkspaces.alphaChanged "ryu-1.0.23-";
    };
    lockChanged = {
      itoa = cargoUnitScopeUnit cargoUnitScopeWorkspaces.lockChanged "itoa-1.0.14-";
      ryu = cargoUnitScopeUnit cargoUnitScopeWorkspaces.lockChanged "ryu-1.0.23-";
    };
    pathSrc = {
      alpha = cargoUnitScopeUnit cargoUnitScopeWorkspaces.pathSrc "scope_alpha-0.1.0-";
      bravo = cargoUnitScopeUnit cargoUnitScopeWorkspaces.pathSrc "scope_bravo-0.1.0-";
    };
  };

  # A linked binary must not retain the source of everything it was compiled
  # against. `file!()` expands to an absolute path, which under cargo-unit is a
  # store path, so every panic location a dependency bakes into its rlib is a
  # store reference the scanner finds in the binary: before the per-unit
  # `--remap-path-prefix`, hyperion's 23 MB `bedwars` held 110 direct
  # references over a 2.5 GiB closure, of which glibc and gcc-lib were the only
  # real ones (#4249). `scope-alpha-cli` reproduces every flavour of that in
  # one small binary: it links its own workspace lib (which asserts), two
  # vendored crates, and std from a toolchain carrying `rust-src`.
  cargoUnitScopeBinary = cargoUnitScopeWorkspaces.base.binaries.scope-alpha-cli;
  cargoUnitScopeBinaryReferences = pkgs.writeDirectReferencesToFile cargoUnitScopeBinary;

  cargoUnitRealWorkspacePolicy = {
    denyUnusedCrateDependencies = false;
    cargoAudit.enable = false;
    cargoMachete.enable = false;
    clippy.enable = false;
  };

  cargoUnitRealWorkspaceSource = {
    name,
    upstream,
    lockFile,
  }:
    pkgs.runCommand "cargo-unit-${name}-source-with-lock" {} ''
      cp -R ${upstream}/. "$out"
      chmod -R u+w "$out"
      cp ${lockFile} "$out/Cargo.lock"
    '';

  cargoUnitRealWorkspace = {
    name,
    owner,
    repo,
    rev,
    hash,
    lockFile,
    buildArgs ? ["--workspace"],
    testArgs ? null,
  }: let
    upstream = pkgs.fetchFromGitHub {
      inherit
        owner
        repo
        rev
        hash
        ;
    };
    src = cargoUnitRealWorkspaceSource {
      inherit name upstream lockFile;
    };
    commonArgs = {
      pname = "cargo-unit-real-workspace-${name}";
      inherit src;
      cargoLock = lockFile;
      workspaceRoot = src;
      policy = cargoUnitRealWorkspacePolicy;
    };
    buildWorkspace = ix.cargoUnit.buildWorkspace (commonArgs // {cargoArgs = buildArgs;});
    testWorkspace =
      if testArgs == null
      then null
      else
        ix.cargoUnit.buildWorkspace (
          commonArgs
          // {
            pname = "cargo-unit-real-workspace-${name}-tests";
            cargoArgs = testArgs;
          }
        );
  in {
    inherit buildWorkspace testWorkspace;
    buildRoots = pkgs.linkFarmFromDrvs "cargo-unit-real-workspace-${name}-roots" buildWorkspace.roots;
    testRoots =
      if testWorkspace == null
      then null
      else
        pkgs.linkFarmFromDrvs "cargo-unit-real-workspace-${name}-tests" (
          # `tests.<binary>` is now `{ all; cases; }` after the per-#[test]
          # split (854b662); `.all` keeps the link-farm at one entry per
          # test binary, the same shape this script expects.
          map (entry: entry.all) (builtins.attrValues testWorkspace.tests)
        );
  };

  # These upstream workspaces currently do not commit Cargo.lock. The fixture
  # locks make the check exercise the same frozen/offline path as downstream
  # Nix packaging without vendoring forked source trees into this repo.
  cargoUnitRealWorkspaces = {
    serde = cargoUnitRealWorkspace {
      name = "serde";
      inherit
        (pins.serde)
        owner
        repo
        rev
        hash
        ;
      lockFile = ./fixtures/cargo-unit-real-workspaces/serde/Cargo.lock;
    };

    thiserror = cargoUnitRealWorkspace {
      name = "thiserror";
      inherit
        (pins.thiserror)
        owner
        repo
        rev
        hash
        ;
      lockFile = ./fixtures/cargo-unit-real-workspaces/thiserror/Cargo.lock;
    };

    indexmap = cargoUnitRealWorkspace {
      name = "indexmap";
      inherit
        (pins.indexmap)
        owner
        repo
        rev
        hash
        ;
      lockFile = ./fixtures/cargo-unit-real-workspaces/indexmap/Cargo.lock;
      testArgs = [
        "--workspace"
        "--tests"
      ];
    };

    regex = cargoUnitRealWorkspace {
      name = "regex";
      inherit
        (pins.regex)
        owner
        repo
        rev
        hash
        ;
      lockFile = ./fixtures/cargo-unit-real-workspaces/regex/Cargo.lock;
      testArgs = [
        "--workspace"
        "--tests"
      ];
    };
  };

  bunSiteFixture = fs.toSource {
    root = ./fixtures/bun-site;
    fileset = fs.unions [
      ./fixtures/bun-site/bin
      ./fixtures/bun-site/bun.lock
      ./fixtures/bun-site/package.json
    ];
  };

  bunSite = ix.buildJsSite pkgs {
    packageManager = "bun";
    pname = "bun-site-fixture";
    version = "0.1.0";
    src = bunSiteFixture;
    buildFlags = [
      "--class"
      "ix bun"
    ];
  };

  bunLockPackage = builtins.head bunSite.bunNodeModules.bunCache.lock.packages;

  npmSiteFixture = fs.toSource {
    root = ./fixtures/npm-site;
    fileset = fs.unions [
      ./fixtures/npm-site/bin
      ./fixtures/npm-site/package-lock.json
      ./fixtures/npm-site/package.json
    ];
  };

  npmSite = ix.buildJsSite pkgs {
    pname = "npm-site-fixture";
    version = "0.1.0";
    src = npmSiteFixture;
    buildFlags = [
      "--class"
      "ix npm"
    ];
  };

  vitestWorkspaceFixture = fs.toSource {
    root = ./fixtures/vitest-workspace;
    fileset = fs.unions [
      ./fixtures/vitest-workspace/package-lock.json
      ./fixtures/vitest-workspace/package.json
      ./fixtures/vitest-workspace/src
      ./fixtures/vitest-workspace/vitest.config.js
    ];
  };

  vitestWorkspace = ix.buildNpmVitest pkgs {
    pname = "vitest-workspace-fixture";
    version = "0.1.0";
    src = vitestWorkspaceFixture;
  };
  vitestWorkspaceCases = builtins.attrValues vitestWorkspace.cases;

  svelteSite = ix.buildSvelteSite pkgs {
    sourceRoot = ./fixtures/npm-site;
    buildFlags = [
      "--class"
      "ix svelte"
    ];
    serve = {
      name = "svelte-site-fixture";
      port = 8180;
      routePrefix = "/fixture";
      extraFlags = [
        "--title"
        "Svelte Site Fixture"
      ];
    };
    devServer = {
      script = "build";
      port = 5177;
    };
  };

  uvAppFixture = fs.toSource {
    root = ./fixtures/uv-app;
    fileset = fs.unions [
      ./fixtures/uv-app/pyproject.toml
      ./fixtures/uv-app/src
      ./fixtures/uv-app/uv.lock
    ];
  };

  uvApplication = ix.buildUvApplication pkgs {
    pname = "uv-app-fixture";
    version = "0.1.0";
    src = uvAppFixture;
    pyChecker = "zuban";
  };

  uvLockedDistribution = builtins.head uvApplication.uvWheelhouse.lock.distributions;
  uvWheelhouseDistributionNames =
    map (
      distribution: distribution.fileName
    )
    uvApplication.uvWheelhouse.distributions;

  fleet = ix.mkFleet {
    # Stand-in for the ix-side CAS manifest builder (the cas-layer.nix
    # contract): a directory with manifest.cas + locator.bin, carrying
    # passthru.toplevel. Only this fixture threads a builder; the other fleet
    # fixtures deliberately leave it unset, proving plan eval never forces
    # `casImage`.
    defaults = [
      (
        {pkgs, ...}: {
          ix.build.casImageBuilder = {toplevel}:
            pkgs.runCommand "cas-image-stub" {passthru = {inherit toplevel;};} ''
              mkdir -p "$out"
              touch "$out/manifest.cas" "$out/locator.bin"
            '';
        }
      )
    ];
    deployment.region = "us-west-1";
    # Fleet-wide per-VM user-store secret attachment; merges with per-node refs.
    deployment.secrets = {
      fleet_default = {
        env = "FLEET_DEFAULT";
      };
    };

    nodes = {
      db = {
        services.ix-postgresql.enable = true;
      };

      web = {
        tags = ["public"];
        groups = ["public-apps"];
        deployment = {
          destination = "fleet-web:latest";
          ipv4 = true;
          secrets.github_token.env = "GH_TOKEN";
        };
        modules = [
          (
            {nodes, ...}: {
              services.remote-desktop.enable = true;
              environment.etc.db-host.text = nodes.db.config.networking.hostName;
            }
          )
        ];
      };

      worker = {
        replicas = 2;
        dependsOn = ["db"];
        updateStrategy.maxUnavailable = 1;
        modules = [
          {
            services.remote-desktop.enable = true;
          }
        ];
      };
    };
  };

  fleetPlan = fleet.planValue.nodes;

  prefixedFleetBase = ix.mkFleet {
    nodes = {
      api = {
        services.openssh.enable = true;
      };
      worker = {
        dependsOn = ["api"];
        groups = ["private-apps"];
        modules = [
          (
            {nodes, ...}: {
              environment.etc.api-host.text = nodes.api.config.networking.hostName;
            }
          )
        ];
      };
    };
  };

  prefixedFleet = prefixedFleetBase.withNodePrefix "tprefix-";

  # `ix.networking.ipv4` is the image-level twin of `deployment.ipv4`, and the
  # same option `ix apply` reads off a flake target before it creates the VM.
  # Either source turns the address on; a node that declares neither keeps the
  # plan default off, so nothing silently starts paying for an address.
  declaredIpv4Fleet = ix.mkFleet {
    nodes = {
      edge.modules = [
        {
          ix.networking.ipv4 = true;
          # A host-side reachability probe is exactly the check the fleet eval
          # refuses without an address, so it doubles as proof the assertion
          # now accepts the image-declared source.
          ix.healthChecks.public-reachable = {
            description = "answers on its public address";
            from = "host";
            requiresIpv4 = true;
            command = ["true"];
          };
        }
      ];
      internal.modules = [{}];
      # Declared nowhere in the image, added by the deployment: the other
      # direction of the union has to keep working.
      deployed.deployment.ipv4 = true;
    };
  };

  declaredIpv4Plan = declaredIpv4Fleet.planValue.nodes;

  # No casImageBuilder is threaded into prefixedFleetBase, so forcing a CAS
  # image must abort with the module system's "used but not defined" error
  # rather than falling back to anything.
  fleetMissingCasBuilderEval = builtins.tryEval (builtins.seq prefixedFleetBase.packages.api true);

  # A local-build node: its source switch runs a plain `nix build`, so the
  # default installable must be the `.#<node>-system` package alias, not the
  # bare `.#<node>` (which only `ix apply`'s resolver expands).
  localBuildFleet = ix.mkFleet {
    nodes.svc = {
      deployment.switch.buildOn = "local";
      modules = [{}];
    };
  };

  # An explicit `sourceInstallable` that happens to equal the bare default must
  # survive `withNodePrefix` unchanged: prefixing keys on provenance (user-set
  # vs defaulted), not on the rendered string.
  explicitInstallableFleet = ix.mkFleet {
    nodes.svc = {
      deployment.switch.sourceInstallable = ".#svc";
      modules = [{}];
    };
  };

  fleetIpv4HealthCheckEval = builtins.tryEval (
    builtins.deepSeq
    (ix.mkFleet {
      nodes.private.modules = [
        {
          ix.healthChecks.public-reachability = {
            from = "host";
            requiresIpv4 = true;
            command = ["true"];
          };
        }
      ];
    }).planValue.nodes.private.healthChecks.public-reachability
    true
  );

  fleetUnknownDependencyEval = builtins.tryEval (
    builtins.deepSeq
    (ix.mkFleet {
      nodes.web = {
        dependsOn = ["db"];
        modules = [
          {
            services.remote-desktop.enable = true;
          }
        ];
      };
    }).planValue.nodes.web.dependsOn
    true
  );

  # `deployment.healthChecks` was historically written as if it selected
  # checks to wait for; nothing ever read it. The plan always carries every
  # declared `ix.healthChecks`, so the dead key must fail eval, not be
  # silently dropped.
  fleetDeploymentHealthChecksEval = builtins.tryEval (
    builtins.deepSeq
    (ix.mkFleet {
      nodes.web = {
        deployment.healthChecks = ["nginx"];
        modules = [{}];
      };
    }).planValue.nodes.web.region
    true
  );

  fleetUnknownDeploymentKeyEval = builtins.tryEval (
    builtins.deepSeq
    (ix.mkFleet {
      nodes.web = {
        deployment.regoin = "us-west-1";
        modules = [{}];
      };
    }).planValue.nodes.web.region
    true
  );

  fleetDependencyCycleEval = builtins.tryEval (
    builtins.deepSeq
    (ix.mkFleet {
      nodes = {
        api = {
          dependsOn = ["worker"];
          modules = [{}];
        };

        worker = {
          dependsOn = ["api"];
          modules = [{}];
        };
      };
    }).planValue.nodes
    true
  );

  # `maxSurge` is Kubernetes vocabulary ix-fleet does not implement (there is
  # no surplus capacity to surge into); a typo'd or aspirational key must fail
  # eval rather than silently deploy with unbounded concurrency.
  fleetUnknownUpdateStrategyKeyEval = builtins.tryEval (
    builtins.deepSeq
    (ix.mkFleet {
      nodes.web = {
        replicas = 2;
        updateStrategy.maxSurge = 1;
        modules = [{}];
      };
    }).planValue.nodes.web-0.updateStrategy
    true
  );

  fleetInvalidMaxUnavailableEval = builtins.tryEval (
    builtins.deepSeq
    (ix.mkFleet {
      nodes.web = {
        replicas = 2;
        updateStrategy.maxUnavailable = 0;
        modules = [{}];
      };
    }).planValue.nodes.web-0.updateStrategy
    true
  );

  factionsExample = let
    fleet = ix.importIx (paths.examples + "/minecraft/factions/default.ix") {
      index = {
        lib = ix;
      };
    };
    config = fleet.nodes.factions;
    service = config.systemd.services.minecraft-world-border;
  in {
    inherit fleet config service;
    cfg = config.services.minecraft;
    managed = {
      config = config.environment.etc."minecraft/managed-config".source;
      datapacks = config.environment.etc."minecraft/managed-datapacks".source;
      dropins = config.environment.etc."minecraft/managed-dropins".source;
      serverFiles = config.environment.etc."minecraft/managed-server-files".source;
    };
  };

  survivalExample = let
    fleet = ix.importIx (paths.examples + "/minecraft/survival/default.ix") {
      index = {
        lib = ix;
      };
    };
    config = fleet.nodes.survival;
  in {
    inherit fleet config;
    inherit
      (config.services)
      floodgate
      geyser
      minecraft
      velocity
      ;
    managed = {
      minecraftConfig = config.environment.etc."minecraft/managed-config".source;
      minecraftServerFiles = config.environment.etc."minecraft/managed-server-files".source;
      velocityConfig = config.environment.etc."velocity/managed-config".source;
      velocityPlugins = config.environment.etc."velocity/managed-plugins".source;
    };
  };

  dailyScraperExample = let
    fleet = ix.importIx (paths.examples + "/python/daily-scraper/default.ix") {
      index = {
        lib = ix;
      };
    };
    config = fleet.nodes.scraper;
  in {
    inherit fleet config;
    plan = fleet.planValue.nodes.scraper;
    service = config.systemd.services.daily-scraper;
    timer = config.systemd.timers.daily-scraper;
  };

  nginxLifecycleExample = let
    fleet = ix.importIx (paths.examples + "/nginx/lifecycle/default.ix") {
      index = {
        lib = ix;
      };
    };
    config = fleet.nodes.nginx;
  in {
    inherit fleet config;
    cfg = config.services.nginx;
    plan = fleet.planValue.nodes.nginx;
  };

  s3StorageExample = let
    fleet = ix.importIx (paths.examples + "/s3/storage/default.ix") {
      index = {
        lib = ix;
      };
    };
    config = fleet.nodes.s3;
  in {
    inherit fleet config;
    cfg = config.services.ix-seaweedfs;
    plan = fleet.planValue.nodes.s3;
  };

  # Replicas, dependsOn, and updateStrategy are fleet-evaluator features no
  # example exercises anymore (ix#8306 removed fleets from the product
  # surface), so this inline spec keeps them covered until the fleet layer
  # itself goes. It reuses the multi-vm/hello modules: the worker resolves
  # the web listener through the `nodes` module argument either way.
  replicasFleet = let
    fleet = ix.mkFleet {
      nodes = {
        web.modules = [(paths.examples + "/multi-vm/hello/web.nix")];
        worker = {
          replicas = 3;
          dependsOn = ["web"];
          updateStrategy.maxUnavailable = 1;
          modules = [(paths.examples + "/multi-vm/hello/worker.nix")];
        };
      };
    };
  in {
    inherit fleet;
    web.plan = fleet.planValue.nodes.web;
    worker.plan = fleet.planValue.nodes.worker-0;
  };

  # The converted example itself: one VM per role, wired through `mkVm`'s
  # `nodes` (peer) argument instead of a fleet spec.
  multiVmHelloExample = let
    example = ix.importIx (paths.examples + "/multi-vm/hello/default.ix") {
      index = {
        lib = ix;
      };
    };
  in {
    worker.plan = example.vms.worker-0.planValue.nodes.worker-0;
  };

  # Same evaluator-coverage story as `replicasFleet`, for replica fan-out:
  # a dependsOn on a replicated node, and the gateway's upstream pool and
  # per-replica probes, expand across every replica.
  replicaFanoutFleet = let
    fleet = ix.mkFleet {
      nodes = {
        cache.modules = [(paths.examples + "/multi-vm/microservices/cache.nix")];
        api = {
          replicas = 3;
          dependsOn = ["cache"];
          updateStrategy.maxUnavailable = 1;
          modules = [(paths.examples + "/multi-vm/microservices/api.nix")];
        };
        gateway = {
          dependsOn = ["api"];
          modules = [(paths.examples + "/multi-vm/microservices/gateway.nix")];
        };
      };
    };
  in {
    inherit fleet;
    gateway = {
      config = fleet.nodes.gateway;
      plan = fleet.planValue.nodes.gateway;
    };
    api.plan = fleet.planValue.nodes.api-0;
    cache.plan = fleet.planValue.nodes.cache;
  };

  # The converted example itself, wired through `mkVm`'s `nodes` (peer)
  # argument instead of a fleet spec.
  multiVmMicroservicesExample = let
    example = ix.importIx (paths.examples + "/multi-vm/microservices/default.ix") {
      index = {
        lib = ix;
      };
    };
  in {
    # The config's own return value, for the `vm-templates` group's
    # pass-through assertion: a config exporting no `templates` must come back
    # out of `renderConfig` exactly as it went in, and checking that against a
    # real config rather than a literal is what keeps it true as this one
    # changes.
    inherit example;
    gateway.config = example.vms.gateway.nodes.gateway;
  };

  # VM templates, RFC 0042 / ix#9242: a config's `templates` + `instances`
  # exports rendered through `index.lib.templates`. Two halves here. The
  # example is the real path -- one config through `renderConfig`, with every
  # rendered node's toplevel forced in the assertions below. The stubs are the
  # guards, one deliberate mistake each, because a guard nobody has watched
  # fail is not a guard.
  vmTemplatesExample = let
    example = ix.importIx (paths.examples + "/templates/workers/default.ix") {
      index = {
        lib = ix;
      };
    };
    rendered = ix.templates.renderConfig example;
    # A template's entire contract is "returns what mkVm returns", so a stub
    # returning that shape reaches every guard without evaluating a NixOS
    # closure.
    stubs = {
      worker = {
        instance,
        name,
        port ? 8080,
      }: {
        nixosConfigurations."${name}" = {inherit instance port;};
      };
      # Names its VM the way RFC 0042 writes it (`worker@1`). That string
      # fails nixpkgs' `networking.hostName` type, so catching it at this
      # boundary is the difference between one line and a module-system trace.
      atNamed = {instance, ...}: {
        nixosConfigurations."worker@${instance}" = {};
      };
      # Forgets `instance`, so every instance of it renders the same node and
      # a plain `//` merge keeps only the last, silently.
      collapsing = _: {nixosConfigurations.worker = {};};
    };
    # Hand-picked rather than generated: the repo has no Nix property-test
    # framework (hegel is Python/Hypothesis, and this is pure eval), so a
    # generator would mean writing one first. This is the adversarial table a
    # generator would have to be steered into anyway -- the node separator
    # inside either half, both halves one character, digits, underscores, and
    # the pair whose node names collide.
    roundTripPairs = [
      {
        template = "worker";
        instance = "1";
      }
      {
        template = "worker-pool";
        instance = "1";
      }
      {
        template = "worker";
        instance = "pool-1";
      }
      {
        template = "w";
        instance = "e";
      }
      {
        template = "worker_pool";
        instance = "eu_west_1";
      }
      {
        template = "9lives";
        instance = "0";
      }
      {
        template = "a-b_c";
        instance = "1-2_3";
      }
    ];
    # nixpkgs' own rule for `networking.hostName`, read out of the module that
    # declares it rather than copied alongside it. `lib/templates.nix` mirrors
    # this pattern because the option declares it inline and there is no
    # `lib.types.hostName` to call; this is what makes the mirror safe, by
    # failing the suite the day a bump changes the rule.
    #
    # `findSingle` is the guard on the guard. It returns its `multiple` argument
    # when more than one line matches, so if that file ever grows a second
    # `strMatching` the extraction yields null and the assertion below fails
    # loudly, rather than silently pinning whichever one came first.
    hostNameTypeLine =
      lib.findSingle (line: lib.hasInfix "strMatching" line) null null
      (lib.splitString "\n" (
        builtins.readFile (nixpkgs + "/nixos/modules/tasks/network-interfaces.nix")
      ));
    nixpkgsHostNamePattern = builtins.head (
      builtins.match ".*strMatching \"(.*)\";" hostNameTypeLine
    );
    # Node names, not instance names: this table is about the hostname rule
    # itself, so comparing verdicts needs neither a separator nor a parse. The
    # middle three are the cases #4452 got wrong.
    nodeNameRows = [
      "worker-1"
      "worker-1-"
      "worker-a_"
      (lib.concatStrings (lib.genList (_: "9") 70))
      "worker--1"
      "w"
      "9"
      "worker-pool-eu_west-1"
    ];
  in {
    inherit rendered roundTripPairs nixpkgsHostNamePattern nodeNameRows;
    nodeNames = builtins.attrNames rendered.nixosConfigurations;
    nodeOf = pair: (ix.templates.parseInstanceName (ix.templates.instanceNameOf pair)).node;
    shardsOf = node: rendered.nixosConfigurations."${node}".config.services.nginx.appendConfig;
    portOf = node: rendered.nixosConfigurations."${node}".config.ix.networking.expose.http.port;
    render = args: ix.templates.renderInstance ({templates = stubs;} // args);
    renderStubConfig = config: ix.templates.renderConfig (config // {templates = stubs;});
    # `tryEval` catches `throw` and a failed `assert`, which is what every
    # guard in lib/templates.nix raises. It does NOT catch nix's own
    # unexpected-argument error, which is why an unknown param is not asserted
    # here: `renderInstance` leaves that case to nix deliberately, since
    # `builtins.functionArgs` cannot tell `{ a, ... }` from `{ a }`. The
    # checked, pre-eval version is the CLI's, against the schema ix2nix
    # generates from a template's annotations (index#4450).
    throws = expression: !(builtins.tryEval expression).success;
  };

  k8sK3sExample = let
    example = ix.importIx (paths.examples + "/k8s/k3s/default.ix") {
      index = {
        lib = ix;
      };
    };
  in {
    server = example.nodes.k3s-server;
    agent = example.nodes.k3s-agent-0;
  };

  nomadClusterExample = let
    example = ix.importIx (paths.examples + "/nomad/cluster/default.ix") {
      index = {
        lib = ix;
      };
    };
  in {
    server = example.vms.nomad-server.nodes.nomad-server;
    client = example.vms.nomad-client-0.nodes.nomad-client-0;
  };

  observabilityStackExample = let
    example = ix.importIx (paths.examples + "/observability/stack/default.ix") {
      index = {
        lib = ix;
      };
    };
    queryTool = config:
      lib.findFirst (
        package: (package.meta.mainProgram or null) == "ix-observe"
      )
      null
      config.environment.systemPackages;
  in {
    observability = let
      config = example.vms.observability.nodes.observability;
    in {
      inherit config;
      cfg = config.services.ix-observability;
      collector = config.services.opentelemetry-collector.settings;
      grafana = config.services.grafana;
      plan = example.vms.observability.planValue.nodes.observability;
      queryTool = queryTool config;
      dashboardPath =
        (builtins.elemAt config.services.grafana.provision.dashboards.settings.providers 0).options.path;
    };
    app = let
      config = example.vms.app.nodes.app;
    in {
      inherit config;
      cfg = config.services.ix-observability;
      collector = config.services.opentelemetry-collector.settings;
      plan = example.vms.app.planValue.nodes.app;
    };
  };

  dailyScraperS3 = let
    config = evalConfig [
      (paths.examples + "/python/daily-scraper/service.nix")
      {
        _module.args.dailyScraper = {
          s3 = {
            uri = "s3://andrew-scraper-output/github";
            deleteRemoved = true;
            awsEnvironmentFile = "/run/secrets/daily-scraper/aws.env";
          };
        };
      }
    ];
  in {
    inherit config;
    service = config.systemd.services.daily-scraper;
  };

  extendedAttributes = let
    config = evalConfig [
      {
        ix.extendedAttributes."/build/ix-xattr-test" = {
          create = true;
          attributes = {
            "user.ix.kind" = "test.path";
            "user.ix.owner" = "ix";
          };
        };
      }
    ];
  in {
    inherit config;
    activationScript = config.system.activationScripts.ix-extended-attributes.text;
  };

  portClaimConflictFailures = failedAssertionsFor [
    {
      services.remote-desktop = {
        enable = true;
        port = 6080;
      };

      services.resource-monitor = {
        enable = true;
        port = 6080;
      };
    }
  ];

  remoteDesktopUnauthenticatedFirewallFailures = failedAssertionsFor [
    {
      services.remote-desktop = {
        enable = true;
        openFirewall = true;
      };
    }
  ];

  remoteDesktopSettingsAuthFirewallFailures = failedAssertionsFor [
    {
      services.remote-desktop = {
        enable = true;
        openFirewall = true;
        auth = "file";
        settings.auth = "none";
      };
    }
  ];

  remoteDesktopBindTcpDriftFailures = failedAssertionsFor [
    {
      services.remote-desktop = {
        enable = true;
        bindAddress = "0.0.0.0";
        port = 6080;
        settings.bind-tcp = "0.0.0.0:6081";
      };
    }
  ];

  resourceMonitorRuntimeDirectoryFailures = let
    failuresFor = runtimeDirectory:
      failedAssertionsFor [
        {
          services.resource-monitor = {
            enable = true;
            inherit runtimeDirectory;
          };
        }
      ];
  in
    map failuresFor [
      "/var/lib/resource-monitor"
      "/run//resource-monitor"
      "/run/resource-monitor/."
      "/run/resource-monitor/../stats"
    ];

  minecraftUnsafeManagedPathFailures = failedAssertionsFor [
    minecraftModule
    defaultMinecraftModule
    {
      services.minecraft = {
        configFiles."client//bad.toml" = {};
        configFiles."/absolute/bad.toml" = {};
        properties.level-name = "../bad-world";
        serverFiles."plugins/../bukkit.yml" = {};
        serverFiles."$(bad).json" = {};
        datapacks.bad = {
          fileName = "../bad";
          files."data/../bad.json" = {};
        };
      };
    }
  ];

  velocityUnsafeManagedPathFailures = failedAssertionsFor [
    {
      services.velocity = {
        enable = true;
        configFiles."plugins/../bad.toml" = {};
        plugins.bad = {
          src = pkgs.writeText "velocity-test-plugin.jar" "";
          fileName = "nested/bad.jar";
        };
      };
    }
  ];

  velocityDuplicatePluginFileNameFailures = failedAssertionsFor [
    {
      services.velocity = {
        enable = true;
        plugins = {
          first = {
            src = pkgs.writeText "velocity-test-first-plugin.jar" "";
            fileName = "shared.jar";
          };

          second = {
            src = pkgs.writeText "velocity-test-second-plugin.jar" "";
            fileName = "shared.jar";
          };
        };
      };
    }
  ];
  velocityConcreteAddress = evalConfig [
    {
      services.velocity = {
        enable = true;
        address = "10.0.0.5";
        port = 25_570;
        openFirewall = false;
      };
    }
  ];

  relativePathUnsafeShellEval = builtins.tryEval (
    builtins.deepSeq (ix.relativePath.shellPath "$out" "../bad") true
  );

  portClaimNamespaceAllowedFailures = failedAssertionsFor [
    {
      ix.networking.portClaims = {
        left = {
          protocol = "tcp";
          port = 1234;
          namespace = "left-netns";
        };

        right = {
          protocol = "tcp";
          port = 1234;
          namespace = "right-netns";
        };
      };
    }
  ];

  portClaimAddressFamilyAllowedFailures = failedAssertionsFor [
    {
      services.minecraft-bedrock = {
        enable = true;
        port = 19_132;
        portv6 = 19_132;
      };
    }
  ];

  base = let
    config = evalConfig [];
    imageConfig = evalConfig [(paths.root + "/images/system/base")];
  in {
    inherit config imageConfig;
    cfg = config.ix.profiles.base;
  };

  # Regression fence for the guest-nix-slowness bug family. Do NOT delete as
  # redundant with the registry-pin assertion: this is the THIRD regression in
  # the same family, each of which passed the prior guard yet still left a fresh
  # VM re-ingesting a ~45k-file source tree through VCFS on first `nix`:
  #   1. missing narHash  — unlocked path: pin, re-hashed every eval (#1748/#1749)
  #   2. missing validity — narHash added, but narHash only short-circuits a path
  #      nix already considers VALID (#1815)
  #   3. missing DB       — the OCI image baked no /nix/var/nix/db/db.sqlite at
  #      all, so the pinned source (present in the image) was never valid; fixed
  #      by `includeNixDB = true` in oci-layer.nix
  #   4. wrong journal mode — the DB was baked WAL-marked (build nix defaults
  #      `use-sqlite-wal = true`) while the image's nix.conf set it false; the
  #      guest's shm-less dotfile-VFS open then fails outright and every
  #      nix-daemon connection resets (ix#6563)
  # The boundary this defends: the built base OCI archive must ship a populated
  # /nix/var/nix/db/db.sqlite whose ValidPaths includes EVERY source the image
  # bakes for in-guest eval — otherwise the first in-guest `nix` re-ingests it —
  # and whose journal mode the image's own nix.conf can actually open.
  # Two classes of baked source, both covered here:
  #   - flake registry pins (nixpkgs and, once baked, index), read off
  #     `nix.registry.*.to.path`; and
  #   - sources baked WITHOUT a registry pin via `system.extraDependencies`
  #     (rust-overlay — forced when index's flake evaluates under `nix run`,
  #     but reached through no registry row), read off that option. The
  #     registry-derived projection cannot catch these, precisely because they
  #     are not registry pins, so they need their own list.
  # A bare db.sqlite, or the registry pin alone, is not enough. Deriving the
  # required paths from the image's OWN config (the registry and
  # `system.extraDependencies`) means a future pin or baked source is covered
  # automatically and this can never silently drift from what ships. This check
  # builds the real base archive and asserts exactly that, so it also proves the
  # DB survives oci-image-builder's re-streaming. It builds an OCI image, so it
  # is its own check (not the pure-eval `eval` aggregate) and runs on Linux.
  # It also defends the self-contained invariant: every layer blob the manifest
  # references must be embedded in the archive as a regular file. The builder
  # once wrote the customisation blob as a symlink into /nix/store, so the
  # archive depended on out-of-tar store state and this check failed whenever
  # the referenced path was not visible at read time (index#2058).
  # All three shapes of `self` the guest `index` registry pin must serve
  # (lib/image/registry-pin.nix). The boundary broke twice on 2026-07-22:
  # unconditional `inherit (self) narHash` failed eval on the narHash-less
  # path-locked seam shape (index#3981), and the fetchTree fallback tried
  # next (#3984) was rejected by pure eval on lazy-tree subpaths. #3988
  # landed the conditional-omit shape. ix#9290 then de-submoduled index into
  # an ordinary ix subdirectory, which added the two SUBPATH shapes: a pin on
  # `<ix-source>/index` is not a store path at all, so nix re-dumps the tree
  # (the whole cost the pin exists to avoid) and `base-image-nix-db` can find
  # no ValidPaths row for it however the image is built. Mock selves because
  # the real `self` only ever has one shape per evaluation.
  imageRegistryPinAssertions = let
    registryPin = import (paths.root + "/lib/image/registry-pin.nix") {inherit lib;};
    # The construction never validates `narHash`, so a labeled mock keeps the
    # fixture honest; a plausible SRI literal would read as a real pin.
    mockNarHash = "sha256-mock+image-registry-pin";
    # Store-path shaped down to the 32-character digest, because that is what
    # the construction matches on, but spelled so nobody mistakes it for a real
    # path.
    mockPath = "${builtins.storeDir}/mockimageregistrypinmockdigest00-source";
    # A subpath of one, which is exactly what a relative-path input
    # (`index.url = "path:./index"`) and a `?dir=index` git ref both hand over.
    mockSubPath = mockPath + "/index";
    # Any real directory: these assertions are about the SHAPE the copy comes
    # out as, not its contents, and a small one keeps the eval-time copy cheap.
    mockSourceRoot = paths.root + "/lib/image";
    # Forcing this is the bug the first assertion below exists to catch: a
    # `self` that already names a pinnable store path must be pinned as it
    # stands, never copied again.
    unusedSourceRoot = throw "registryPin copied sourceRoot for a self whose outPath is already a top-level -source store path";
    subPathPin = self:
      registryPin {
        inherit self;
        sourceRoot = mockSourceRoot;
      };
    # ix's `index.url = "path:./index"`: a subpath, and no narHash anywhere.
    seamPin = subPathPin {outPath = mockSubPath;};
    # `nix build ./index#...` inside an ix checkout, which resolves to
    # `git+file://...?dir=index`: the same subpath, plus a narHash that belongs
    # to the enclosing repository rather than to the pinned subtree.
    dirPin = subPathPin {
      outPath = mockSubPath;
      narHash = mockNarHash;
    };
  in [
    {
      assertion =
        registryPin {
          self = {
            outPath = mockPath;
            narHash = mockNarHash;
          };
          sourceRoot = unusedSourceRoot;
        }
        == {
          type = "path";
          path = mockPath;
          narHash = mockNarHash;
        };
      message = "a narHash-bearing self already on a top-level -source store path must be pinned as it stands, narHash kept: an unlocked path pin re-hashes and re-copies the source tree on every in-guest eval (#1748)";
    }
    {
      assertion =
        registryPin {
          self = {outPath = mockPath;};
          sourceRoot = unusedSourceRoot;
        }
        == {
          type = "path";
          path = mockPath;
        };
      message = "a narHash-less self (path-locked seam input, index#3981) must yield a pin that omits narHash; the whole-attrset equality also proves the construction evaluates on that shape";
    }
    {
      # nix's PathInputScheme (libfetchers/path.cc) reaches its no-copy path
      # only for a store path named exactly `source`, so this spells out the
      # digest rather than accepting any `-source` suffix: `<digest>-index-source`
      # would satisfy the loose form and still be re-dumped on every eval.
      assertion = builtins.match "${builtins.storeDir}/[a-z0-9]{32}-source" seamPin.path != null;
      message = "a self whose outPath is a subpath (ix#9290's `path:./index`) must be pinned on a copy that is itself a top-level store path named exactly `source`, or nix re-dumps the tree on every in-guest eval and no ValidPaths row can ever match it";
    }
    {
      assertion = !(seamPin ? narHash);
      message = "a subpath self must not carry a narHash into the pin";
    }
    {
      # The one shape carrying BOTH a subpath outPath and a narHash.
      assertion = !(dirPin ? narHash);
      message = "a ?dir= self's narHash describes the enclosing repository, not the pinned subtree, so it must be dropped rather than locking the pin to content it does not name";
    }
  ];

  baseImageNixDb = let
    # Every `path`-type registry pin the image ships. Guard on the type so a
    # non-path entry (e.g. a default indirect/github registry row) can't break
    # the projection; those don't bake a source and aren't what this defends.
    registryPaths = lib.pipe base.imageConfig.nix.registry [
      builtins.attrValues
      (builtins.filter (entry: (entry.to.type or null) == "path" && entry.to ? path))
      (map (entry: entry.to.path))
    ];
    # Sources baked into the system closure without a registry pin (the
    # `extraBakedSources` list in lib/image/default.nix). `toString` so a path
    # value renders as its store path for the sqlite lookup below.
    extraBakedPaths = map toString base.imageConfig.system.extraDependencies;
  in
    pkgs.runCommand "ix-test-base-image-nix-db" {
      nativeBuildInputs = [
        pkgs.gnutar
        pkgs.sqlite
        pkgs.gnugrep
        pkgs.coreutils
        pkgs.jq
      ];
      archive = base.imageConfig.ix.build.ociImage;
      requiredPaths = registryPaths ++ extraBakedPaths;
      # SQLite header bytes 18/19 (file-format versions): "1 1" = rollback
      # journal, "2 2" = WAL. Derived from the image's OWN nix.conf so the
      # assertion tracks the setting instead of hardcoding a mode.
      expectedDbFormat =
        if base.imageConfig.nix.settings.use-sqlite-wal or true
        then "2 2"
        else "1 1";
    } ''
      mkdir extract db
      tar -C extract -xf "$archive"
      # Walk only the layer blobs the manifest declares: config/manifest JSON
      # blobs share blobs/sha256/ and are not tars, so globbing the dir would
      # need tar errors suppressed, and that suppression is what let a dangling
      # store-symlink blob read as "db not here" instead of failing (index#2058).
      manifest_digest=$(jq -r '.manifests[0].digest' extract/index.json | cut -d: -f2)
      layer_digests=$(jq -r '.layers[].digest' "extract/blobs/sha256/$manifest_digest" | cut -d: -f2)
      found=
      for digest in $layer_digests; do
        blob="extract/blobs/sha256/$digest"
        if [ -L "$blob" ] || [ ! -f "$blob" ]; then
          echo "error: layer blob sha256:$digest is missing or not a regular file; the archive is not self-contained (index#2058)" >&2
          exit 1
        fi
        # No stderr suppression: a layer that cannot be listed is a corrupt
        # archive and must fail loudly, not read as "db not here".
        listing=$(tar -tf "$blob")
        if grep -qx './nix/var/nix/db/db.sqlite' <<< "$listing"; then
          tar -C db -xf "$blob" ./nix/var/nix/db/db.sqlite
          found=1
        fi
      done
      if [ -z "$found" ]; then
        echo "error: no OCI layer contains nix/var/nix/db/db.sqlite; the image ships no store DB (includeNixDB off?)" >&2
        exit 1
      fi
      dbfile=db/nix/var/nix/db/db.sqlite
      # Every registry-pinned source must be a VALID path in the shipped DB, or
      # the guest re-ingests it on first eval.
      for src in $requiredPaths; do
        # ValidPaths only ever holds top-level store paths, so a subpath scores
        # zero rows for a reason that has nothing to do with what the image
        # ships, and reporting it as a missing row sends the reader hunting
        # through the DB instead of at the pin. ix#9290 made exactly this
        # mistake reachable by turning index into an ix subdirectory, so
        # `self.outPath` became `<ix-source>/index`. Say which failure it is.
        case "$src" in
          /nix/store/*/*)
            echo "error: baked source $src is a subpath of a store path, not a store path of its own; nix's PathInputScheme re-dumps the tree rather than using it, and no ValidPaths row can ever match (ix#9290)" >&2
            exit 1
            ;;
        esac
        count=$(sqlite3 "$dbfile" \
          "SELECT count(*) FROM ValidPaths WHERE path = '$src';")
        if [ "$count" != "1" ]; then
          echo "error: registry-pinned source $src is not registered valid in the image nix DB (found $count rows)" >&2
          echo "ValidPaths sample:" >&2
          sqlite3 "$dbfile" "SELECT path FROM ValidPaths LIMIT 20;" >&2
          exit 1
        fi
      done
      # Journal-mode agreement (regression 4, ix#6563). A complete ValidPaths
      # is worthless if the guest cannot open the DB at all: with
      # `use-sqlite-wal = false` nix opens it on SQLite's unix-dotfile VFS (no
      # shared memory), which refuses WAL-marked databases (SQLITE_CANTOPEN),
      # and every nix-daemon connection dies with "Connection reset by peer".
      format=$(od -An -tu1 -j18 -N2 "$dbfile")
      format=$(echo $format)
      if [ "$format" != "$expectedDbFormat" ]; then
        echo "error: baked db.sqlite header format is '$format' but the image's use-sqlite-wal setting requires '$expectedDbFormat'; the guest cannot open this DB (ix#6563)" >&2
        exit 1
      fi
      mkdir -p "$out"
    '';

  # --- Language helpers -----------------------------------------------------

  languages = {
    elixirLatest = ix.languages.elixir.toolchain pkgs {version = "latest";};
    erlangLatest = ix.languages.erlang.toolchain pkgs {version = "latest";};
    # Forcing `drvPath` is the whole point, and it covers every version
    # table in `lib/languages`, not just the BEAM ones. Nixpkgs retires an
    # attribute in three shapes and only one of them is visible at lookup:
    # a `throw` alias fails immediately, a dropped attribute raises
    # `attribute missing`, but a `warnAlias` wraps every attribute of the
    # derivation except meta/name/type/outputName in `lib.warn`, so the
    # entry resolves fine until something forces it -- and then
    # `abort-on-warn` kills the eval of whatever host or image reached it.
    # An insecure package (gradle 7) hides the same way, behind an assert
    # on `drvPath`. Advertising a version this repo cannot instantiate is
    # the bug; forcing every advertised version is the check.
    languageTableDrvPaths = builtins.concatStringsSep " " (
      builtins.map (version: (ix.languages.elixir.toolchain pkgs {inherit version;}).drvPath) ["latest" "1.18" "1.19" "1.20"]
      ++ builtins.map (version: (ix.languages.erlang.toolchain pkgs {inherit version;}).drvPath) ["latest" "27" "28" "29"]
      ++ builtins.map (version: (ix.languages.go.toolchain pkgs {inherit version;}).drvPath) ["latest" "1.25" "1.26"]
      ++ builtins.map (version: (ix.languages.zig.toolchain pkgs {inherit version;}).drvPath) ["latest" "0.13" "0.14" "0.15" "0.16"]
      ++ builtins.map (version: (ix.languages.javascript.node pkgs {inherit version;}).drvPath) ["latest" "22" "24" "26"]
      ++ builtins.map (version: (ix.languages.python.interpreter pkgs {inherit version;}).drvPath) ["3.11" "3.12" "3.13" "3.14"]
      ++ builtins.map (version:
        (ix.languages.cpp.compiler pkgs {
          vendor = "gcc";
          inherit version;
        })
        .drvPath) ["latest" "13" "14" "15" "16"]
      ++ builtins.map (version:
        (ix.languages.cpp.compiler pkgs {
          vendor = "clang";
          inherit version;
        })
        .drvPath) ["latest" "18" "19" "20" "21" "22"]
      ++ builtins.concatMap (distribution:
        builtins.map (version:
          (ix.languages.java.jdk pkgs {
            inherit distribution version;
          })
          .drvPath)
        (
          if distribution == "corretto"
          then ["11" "17" "21" "25"]
          else ["8" "11" "17" "21" "25"]
        )) ["openjdk" "temurin" "corretto" "zulu"]
      ++ builtins.map (version: (ix.languages.java.gradle pkgs {inherit version;}).drvPath) ["8" "9"]
    );
    erlangRebarDefault = ix.languages.erlang.rebar3 pkgs {};
    erlangRebarExplicit = ix.languages.erlang.rebar3 pkgs {erlang = pkgs.beamPackages.erlang;};
    pythonMissingVersion = builtins.tryEval (
      builtins.deepSeq (ix.languages.python.interpreter pkgs {}).pythonVersion true
    );
    pythonUnknown = builtins.tryEval (
      builtins.deepSeq (ix.languages.python.interpreter pkgs {version = "3.99";}).pythonVersion true
    );

    rustMissingVersion = builtins.tryEval (
      builtins.deepSeq (ix.languages.rust.toolchain pkgs {channel = "nightly";}).name true
    );
    rustPinnedNightly = ix.languages.rust.toolchain pkgs {
      channel = "nightly";
      version = rustPinnedNightlyDate;
    };
    rustExtraComponents = ix.languages.rust.toolchain pkgs {
      channel = "nightly";
      version = rustPinnedNightlyDate;
      components = [
        "cargo"
        "rust-std"
        "rustc"
        "rust-src"
        "rustfmt"
      ];
    };
    rustBadChannel = builtins.tryEval (
      builtins.deepSeq (ix.languages.rust.toolchain pkgs {channel = "nighty";}).name true
    );
    rustBadProfile = builtins.tryEval (
      builtins.deepSeq (ix.languages.rust.toolchain pkgs {profile = "extreme";}).name true
    );

    javaMissingDistribution = builtins.tryEval (
      builtins.deepSeq (ix.languages.java.jdk pkgs {version = "21";}).name true
    );
    javaBadDistribution = builtins.tryEval (
      builtins.deepSeq
      (ix.languages.java.jdk pkgs {
        version = "21";
        distribution = "openjdkk";
      }).name
      true
    );
    javaBadVersion = builtins.tryEval (
      builtins.deepSeq
      (ix.languages.java.jdk pkgs {
        version = "22";
        distribution = "temurin";
      }).name
      true
    );
  };

  # --- Minestom + YourKit wiring -------------------------------------------

  minestomYourkit = let
    yourkitConfig = evalConfig [
      {
        services.minestom = {
          enable = true;
          serverJar = pkgs.runCommand "fake-minestom.jar" {} "touch $out";
          yourkit = {
            enable = true;
            listen = "all";
            openFirewall = true;
            sessionName = "minestom-eval-test";
          };
        };
      }
    ];
    unit = yourkitConfig.systemd.services.minestom;
  in {
    inherit yourkitConfig;
    execStart = unit.serviceConfig.ExecStart;
    firewallTcpPorts = yourkitConfig.networking.firewall.allowedTCPPorts;
    portClaim = yourkitConfig.ix.networking.portClaims.minestom-yourkit or null;
  };

  minestomNoYourkit = let
    noYourkitConfig = evalConfig [
      {
        services.minestom = {
          enable = true;
          serverJar = pkgs.runCommand "fake-minestom.jar" {} "touch $out";
        };
      }
    ];
    unit = noYourkitConfig.systemd.services.minestom;
  in {
    inherit noYourkitConfig;
    execStart = unit.serviceConfig.ExecStart;
    portClaim = noYourkitConfig.ix.networking.portClaims.minestom-yourkit or null;
  };

  minecraftBlocksExample = let
    example = ix.importIx (paths.examples + "/minecraft/blocks/default.ix") {
      index = {
        lib = ix;
      };
    };
    # The buildable artifacts (plugin jar, integration check) built directly
    # so the integration check can be pulled into the `eval` aggregate via
    # `helperScript`.
    packages = import (paths.examples + "/minecraft/blocks/packages.nix") {inherit ix pkgs;};
    schema = import (paths.examples + "/minecraft/blocks/schema.nix") {inherit lib;};
  in {
    inherit packages schema;
    log = let
      config = example.vms.log.nodes.log;
    in {
      inherit config;
      plan = example.vms.log.planValue.nodes.log;
      kafka = config.services.apache-kafka;
    };
    view = let
      config = example.vms.view.nodes.view;
    in {
      inherit config;
      plan = example.vms.view.planValue.nodes.view;
      obs = config.services.ix-observability;
      initUnit = config.systemd.services.mc-blocks-view-init;
    };
    producer = let
      config = example.vms.producer.nodes.producer;
    in {
      inherit config;
      plan = example.vms.producer.planValue.nodes.producer;
      minecraft = config.services.minecraft;
      agent = config.services.ix-observability;
      shipUnit = config.systemd.services.mc-blocks-ship;
    };
  };
  invalidSecretNameEval = builtins.tryEval (
    builtins.deepSeq
    (ix.mkFleet {
      deployment.secrets.BAD_SECRET.env = "BAD_SECRET";
      nodes.web = {
        services.openssh.enable = true;
      };
    }).planValue
    true
  );

  # --- wrapPackage typed argument surface (RFC 0008) -------------------------
  # The module schema must reject unknown keys and a missing `mainProgram` at
  # eval time, and stay introspectable via `.options` / `.optionsDoc`.
  wrappedHello = ix.wrapPackage pkgs {
    package = pkgs.hello;
    # Hostile literal: `$`, backticks, and quotes must reach the wrapper
    # verbatim (heredoc + runtime-shell escaping), never expand at build time.
    env.WRAP_FIXTURE = "literal $HOME `code` \"quoted\"";
    # Exercises the PATH line; the helpers check asserts the wrapper defers
    # `$PATH` to runtime instead of baking the build sandbox PATH.
    pathSuffix = [pkgs.hello];
  };
  wrapPackageTypoEval = builtins.tryEval (
    builtins.seq
    (ix.wrapPackage pkgs {
      package = pkgs.hello;
      symlinkz.hello-alias = "hello";
    }).drvPath
    true
  );
  # A minimal fixture (not an overridden `hello`) so the only reachable throw
  # when forcing the wrapper drvPath is the builder's own missing-`mainProgram`
  # message; a real package's own eval could throw first and pass this
  # vacuously.
  wrapPackageNoMainProgramEval = builtins.tryEval (
    builtins.seq
    (ix.wrapPackage pkgs {
      package = pkgs.stdenv.mkDerivation {
        pname = "wrap-package-no-main-fixture";
        version = "0";
        strictDeps = true;
        dontUnpack = true;
      };
    }).drvPath
    true
  );
  wrapPackageMainProgramDoc =
    lib.findFirst (
      opt: opt.name == "mainProgram"
    )
    null
    ix.wrapPackage.optionsDoc;

  # --- Module and example assertion groups ----------------------------------

  # --- Idiomatic fleet API (expose / healthChecks.unit / endpoint) ----------
  idiomaticExpose = evalConfig [
    {
      networking.hostName = "svc-a";
      ix = {
        networking.expose = {
          web = {
            port = 8080;
            description = "demo web listener";
          };
          metrics = {
            port = 9090;
            # Opened by something else; only register the claim + discovery.
            firewall = false;
          };
          dns = {
            port = 53;
            protocol = "udp";
          };
        };
        healthChecks = {
          web.unit = "nginx";
          cron.unit = "backup.timer";
          ready.http = {
            port = 8080;
            path = "/healthz";
          };
          peer.tcp = {
            host = "svc-b";
            port = 5432;
          };
        };
      };
    }
  ];

  idiomaticUnitConflictFailures = failedAssertionsFor [
    {
      ix.healthChecks.bad = {
        unit = "nginx";
        command = ["true"];
      };
    }
  ];

  idiomaticMultiSugarFailures = failedAssertionsFor [
    {
      ix.healthChecks.bad = {
        unit = "nginx";
        http.port = 8080;
      };
    }
  ];

  # `http`/`tcp` sugar execs in-guest store paths, which do not exist on the
  # operator's machine; host-side probes need an explicit command.
  idiomaticHostProbeSugarFailures = failedAssertionsFor [
    {
      ix.healthChecks.bad = {
        from = "host";
        tcp.port = 5432;
      };
    }
  ];

  idiomaticExposeCollisionFailures = failedAssertionsFor [
    {
      ix.networking = {
        expose.first.port = 7000;
        portClaims.second = {
          protocol = "tcp";
          port = 7000;
        };
      };
    }
  ];

  # The multi-node ix-spark service (Spark master/worker over Tailscale + a Spark
  # Connect server on the master). role defaults to "master".
  ixSparkMaster = evalConfig [
    withIndexLib
    {
      services.ix-spark = {
        enable = true;
        openFirewall = true;
      };
    }
  ];
  ixSparkWorker = evalConfig [
    withIndexLib
    {
      services.ix-spark = {
        enable = true;
        role = "worker";
        masterAddress = "100.64.0.1";
        openFirewall = true;
      };
    }
  ];

  # --- patched-src series restriction ----------------------------------------
  # Pure-eval facts about `patchedSrc`'s optional series restriction, still
  # exercised by downstream patch-dir forks (ix) and the test fixtures here.
  patchedSrcFixture = args:
    ix.patchedSrc ({
        name = "patched-src-fixture";
        src = ./fixtures/patched-src;
        patchDir = ./fixtures/patched-src;
      }
      // args);
  # Forcing `.patches` runs the eval-time selection + canonical assertions
  # without building anything.
  patchedSrcSubset = patchedSrcFixture {patchNames = ["0001-canonical.patch"];};
  patchedSrcAlternate = ix.patchedSrc {
    name = "patched-src-alternate-fixture";
    src = ./fixtures/patched-src;
    patchDir = ./fixtures/patched-src-alternate;
    patchNames = [
      "0002-canonical.patch"
      "0001-canonical.patch"
    ];
  };
  patchedSrcSubsetEval = builtins.tryEval (
    builtins.deepSeq patchedSrcSubset.patches true
  );
  patchedSrcPatchSetEval = builtins.tryEval (
    let
      result = {
        count = patchedSrcSubset.patchSet.count;
        names = map (patch: patch.name) patchedSrcSubset.patchSet.patches;
        hashLength = builtins.stringLength patchedSrcSubset.patchSet.digest;
        hashChangesWithContent = patchedSrcSubset.patchSet.digest != patchedSrcAlternate.patchSet.digest;
        alternateNames = map (patch: patch.name) patchedSrcAlternate.patchSet.patches;
      };
    in
      builtins.deepSeq result result
  );
  patchedSrcSubsetNonCanonicalEval = builtins.tryEval (
    builtins.deepSeq (patchedSrcFixture {patchNames = ["0002-noncanonical.patch"];}).patches true
  );
  patchedSrcSubsetUnknownEval = builtins.tryEval (
    builtins.deepSeq (patchedSrcFixture {patchNames = ["9999-missing.patch"];}).patches true
  );
  patchedSrcDefaultEval = builtins.tryEval (
    builtins.deepSeq (patchedSrcFixture {}).patches true
  );

  # The rust build policy's link-arg surface. Imported directly rather than
  # reached through `ix.cargoUnit`, because the resolved args are not on any
  # public attribute and the point is to pin what rustc is told, not to build
  # anything. `pins` and the check builders are unused by
  # `rustcArgsForPolicyForPlatform`, so they are stubbed.
  rustPolicy = import (paths.root + "/lib/rust/policy.nix") {
    inherit lib pkgs;
    clippyPackage = pkgs.hello;
    vendorConfigScript = _: "";
    cargoLockFile = _: null;
    pins = {loadPins = _: {};};
  };
  rustPolicyLinkArgs = userPolicy: platform:
    rustPolicy.rustcArgsForPolicyForPlatform (rustPolicy.resolvePolicy userPolicy) platform;
  buildIdArg = ["-C" "link-arg=-Wl,--build-id=sha1"];
  moldArg = ["-C" "link-arg=-fuse-ld=mold"];

  groups = {
    macos-guests = [
      {
        assertion = macosGuestAgent.enable;
        message = "declared macOS guests should enable their host launchd agent";
      }
      {
        assertion =
          builtins.elemAt macosGuestAgent.config.ProgramArguments 1
          == "run-macos"
          && builtins.elemAt macosGuestAgent.config.ProgramArguments 3 == "/Users/agent/.local/share/vmkit/guests/test"
          && builtins.elemAt macosGuestAgent.config.ProgramArguments 5 == "0e:c9:c7:6c:25:a8";
        message = "the host launchd agent should pass the declared bundle and MAC to vmkit run-macos";
      }
      {
        assertion =
          macosGuestAgent.config.KeepAlive
          && macosGuestAgent.config.RunAtLoad
          && macosGuestAgent.config.ExitTimeOut == 20;
        message = "launchd should keep the guest alive with the SIGKILL backstop above vmkit's 10s+5s shutdown escalation (index#3766)";
      }
    ];
    # A GNU build-id is the key debuginfod, a separate .debug file,
    # `coredumpctl debug`, a continuous profiler's symbol cache and Antithesis's
    # coverage symbolization all look an address up by, and none of them has a
    # fallback. Neither rustc nor mold emits it unless asked, so these assertions
    # pin the ask itself: ix's build gate fails naming the binary when the note
    # is missing, and its error message points here (indexable-inc/ix#8936).
    rust-build-id = [
      {
        # Every equality assertion here pins `linker.useMold`, because its
        # default is the *eval host's* platform: a bare `{}` policy yields the
        # build-id arg alone on a darwin host and `mold ++ build-id` on a linux
        # one. This suite is only reached by the x86_64-linux gate, so an
        # expectation written from the darwin result passes locally and fails in
        # CI, which is how index#4296 turned main red (index#4312).
        assertion = rustPolicyLinkArgs {linker.useMold = false;} "x86_64-unknown-linux-gnu" == buildIdArg;
        message = "a linux-gnu link should be told to emit a sha1 GNU build-id";
      }
      {
        assertion = rustPolicyLinkArgs {linker.useMold = false;} "x86_64-unknown-linux-musl" == buildIdArg;
        message = "a linux-musl link should be told to emit a sha1 GNU build-id";
      }
      {
        # The note is gated on the ELF triple rather than on the linker choice,
        # so a mold link carries it too, appended after the mold arg rather than
        # replacing it. Equality rather than containment so a stray arg is
        # caught, and this is the combination the fleet actually links with.
        assertion = rustPolicyLinkArgs {linker.useMold = true;} "x86_64-unknown-linux-gnu" == (moldArg ++ buildIdArg);
        message = "a mold linux link should be told to emit a sha1 GNU build-id as well";
      }
      {
        # Mach-O has no GNU build-id (it carries an LC_UUID), so the flag would
        # be an unrecognized linker argument rather than a no-op. Absence rather
        # than equality here because the darwin lld branch is gated on the eval
        # host as well, which no user policy can override.
        assertion = !(lib.elem "link-arg=-Wl,--build-id=sha1" (rustPolicyLinkArgs {} "aarch64-apple-darwin"));
        message = "a darwin link should not be told to emit a GNU build-id";
      }
      {
        assertion =
          rustPolicyLinkArgs {
            linker.buildId = false;
            linker.useMold = false;
          } "x86_64-unknown-linux-gnu"
          == [];
        message = "linker.buildId = false should emit no build-id arg";
      }
      {
        # No freeformType on the policy schema, so a misspelled knob throws
        # rather than silently defaulting the real one back on.
        assertion = !(builtins.tryEval (rustPolicyLinkArgs {linker.buidlId = true;} "x86_64-unknown-linux-gnu")).success;
        message = "a misspelled linker option should fail evaluation";
      }
    ];
    security-roots = [
      {
        assertion =
          securityRootJson
          == {
            attr = "packages.${pkgs.stdenv.hostPlatform.system}.hello";
            name = "hello";
            class = "distributed-cli";
            owner = "indexable-inc/index";
            environment = "none";
            exposure = "local";
            criticality = "low";
            slaHours = 168;
          };
        message = "security root policy should cross the nix eval JSON boundary without derivation state";
      }
      {
        assertion = !invalidSecurityRootClass.success;
        message = "security roots should reject unknown class values at evaluation";
      }
      {
        assertion = !invalidSecurityRootSla.success;
        message = "security roots should reject non-positive SLA hours at evaluation";
      }
      {
        assertion = !(securityRootJson ? path);
        message = "evaluated security root policy must not serialize an unrealized derivation path";
      }
    ];
    # efx terranix-port parity: the ported stacks under tests/efx must render
    # exactly the golden plan IR the efx CLI's tests parse, and everything the
    # translator cannot express must throw. See tests/efx-plan.nix.
    efx = import ./efx-plan.nix {inherit lib ix paths;};
    patched-src-series = [
      {
        assertion = patchedSrcSubsetEval.success;
        message = "patchedSrc should accept a patchNames subset of the discovered series";
      }
      {
        assertion =
          patchedSrcPatchSetEval.success
          && patchedSrcPatchSetEval.value
          == {
            count = 1;
            names = ["0001-canonical.patch"];
            hashLength = 64;
            hashChangesWithContent = true;
            alternateNames = [
              "0001-canonical.patch"
              "0002-canonical.patch"
            ];
          };
        message = "patchedSrc should expose a content-sensitive SHA-256 identity for the selected series";
      }
      {
        assertion = !patchedSrcSubsetNonCanonicalEval.success;
        message = "patchedSrc should still assert canonical patch format on a selected subset";
      }
      {
        assertion = !patchedSrcSubsetUnknownEval.success;
        message = "patchedSrc should reject a patchNames entry naming no patch file in the series";
      }
      {
        # The default (null) selection must keep discovering the WHOLE dir:
        # the fixture dir contains a non-canonical patch, so full discovery
        # can only succeed by skipping files, which would be the regression.
        assertion = !patchedSrcDefaultEval.success;
        message = "patchedSrc default discovery should still walk every patch file (and thus trip on the non-canonical fixture)";
      }
    ];
    wrap-package = [
      {
        assertion = !wrapPackageTypoEval.success;
        message = "wrapPackage should reject unknown argument names at eval time";
      }
      {
        assertion = !wrapPackageNoMainProgramEval.success;
        message = "wrapPackage should throw when mainProgram is unset and the package lacks meta.mainProgram";
      }
      {
        assertion = wrappedHello.meta.mainProgram == "hello" && wrappedHello.unwrapped == pkgs.hello;
        message = "wrapPackage should default mainProgram from the package meta and expose the unwrapped package";
      }
      {
        assertion = lib.isString ix.wrapPackage.options.resources.description;
        message = "wrapPackage should expose an introspectable option schema at .options";
      }
      {
        # `defaultText` stands in for the config-computed default; without it
        # the doc view would present `mainProgram` as required. The null guard
        # keeps a missing entry a clean assertion failure instead of an
        # attribute-selection crash inside mkTest.
        assertion =
          wrapPackageMainProgramDoc
          != null
          && wrapPackageMainProgramDoc.default.text == "package.meta.mainProgram";
        message = "wrapPackage optionsDoc should render the computed mainProgram default via defaultText";
      }
    ];
    mcp = [
      {
        assertion =
          sampleCodexMcpEntry "mcp_servers.index.default_tools_approval_mode"
          == {
            key = "mcp_servers.index.default_tools_approval_mode";
            value = "\"approve\"";
          };
        message = "Codex MCP entries should approve trusted default server tools by default";
      }
      {
        assertion =
          lib.all (name: builtins.elem name sampleMcpServers.index.envVars) googleOauthEnvVars;
        message = "index MCP should declare the Google OAuth client environment variables";
      }
      {
        assertion = let
          entry = sampleCodexMcpEntry "mcp_servers.index.env_vars";
        in
          entry
          != null
          && lib.all (name: lib.hasInfix (builtins.toJSON name) entry.value) googleOauthEnvVars;
        message = "Codex MCP config should forward the Google OAuth client environment variables";
      }
      {
        assertion =
          lib.all (
            name: sampleClaudeMcpServers.index.env.${name} == "\${${name}:-}"
          )
          googleOauthEnvVars;
        message = "Claude MCP config should forward the Google OAuth client environment variables";
      }
      {
        assertion =
          sampleCodexMcpEntry "mcp_servers.exa.url"
          == {
            key = "mcp_servers.exa.url";
            value = "\"https://mcp.exa.ai/mcp\"";
          };
        message = "Codex MCP entries should include the Exa MCP server";
      }
      {
        assertion =
          sampleCodexMcpEntryWithoutIndex "mcp_servers.exa.url"
          == {
            key = "mcp_servers.exa.url";
            value = "\"https://mcp.exa.ai/mcp\"";
          };
        message = "Codex MCP entries should include the Exa MCP server even when index MCP is unavailable";
      }
      {
        assertion =
          agentCommon.defaultServers.index.command
          == lib.getExe repoPackages.mcp-ex
          && agentCommon.defaultServers.index.args == [];
        message = "agent wrappers should use the argument-free Elixir MCP server by default";
      }
      {
        assertion =
          ix.mcp.houseServers {
            indexCommand = "/bin/ix-mcp";
          }
          == ix.mcp.defaultServers {
            indexCommand = "/bin/ix-mcp";
          };
        message = "MCP registry should keep houseServers as a compatibility alias for defaultServers";
      }
      {
        assertion =
          lib.all (
            entry: lib.hasPrefix "mcp_servers.index." entry.key || lib.hasPrefix "mcp_servers.exa." entry.key
          )
          sampleCodexMcpEntries;
        message = "Codex MCP entries should be limited to index and Exa when index MCP is available";
      }
      {
        assertion =
          (ix.mcp.toClaudeJson (ix.mcp.optionalServers {blenderMcp = "/bin/blender-mcp";})).blender.command
          == "/bin/blender-mcp";
        message = "Opt-in Blender MCP server should render for Claude with the consumer's binary";
      }
      {
        assertion = let
          servers = ix.mcp.optionalServers {blenderLabMcp = "/bin/blender-lab-mcp";};
        in
          builtins.attrNames servers
          == ["blender-lab"]
          && servers.blender-lab.env.BLENDER_MCP_PORT == "9877";
        message = "Opt-in Blender Lab server should render alone with its non-default port";
      }
    ];

    provider-prompts = [
      {
        assertion = lib.hasInfix "You are Claude Code." sampleClaudeSystemPrompt;
        message = "Claude wrapper prompt should identify Claude Code";
      }
      {
        assertion = lib.hasInfix "You are Codex." sampleCodexSystemPrompt;
        message = "Codex wrapper prompt should identify Codex";
      }
      {
        assertion =
          lib.hasInfix "via Codex" sampleCodexSystemPrompt
          && !(lib.hasInfix "via Claude Code" sampleCodexSystemPrompt);
        message = "Codex prompt should disclose outward messages as Codex, not Claude Code";
      }
      {
        assertion =
          lib.hasInfix "via Claude Code" sampleClaudeSystemPrompt
          && !(lib.hasInfix "via Codex" sampleClaudeSystemPrompt);
        message = "Claude prompt should disclose outward messages as Claude Code, not Codex";
      }
      {
        # `system`-tagged rules (identity, harness basics) belong only to the
        # full system-prompt render; a context file rides on the stock prompt.
        assertion =
          !(lib.hasInfix "You are Claude Code" sampleClaudeContextPrompt)
          && !(lib.hasInfix "You are Codex" sampleCodexContextPrompt);
        message = "Context renders should drop system-tagged identity rules";
      }
      {
        # Runtime tags survive independently of the kind axis: the
        # claude-code-tagged build-observability rule stays in both Claude
        # renders and never reaches Codex.
        assertion =
          lib.hasInfix "nix store builds --json" sampleClaudeSystemPrompt
          && lib.hasInfix "nix store builds --json" sampleClaudeContextPrompt
          && !(lib.hasInfix "nix store builds --json" sampleCodexSystemPrompt);
        message = "Runtime-tagged rules should follow their runtime across kinds";
      }
      {
        # Untagged rules render everywhere, including context files.
        assertion =
          lib.hasInfix "question behind the question" sampleClaudeContextPrompt
          && lib.hasInfix "question behind the question" sampleCodexContextPrompt;
        message = "Untagged rules should render into context files";
      }
    ];

    ix-spark = [
      {
        # The master node runs the master, a co-located worker, and the Spark
        # Connect server fleet.spark() dials.
        assertion =
          (ixSparkMaster.systemd.services ? spark-master)
          && (ixSparkMaster.systemd.services ? spark-worker)
          && (ixSparkMaster.systemd.services ? spark-connect);
        message = "ix-spark master should run master + worker + connect daemons";
      }
      {
        # Connect (15002) and master RPC (7077) are opened on the master, on
        # the tailscale interface only.
        assertion = let
          ports = ixSparkMaster.networking.firewall.interfaces.tailscale0.allowedTCPPorts;
        in
          builtins.elem 15_002 ports && builtins.elem 7077 ports;
        message = "ix-spark master should open the Connect (15002) and master (7077) ports on tailscale0";
      }
      {
        # Spark's master RPC and Connect server are unauthenticated (a job
        # submission is code execution), so nothing may open them on the GLOBAL
        # firewall -- same exposure class as ix-ray (index#1800 review).
        assertion = let
          globalPorts = ixSparkMaster.networking.firewall.allowedTCPPorts;
        in
          builtins.all (p: !(builtins.elem p globalPorts)) [
            7077
            7078
            7079
            7080
            15_002
          ];
        message = "ix-spark must never open its ports on the global firewall, only on tailscale0";
      }
      {
        # A worker only runs a worker joining the remote master: no master, no
        # connect, and it must not open the master's ports.
        assertion = let
          ports = ixSparkWorker.networking.firewall.interfaces.tailscale0.allowedTCPPorts;
        in
          (ixSparkWorker.systemd.services ? spark-worker)
          && !(ixSparkWorker.systemd.services ? spark-master)
          && !(ixSparkWorker.systemd.services ? spark-connect)
          && !(builtins.elem 7077 ports)
          && !(builtins.elem 15_002 ports);
        message = "ix-spark worker should run only a worker and open no master/connect ports";
      }
      {
        # A worker with no masterAddress cannot know where to join: fail eval.
        assertion = let
          failures = failedAssertionsFor [
            withIndexLib
            {
              services.ix-spark = {
                enable = true;
                role = "worker";
              };
            }
          ];
        in
          builtins.any (a: lib.hasInfix "masterAddress" a.message) failures;
        message = "ix-spark worker should fail evaluation without a masterAddress";
      }
    ];

    idiomatic-fleet-api = [
      {
        assertion =
          idiomaticExpose.ix.healthChecks.web.command
          == [
            (lib.getExe' idiomaticExpose.systemd.package "systemctl")
            "is-active"
            "--quiet"
            "nginx.service"
          ];
        message = "ix.healthChecks.<name>.unit should derive a `systemctl is-active` probe and add the .service suffix";
      }
      {
        assertion = lib.last idiomaticExpose.ix.healthChecks.cron.command == "backup.timer";
        message = "ix.healthChecks.<name>.unit should keep an explicit unit type suffix (.timer)";
      }
      {
        assertion = idiomaticUnitConflictFailures != [];
        message = "ix.healthChecks should reject setting both `unit` and a custom `command`";
      }
      {
        assertion = let
          command = idiomaticExpose.ix.healthChecks.ready.command;
        in
          lib.hasSuffix "/bin/curl" (builtins.head command)
          && builtins.tail command
          == [
            "--fail"
            "--silent"
            "--show-error"
            "http://127.0.0.1:8080/healthz"
          ];
        message = "ix.healthChecks.<name>.http should derive a curl --fail probe (httpGet semantics: any status >= 400 is unhealthy)";
      }
      {
        assertion = let
          command = idiomaticExpose.ix.healthChecks.peer.command;
        in
          lib.hasSuffix "/bin/nc" (builtins.head command)
          && builtins.tail command
          == [
            "-z"
            "svc-b"
            "5432"
          ];
        message = "ix.healthChecks.<name>.tcp should derive an `nc -z` connect probe against the given host";
      }
      {
        # The plan strips string context from check argv, so the probe
        # binaries must ride the system closure explicitly.
        assertion =
          lib.any (p: (p.pname or "") == "curl") idiomaticExpose.environment.systemPackages
          && lib.any (p: (p.pname or "") == "netcat-openbsd") idiomaticExpose.environment.systemPackages;
        message = "declaring http/tcp probes should pin curl and nc into the image closure";
      }
      {
        assertion = idiomaticMultiSugarFailures != [];
        message = "ix.healthChecks should reject setting two probe sugars on one check";
      }
      {
        assertion = idiomaticHostProbeSugarFailures != [];
        message = "ix.healthChecks should reject http/tcp probe sugar on host-side checks";
      }
      {
        assertion = let
          c = idiomaticExpose.ix.networking.portClaims;
        in
          c.web.port == 8080 && c.web.protocol == "tcp" && c.metrics.port == 9090 && c.dns.protocol == "udp";
        message = "ix.networking.expose should register a port claim per listener";
      }
      {
        assertion = let
          fw = idiomaticExpose.networking.firewall;
        in
          builtins.elem 8080 fw.allowedTCPPorts
          && !(builtins.elem 9090 fw.allowedTCPPorts)
          && builtins.elem 53 fw.allowedUDPPorts;
        message = "ix.networking.expose should open the firewall by default, skip it when firewall = false, and use the listener's protocol";
      }
      {
        assertion = idiomaticExposeCollisionFailures != [];
        message = "ix.networking.expose should feed the port-claim registry so it collides with a conflicting portClaim";
      }
      {
        assertion = let
          e = ix.endpoint {
            host = "db";
            port = 5432;
          };
        in
          "${e}" == "db:5432" && e.host == "db" && e.port == 5432 && e.authority == "db:5432";
        message = "ix.endpoint should stringify to host:port and expose its parts";
      }
      {
        assertion =
          (ix.endpoint {
            host = "h";
            port = 80;
            scheme = "http";
            path = "/x";
          }).url
          == "http://h:80/x";
        message = "ix.endpoint should build a scheme URL when given a scheme";
      }
      {
        assertion = "${ix.endpointOf {config = idiomaticExpose;} "web"}" == "svc-a:8080";
        message = "ix.endpointOf should resolve a peer's exposed listener to its east-west host:port";
      }
    ];

    base = [
      {
        assertion = base.cfg.shellWorkspace.enable;
        message = "base profile should enable the ix shell workspace by default";
      }
      # A guest unit that can never start does not just fail its own boot, it
      # fails EVERY switch: switch-to-configuration exits nonzero when a unit
      # failed, and the node agent rejects the whole switch on any nonzero exit,
      # so two permanently-dead units made every `ix apply` of every guest
      # report failure while the guest had switched cleanly (ENG-11063).
      #
      # The units that did it were invented at RUNTIME by
      # systemd-getty-generator: serial-getty@ttyS0 from `console=ttyS0` on the
      # host-owned cmdline, and serial-getty@hvc0 because the generator probes
      # /sys/class/tty/hvc0, which the virtio-console driver registers whether
      # or not a host end is attached. No eval-time check can watch a generator
      # run, so this holds the other half of the fix instead: that the platform
      # keeps both instances masked. Neither device can carry a login session at
      # all (hvc0's receiveq is deliberately never drained, ttyS0 is kernel
      # printk plus a captured log plus a single-holder `ix serial` attach), and
      # neither could start regardless while no .device unit ever activates in a
      # guest (ENG-11064).
      {
        assertion =
          base.config.systemd.units."serial-getty@ttyS0.service".enable
          == false
          && base.config.systemd.units."serial-getty@hvc0.service".enable == false;
        message = "ix guests must mask the serial gettys systemd-getty-generator invents for ttyS0 and hvc0; either one left unmasked fails every switch (ENG-11063)";
      }
      # The other way one apply broke every guest: nixpkgs puts
      # `RequiresMountsFor = [ "/tmp" ]` on the system bus, which is
      # `Requires=` plus `After=`, so systemd stops the bus whenever it stops
      # tmp.mount -- and switch-to-configuration is issuing that very job over
      # that very bus. Removing the tmpfs killed the switch mid-transaction
      # and left /tmp at 0555 (ENG-11080).
      #
      # `modules/system/dbus-survives-mount-removal.nix` drops the edge to
      # `WantsMountsFor=`, keeping the ordering and losing the teardown
      # propagation, and tests/switch-stops-a-mount-vm.nix runs a real switch
      # to prove that works. What the VM test cannot see is whether an IMAGE
      # imports the module at all, which is the failure this assertion holds:
      # the module could be perfect and unreferenced.
      {
        # `enable` and `implementation` are in the assertion rather than
        # assumed: nixpkgs only attaches `RequiresMountsFor` under the broker,
        # so on the reference daemon the checks below would hold with nothing
        # overridden at all and the fix aimed at a unit nobody runs. Then this
        # would pass vacuously, which is the worse failure -- a guard reporting
        # green for a reason unrelated to the property. `implementation` alone
        # is not enough, because it reads "broker" whether or not dbus is on.
        assertion =
          base.config.services.dbus.enable
          && base.config.services.dbus.implementation == "broker"
          && base.config.systemd.services.dbus-broker.unitConfig.RequiresMountsFor == []
          && base.config.systemd.services.dbus-broker.unitConfig.WantsMountsFor == ["/tmp"];
        message = "ix guests must not give the system bus a Requires-strength dependency on /tmp; a switch that removes the mount then stops the bus it is talking over (ENG-11080)";
      }
      {
        # The FHS compat heal exists because /bin, /sbin and /usr were baked as
        # absolute symlinks into the closure that built the IMAGE, which the
        # guest's own nix-gc then collects: /bin/sh stopped existing, and
        # dbus-broker became unstartable with 226/NAMESPACE. Naming any
        # /nix/store path as the replacement target would rebuild exactly that
        # bug, so the heal has to point at an indirection the system maintains.
        # Asserting the ABSENCE of a store path rather than the presence of a
        # literal keeps this a check on the property that matters instead of a
        # second copy of the script.
        assertion = !lib.hasInfix "/nix/store" base.config.system.activationScripts.fhsCompatLinks;
        message = "the FHS compat heal must repoint at /run/current-system, never at a store path a guest nix-gc can collect (ENG-11063)";
      }
      {
        assertion = base.config.users.users.root.shell.meta.mainProgram == "zsh";
        message = "base profile should make root land in zsh (via platform users.defaultUserShell)";
      }
      {
        assertion = base.config.environment.localBinInPath;
        message = "base profile should put each user's ~/.local/bin on the login PATH";
      }
      {
        assertion = base.config.programs.starship.enable;
        message = "base profile should wire the prompt at system level, where it lands in /etc/zshrc and needs no activation";
      }
      {
        assertion = !base.config.home-manager.users.root.programs.starship.enable;
        message = "base profile should not ALSO wire the prompt through Home Manager: two starship inits would run per shell";
      }
      {
        assertion = lib.hasInfix "starship" base.config.programs.zsh.promptInit;
        message = "base profile's system prompt should reach zsh's promptInit, which is what /etc/zshrc runs";
      }
      {
        assertion = base.config.home-manager.users.root.programs.fzf.historyWidget.command == "";
        message = "base profile should leave Ctrl-R history search to Atuin";
      }
      {
        assertion =
          lib.any (
            rule: lib.hasPrefix "d ${base.cfg.shellWorkspace.directory} " rule
          )
          base.config.systemd.tmpfiles.rules;
        message = "base profile should pre-create the workspace directory via systemd-tmpfiles";
      }
      {
        # Regression fence for ix#8389: a rootfs captured while nix held the
        # unix-dotfile lock boots with /nix/var/nix/db/db.sqlite.lock still
        # present, and every guest nix invocation then spins forever in
        # SQLITE_BUSY retries. The lock cleanup must run unconditionally at
        # boot (not gated behind shellWorkspace or any other toggle).
        assertion = builtins.elem "R! /nix/var/nix/db/db.sqlite.lock" base.config.systemd.tmpfiles.rules;
        message = "base profile should remove a stale SQLite dotfile lock on the nix DB at boot (ix#8389)";
      }
      {
        assertion = let
          firewall = base.config.networking.firewall;
        in
          builtins.elem 5001 firewall.allowedTCPPorts && builtins.elem 8443 firewall.allowedUDPPorts;
        message = "base profile should expose ix guest sidecar ports through the in-guest firewall";
      }
      {
        assertion = let
          claims = base.config.ix.networking.portClaims;
        in
          claims.ix-console.protocol
          == "tcp"
          && claims.ix-console.port == 5001
          && claims.ix-console.address == "*"
          && claims.ix-agent.protocol == "udp"
          && claims.ix-agent.port == 8443
          && claims.ix-agent.address == "*";
        message = "base profile should reserve ix guest sidecar listener ports";
      }
      {
        assertion = base.imageConfig.ix.image.name == "ix/base";
        message = "base image should publish from the ix/base repository";
      }
      {
        assertion = !base.config.networking.resolvconf.enable;
        message = "image platform should preserve the runtime DNS configuration written by ix-vm-guest";
      }
      {
        assertion = lib.elemAt base.config.nix.settings.substituters 0 == "https://cache.ix.dev";
        message = "base profile should route Nix through cache.ix.dev before fallback substituters";
      }
      {
        assertion = base.config.nix.package == repoPackages.nix-ix;
        message = "base images must use IX's wasm-enabled Nix so ix apply can evaluate the default ix2nix-wasm scaffold";
      }
      {
        assertion = let
          pin = base.config.nix.registry.nixpkgs.to;
        in
          pin.type
          == "path"
          && pin.path == nixpkgs.outPath
          && pin.narHash == nixpkgs.narHash
          && builtins.isString pin.path;
        message = "image nixpkgs registry pin must carry the input's narHash (an unlocked path: pin makes every in-VM nix eval re-copy the whole tree through VCFS, ~3 min per invocation) and use the outPath string so toJSON does not bake a duplicate nixpkgs into the image";
      }
    ];

    factions = [
      {
        assertion =
          factionsExample.cfg.worldBorder.enable
          && factionsExample.cfg.worldBorder.diameter == 12_000
          && factionsExample.cfg.properties.max-world-size == 6000;
        message = "factions example should declare a managed world border";
      }
      {
        assertion = let
          ports = factionsExample.config.networking.firewall.allowedTCPPorts;
        in
          builtins.elem factionsExample.cfg.port ports
          && builtins.elem 8100 ports
          && !(builtins.elem factionsExample.cfg.rcon.port ports);
        message = "factions example should keep RCON private while exposing Minecraft and BlueMap";
      }
      {
        assertion = builtins.elem 24_454 factionsExample.config.networking.firewall.allowedUDPPorts;
        message = "factions example should expose Simple Voice Chat on the default UDP port";
      }
      {
        assertion = let
          claims = factionsExample.config.ix.networking.portClaims;
        in
          lib.all (claim: builtins.hasAttr claim claims) [
            "minecraft"
            "minecraft-rcon"
            "bluemap"
            "simple-voice-chat"
          ]
          && claims.simple-voice-chat.protocol == "udp"
          && claims.simple-voice-chat.port == 24_454;
        message = "factions example should register every service listener in ix.networking.portClaims";
      }
      {
        assertion = let
          checks = factionsExample.fleet.planValue.nodes.factions.healthChecks;
          mcProbe = lib.getExe repoPackages.mc-probe;
          systemctl = lib.getExe' factionsExample.config.systemd.package "systemctl";
        in
          checks.minecraft.from
          == "guest"
          && checks.minecraft.attempts == 30
          && checks.minecraft.command
          == [
            systemctl
            "is-active"
            "--quiet"
            "minecraft.service"
          ]
          # The SLP check is the interesting one: it proves the Minecraft
          # protocol speaker is up (not just the unit), and asserts the MOTD
          # so a misrouted image lands as a check failure instead of silently
          # serving Survival players a Factions world.
          && checks.minecraft-status.from == "guest"
          && checks.minecraft-status.command
          == [
            mcProbe
            "127.0.0.1:25565"
            "--motd-contains"
            "ix Factions | territory, raids, shops"
          ]
          # factions exposes Java publicly, so the host-side reachability
          # probe is what catches firewall or routing regressions.
          && checks.minecraft-reachable.from == "host"
          && checks.minecraft-reachable.command
          == [
            "nc"
            "-z"
            "-w"
            "5"
            "$IX_NODE_IPV4"
            "25565"
          ]
          && lib.any (
            package: lib.getName package == "mc-probe"
          )
          factionsExample.config.environment.systemPackages;
        message = "factions should layer systemctl + SLP-with-MOTD + host TCP probes";
      }
    ];

    survival = [
      {
        assertion =
          survivalExample.velocity.enable
          && survivalExample.velocity.servers.survival == "127.0.0.1:25566"
          && survivalExample.velocity.try == ["survival"]
          && survivalExample.velocity.forwarding.mode == "modern";
        message = "survival example should route Velocity to the local Paper backend";
      }
      {
        assertion =
          survivalExample.geyser.enable
          && survivalExample.geyser.remote.authType == "floodgate"
          && survivalExample.floodgate.enable;
        message = "survival example should enable Geyser with Floodgate auth";
      }
      {
        assertion =
          survivalExample.minecraft.paper.enable
          && survivalExample.minecraft.version == "26.1.2"
          && survivalExample.minecraft.port == 25_566
          && !survivalExample.minecraft.openFirewall
          && !survivalExample.minecraft.properties.online-mode;
        message = "survival example should keep Paper behind the proxy";
      }
      {
        assertion = let
          ports = survivalExample.config.networking.firewall.allowedTCPPorts;
        in
          builtins.elem 25_565 ports
          && !(builtins.elem 25_566 ports)
          && !(builtins.elem survivalExample.minecraft.rcon.port ports);
        message = "survival example should expose Velocity while keeping backend and RCON private";
      }
      {
        assertion = builtins.elem 19_132 survivalExample.config.networking.firewall.allowedUDPPorts;
        message = "survival example should expose Geyser's Bedrock UDP listener";
      }
      {
        assertion = let
          claims = survivalExample.config.ix.networking.portClaims;
        in
          lib.all (claim: builtins.hasAttr claim claims) [
            "velocity"
            "minecraft"
            "minecraft-rcon"
            "geyser"
          ]
          && claims.velocity.port == 25_565
          && claims.minecraft.port == 25_566
          && claims.geyser.protocol == "udp"
          && claims.geyser.port == 19_132;
        message = "survival example should register proxy, backend, RCON, and Bedrock listeners";
      }
      {
        assertion = let
          checks = survivalExample.fleet.planValue.nodes.survival.healthChecks;
          mcProbe = lib.getExe repoPackages.mc-probe;
        in
          checks.velocity.from
          == "guest"
          && checks.minecraft.from == "guest"
          # Velocity faces the public network in this topology, so it gets a
          # host TCP probe. The Paper backend stays openFirewall = false, so
          # its only host-observable signal is via Velocity itself.
          && checks.velocity-reachable.from == "host"
          && checks.velocity-reachable.command
          == [
            "nc"
            "-z"
            "-w"
            "5"
            "$IX_NODE_IPV4"
            "25565"
          ]
          && !(checks ? minecraft-reachable)
          && lib.any (
            package: lib.getName package == "mc-probe"
          )
          survivalExample.config.environment.systemPackages
          # SLP checks on both: Velocity proves the proxy answers; the
          # backend SLP proves the actual game server isn't dead behind a
          # healthy proxy.
          && checks.velocity-status.command
          == [
            mcProbe
            "127.0.0.1:25565"
            "--motd-contains"
            "ix Survival"
          ]
          && checks.minecraft-status.command
          == [
            mcProbe
            "127.0.0.1:25566"
            "--motd-contains"
            "ix Survival"
          ];
        message = "survival should expose layered guest/host probes with MOTD-aware SLP on both proxy and backend";
      }
    ];

    python-daily-scraper = [
      {
        assertion =
          builtins.any (
            package: (package.meta.mainProgram or null) == "daily-scraper"
          )
          dailyScraperExample.config.environment.systemPackages
          && lib.hasInfix "--repo indexable-inc/index" dailyScraperExample.service.serviceConfig.ExecStart;
        message = "python-daily-scraper example should package and enable the scraper";
      }
      {
        assertion =
          dailyScraperExample.service.serviceConfig.Type
          == "oneshot"
          && dailyScraperExample.service.serviceConfig.DynamicUser
          && dailyScraperExample.service.serviceConfig.StateDirectory == "daily-scraper"
          && dailyScraperExample.service.serviceConfig.WorkingDirectory == "/var/lib/daily-scraper";
        message = "python-daily-scraper example should render a stateful oneshot systemd service";
      }
      {
        assertion =
          builtins.elem "network-online.target" dailyScraperExample.service.after
          && builtins.elem "network-online.target" dailyScraperExample.service.wants;
        message = "python-daily-scraper service should wait for network readiness";
      }
      {
        assertion =
          lib.hasInfix "/var/lib/daily-scraper/parquet" dailyScraperExample.service.serviceConfig.ExecStart
          && lib.hasInfix "--repo indexable-inc/index" dailyScraperExample.service.serviceConfig.ExecStart;
        message = "python-daily-scraper service should pass the durable output directory and repository";
      }
      {
        assertion =
          dailyScraperExample.timer.timerConfig.OnCalendar
          == "*-*-* 03:17:00 UTC"
          && dailyScraperExample.timer.timerConfig.Persistent
          && dailyScraperExample.timer.timerConfig.RandomizedDelaySec == "20m"
          && dailyScraperExample.timer.timerConfig.Unit == "daily-scraper.service";
        message = "python-daily-scraper example should run from a persistent daily timer";
      }
      {
        assertion = let
          check = dailyScraperExample.plan.healthChecks.daily-scraper;
        in
          check.from
          == "guest"
          && check.command
          == [
            (lib.getExe' dailyScraperExample.config.systemd.package "systemctl")
            "is-active"
            "--quiet"
            "daily-scraper.timer"
          ];
        # No listener for the operator to probe, so the guest unit check is
        # the whole story. The explicit `from = "guest"` rules out a future
        # default-flip accidentally turning this into an unrunnable host check.
        message = "python-daily-scraper fleet plan should include a guest-side timer health check";
      }
      {
        assertion = !dailyScraperExample.plan.ipv4 && dailyScraperExample.plan.snapshot;
        message = "python-daily-scraper fleet plan should keep the worker private with snapshots on";
      }
      {
        assertion =
          lib.hasInfix "s3 sync --only-show-errors /var/lib/daily-scraper/parquet s3://andrew-scraper-output/github --delete" dailyScraperS3.service.serviceConfig.ExecStartPost
          && dailyScraperS3.service.serviceConfig.LoadCredential
          == [
            "aws-env:/run/secrets/daily-scraper/aws.env"
          ]
          && dailyScraperS3.service.serviceConfig.EnvironmentFile == "%d/aws-env";
        message = "python-daily-scraper service should support S3 sync through systemd credentials";
      }
    ];

    nginx-lifecycle = [
      {
        assertion = nginxLifecycleExample.plan.recreateOnUp;
        message = "nginx-lifecycle fleet plan should recreate the VM on every ix-fleet up run";
      }
      {
        assertion =
          nginxLifecycleExample.cfg.enable
          && nginxLifecycleExample.cfg.virtualHosts.localhost.locations."/".return
          == "200 'ix nginx lifecycle ok\n'";
        message = "nginx-lifecycle example should serve a fixed HTTP success body";
      }
      {
        assertion = let
          claims = nginxLifecycleExample.config.ix.networking.portClaims;
        in
          claims.nginx.protocol
          == "tcp"
          && claims.nginx.port == 80
          && builtins.elem 80 nginxLifecycleExample.config.networking.firewall.allowedTCPPorts;
        message = "nginx-lifecycle example should declare and open its HTTP listener";
      }
      {
        assertion = let
          checks = nginxLifecycleExample.plan.healthChecks;
        in
          checks.nginx.from
          == "guest"
          && checks.nginx.command
          == [
            (lib.getExe' nginxLifecycleExample.config.systemd.package "systemctl")
            "is-active"
            "--quiet"
            "nginx.service"
          ]
          && checks.nginx-http.from == "guest"
          && lib.hasSuffix "/bin/curl" (builtins.head checks.nginx-http.command)
          && builtins.tail checks.nginx-http.command
          == [
            "--fail"
            "--silent"
            "--show-error"
            "http://127.0.0.1/"
          ];
        message = "nginx-lifecycle fleet plan should prove the service unit and HTTP loopback path";
      }
    ];

    fleet-replicas = [
      {
        assertion =
          replicasFleet.fleet.planValue.order
          == [
            "web"
            "worker-0"
            "worker-1"
            "worker-2"
          ]
          && replicasFleet.worker.plan.dependsOn == ["web"];
        message = "the fleet evaluator should expand three worker replicas that depend on the web node";
      }
      {
        assertion = let
          check = replicasFleet.web.plan.healthChecks.http-loopback;
        in
          check.from
          == "guest"
          && lib.hasSuffix "/bin/curl" (builtins.head check.command)
          && lib.last check.command == "http://127.0.0.1:8080/";
        message = "the web node should desugar its http probe into a loopback curl";
      }
      {
        assertion = let
          check = replicasFleet.worker.plan.healthChecks.web-reachable;
        in
          check.from == "guest" && lib.last check.command == "http://web:8080/";
        message = "workers should probe the web endpoint resolved by node name";
      }
      {
        assertion =
          replicasFleet.worker.plan.updateStrategy.maxUnavailable
          == 1
          && replicasFleet.web.plan.updateStrategy == null;
        message = "workers should roll one at a time while the singleton web node carries no strategy";
      }
    ];

    multi-vm-hello = [
      {
        assertion = let
          check = multiVmHelloExample.worker.plan.healthChecks.web-reachable;
        in
          check.from == "guest" && lib.last check.command == "http://web:8080/";
        message = "multi-vm-hello worker should resolve the web VM through the mkVm peer seam";
      }
    ];

    multi-vm-microservices = [
      {
        assertion =
          builtins.attrNames multiVmMicroservicesExample.gateway.config.services.nginx.upstreams.api.servers
          == [
            "api-0:8080"
            "api-1:8080"
            "api-2:8080"
          ];
        message = "multi-vm-microservices gateway should resolve every api VM through the mkVm peer seam";
      }
    ];

    vm-templates = [
      {
        # The whole of the mapping the ix CLI half will depend on: an instance
        # name splits at its `@`, and the node name rejoins the halves with the
        # separator a DNS label allows. Nothing else in either repo is allowed
        # to rediscover this.
        assertion =
          ix.templates.parseInstanceName "worker-pool@eu_1"
          == {
            template = "worker-pool";
            instance = "eu_1";
            node = "worker-pool-eu_1";
          };
        message = "an instance name should split at its @ and rejoin as a DNS-label node name";
      }
      {
        # The round trip, over the adversarial table rather than one name:
        # every (template, instance) pair survives being spelled as an instance
        # name and parsed back. Written with no separator literal on either
        # side, so it holds the two functions against each other rather than
        # against a constant restated here. The non-empty guard is not
        # decoration: `lib.all` over an empty list is true, which is how a
        # table that stopped being populated would read as a pass.
        assertion =
          vmTemplatesExample.roundTripPairs
          != []
          && lib.all (
            pair: let
              parsed = ix.templates.parseInstanceName (ix.templates.instanceNameOf pair);
            in
              {inherit (parsed) template instance;} == pair
          )
          vmTemplatesExample.roundTripPairs;
        message = "every template/instance pair should survive being spelled as an instance name and parsed back";
      }
      {
        # Why there are two spellings at all, made executable rather than
        # argued. These two pairs have different instance names and the SAME
        # node name, so a node name cannot be parsed back into its halves and
        # `-` could not have been the instance separator. This is the check
        # that would have caught the ambiguity if anyone had reached for `-`.
        assertion = let
          pool = {
            template = "worker-pool";
            instance = "1";
          };
          nested = {
            template = "worker";
            instance = "pool-1";
          };
        in
          ix.templates.instanceNameOf pool
          != ix.templates.instanceNameOf nested
          && vmTemplatesExample.nodeOf pool == vmTemplatesExample.nodeOf nested;
        message = "an instance name should be injective where a node name is not, which is why the two separators differ";
      }
      {
        assertion = lib.all vmTemplatesExample.throws [
          (ix.templates.parseInstanceName "worker")
          (ix.templates.parseInstanceName "worker-1")
          (ix.templates.parseInstanceName "@1")
          (ix.templates.parseInstanceName "worker@")
          (ix.templates.parseInstanceName "worker@a@b")
          (ix.templates.parseInstanceName "worker@a/b")
          (ix.templates.parseInstanceName "-worker@1")
          # The three #4452 admitted: a trailing '-' or '_' on a half, and a
          # joined name over the hostname length limit. All three passed the
          # per-half check and then failed `networking.hostName`, which is the
          # error path the guard exists to close.
          (ix.templates.parseInstanceName "worker@1-")
          (ix.templates.parseInstanceName "worker@a_")
          (ix.templates.parseInstanceName (
            "worker@" + lib.concatStrings (lib.genList (_: "9") 70)
          ))
        ];
        message = "a malformed instance name, a node name included, should be refused at the parse rather than by a hostname option four layers down";
      }
      {
        # The mirror in lib/templates.nix is only safe because this fires when it
        # stops matching nixpkgs. Verdicts are compared, not pattern strings:
        # what has to hold is that the two agree about every name, not that they
        # are spelled the same way.
        assertion =
          vmTemplatesExample.nodeNameRows
          != []
          && lib.all (
            node:
              ix.templates.isNodeName node
              == (builtins.match vmTemplatesExample.nixpkgsHostNamePattern node != null)
          )
          vmTemplatesExample.nodeNameRows;
        message = "isNodeName should agree with nixpkgs' own networking.hostName pattern on every name in the table";
      }
      {
        # Pins the length limit to the pattern without restating 63 anywhere:
        # a name of exactly that length is accepted, one character more is not.
        assertion = let
          filled = count: lib.concatStrings (lib.genList (_: "a") count);
        in
          ix.templates.isNodeName (filled ix.templates.nodeNameMaxLength)
          && !(ix.templates.isNodeName (filled (ix.templates.nodeNameMaxLength + 1)));
        message = "the node-name length limit should be exactly the longest name the hostname pattern accepts";
      }
      {
        # A deliberate loosening, recorded because it is visible: #4452's
        # per-half check refused an instance id starting with the node
        # separator, and `worker--1` is a hostname nixpkgs accepts. A guard
        # stricter than the option it stands in front of rejects working
        # configs, which is its own kind of wrong.
        assertion = let
          parsed = ix.templates.parseInstanceName "worker@-1";
        in
          parsed.instance == "-1" && ix.templates.isNodeName parsed.node;
        message = "an instance id starting with the node separator should be accepted, because the hostname it renders is legal";
      }
      {
        assertion = vmTemplatesExample.throws (vmTemplatesExample.render {name = "atNamed@1";});
        message = "a template naming its VM worker@1 should be refused: @ is legal in neither a hostname nor an OCI repository";
      }
      {
        assertion = vmTemplatesExample.throws (
          vmTemplatesExample.renderStubConfig {
            instances = {
              "collapsing@1" = {};
              "collapsing@2" = {};
            };
          }
        );
        message = "a template that ignores its instance id should be refused, not silently render one VM for two instances";
      }
      {
        assertion = vmTemplatesExample.throws (
          vmTemplatesExample.renderStubConfig {
            nixosConfigurations.worker-1 = {};
            instances."worker@1" = {};
          }
        );
        message = "an instance whose node name collides with a named VM should be refused, not merged over it";
      }
      {
        assertion = vmTemplatesExample.throws (vmTemplatesExample.render {name = "workr@1";});
        message = "an unknown template name should be refused with the available names listed";
      }
      {
        assertion = vmTemplatesExample.throws (
          vmTemplatesExample.render {
            name = "worker@1";
            params = {instance = "9";};
          }
        );
        message = "params that restate the injected instance identity should be refused rather than silently winning or losing";
      }
      {
        assertion = vmTemplatesExample.throws (ix.templates.renderConfig {instances."worker@1" = {};});
        message = "an instances block with no templates to render it should be refused";
      }
      {
        # The property every config written before this feature relies on:
        # `renderConfig` over a config exporting neither key is the identity on
        # `nixosConfigurations`. This is what makes adding the seam to a
        # flake.nix safe before there is anything for it to render.
        assertion = let
          passthrough = ix.templates.renderConfig multiVmMicroservicesExample.example;
        in
          builtins.attrNames passthrough.nixosConfigurations
          == builtins.attrNames multiVmMicroservicesExample.example.nixosConfigurations
          && passthrough.instances == {};
        message = "a config exporting no templates should render exactly the VMs it already declares, and no instances";
      }
      {
        # Guard the guard, by name rather than by count: an empty
        # `nixosConfigurations` would make the next assertion vacuously true,
        # and a count would break the moment somebody adds an instance to the
        # example, which is the digit the example exists to be able to turn.
        assertion = lib.all (node: builtins.elem node vmTemplatesExample.nodeNames) [
          "web"
          "worker-1"
          "worker-2"
        ];
        message = "the templates-workers example should render its named VM and both declared instances";
      }
      {
        # Forces the whole module system for every rendered node without
        # building any of it: the drvPath string still has to be computed, so a
        # type error, a missing attribute or a port collision throws here,
        # while `unsafeDiscardStringContext` keeps the check from depending on
        # those closures. It costs seconds and it is what makes changing a
        # template as safe as changing a named VM.
        assertion =
          lib.all (
            entry:
              builtins.isString (
                builtins.unsafeDiscardStringContext entry.config.system.build.toplevel.drvPath
              )
          )
          (builtins.attrValues vmTemplatesExample.rendered.nixosConfigurations);
        message = "every VM the templates-workers example renders should evaluate to a system derivation";
      }
      {
        # Params have to reach the guest, template default and instance
        # override alike: worker@1 declares no params at all and worker@2 sets
        # both. Two mechanisms, deliberately: `shards` lands in a generated
        # nginx directive and `port` in the option that opens the firewall and
        # claims the port.
        assertion =
          vmTemplatesExample.shardsOf "worker-1"
          == "worker_processes 1;"
          && vmTemplatesExample.shardsOf "worker-2" == "worker_processes 4;"
          && vmTemplatesExample.portOf "worker-1" == 8080
          && vmTemplatesExample.portOf "worker-2" == 8081;
        message = "a template default and an instance override should both reach the rendered guest";
      }
    ];

    fleet-replica-fanout = [
      {
        assertion =
          replicaFanoutFleet.fleet.planValue.order
          == [
            "api-0"
            "api-1"
            "api-2"
            "cache"
            "gateway"
          ]
          && replicaFanoutFleet.api.plan.dependsOn == ["cache"]
          && replicaFanoutFleet.gateway.plan.dependsOn
          == [
            "api-0"
            "api-1"
            "api-2"
          ];
        message = "the fleet evaluator should expand the gateway's api dependency across every replica";
      }
      {
        assertion = replicaFanoutFleet.api.plan.updateStrategy.maxUnavailable == 1;
        message = "api replicas should carry the rolling-update window into the plan";
      }
      {
        # The gateway enumerates api replicas at eval time, so raising
        # `replicas` grows the upstream pool without touching gateway.nix.
        assertion =
          builtins.attrNames replicaFanoutFleet.gateway.config.services.nginx.upstreams.api.servers
          == [
            "api-0:8080"
            "api-1:8080"
            "api-2:8080"
          ];
        message = "fleet-microservices gateway should discover every api replica into its nginx upstream pool";
      }
      {
        assertion = let
          checks = replicaFanoutFleet.gateway.plan.healthChecks;
        in
          lib.last checks.upstream-api-0.command
          == "http://api-0:8080/healthz"
          && lib.last checks.upstream-api-1.command == "http://api-1:8080/healthz"
          && lib.last checks.upstream-api-2.command == "http://api-2:8080/healthz"
          && lib.last checks.proxies-to-api.command == "http://127.0.0.1:8080/";
        message = "fleet-microservices gateway should generate one http probe per discovered api replica plus an end-to-end proxy probe";
      }
      {
        assertion = let
          check = replicaFanoutFleet.api.plan.healthChecks.cache-reachable;
        in
          lib.hasSuffix "/bin/nc" (builtins.head check.command)
          && builtins.tail check.command
          == [
            "-z"
            "cache"
            "6379"
          ];
        message = "fleet-microservices api replicas should tcp-probe the cache endpoint across nodes";
      }
      {
        assertion = let
          check = replicaFanoutFleet.cache.plan.healthChecks.accepting-connections;
        in
          lib.hasSuffix "/bin/nc" (builtins.head check.command)
          && builtins.tail check.command
          == [
            "-z"
            "127.0.0.1"
            "6379"
          ];
        message = "fleet-microservices cache should desugar tcp.port into a loopback nc probe";
      }
    ];

    k8s-k3s = let
      inherit (k8sK3sExample) server agent;
      deployment = builtins.head server.services.k3s.manifests.whoami.content;
      container = builtins.head deployment.spec.template.spec.containers;
      podImage =
        lib.findFirst (drv: drv ? imageName) null server.services.k3s.images;
    in [
      {
        # Defends the cross-file contract between image.nix (what containerd
        # imports) and workload.nix (what the pod spec asks for): a drifted
        # name or tag would ErrImageNeverPull at runtime.
        assertion =
          podImage
          != null
          && container.image == "${podImage.imageName}:${podImage.imageTag}"
          && container.imagePullPolicy == "Never";
        message = "k8s-k3s deployment must reference the exact image preloaded into containerd, never a registry";
      }
      {
        assertion =
          agent.services.k3s.serverAddr
          == "https://${server.ix.networking.eastWest.hostName}:6443";
        message = "k8s-k3s agents should join the API server by its east-west hostname";
      }
      {
        # kube-proxy answers the Service's NodePort on every node, so agents
        # must open it too, not just the server that declares the manifest.
        assertion =
          agent.ix.networking.expose.whoami-nodeport.port
          == (builtins.head (builtins.elemAt server.services.k3s.manifests.whoami.content 1).spec.ports).nodePort;
        message = "k8s-k3s agents should open the whoami NodePort declared in the Service manifest";
      }
    ];

    nomad-cluster = let
      inherit (nomadClusterExample) server client;
      whoami =
        lib.findFirst (pkg: (pkg.meta.mainProgram or null) == "whoami-http") null
        client.environment.systemPackages;
    in [
      {
        assertion =
          server.services.nomad.settings.server.enabled
          && server.services.nomad.settings.server.bootstrap_expect == 1;
        message = "nomad-cluster server should bootstrap a single-server raft";
      }
      {
        # The job's artifact is a raw store path executed by raw_exec; both
        # halves of that contract live on the client.
        assertion =
          client.services.nomad.settings.plugin.raw_exec.config.enabled
          && whoami != null;
        message = "nomad-cluster clients must enable raw_exec and pin the whoami binary into their closure";
      }
      {
        assertion =
          client.services.nomad.settings.client.servers
          == [server.ix.networking.eastWest.hostName];
        message = "nomad-cluster clients should register with the server by its east-west hostname";
      }
    ];

    s3-storage = [
      {
        assertion = s3StorageExample.cfg.enable && s3StorageExample.cfg.configFile != null;
        message = "s3-storage example should enable SeaweedFS with an S3 identities config";
      }
      {
        assertion = !(s3StorageExample.plan.recreateOnUp or false);
        message = "s3-storage node should persist data across ix-fleet up, not recreate";
      }
      {
        # Defends the module's headline claim: only the S3 port is exposed.
        # `samePorts` (not `elem`) fails if the master/volume/filer ports
        # ever leak into the firewall alongside the base sidecar ports.
        assertion = let
          claims = s3StorageExample.config.ix.networking.portClaims;
        in
          claims.ix-seaweedfs.protocol
          == "tcp"
          && claims.ix-seaweedfs.port == 8333
          && samePorts s3StorageExample.config.networking.firewall.allowedTCPPorts (
            baseFirewallTcpPorts ++ [8333]
          );
        message = "s3-storage example should open only the S3 port, not master/volume/filer";
      }
      {
        assertion = let
          check = s3StorageExample.plan.healthChecks.ix-seaweedfs;
        in
          check.from
          == "guest"
          && lib.hasSuffix "/bin/curl" (builtins.head check.command)
          && lib.last check.command == "http://127.0.0.1:8333/healthz";
        message = "s3-storage health check should probe the unauthenticated S3 /healthz route";
      }
      {
        # The module must refuse an S3 endpoint with neither credentials nor
        # an explicit anonymous opt-in, rather than silently serving open.
        assertion = let
          failures = failedAssertionsFor [{services.ix-seaweedfs.enable = true;}];
        in
          builtins.any (a: lib.hasInfix "configFile" a.message) failures;
        message = "ix-seaweedfs should fail evaluation when run with no credentials and no allowAnonymous";
      }
      {
        # The example supplies a configFile, so it must clear that gate.
        assertion = failedAssertionsFor [(paths.examples + "/s3/storage/service.nix")] == [];
        message = "s3-storage example should satisfy the ix-seaweedfs credentials assertion";
      }
    ];

    observability-stack = [
      {
        assertion =
          observabilityStackExample.observability.cfg.stack.enable
          && observabilityStackExample.observability.cfg.agent.enable
          && observabilityStackExample.observability.config.services.clickhouse.enable
          && observabilityStackExample.observability.config.services.grafana.enable
          && observabilityStackExample.observability.config.services.opentelemetry-collector.enable;
        message = "observability-stack should enable the full local observability stack";
      }
      {
        assertion =
          observabilityStackExample.observability.config.services.opentelemetry-collector.package.pname
          == "otelcol-contrib";
        message = "ix-observability should use the contrib collector so ClickHouse export is available";
      }
      {
        assertion =
          observabilityStackExample.observability.collector.receivers.otlp.protocols.grpc.endpoint
          == "0.0.0.0:4317"
          && observabilityStackExample.observability.collector.exporters.clickhouse.database == "otel"
          && observabilityStackExample.observability.collector.exporters.clickhouse.traces_table_name
          == "otel_traces"
          # The corpus moved off the OTel bus to its own Parquet log (#736), so the
          # logs pipeline is telemetry-only again: ClickHouse (plus forward on an
          # agent). Assert ClickHouse is an exporter rather than pinning the exact
          # list, which breaks on every legitimate addition to the pipeline.
          && builtins.elem "clickhouse" observabilityStackExample.observability.collector.service.pipelines.logs.exporters;
        message = "observability-stack collector should receive OTLP and export logs/traces/metrics to ClickHouse";
      }
      {
        assertion = let
          datasource = builtins.head observabilityStackExample.observability.grafana.provision.datasources.settings.datasources;
        in
          datasource.uid
          == "ix-clickhouse"
          && datasource.type == "grafana-clickhouse-datasource"
          && datasource.jsonData.traces.defaultTable == "otel_traces"
          && datasource.jsonData.logs.defaultTable == "otel_logs";
        message = "observability-stack should provision Grafana with the ClickHouse OTel datasource";
      }
      {
        assertion =
          observabilityStackExample.observability.plan.l7ProxyPorts
          == [3000]
          && builtins.elem 3000 observabilityStackExample.observability.config.networking.firewall.allowedTCPPorts
          && builtins.elem 4317 observabilityStackExample.observability.config.networking.firewall.allowedTCPPorts
          && builtins.elem 9000 observabilityStackExample.observability.config.networking.firewall.allowedTCPPorts;
        message = "observability-stack should expose Grafana, OTLP, and ClickHouse for the example fleet";
      }
      {
        assertion =
          observabilityStackExample.app.cfg.stack.enable
          == false
          && observabilityStackExample.app.cfg.agent.enable
          && observabilityStackExample.app.cfg.agent.endpoint == "observability:4317"
          && observabilityStackExample.app.collector.exporters.otlp.endpoint == "observability:4317"
          && observabilityStackExample.app.collector.receivers."filelog/app".include
          == ["/var/log/ix-observability-demo/app.log"]
          && observabilityStackExample.app.collector.service.pipelines.logs.exporters == ["otlp"];
        message = "observability-stack app node should run an agent collector that forwards file logs and OTLP";
      }
      {
        assertion = let
          checks = observabilityStackExample.app.plan.healthChecks;
        in
          checks.observability-demo.from
          == "guest"
          && checks.observability-ingested.attempts == 60
          && checks.observability-ingested.timeoutSec == 10;
        message = "observability-stack app node should prove local emission and ClickHouse ingestion";
      }
      {
        assertion = observabilityStackExample.observability.queryTool != null;
        message = "observability-stack should install the ix-observe query helper for agents";
      }
    ];

    minecraft-blocks = [
      {
        # LOG: a single-node Kafka broker in KRaft mode (both roles), with the
        # one durable topic. This is the source of truth, not the transport.
        assertion =
          minecraftBlocksExample.log.kafka.enable
          && minecraftBlocksExample.log.kafka.formatLogDirs
          && minecraftBlocksExample.log.kafka.settings."process.roles"
          == [
            "broker"
            "controller"
          ];
        message = "minecraft-blocks log node should run a KRaft Kafka broker as the durable log";
      }
      {
        # Only the broker port is exposed, and it is claimed.
        assertion = let
          claims = minecraftBlocksExample.log.config.ix.networking.portClaims;
        in
          claims.kafka.port
          == 9092
          && builtins.elem 9092 minecraftBlocksExample.log.config.networking.firewall.allowedTCPPorts;
        message = "minecraft-blocks log node should expose and claim the Kafka broker port";
      }
      {
        # VIEW: reuses the shared observability ClickHouse (one server), with
        # the collector and Grafana, not a second ClickHouse.
        assertion =
          minecraftBlocksExample.view.obs.enable
          && minecraftBlocksExample.view.obs.stack.enable
          && minecraftBlocksExample.view.config.services.clickhouse.enable
          && minecraftBlocksExample.view.config.services.opentelemetry-collector.enable;
        message = "minecraft-blocks view node should run the shared observability ClickHouse plus collector";
      }
      {
        # The view-init oneshot creates the minecraft DB, table, Kafka queue,
        # and MV after ClickHouse is up.
        assertion = let
          unit = minecraftBlocksExample.view.initUnit;
        in
          unit.serviceConfig.Type == "oneshot" && builtins.elem "clickhouse.service" unit.requires;
        message = "minecraft-blocks view node should initialize the spatial view once ClickHouse is up";
      }
      {
        # The view health check confirms all three minecraft objects exist
        # (table, Kafka queue, materialized view).
        assertion = let
          check = minecraftBlocksExample.view.plan.healthChecks.mc-blocks-view;
        in
          check.from == "guest" && check.attempts == 60;
        message = "minecraft-blocks view node should health-check the spatial view, queue, and MV";
      }
      {
        # PRODUCER: a Paper server with the custom block-events plugin shipped
        # via `src` (a built jar), not a catalog slug.
        assertion =
          minecraftBlocksExample.producer.minecraft.enable
          && minecraftBlocksExample.producer.minecraft.paper.enable
          && minecraftBlocksExample.producer.minecraft.plugins.block-events.enable
          && minecraftBlocksExample.producer.minecraft.plugins.block-events.src != null
          && minecraftBlocksExample.producer.minecraft.plugins.block-events.pluginName == "BlockEvents";
        message = "minecraft-blocks producer should run Paper with the custom block-events plugin";
      }
      {
        # Both legs are real on the producer: the domain-fact transport ships to
        # Kafka, and the OTel agent forwards server telemetry to the collector.
        # Telemetry is collected from the journal (the minecraft service stdout),
        # not by tailing the server's private, DynamicUser-unreadable log file.
        assertion = let
          ship = minecraftBlocksExample.producer.shipUnit;
          agent = minecraftBlocksExample.producer.agent;
        in
          ship.serviceConfig.Restart
          == "always"
          && agent.stack.enable == false
          && agent.agent.enable
          && agent.agent.journal.enable
          && agent.agent.filelog.paths == []
          && agent.resourceAttributes."ix.app" == "minecraft-blocks";
        message = "minecraft-blocks producer should run both the Kafka transport and the journal-based telemetry agent";
      }
      {
        # The schema is the single source of truth: the Morton ORDER BY, the
        # signed-coordinate offset, and the per-axis minmax skip indexes (which
        # are what actually prune the bounding-box query) all come from it.
        assertion = let
          inherit (minecraftBlocksExample) schema;
        in
          schema.coordOffset
          == 1_048_576
          && lib.hasInfix "mortonEncode" schema.createTableSql
          && lib.hasInfix "toUInt32(x + 1048576)" schema.mortonExpr
          && builtins.length schema.mortonFields == 3
          && lib.hasInfix "INDEX idx_x x TYPE minmax" schema.createTableSql
          && lib.hasInfix "INDEX idx_z z TYPE minmax" schema.createTableSql
          && lib.hasInfix "index_granularity = ${toString schema.indexGranularity}" schema.createTableSql;
        message = "minecraft-blocks schema should drive a Z-order ORDER BY plus per-axis minmax skip indexes over offset-shifted signed coordinates";
      }
      {
        # Replay must be idempotent: the table is a ReplacingMergeTree keyed on
        # the placement identity (the ORDER BY tuple), so an at-least-once
        # transport re-sending a record collapses it back to one row. The dedup
        # key is the same ORDER BY tuple the spatial query relies on, so this
        # engine choice never changes the query path, it only folds duplicates.
        assertion = let
          inherit (minecraftBlocksExample) schema;
        in
          lib.hasInfix "ReplacingMergeTree" schema.createTableSql
          && !lib.hasInfix "ENGINE = MergeTree" schema.createTableSql
          && lib.hasInfix "ORDER BY (world, ${schema.mortonExpr}, timestamp)" schema.createTableSql;
        message = "minecraft-blocks view should be a ReplacingMergeTree keyed on the placement identity so replay is idempotent";
      }
      {
        # One bounding box: box.json drives the generator's in-box region, the
        # Nix schema's derived predicate, and the integration check, so the
        # asserted in-box count cannot drift from a fixture edit. The derived SQL
        # predicate must be half-open per axis and bound to the box's world.
        assertion = let
          inherit (minecraftBlocksExample) schema;
          inherit (schema) box;
        in
          box.world
          == "overworld"
          && box.x
          == [
            0
            16
          ]
          && lib.hasInfix "world = 'overworld'" schema.boxPredicate
          && lib.hasInfix "x >= 0 AND x < 16" schema.boxPredicate
          && lib.hasInfix "z >= 0 AND z < 16" schema.boxPredicate;
        message = "minecraft-blocks bounding box should come from one box.json definition shared by the schema predicate and the fixture generator";
      }
    ];

    networking = [
      {
        assertion =
          lib.any (
            failure: lib.hasInfix "ix.networking.portClaims has same-namespace port collisions" failure.message
          )
          portClaimConflictFailures;
        message = "ix.networking.portClaims should fail eval when two services claim the same-namespace socket";
      }
      {
        assertion = portClaimNamespaceAllowedFailures == [];
        message = "ix.networking.portClaims should allow the same port in separate network namespaces";
      }
      {
        assertion = portClaimAddressFamilyAllowedFailures == [];
        message = "ix.networking.portClaims should allow the same UDP port on separate IPv4 and IPv6 bind addresses";
      }
    ];

    managed-paths = [
      {
        assertion =
          ix.relativePath.isSafe "plugins/BlueMap/core.conf"
          && !(ix.relativePath.isSafe "../core.conf")
          && !(ix.relativePath.isSafe "plugins//core.conf")
          && ix.relativePath.isSafeName "Geyser-Velocity.jar"
          && !(ix.relativePath.isSafeName "nested/Geyser-Velocity.jar");
        message = "ix.relativePath should distinguish safe managed paths from unsafe segments and names";
      }
      {
        assertion =
          ix.relativePath.shellPath "$out" "plugins/Blue Map/core.conf"
          == "\"$out\"/'plugins/Blue Map/core.conf'"
          && ix.relativePath.shellParent "$out" "plugins/Blue Map/core.conf" == "\"$out\"/'plugins/Blue Map'"
          && ix.relativePath.shellParent "$out" "server.properties" == "\"$out\""
          && !relativePathUnsafeShellEval.success;
        message = "ix.relativePath shell helpers should quote safe relative paths and reject unsafe paths";
      }
      {
        assertion = let
          failure =
            lib.findFirst (
              f: lib.hasInfix "services.minecraft managed paths must be relative paths" f.message
            )
            null
            minecraftUnsafeManagedPathFailures;
          msg =
            if failure != null
            then failure.message
            else "";
        in
          failure
          != null
          && lib.hasInfix "services.minecraft.configFiles.client//bad.toml" msg
          && lib.hasInfix "services.minecraft.configFiles./absolute/bad.toml" msg
          && lib.hasInfix "services.minecraft.serverFiles.plugins/../bukkit.yml" msg
          && lib.hasInfix "services.minecraft.serverFiles.$(bad).json" msg
          && lib.hasInfix "services.minecraft.datapacks.bad.fileName=../bad" msg
          && lib.hasInfix "services.minecraft.datapacks.bad.files.data/../bad.json" msg
          && lib.hasInfix "services.minecraft world directory ../bad-world" msg;
        message = "minecraft managed file options should reject unsafe relative paths at eval time";
      }
      {
        assertion =
          lib.any (
            failure: lib.hasInfix "services.velocity.configFiles contains unsafe relative paths" failure.message
          )
          velocityUnsafeManagedPathFailures;
        message = "velocity managed config files should reject unsafe relative paths at eval time";
      }
      {
        assertion =
          lib.any (
            failure: lib.hasInfix "services.velocity.plugins contains unsafe plugin file names" failure.message
          )
          velocityUnsafeManagedPathFailures;
        message = "velocity plugin file names should reject nested or unsafe paths at eval time";
      }
      {
        assertion =
          lib.any (
            failure:
              lib.hasInfix "services.velocity.plugins contains duplicate plugin file names" failure.message
          )
          velocityDuplicatePluginFileNameFailures;
        message = "velocity plugin file names should reject duplicate managed jar names at eval time";
      }
    ];

    extended-attributes = [
      {
        assertion = builtins.hasAttr "/build/ix-xattr-test" extendedAttributes.config.ix.extendedAttributes;
        message = "generic ix.extendedAttributes should expose absolute runtime paths";
      }
      {
        assertion =
          builtins.any (
            p: (p.pname or null) == "attr"
          )
          extendedAttributes.config.environment.systemPackages;
        message = "generic ix.extendedAttributes should add attr tools for runtime inspection";
      }
      {
        assertion =
          lib.hasInfix "/bin/setfattr" extendedAttributes.activationScript
          && lib.hasInfix "user.ix.kind" extendedAttributes.activationScript;
        message = "generic ix.extendedAttributes should render setfattr activation commands";
      }
      {
        assertion = lib.hasInfix "refusing to set extended attributes on symlink" extendedAttributes.activationScript;
        message = "generic ix.extendedAttributes should avoid following symlinks";
      }
      {
        assertion = lib.hasInfix "filesystem does not support extended attributes" extendedAttributes.activationScript;
        message = "generic ix.extendedAttributes should warn instead of failing on unsupported filesystems";
      }
    ];

    dev-base = [
      {
        assertion =
          builtins.elem "claude-code" developmentBase.packageNames
          && builtins.elem "codex" developmentBase.packageNames;
        message = "dev base should ship the Claude Code and Codex CLIs";
      }
      {
        # Global allowUnfree would let every unfree package slip in. The
        # image is supposed to grant exactly one exception, by name.
        assertion = !(developmentBase.config.nixpkgs.config.allowUnfree or false);
        message = "dev base should not enable allowUnfree globally; use the predicate";
      }
      {
        assertion = !(builtins.elem "cursor-cli" developmentBase.packageNames);
        message = "dev base should keep unrelated unfree CLIs out of the image";
      }
      {
        assertion = let
          policy = gates:
            import (paths.packagesRoot + "/agent/policy/permissions.nix") ({inherit lib;} // gates);
          # The overlay build's real gate combination: exa is baked
          # unconditionally by the default server set, the kernel is not
          # (repoPackages.mcp-ex is out of scope there).
          overlay = policy {exaSearchBaked = true;};
          baked = policy {
            indexKernelBaked = true;
            exaSearchBaked = true;
          };
        in
          # Without the kernel only the merge protections, the unconditional
          # bundled-skill deny (#3607) and the exa-superseded web pair
          # remain: the stock shell/file/search tools are that agent's whole
          # surface. `dataviz` carries no deny: the claude-code wrapper
          # removes it via `skillOverrides` instead (#3659).
          overlay.claude.deniedToolPatterns
          == [
            "Bash(gh pr merge*--admin*)"
            "Bash(gh pr merge*--force*)"
            "Skill(artifact-design)"
            "WebSearch"
            "WebFetch"
          ]
          && !(overlay.codex.forcedSettings.features ? shell_tool)
          && overlay.codex.forcedSettings.features.standalone_web_search == false
          # With the index kernel + exa baked, file and web tools are folded in.
          # Both agents keep their native shell as the direct command path and
          # as the kernel-outage path (index#4080).
          && !(builtins.elem "Bash" baked.claude.deniedToolPatterns)
          && builtins.all (tool: builtins.elem tool baked.claude.deniedToolPatterns) [
            "Read"
            "Write"
            "Edit"
            "NotebookEdit"
            "Glob"
            "Grep"
            "CronCreate"
            "CronDelete"
            "CronList"
            "WebSearch"
            "WebFetch"
          ]
          && !(baked.codex.forcedSettings.features ? shell_tool)
          && !(baked.codex.forcedSettings.features ? unified_exec)
          && baked.codex.forcedSettings.features.standalone_web_search == false;
        message = "agent policy should gate kernel/exa-superseded native tools on the baked MCP servers";
      }
      {
        assertion = let
          forced = repoPackages.codex.passthru.specValue.forced;
        in
          lib.all
          (
            key:
              builtins.elem {
                key = "features.${key}";
                value = "false";
              }
              forced
          )
          [
            "browser_use"
            "browser_use_external"
            "computer_use"
            "image_generation"
            "in_app_browser"
            "standalone_web_search"
          ]
          && !(builtins.elem "features.shell_tool" (map (entry: entry.key) forced))
          && !(builtins.elem "features.unified_exec" (map (entry: entry.key) forced));
        message = "Codex wrapper should render built-in tool disables into forced launch config";
      }
      {
        assertion = let
          forcedKeys = map (entry: entry.key) repoPackages.codex.passthru.specValue.forced;
          softKeys = map (entry: entry.key) repoPackages.codex.passthru.specValue.soft;
        in
          builtins.elem "mcp_servers.index.command" forcedKeys
          && !builtins.elem "mcp_servers.index.command" softKeys
          && !builtins.elem "mcp_servers.exa.url" forcedKeys
          && builtins.elem "mcp_servers.exa.url" softKeys;
        message = "Codex wrapper should force local MCP commands and keep remote MCP URLs soft";
      }
      {
        # Bypass-permissions is enforced through Claude's managed-settings layer
        # (/etc/claude-code/managed-settings.json): read-only, highest precedence,
        # leaving ~/.claude/settings.json app-owned. Pin both keys so a refactor
        # that drops them can't silently restore per-tool prompts. `.text` is a
        # plain string (no IFD) so fromJSON can read it in eval.
        assertion = let
          managed =
            builtins.fromJSON
            developmentBase.config.environment.etc."claude-code/managed-settings.json".text;
        in
          managed.permissions.defaultMode == "bypassPermissions" && managed.skipDangerousModePermissionPrompt;
        message = "dev base should enforce root's Claude Code bypass via managed-settings.json";
      }
      {
        # Channels are LOADED by argv and ALLOWED by policy, and the two sit in
        # different scopes: `--dangerously-load-development-channels` wires the
        # transport, while `channelsEnabled` in the managed layer decides
        # whether a loaded channel may push at all. 2.1.220 blocks on
        # `e!==null&&e.channelsEnabled!==!0`, so the mere presence of a
        # managed-settings file turns channels off -- and a managed-settings
        # file is exactly what every consumer of this render installs. Pin the
        # key true in both places the render lands, so nothing can go back to
        # shipping a channel that is loaded but muted.
        assertion = let
          policy = homeAgentConfig.programs.claude-code.package.passthru.settingsPolicy;
          managed =
            builtins.fromJSON
            developmentBase.config.environment.etc."claude-code/managed-settings.json".text;
        in
          policy.channelsEnabled && managed.channelsEnabled;
        message = "claude-code should enable channels by default through the managed settings render, both in a home configuration and in the dev image";
      }
      {
        # The default is a house default rather than a controlled key, so a host
        # that must refuse inbound pushes can still say no. Declared through
        # `defaults` (the extraSettings layer, which outranks the house
        # posture), it has to reach the render as false; if it could not, the
        # default would be a lock and not a default.
        assertion =
          !(homeAgentHome.extendModules {
            modules = [{programs.claude-code.defaults.channelsEnabled = false;}];
          })
          .config.programs.claude-code.package.passthru.settingsPolicy.channelsEnabled;
        message = "a host must be able to turn claude-code channels back off through programs.claude-code.defaults";
      }
      {
        assertion =
          builtins.elem homeAgentConfig.programs.claude-code.finalPackage homeAgentConfig.home.packages
          && builtins.elem homeAgentConfig.programs.codex.finalPackage homeAgentConfig.home.packages;
        message = "agent Home Manager modules should install their configured packages when enabled";
      }
      {
        assertion =
          homeAgentConfig.home.file.".config/codex-test/hooks.json".source
          == homeAgentConfig.programs.codex.finalPackage.hooksJson;
        message = "Codex Home Manager module should install the shared hook policy under the configured Codex home";
      }
      {
        # #4312: nothing may write the user settings.json, which Claude Code
        # rewrites from memory on any /model or /config toggle. Policy rides
        # the managed layer instead, so pin that the Home Manager module
        # declares no file under the configured Claude home beyond the PATH
        # pin, and that the policy render a host hands the managed layer still
        # carries the controlled hook and deny policy.
        assertion = let
          policy = homeAgentConfig.programs.claude-code.package.passthru.settingsPolicy;
        in
          !(homeAgentConfig.home.file ? ".claude/settings.json")
          && policy ? hooks
          && policy ? statusLine
          && builtins.elem "Artifact" policy.permissions.deny
          && !(policy ? theme)
          && !(policy ? model);
        message = "Claude Code Home Manager module must leave settings.json app-owned, and the policy render must carry hooks/permissions without the app-owned preference keys";
      }
      {
        # #4224: denying a tool that another tool's description tells the model
        # to use does not stop the model, it picks the fallback that
        # description forbids one paragraph later. The Agent description names
        # SendMessage for redirecting a running subagent, and without it the
        # only move left is a second Agent call on the first one's files
        # (observed, ENG-10401); the Bash description names Monitor for waiting
        # on a condition. Pin both out of the deny list, and pin that the
        # experimental agent-teams env var rides along with SendMessage: the
        # tool stays hidden without it, so the permission row alone is a lie.
        assertion = let
          policy = homeAgentConfig.programs.claude-code.package.passthru.settingsPolicy;
        in
          builtins.all (tool: !(builtins.elem tool policy.permissions.deny)) [
            "Monitor"
            "ReportFindings"
            "SendMessage"
          ]
          && policy.env.CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS == "1";
        message = "claude-code must not deny tools its own tool descriptions tell the model to use, and SendMessage must bake the agent-teams env var it needs";
      }
      {
        assertion =
          builtins.elem {
            key = "agents.max_depth";
            value = "4";
          }
          homeAgentConfig.programs.codex.finalPackage.passthru.specValue.soft;
        message = "Codex Home Manager module should pass soft settings through the package wrapper";
      }
      {
        assertion =
          !lib.strings.hasInfix "Publish substantial work" (
            builtins.readFile homeAgentConfig.programs.codex.finalPackage.passthru.modelInstructionsFile
          );
        message = "Codex Home Manager module should thread systemPrompt.omitRules into the package wrapper";
      }
      {
        # Claude counterpart of the codex threading check above: the package
        # is left defaulted here, so the module's omitRules fold must strip
        # the omitted rule's text from the baked prompt (index#3537).
        assertion =
          !lib.strings.hasInfix "Publish substantial work"
          homeAgentConfig.programs.claude-code.package.passthru.systemPrompt;
        message = "Claude Code Home Manager module should thread systemPrompt.omitRules into the package wrapper";
      }
      {
        # index#3537: with an explicitly-set package the omitRules fold is
        # discarded, which once shipped permissions that allowed force-merge
        # under a prompt that forbade it; both modules must trip their guard
        # assertion instead of half-applying the policy.
        assertion =
          homeAgentExplicitPackageFails "claude-code"
          && homeAgentExplicitPackageFails "codex";
        message = "agent Home Manager modules should fail eval when systemPrompt.omitRules rides an explicitly-set package";
      }
      {
        # When explicitly enabled, the native `context` option carries the
        # context render (house rules without the system-tagged basics).
        assertion =
          lib.hasInfix "question behind the question" homeAgentConfig.programs.claude-code.context
          && !(lib.hasInfix "You are Claude Code" homeAgentConfig.programs.claude-code.context)
          && lib.hasInfix "question behind the question" homeAgentConfig.programs.codex.context
          && !(lib.hasInfix "You are Codex" homeAgentConfig.programs.codex.context);
        message = "agent Home Manager modules should default the global context files to the house context render";
      }
      {
        # housePlugin defaults on: Codex gets the plugin as config-declared
        # local-marketplace soft settings.
        assertion =
          builtins.any (
            entry: entry.key == "plugins.index@index.enabled" && entry.value == "true"
          )
          homeAgentConfig.programs.codex.finalPackage.passthru.specValue.soft;
        message = "Codex Home Manager module should declare the index plugin through soft settings";
      }
    ];

    subagent-cache = let
      svc =
        (evalConfig [
          {
            services.subagent-cache = {
              enable = true;
              bind = "100.64.0.1:3013";
              environmentFiles = ["/run/subagent-cache/db.env"];
              ttlDays = 14;
            };
          }
        ]).systemd.services.subagent-cache;
    in [
      {
        assertion =
          svc.environment.SUBAGENT_CACHE_BIND
          == "100.64.0.1:3013"
          && svc.environment.SUBAGENT_CACHE_TTL_DAYS == "14";
        message = "subagent-cache module should pass bind and ttlDays through to the daemon env";
      }
      {
        # Secrets ride EnvironmentFile, never the unit environment (which lands
        # in the world-readable store).
        assertion =
          svc.serviceConfig.EnvironmentFile
          == ["/run/subagent-cache/db.env"]
          && !(svc.environment ? DATABASE_URL)
          && !(svc.environment ? ANTHROPIC_API_KEY);
        message = "subagent-cache module must deliver secrets via EnvironmentFile, not the unit environment";
      }
    ];

    vitest = [
      {
        assertion = builtins.length vitestWorkspaceCases == 2;
        message = "vitest workspace fixture should enumerate one case per project";
      }
      {
        assertion =
          lib.all (
            case:
              case.testProject
              != null
              && case.testFile == "src/shared.test.js"
              && case.vitestArgs
              == [
                "src/shared.test.js"
                "--project"
                case.testProject
                "--testNamePattern"
                "^shared project case$"
              ]
          )
          vitestWorkspaceCases;
        message = "vitest per-case checks should filter project-specific manifest entries by project";
      }
    ];

    minecraft = [
      {
        assertion = minecraft.cfg.version == "26.1.2";
        message = "default Minecraft module should follow versions.nix default runtime version";
      }
      {
        assertion = minecraft.cfg.properties.max-players == 100_000;
        message = "default Minecraft module should allow the large ix player ceiling";
      }
      {
        assertion =
          minecraft.cfg.properties.online-mode && minecraft.cfg.properties.enforce-secure-profile;
        message = "default Minecraft module should keep account authentication and secure profiles explicit";
      }
      {
        assertion =
          velocityConcreteAddress.ix.healthChecks.velocity-status.command
          == [
            (lib.getExe repoPackages.mc-probe)
            "10.0.0.5:25570"
          ];
        message = "velocity SLP health checks should probe concrete bind addresses";
      }
      {
        assertion =
          minecraft.cfg.properties.gamemode
          == "survival"
          && !minecraft.cfg.properties.force-gamemode
          && minecraft.cfg.properties.pvp
          && !minecraft.cfg.properties.hardcore
          && minecraft.cfg.properties.spawn-protection == 16
          && !minecraft.cfg.properties.allow-flight
          && !minecraft.cfg.properties.enable-command-block;
        message = "default Minecraft module should keep conservative gameplay and command defaults";
      }
      {
        assertion =
          minecraft.cfg.properties.view-distance
          == 32
          && minecraft.cfg.properties.simulation-distance == 32;
        message = "default Minecraft module should use the high-distance template defaults";
      }
      {
        assertion = lib.all (slug: builtins.hasAttr slug minecraft.config.services.minecraft.mods) [
          "fabric-api"
          "lithium"
          "c2me-fabric"
          "spark"
          "grimac"
        ];
        message = "default Minecraft module should include the 26.1.2 Fabric server mod set";
      }
      {
        assertion = lib.getName minecraft.config.services.minecraft.javaPackage == "temurin-jre-bin";
        message = "default Fabric minecraft should use Temurin";
      }
      {
        assertion = lib.hasInfix "/bin/java" minecraft.service.config.ExecStart;
        message = "minecraft ExecStart should launch Java";
      }
      {
        assertion = lib.hasInfix "-XX:MaxRAMPercentage=85" minecraft.service.config.ExecStart;
        message = "minecraft should use MaxRAMPercentage for auto-scaling heap";
      }
      {
        assertion = lib.hasInfix "-XX:+UseG1GC" minecraft.service.config.ExecStart;
        message = "minecraft should include the default modern server GC flags";
      }
      {
        assertion =
          lib.hasInfix "-jar" minecraft.service.config.ExecStart
          && lib.hasInfix "nogui" minecraft.service.config.ExecStart;
        message = "minecraft ExecStart should launch the configured server jar in nogui mode";
      }
      {
        assertion = lib.hasInfix "minecraft-hot-reload-agent.jar=socket=/run/minecraft-hot-reload/socket" minecraft.service.config.ExecStart;
        message = "Fabric minecraft should start the hot reload Java agent";
      }
      {
        assertion = builtins.length minecraft.service.unit.reloadTriggers == 3;
        message = "minecraft managed files should trigger systemd reloads rather than unit restarts";
      }
      {
        assertion = lib.hasInfix "minecraft-sync-managed" minecraft.service.unit.preStart;
        message = "minecraft preStart should sync managed files from /etc";
      }
      {
        assertion = !(lib.hasInfix "fabric-api" minecraft.service.unit.preStart);
        message = "minecraft preStart should not embed managed mod store paths in the unit";
      }
      {
        assertion =
          minecraft.config.ix.extendedAttributes."/var/lib/minecraft".attributes."user.ix.kind"
          == "minecraft.server-root";
        message = "minecraft should label its runtime data directory through the generic xattr module";
      }
      {
        assertion =
          minecraft.config.ix.extendedAttributes."/var/lib/minecraft/world/region".attributes."user.ix.kind"
          == "minecraft.region-directory"
          && minecraft.config.ix.extendedAttributes."/var/lib/minecraft/world/region".attributes."user.ix.minecraft.dimension"
          == "overworld";
        message = "minecraft should label overworld region directories through the generic xattr module";
      }
      {
        assertion =
          minecraft.config.ix.extendedAttributes."/var/lib/minecraft/world/DIM-1/region".attributes."user.ix.minecraft.dimension"
          == "nether"
          && minecraft.config.ix.extendedAttributes."/var/lib/minecraft/world/DIM1/region".attributes."user.ix.minecraft.dimension"
          == "end";
        message = "minecraft should label Nether and End region directories through the generic xattr module";
      }
      # rcon coverage stays on the minecraft default module because the option
      # surface lives in `services.minecraft`, not in a paper-specific module.
      {
        assertion = minecraft.rcon.cfg.rcon.enable;
        message = "minecraft RCON should be enabled through a typed option";
      }
      {
        assertion = !(minecraft.rcon.cfg.properties ? "rcon.password");
        message = "typed minecraft RCON should not put the password in Nix-managed server.properties";
      }
      {
        assertion = samePorts minecraft.rcon.config.networking.firewall.allowedTCPPorts (
          baseFirewallTcpPorts ++ [minecraft.rcon.cfg.port]
        );
        message = "typed minecraft RCON should keep the RCON port private by default";
      }
      {
        assertion = samePorts minecraft.rcon.openFirewall.config.networking.firewall.allowedTCPPorts (
          baseFirewallTcpPorts
          ++ [
            minecraft.rcon.openFirewall.cfg.port
            minecraft.rcon.openFirewall.cfg.rcon.port
          ]
        );
        message = "typed minecraft RCON should open the firewall only when requested";
      }
      {
        assertion = minecraft.worldBorder.cfg.worldBorder.enable;
        message = "typed minecraft world border should expose an enable flag";
      }
      {
        assertion =
          minecraft.worldBorder.cfg.worldBorder.center.x
          == 100
          && minecraft.worldBorder.cfg.worldBorder.center.z == -50
          && minecraft.worldBorder.cfg.worldBorder.diameter == 8000;
        message = "typed minecraft world border should keep center and diameter settings";
      }
      {
        assertion = minecraft.worldBorder.cfg.rcon.enable;
        message = "typed minecraft world border should enable local RCON by default";
      }
      {
        assertion = samePorts minecraft.worldBorder.config.networking.firewall.allowedTCPPorts (
          baseFirewallTcpPorts ++ [minecraft.worldBorder.cfg.port]
        );
        message = "typed minecraft world border should keep the RCON port private";
      }
      {
        assertion =
          minecraft.worldBorder.service.after
          == ["minecraft.service"]
          && minecraft.worldBorder.service.requires == ["minecraft.service"];
        message = "typed minecraft world border should run after the Minecraft service is required";
      }
      {
        assertion = minecraft.access.cfg.properties.white-list;
        message = "typed minecraft whitelist should enable server.properties white-list";
      }
      {
        assertion = minecraft.access.cfg.properties.enforce-whitelist;
        message = "typed minecraft whitelist should enable enforce-whitelist by default";
      }
      {
        assertion = !(minecraft.access.cfg.serverFiles ? "whitelist.json");
        message = "typed minecraft whitelist should not symlink the mutable whitelist file through serverFiles";
      }
      {
        assertion = !(minecraft.access.cfg.serverFiles ? "ops.json");
        message = "typed minecraft operators should not symlink the mutable ops file through serverFiles";
      }
      {
        assertion = builtins.elem minecraft.access.managed.access minecraft.access.service.unit.restartTriggers;
        message = "typed minecraft access changes should restart the server so Minecraft rereads mutable access files";
      }
      {
        assertion = builtins.hasAttr "generated/example.snbt" minecraft.nbt.cfg.serverFiles;
        message = "minecraft serverFiles should accept readable SNBT files";
      }
      {
        assertion = builtins.hasAttr "generated/example.nbt" minecraft.nbt.cfg.serverFiles;
        message = "minecraft serverFiles should accept binary NBT files";
      }
      {
        assertion = builtins.hasAttr "generated/client.snbt" minecraft.nbt.cfg.configFiles;
        message = "minecraft configFiles should accept readable SNBT files";
      }
      {
        assertion = minecraft.datapacks.cfg.datapacks.max-height.worlds == ["My World"];
        message = "minecraft datapacks should default to the configured level-name world";
      }
      {
        assertion = builtins.hasAttr "/var/lib/minecraft/My World/datapacks" minecraft.datapacks.config.ix.extendedAttributes;
        message = "minecraft datapacks should annotate target world datapack directories";
      }
      {
        assertion = builtins.elem minecraft.datapacks.managed.datapacks minecraft.datapacks.service.unit.restartTriggers;
        message = "minecraft datapack changes should restart the server so registries are reloaded";
      }
    ];

    "minecraft_1.21.11-paper" = [
      {
        assertion = builtins.length minecraft.paper.service.unit.reloadTriggers == 3;
        message = "Paper minecraft managed plugins should trigger systemd reloads";
      }
      {
        assertion = !(minecraft.paper.service.config ? RuntimeDirectory);
        message = "Paper minecraft should not start the JVM hot reload socket";
      }
      {
        assertion = !(minecraft.paper.cfg.properties ? "rcon.password");
        message = "Paper minecraft should not put the RCON password in Nix-managed server.properties";
      }
      {
        assertion = samePorts minecraft.paper.config.networking.firewall.allowedTCPPorts (
          baseFirewallTcpPorts ++ [minecraft.paper.cfg.port]
        );
        message = "Paper minecraft should not expose the local RCON reload port through the firewall";
      }
    ];

    "minecraft_26.1.2-paper" = [
      {
        assertion = builtins.hasAttr "pvpindex-factions" minecraft.paperPlugins.cfg.pluginCatalog;
        message = "Paper minecraft should seed pluginCatalog from the generated 26.1.2 Paper catalog";
      }
      {
        assertion = builtins.elem 24_455 minecraft.paperPlugins.config.networking.firewall.allowedUDPPorts;
        message = "Simple Voice Chat should open its UDP port when installed as a Paper plugin";
      }
      {
        assertion =
          minecraft.paperPlugins.cfg.serverFiles."plugins/voicechat/voicechat-server.properties".port
          == 24_455;
        message = "Simple Voice Chat should render Paper plugin config under plugins/voicechat";
      }
      {
        assertion =
          minecraft.paperPlugins.cfg.worlds.factions.generator
          == "TerraformGenerator"
          && minecraft.paperPlugins.cfg.worlds.factions_nether.generator == "TerraformGenerator"
          && minecraft.paperPlugins.cfg.worlds.factions_the_end.generator == "TerraformGenerator";
        message = "TerraformGenerator should bind every configured world to its generator";
      }
      {
        assertion =
          minecraft.paperPlugins.cfg.bukkit.worlds.factions.generator
          == "TerraformGenerator"
          && minecraft.paperPlugins.cfg.bukkit.worlds.factions_nether.generator == "TerraformGenerator"
          && minecraft.paperPlugins.cfg.bukkit.worlds.factions_the_end.generator == "TerraformGenerator";
        message = "Minecraft worlds should render to bukkit.yml world generator entries";
      }
    ];

    minecraft-bedrock = [
      {
        assertion = bedrock.cfg.enable;
        message = "minecraft-bedrock module should enable services.minecraft-bedrock";
      }
      {
        assertion =
          bedrock.cfg.settings.server-port
          == bedrock.cfg.port
          && bedrock.cfg.settings.server-portv6 == bedrock.cfg.portv6;
        message = "minecraft-bedrock server.properties should follow the configured UDP ports";
      }
      {
        assertion = samePorts bedrock.config.networking.firewall.allowedUDPPorts (
          baseFirewallUdpPorts
          ++ [
            bedrock.cfg.port
            bedrock.cfg.portv6
          ]
        );
        message = "minecraft-bedrock firewall should open only the configured UDP ports plus ix sidecar ports";
      }
      {
        assertion = lib.hasInfix "/bin/bedrock_server" bedrock.service.config.ExecStart;
        message = "minecraft-bedrock ExecStart should launch bedrock_server";
      }
    ];

    remote-desktop = [
      {
        assertion = remoteDesktop.cfg.enable;
        message = "remote-desktop fixture should enable services.remote-desktop";
      }
      {
        assertion = lib.getName remoteDesktop.cfg.package == lib.getName pkgs.xpra;
        message = "remote-desktop should default to the nixpkgs Xpra package";
      }
      {
        assertion = remoteDesktop.cfg.openFirewall;
        message = "remote-desktop fixture should explicitly open the browser port";
      }
      {
        assertion = remoteDesktop.cfg.allowUnauthenticated;
        message = "remote-desktop fixture should explicitly allow unauthenticated browser access";
      }
      {
        assertion = !remoteDesktopModuleDefault.cfg.openFirewall;
        message = "remote-desktop module should keep the browser port closed unless callers opt in";
      }
      {
        assertion = samePorts remoteDesktopModuleDefault.config.networking.firewall.allowedTCPPorts baseFirewallTcpPorts;
        message = "remote-desktop module default should leave only ix sidecar TCP ports open";
      }
      {
        assertion =
          lib.any (
            failure:
              lib.hasInfix "rendered Xpra auth = \"none\" requires services.remote-desktop.allowUnauthenticated = true" failure.message
          )
          remoteDesktopUnauthenticatedFirewallFailures;
        message = "remote-desktop should reject unauthenticated firewall exposure unless it is explicit";
      }
      {
        assertion =
          lib.any (
            failure:
              lib.hasInfix "rendered Xpra auth = \"none\" requires services.remote-desktop.allowUnauthenticated = true" failure.message
          )
          remoteDesktopSettingsAuthFirewallFailures;
        message = "remote-desktop should check settings.auth overrides before opening the firewall";
      }
      {
        assertion =
          lib.any (
            failure:
              lib.hasInfix "settings.bind-tcp must match services.remote-desktop.bindAddress" failure.message
          )
          remoteDesktopBindTcpDriftFailures;
        message = "remote-desktop should reject a bind-tcp override that disagrees with the claimed listener";
      }
      {
        assertion = remoteDesktop.config.users.users.remote-desktop.isSystemUser;
        message = "remote-desktop user should be a system user";
      }
      {
        assertion = samePorts remoteDesktop.config.networking.firewall.allowedTCPPorts (
          baseFirewallTcpPorts ++ [remoteDesktop.cfg.port]
        );
        message = "remote-desktop firewall should open only the configured browser port plus ix sidecar ports";
      }
      {
        assertion = !(remoteDesktop.config.systemd.services ? xvfb);
        message = "remote-desktop should not use a standalone Xvfb service";
      }
      {
        assertion = !(remoteDesktop.config.systemd.services ? x11vnc);
        message = "remote-desktop should not use x11vnc";
      }
      {
        assertion = !(remoteDesktop.config.systemd.services ? novnc);
        message = "remote-desktop should not use a separate noVNC websockify service";
      }
    ];

    resource-monitor = [
      {
        assertion = resourceMonitor.service.config.DynamicUser;
        message = "resource-monitor stats writer should run as a dynamic systemd user";
      }
      {
        assertion = resourceMonitor.service.config.NoNewPrivileges;
        message = "resource-monitor stats writer should reject new privileges";
      }
      {
        assertion = resourceMonitor.service.config.ProtectSystem == "strict";
        message = "resource-monitor stats writer should use the shared strict filesystem hardening";
      }
      {
        assertion = resourceMonitor.service.config.RuntimeDirectory == "ix/resource-monitor";
        message = "resource-monitor should preserve nested /run runtime directory paths";
      }
      {
        assertion = lib.hasInfix "/run/ix/resource-monitor" resourceMonitor.service.config.ExecStart;
        message = "resource-monitor stats writer should write to the configured runtime directory";
      }
      {
        assertion =
          resourceMonitor.config.services.nginx.virtualHosts.resource-monitor.locations."/stats.json".root
          == "/run/ix/resource-monitor";
        message = "resource-monitor nginx should serve stats from the configured runtime directory";
      }
      {
        assertion =
          lib.all (
            failures:
              lib.any (
                failure:
                  lib.hasInfix "services.resource-monitor.runtimeDirectory must be a managed /run subdirectory" failure.message
              )
              failures
          )
          resourceMonitorRuntimeDirectoryFailures;
        message = "resource-monitor should reject runtime directories outside /run and unsafe /run segments";
      }
    ];

    helpers = [
      {
        assertion = missingPackageMetadata == [];
        message =
          "packages with default.nix should declare package.nix metadata entries: "
          + lib.concatStringsSep ", " missingPackageMetadata;
      }
      {
        assertion = lib.getName standaloneJvmProfile.config.ix.profiles.jvm.package == "temurin-jre-bin";
        message = "exported JVM profile should evaluate with plain nixpkgs and no repo overlay";
      }
      {
        assertion = cargoUnitWorkspace.unusedCrateDependenciesByPackage != {};
        message = "cargo-unit workspaces should expose per-crate unused dependency policy checks by default";
      }
      {
        assertion = lib.hasInfix "--ordered-shutdown" processComposeApplication.passthru.tests.dryRun.buildCommand;
        message = "process-compose dry-run checks should include runtime wrapper arguments";
      }
      {
        assertion = goUnitWorkspace.sourceAudit.module.lockFile == "go.sum";
        message = "go-unit workspaces should require and report the Go module lockfile";
      }
      {
        assertion = goUnitWorkspace.packages ? root;
        message = "go-unit workspaces should expose package-shaped build derivations";
      }
      {
        assertion = goUnitWorkspace.packages.root.goUnit.goSum == goUnitFixture + "/go.sum";
        message = "go-unit package derivations should pass go.sum through to buildGoModule";
      }
      {
        assertion =
          goUnitWorkspace.vendorHashKey == "946e64650b103a2fe8d7518f522acad2ba766bd2c3700066125f33206d400b66";
        message = "go-unit workspaces should derive the vendor hash key from go.mod and go.sum";
      }
      {
        assertion =
          goUnitWorkspace.packages.root.goUnit.vendorHashFile == goUnitFixture + "/go-modules.nix";
        message = "go-unit package derivations should use the module-owned vendor hash file by default";
      }
      {
        assertion =
          goUnitWorkspace.packages.root.goUnit.goToolchain
          == ix.languages.go.toolchain pkgs {version = "latest";};
        message = "go-unit package derivations should use the selected Go toolchain";
      }
      {
        assertion = goUnitWorkspace.packages.root.goUnit.env.GOFLAGS == "-mod=readonly";
        message = "go-unit package derivations should preserve buildGoModule env values";
      }
      {
        assertion = goUnitWorkspace.tests ? root;
        message = "go-unit workspaces should expose package-shaped test derivations";
      }
      {
        assertion = goUnitNestedWorkspace.sourceAudit.module.relative == "module";
        message = "go-unit workspaces should resolve default go.mod and go.sum below modRoot";
      }
      {
        assertion = goUnitStdlibWorkspace.sourceAudit.module.lockFile == null;
        message = "go-unit workspaces should allow stdlib-only modules without go.sum";
      }
      {
        assertion = goUnitStdlibWorkspace.packages.root.goUnit.goSum == null;
        message = "go-unit package derivations should pass null goSum for modules without go.sum";
      }
      {
        assertion = goUnitDerivedStdlibWorkspace.packages.root.goUnit.goSum == null;
        message = "go-unit derivation sources should allow no-sum modules when go.mod is readable";
      }
      {
        assertion =
          goUnitDerivedWorkspaceWithVendorHashFile.packages.root.goUnit.vendorHashKey
          == goUnitWorkspace.vendorHashKey;
        message = "go-unit derivation sources should use explicit vendor hash files by key";
      }
      {
        assertion = !goUnitDerivedUnreadableNoSumEval.success;
        message = "go-unit derivation sources should reject no-sum mode when go.mod is unreadable";
      }
      {
        assertion = !goUnitDerivedMissingGoSumKeyEval.success;
        message = "go-unit derivation sources should not derive vendor keys from go.mod alone";
      }
      {
        assertion = !goUnitMissingGoModEval.success;
        message = "go-unit local sources should reject missing go.mod during eval";
      }
      {
        assertion = !goUnitMissingGoModPackagesEval.success;
        message = "go-unit local package surfaces should reject missing go.mod during eval";
      }
      {
        assertion = !goUnitMissingGoSumEval.success;
        message = "go-unit local sources should reject missing go.sum even with a direct vendor hash";
      }
      {
        assertion = !goUnitMissingGoSumNoSumEval.success;
        message = "go-unit local sources with external requirements should not use the no-sum path";
      }
      {
        assertion = !goUnitRequireNoSpaceNoSumEval.success;
        message = "go-unit no-sum detection should reject compact require blocks";
      }
      {
        assertion = !goUnitMissingExplicitGoSumEval.success;
        message = "go-unit explicit go.sum paths should reject filtered-out files during eval";
      }
      {
        assertion = !goUnitPackageCollisionEval.success;
        message = "go-unit workspaces should reject package patterns with colliding output names";
      }
      {
        assertion = zigApplication.passthru.tests ? lib && zigApplication.passthru.tests ? exe;
        message = "Zig packages should expose named test steps as separate derivations";
      }
      {
        assertion = zigApplication.passthru.testSteps.lib == "test-lib";
        message = "Zig packages should retain the build step names behind test derivations";
      }
      {
        assertion = zigDepsApplication.passthru.zigDeps != null;
        message = "Zig packages should materialize remote build.zig.zon dependencies through a cache derivation";
      }
      {
        assertion = !invalidSecretNameEval.success;
        message = "secret refs should reject unsafe relative names during eval";
      }
      {
        # cargoAudit is on by default (lib/rust.nix defaultPolicy): the advisory
        # scan is an offline, lockfile-only runCommand, so every workspace gets
        # it unless it opts out. A no-policy fixture must expose it.
        assertion = cargoUnitWorkspace.policyChecks ? cargoAudit;
        message = "cargo-unit workspaces should expose a cargo-audit policy check by default";
      }
      {
        # Clippy is a per-crate gate (clippyByPackage), not one workspace
        # aggregate, so editing one crate rebuilds only its clippy check.
        assertion = cargoUnitWorkspace.clippyByPackage != {};
        message = "cargo-unit workspaces should expose per-crate clippy gates";
      }
      {
        assertion = !(cargoUnitWorkspace.policyChecks ? cargoClippy);
        message = "cargo-unit buildWorkspace should suppress the legacy workspace-level cargoClippy when per-unit clippy is on";
      }
      {
        # The null default has to keep meaning "every package". The allowlist is
        # opt-in, and a new branch through a function is exactly where a default
        # quietly stops working; `!= {}` above would still pass if the default
        # started filtering to a subset.
        assertion =
          builtins.attrNames cargoUnitClippyUnfiltered.clippyByPackage
          == [
            "scope-alpha"
            "scope-bravo"
          ];
        message = "cargo-unit clippy.packages = null should gate every package in the workspace";
      }
      {
        # One of the two, so this fails against a filter that does nothing as
        # well as against one that drops everything.
        assertion = builtins.attrNames cargoUnitClippyAllowlisted.clippyByPackage == ["scope-alpha"];
        message = "cargo-unit clippy.packages should gate exactly the listed cargo package names";
      }
      {
        assertion = builtins.all lib.isDerivation (
          builtins.attrValues cargoUnitClippyAllowlisted.clippyByPackage
        );
        message = "cargo-unit clippy.packages should leave the surviving entries as ordinary check derivations";
      }
      {
        assertion = !cargoUnitClippyUnknownEval.success;
        message = "cargo-unit clippy.packages should refuse an entry that names no package in the workspace";
      }
      {
        # Each package's clippy gate is one derivation (a symlinkJoin over only
        # that package's per-unit clippy derivations) that callers can
        # string-coerce like any other check.
        assertion = builtins.all lib.isDerivation (builtins.attrValues cargoUnitWorkspace.clippyByPackage);
        message = "cargo-unit clippyByPackage entries should each be a single derivation";
      }
      {
        # The per-unit fan-out lives at `clippyUnits` for callers that want
        # individual unit derivations (e.g. exposing one flake check per
        # crate).
        assertion = builtins.isAttrs cargoUnitWorkspace.clippyUnits;
        message = "cargo-unit clippyUnits should be a fan-out attrset, one entry per linted unit";
      }
      {
        assertion = builtins.length (builtins.attrNames cargoUnitWorkspace.clippyUnits) >= 2;
        message = "cargo-unit clippyUnits should produce multiple per-unit derivations for a multi-target fixture";
      }
      {
        assertion = builtins.all (unit: lib.isDerivation unit) (
          builtins.attrValues cargoUnitWorkspace.clippyUnits
        );
        message = "cargo-unit clippyUnits entries should each be a derivation";
      }
      {
        assertion = cargoUnitWorkspace.policy.clippy.package.unchecked.pname == "llm-clippy";
        message = "cargo-unit clippy checks should use llm-clippy by default";
      }
      {
        assertion = let
          denied = cargoUnitWorkspace.policy.clippy.deniedLints;
        in
          denied == [];
        message = "cargo-unit clippy policy should defer default lint levels to Cargo.toml";
      }
      {
        assertion = cargoUnitWorkspace.policyChecks ? cargoMachete;
        message = "cargo-unit workspaces should expose a cargo-machete policy check by default";
      }
      {
        assertion = !(cargoUnitWorkspace.binaries.cargo-unit-hello ? unchecked);
        message = "cargo-unit package outputs should stay independent from aggregate policy checks";
      }
      {
        assertion = builtins.hasAttr "cargo_unit_hello-all" cargoUnitSelectedHello.passthru.tests;
        message = "selectBinaryWithTests should schedule package-owned test binaries";
      }
      {
        assertion = builtins.all (test: lib.isDerivation test) (
          builtins.attrValues cargoUnitSelectedHello.passthru.tests
        );
        message = "selectBinaryWithTests should expose only derivations in passthru.tests";
      }
      {
        assertion = builtins.hasAttr "cargo_unit_hello-tests-returns_greeting" cargoUnitSelectedHello.passthru.tests;
        message = "selectBinaryWithTests should expose per-case test derivations by default";
      }
      {
        assertion = builtins.all (binary: builtins.hasAttr binary cargoUnitBinaries) [
          "cargo-unit-goodbye"
          "cargo-unit-hello"
        ];
        message = "cargo-unit should build several binary roots from one workspace graph";
      }
      {
        assertion = builtins.hasAttr "cargo_unit_hello" cargoUnitWorkspace.targetSets.test.tests;
        message = "cargo-unit workspaces should expose test targets as separate checks";
      }
      {
        assertion = builtins.hasAttr "cargo_unit_hello" cargoUnitWorkspace.nextestByTarget;
        message = "cargo-unit workspaces should expose cargo-nextest checks per test target";
      }
      {
        assertion =
          cargoUnitWorkspace.testChecksByTarget.cargo_unit_hello.drvPath
          == cargoUnitWorkspace.nextestByTarget.cargo_unit_hello.drvPath;
        message = "cargo-unit testChecksByTarget should use cargo-nextest when policy.tests.useNextest is enabled";
      }
      {
        assertion =
          cargoUnitWorkspace.nextestByTarget.cargo_unit_hello.NEXTEST_HIDE_PROGRESS_BAR
          == "true"
          && cargoUnitWorkspace.nextestByTarget.cargo_unit_hello.NEXTEST_NO_INPUT_HANDLER == "true"
          && cargoUnitWorkspace.nextestByTarget.cargo_unit_hello.NEXTEST_SHOW_PROGRESS == "none";
        message = "cargo-unit cargo-nextest checks should force non-interactive reporter environment";
      }
      {
        assertion = lib.isDerivation cargoUnitWorkspace.testChecksAll;
        message = "cargo-unit workspaces should expose an aggregate test-check derivation";
      }
      {
        assertion = cargoUnitWorkspace.doctests != {};
        message = "cargo-unit workspaces should expose doctest targets as separate checks";
      }
      {
        assertion = cargoUnitWorkspace.targetSets.build.doctests != {};
        message = "cargo-unit target sets should expose doctest targets next to build roots";
      }
      {
        assertion = builtins.hasAttr "greeting" cargoUnitWorkspace.targetSets.bench.benchmarks;
        message = "cargo-unit workspaces should expose benchmark targets separately from tests";
      }
      {
        assertion = builtins.hasAttr "greeting" cargoUnitWorkspace.benchmarks;
        message = "cargo-unit workspaces should expose aggregate benchmark targets";
      }
      {
        assertion = cargoUnitWorkspace ? testPlan;
        message = "cargo-unit workspaces should expose a reusable test plan";
      }
      {
        assertion = cargoUnitWorkspace ? coverageReport;
        message = "cargo-unit workspaces should expose a reusable coverage report";
      }
      {
        assertion = cargoUnitWorkspace ? makeCoverageReport;
        message = "cargo-unit workspaces should expose a customizable coverage report builder";
      }
      {
        assertion = cargoUnitWorkspace ? benchmarkPlan;
        message = "cargo-unit workspaces should expose a reusable benchmark plan";
      }
      {
        assertion = cargoUnitWorkspace ? compareTangoBenchmarks;
        message = "cargo-unit workspaces should expose a Tango comparison builder";
      }
      {
        assertion =
          cargoUnitWorkspace.targetSets.build.binaries.cargo-unit-hello.drvPath
          == cargoUnitWorkspace.binaries.cargo-unit-hello.drvPath;
        message = "cargo-unit should expose named target-set outputs without losing aggregate outputs";
      }
      {
        assertion =
          cargoUnitSubsetWorkspace.binaries.cargo-unit-hello.drvPath
          == cargoUnitWorkspace.targetSets.build.binaries.cargo-unit-hello.drvPath;
        message = "narrowing cargoTargets must yield identical root derivations; select roots lazily from the multi-target workspace instead of a subset buildWorkspace";
      }
      {
        assertion = cargoUnitPolicyDisabledWorkspace.policyChecks == {};
        message = "cargo-unit policy checks should be disableable for generated workspaces";
      }
      {
        # A Darwin consumer forces the (x86_64-linux) vendor dir at eval
        # through the cross lane's IFD and can only ever satisfy it by
        # substitution; linkFarm's `allowSubstitutes = false` default made
        # that impossible (#1711). Pin the override so a vendor-dir refactor
        # cannot silently regress Darwin eval of cross packages.
        assertion = cargoUnitWorkspace.vendorDir.allowSubstitutes or false;
        message = "the aggregate cargo vendor dir must set allowSubstitutes = true; Darwin cross consumers can only substitute it (#1711)";
      }
      {
        assertion = cargoUnitScope.base.alpha.drvPath != cargoUnitScope.alphaChanged.alpha.drvPath;
        message = "cargo-unit should rebuild the changed workspace crate";
      }
      {
        assertion = cargoUnitScope.base.bravo.drvPath == cargoUnitScope.alphaChanged.bravo.drvPath;
        message = "cargo-unit should keep unrelated workspace crate derivations stable when one crate source changes";
      }
      {
        assertion = cargoUnitScope.base.itoa.drvPath == cargoUnitScope.alphaChanged.itoa.drvPath;
        message = "cargo-unit should keep locked transitive dependency derivations stable when workspace source changes";
      }
      {
        assertion = cargoUnitScope.base.ryu.drvPath == cargoUnitScope.alphaChanged.ryu.drvPath;
        message = "cargo-unit should keep unrelated locked dependency derivations stable when workspace source changes";
      }
      {
        assertion = cargoUnitScope.base.itoa.drvPath != cargoUnitScope.lockChanged.itoa.drvPath;
        message = "cargo-unit should rebuild the locked dependency whose Cargo.lock entry changed";
      }
      {
        assertion = cargoUnitScope.base.ryu.drvPath == cargoUnitScope.lockChanged.ryu.drvPath;
        message = "cargo-unit should keep unrelated locked dependency derivations stable when another transitive dependency changes";
      }
      {
        # The planner runs against a manifest-scoped stub of `src` (#3900):
        # base and alphaChanged differ only in one crate's source body, so
        # they must plan through the SAME unit-graph derivation.
        assertion =
          cargoUnitScopeWorkspaces.base.unitGraphJson.drvPath
          == cargoUnitScopeWorkspaces.alphaChanged.unitGraphJson.drvPath;
        message = "cargo-unit should keep the unit-graph planner derivation stable when only a crate source body changes (#3900)";
      }
      {
        # The render stage include-scans real file contents, so it must
        # still re-run on a source body change.
        assertion =
          cargoUnitScopeWorkspaces.base.unitsNix.drvPath
          != cargoUnitScopeWorkspaces.alphaChanged.unitsNix.drvPath;
        message = "cargo-unit should re-render units.nix when a crate source body changes";
      }
      {
        assertion =
          cargoUnitScopeWorkspaces.base.unitGraphJson.drvPath
          != cargoUnitScopeWorkspaces.lockChanged.unitGraphJson.drvPath;
        message = "cargo-unit should re-plan the unit graph when Cargo.lock changes";
      }
      {
        assertion = builtins.any (
          source: source.base == "workspace" && source.scope == "package" && source.relative == "crates/alpha"
        ) (builtins.attrValues cargoUnitScopeWorkspaces.base.sourceAudit);
        message = "cargo-unit source audit should record package-shaped workspace sources";
      }
      {
        # Forcing this at all is most of the test: with `src` a bare path the
        # render IFD used to abort with "outside workspace root" before any
        # audit existed to read (#4239). The relative paths are then what says
        # the tie-back found the right root and not merely a root: a rebase
        # onto some other tree would still render, just with member paths
        # carved from the wrong place.
        assertion =
          builtins.sort builtins.lessThan (
            map (source: source.relative) (
              builtins.filter (source: source.base == "workspace") (
                builtins.attrValues cargoUnitScopeWorkspaces.pathSrc.sourceAudit
              )
            )
          )
          == [
            "crates/alpha"
            "crates/bravo"
          ];
        message = "cargo-unit should scope local units against `src` when `src` and `workspaceRoot` are the same bare path (#4239)";
      }
      {
        # Same members, same contents, so the two spellings of one source tree
        # must produce the same unit derivations. This is what a tie-back that
        # resolved to a convenient root rather than the right one would fail:
        # different relatives mean differently scoped sources mean different
        # units, built from the wrong subtree.
        assertion =
          cargoUnitScope.pathSrc.alpha.drvPath
          == cargoUnitScope.base.alpha.drvPath
          && cargoUnitScope.pathSrc.bravo.drvPath == cargoUnitScope.base.bravo.drvPath;
        message = "a bare-path `src` and a pre-filtered `src` over the same tree must render identical units (#4239)";
      }
      {
        assertion = builtins.any (
          source:
            source.base
            == "vendor-package"
            && source.scope == "package"
            && source.sourceKey == "registry+https://github.com/rust-lang/crates.io-index#itoa@1.0.18"
        ) (builtins.attrValues cargoUnitScopeWorkspaces.base.sourceAudit);
        message = "cargo-unit source audit should record full dependency source identity";
      }
      {
        # cargo-machete is dropped in favor of the per-crate
        # unused_crate_dependencies (rustc) gate (lib/rust/workspace.nix).
        assertion = !(repoPackages.minecraft-nbt.passthru.policyChecks ? cargoMachete);
        message = "repo Rust packages should not expose cargo-machete (dropped for the per-crate unused-deps gate)";
      }
      {
        # cargoAudit is lockfile-scoped and exposed once at the workspace level
        # (per-system rust-cargoAudit), not aliased onto every crate.
        assertion = !(repoPackages.minecraft-nbt.passthru.policyChecks ? cargoAudit);
        message = "repo Rust packages should not alias the workspace cargoAudit per crate";
      }
      {
        # Repo packages route through `cargoUnit.buildWorkspace` via
        # `ix.rustWorkspace.units`, so they pick up their own per-crate clippy
        # gate (clippyByPackage) rather than a workspace-wide aggregate or the
        # legacy `cargoClippy` single derivation.
        assertion = repoPackages.minecraft-nbt.passthru.policyChecks ? clippy;
        message = "repo Rust packages should expose a per-crate clippy policy check";
      }
      {
        assertion = repoPackages.minecraft-nbt.passthru.policyChecks ? unusedCrateDependencies;
        message = "repo Rust packages with dependencies should expose a per-crate unused-crate-dependencies check";
      }
      {
        assertion = !(repoPackages.minecraft-nbt.passthru.policyChecks ? cargoClippy);
        message = "repo Rust packages should not also expose the legacy workspace-level cargoClippy when per-unit clippy is active";
      }
      {
        assertion =
          repoPackages.minecraft-nbt.passthru.policy.clippy.package.unchecked.pname == "llm-clippy";
        message = "repo Rust clippy checks should use llm-clippy by default";
      }
      {
        assertion = let
          denied = repoPackages.minecraft-nbt.passthru.policy.clippy.deniedLints;
        in
          denied == [];
        message = "repo Rust clippy policy should defer default lint levels to Cargo.toml";
      }
      {
        assertion = repoPackages.minecraft-nbt.passthru.tests ? package;
        message = "repo Rust package builds should be exposed as flake-checkable tests";
      }
      {
        assertion = repoPackages.minecraft-nbt.passthru.tests ? unusedCrateDependencies;
        message = "repo Rust per-crate policy checks should be exposed as flake-checkable tests";
      }
      {
        assertion = !(repoPackages.dag-runner.passthru ? unchecked);
        message = "repo Rust package outputs should not wrap unrelated workspace policy checks";
      }
      {
        # dag-runner's integration test target is renamed `integration_dag_runner`
        # (packages/dag-runner/Cargo.toml) so it does not collide with the other
        # `integration` test targets (git-log-pretty, clone-detect) in cargo-unit's
        # flat target namespace. The unique name keeps the generated key stable
        # instead of `-<version>-<hash>`-suffixed.
        assertion = builtins.hasAttr "integration_dag_runner-all" repoPackages.dag-runner.passthru.tests;
        message = "repo Rust package tests should include package-owned integration test targets";
      }
      {
        assertion = builtins.hasAttr "minecraft_nbt-all" repoPackages.minecraft-nbt.passthru.tests;
        message = "repo Rust package tests should include package-owned library test targets";
      }
      {
        assertion = builtins.hasAttr "property-all" repoPackages.minecraft-nbt.passthru.tests;
        message = "repo Rust package tests should include package-owned property test targets";
      }
      {
        assertion = builtins.hasAttr "doctest-minecraft_nbt-all" repoPackages.minecraft-nbt.passthru.tests;
        message = "repo Rust package tests should include package-owned doctest targets";
      }
      {
        assertion = minecraft.config.ix.build.ociEfficiency.enable;
        message = "OCI image builds should check layer efficiency by default";
      }
      {
        assertion =
          bunLockPackage.name
          == "clsx"
          && bunLockPackage.version == "2.1.1"
          && lib.hasPrefix "sha512-" bunLockPackage.integrity;
        message = "bun lock helper should derive registry fetch metadata from bun.lock";
      }
      {
        assertion =
          uvLockedDistribution.name
          == "click"
          && uvLockedDistribution.version == "8.1.7"
          && lib.hasPrefix "sha256-" uvLockedDistribution.hash;
        message = "uv lock helper should derive registry fetch metadata from uv.lock";
      }
      {
        assertion =
          builtins.elem "click-8.1.7-py3-none-any.whl" uvWheelhouseDistributionNames
          && !(builtins.elem "click-8.1.7.tar.gz" uvWheelhouseDistributionNames);
        message = "uv wheelhouses should prefer compatible wheels over sdists";
      }
      {
        assertion =
          ix.deepMerge.strict
          {
            a = {
              x = 1;
            };
            b = 2;
          }
          {
            a = {
              y = 3;
            };
            c = 4;
          }
          == {
            a = {
              x = 1;
              y = 3;
            };
            b = 2;
            c = 4;
          };
        message = "deepMerge.strict should recursively union disjoint subtrees";
      }
      {
        assertion =
          !(builtins.tryEval (builtins.deepSeq (ix.deepMerge.strict {a.b = 1;} {a.b = 2;}) null)).success;
        message = "deepMerge.strict should throw on a colliding leaf";
      }
      {
        assertion =
          ix.deepMerge.rhs
          {
            Service = {
              ExecStart = "/run/wrapped";
              Restart = "on-failure";
            };
          }
          {
            Service = {
              Restart = "always";
              MemoryMax = "512M";
            };
          }
          == {
            Service = {
              ExecStart = "/run/wrapped";
              Restart = "always";
              MemoryMax = "512M";
            };
          };
        message = "deepMerge.rhs should override leaves while keeping sibling keys at the same path";
      }
      {
        assertion =
          ix.deepMerge.rhs {pkg = pkgs.hello;} {pkg = pkgs.coreutils;} == {pkg = pkgs.coreutils;};
        message = "deepMerge.rhs should treat derivations as atomic leaves";
      }
      {
        assertion =
          !(builtins.tryEval (
            builtins.deepSeq (ix.deepMerge.strict {pkg = pkgs.hello;} {pkg = pkgs.coreutils;}) null
          )).success;
        message = "deepMerge.strict should throw on a derivation collision instead of recursing into it";
      }
      {
        assertion =
          ix.deepMerge.strictList [
            {a.x = 1;}
            {a.y = 2;}
            {b = 3;}
          ]
          == {
            a = {
              x = 1;
              y = 2;
            };
            b = 3;
          };
        message = "deepMerge.strictList should fold strict over a list of disjoint trees";
      }
    ];

    languages = [
      {
        assertion = languages.elixirLatest.drvPath == pkgs.beamPackages.elixir.drvPath;
        message = "ix.languages.elixir latest should follow beamPackages.elixir";
      }
      {
        assertion = languages.erlangLatest.drvPath == pkgs.beamPackages.erlang.drvPath;
        message = "ix.languages.erlang latest should follow beamPackages.erlang";
      }
      {
        assertion = builtins.isString languages.languageTableDrvPaths;
        message = "every version advertised by an ix.languages table should instantiate: no deprecation warning, no removed alias, no insecure package";
      }
      {
        assertion = languages.erlangRebarDefault.drvPath == languages.erlangRebarExplicit.drvPath;
        message = "ix.languages.erlang rebar3 should default to beamPackages.erlang";
      }
      {
        assertion = !languages.pythonMissingVersion.success;
        message = "ix.languages.python should require an explicit interpreter version";
      }
      {
        assertion = !languages.pythonUnknown.success;
        message = "ix.languages.python should throw on an unknown version instead of returning a missing-attr error";
      }
      {
        assertion = !languages.rustMissingVersion.success;
        message = "ix.languages.rust should require an explicit toolchain version";
      }
      {
        assertion = languages.rustExtraComponents.drvPath != languages.rustPinnedNightly.drvPath;
        message = "ix.languages.rust should let callers extend the component set";
      }
      {
        assertion = !languages.rustBadChannel.success;
        message = "ix.languages.rust should reject unknown channels with errors.assertEnum";
      }
      {
        assertion = !languages.rustBadProfile.success;
        message = "ix.languages.rust should reject unknown profiles with errors.assertEnum";
      }
      {
        assertion = !languages.javaMissingDistribution.success;
        message = "ix.languages.java should require an explicit JDK distribution";
      }
      {
        assertion = !languages.javaBadDistribution.success;
        message = "ix.languages.java should reject unknown distributions with errors.assertEnum";
      }
      {
        assertion = !languages.javaBadVersion.success;
        message = "ix.languages.java should reject unknown versions with errors.requireAttr";
      }
      {
        assertion = lib.hasInfix "-agentpath:" minestomYourkit.execStart;
        message = "services.minestom.yourkit.enable should inject -agentpath: into the JVM command";
      }
      {
        assertion = lib.hasInfix "port=10001" minestomYourkit.execStart;
        message = "services.minestom.yourkit should pass the default YourKit port through the agent options";
      }
      {
        assertion = lib.hasInfix "listen=all" minestomYourkit.execStart;
        message = "services.minestom.yourkit.listen = \"all\" should appear in the agent options";
      }
      {
        assertion = lib.hasInfix "sessionname=minestom-eval-test" minestomYourkit.execStart;
        message = "services.minestom.yourkit.sessionName should appear in the agent options";
      }
      {
        assertion = builtins.elem 10_001 minestomYourkit.firewallTcpPorts;
        message = "services.minestom.yourkit.openFirewall should open the YourKit port in the firewall";
      }
      {
        assertion = minestomYourkit.portClaim != null && minestomYourkit.portClaim.port == 10_001;
        message = "services.minestom.yourkit.enable should register a portClaim for the YourKit port";
      }
      {
        assertion = !(lib.hasInfix "-agentpath:" minestomNoYourkit.execStart);
        message = "services.minestom without yourkit.enable should NOT include -agentpath:";
      }
      {
        assertion = minestomNoYourkit.portClaim == null;
        message = "services.minestom without yourkit.enable should NOT register a yourkit portClaim";
      }
    ];

    fleet = [
      {
        assertion = builtins.pathExists (paths.examples + "/nixos/switch/flake.nix");
        message = "native ix apply examples should include the flake.nix entrypoint the CLI resolves";
      }
      {
        assertion =
          builtins.pathExists (paths.examples + "/dev/vm/default.ix")
          && builtins.pathExists (paths.examples + "/dev/vm/dev.nix");
        message = "dev-vm should expose default.ix as the mkDev entrypoint and dev.nix as the editable module";
      }
      {
        assertion =
          lib.all (
            rel: let
              text = builtins.readFile (paths.examples + "/${rel}/README.md");
            in
              lib.hasInfix "ix apply" text && !(lib.hasInfix "ix.nix" text)
          )
          applyReadmes;
        message = "example READMEs should document the ix apply entrypoint against default.ix, with no stale ix.nix references";
      }
      {
        assertion = fleet.nodes.db.networking.hostName == "db";
        message = "fleet nodes should default hostName to the node name";
      }
      {
        assertion = fleet.nodes.db.ix.networking.eastWest.hostName == "db";
        message = "fleet nodes should expose their east-west host name through ix.networking";
      }
      {
        assertion = fleet.nodes.web.environment.etc.db-host.text == "db";
        message = "fleet node modules should be able to reference nodes.<name>.config";
      }
      {
        assertion = fleet.nodes.db.services.ix-postgresql.enable;
        message = "fleet plain attrset nodes should be treated as modules";
      }
      {
        assertion =
          fleetPlan.web.bootstrapImage == "registry.ix.dev/ix/test-cluster-bootstrap:zstd-tools-2026-05-12";
        message = "fleet switches should create missing nodes from the shared NixOS bootstrap image";
      }
      {
        assertion = fleetPlan.web.replacementImage.destination == "fleet-web:latest";
        message = "fleet wrapped-node deployment destination should flow into the replacement image plan";
      }
      {
        assertion = fleetPlan.web.system == "${fleet.nodes.web.system.build.toplevel}";
        message = "fleet plans should expose the NixOS system closure for switch";
      }
      {
        assertion = fleet.systemPackages.web-system == fleet.nodes.web.system.build.toplevel;
        message = "fleet system package outputs should match default source switch installables";
      }
      {
        assertion =
          fleet.nixosConfigurations.web.config.system.build.toplevel == fleet.nodes.web.system.build.toplevel;
        message = "fleet should expose nixosConfigurations.<node> so `ix apply .#<node>` (native multi-VM switch) resolves the node toplevel";
      }
      {
        assertion = fleet.packages.web == fleet.nodes.web.ix.build.casImage;
        message = "fleet replacement package outputs should keep node names and resolve to the CAS image";
      }
      {
        assertion =
          fleetPlan.web.switch
          == {
            target = builtins.unsafeDiscardStringContext fleet.nodes.web.system.build.toplevel.drvPath;
            buildOn = "remote";
            sourceInstallable = ".#web";
            overrideInputs = {};
          };
        message = "fleet plans should default to local eval and remote build switch metadata";
      }
      {
        assertion = fleetPlan.web.replacementImage.sourceInstallable == ".#web";
        message = "fleet plans should reference the replacement image only by flake installable; forcing the CAS image at plan eval would IFD-build every node's system closure";
      }
      {
        assertion = !fleetMissingCasBuilderEval.success;
        message = "forcing a node's CAS image without the ix-side casImageBuilder must abort eval, never fall back";
      }
      {
        assertion = fleetPlan.web.region == "us-west-1";
        message = "fleet nodes should inherit the top-level deployment region";
      }
      {
        assertion = fleetPlan.web.tags == ["public"];
        message = "fleet wrapped-node tags should flow into the generated plan";
      }
      {
        assertion = fleetPlan.web.groups == ["public-apps"];
        message = "fleet wrapped-node east-west groups should flow into the generated plan";
      }
      {
        assertion = declaredIpv4Plan.edge.ipv4;
        message = "an image declaring ix.networking.ipv4 should get a public address in the plan without deployment.ipv4";
      }
      {
        assertion = declaredIpv4Plan.deployed.ipv4;
        message = "deployment.ipv4 should still turn the public address on for an image that does not declare one";
      }
      {
        # The plan alone is not enough: `ix apply` never reads it. This is the
        # assertion that would have caught ENG-10846, where two proxies came up
        # with no address because `deployment.ipv4` stopped at the plan.
        assertion = declaredIpv4Fleet.nodes.deployed.ix.networking.ipv4;
        message = "deployment.ipv4 must reach the evaluated system as ix.networking.ipv4, which is the only place ix apply looks";
      }
      {
        assertion = !declaredIpv4Fleet.nodes.internal.ix.networking.ipv4;
        message = "a node whose deployment does not ask for an address must not have one forced into its evaluated system";
      }
      {
        assertion = !declaredIpv4Plan.internal.ipv4;
        message = "a node that declares no public address anywhere should not be given one";
      }
      {
        assertion =
          declaredIpv4Plan.edge.healthChecks ? public-reachable;
        message = "a requiresIpv4 health check should be accepted when the image itself declares ix.networking.ipv4";
      }
      {
        assertion = fleetPlan.web.ipv4;
        message = "fleet wrapped-node deployment overrides should flow into the generated plan";
      }
      {
        assertion = let
          check = fleetPlan.db.healthChecks.ix-postgresql;
          pgIsReady = lib.getExe' fleet.nodes.db.services.postgresql.package "pg_isready";
        in
          check.from
          == "guest"
          && check.command
          == [
            pgIsReady
            "--quiet"
            "--host"
            "/run/postgresql"
            "--port"
            "5432"
          ]
          && check.timeoutSec == 30;
        message = "fleet plans should carry pg_isready-backed Postgres readiness checks";
      }
      {
        assertion = !fleetIpv4HealthCheckEval.success;
        message = "fleet plans should reject host-side IPv4 checks on private nodes";
      }
      {
        assertion = !fleetUnknownDependencyEval.success;
        message = "fleet plans should reject unknown dependsOn entries during eval";
      }
      {
        assertion = !fleetDeploymentHealthChecksEval.success;
        message = "fleet plans should reject the dead deployment.healthChecks selector during eval";
      }
      {
        assertion = !fleetUnknownDeploymentKeyEval.success;
        message = "fleet plans should reject unknown deployment keys during eval";
      }
      {
        assertion = !fleetDependencyCycleEval.success;
        message = "fleet plans should reject cyclic dependsOn entries during eval";
      }
      {
        assertion =
          fleetPlan.web.secrets
          == [
            {
              name = "fleet_default";
              target = {
                name = "FLEET_DEFAULT";
                injectAs = "env";
              };
            }
            {
              name = "github_token";
              target = {
                name = "GH_TOKEN";
                injectAs = "env";
              };
            }
          ]
          && fleetPlan.db.secrets
          == [
            {
              name = "fleet_default";
              target = {
                name = "FLEET_DEFAULT";
                injectAs = "env";
              };
            }
          ];
        message = "per-VM secret attachments should merge fleet-wide and node-level refs";
      }
      {
        assertion = fleetPlan.worker-0.baseName == "worker" && fleetPlan.worker-1.replicaIndex == 1;
        message = "fleet replicas should expand into stable node identities";
      }
      {
        assertion =
          fleetPlan.worker-0.updateStrategy.maxUnavailable
          == 1
          && fleetPlan.worker-1.updateStrategy.maxUnavailable == 1
          && fleetPlan.web.updateStrategy == null;
        message = "fleet updateStrategy should flow into every replica's plan and default to null";
      }
      {
        assertion = !fleetUnknownUpdateStrategyKeyEval.success;
        message = "fleet plans should reject unknown updateStrategy keys during eval";
      }
      {
        assertion = !fleetInvalidMaxUnavailableEval.success;
        message = "fleet plans should reject a non-positive updateStrategy.maxUnavailable during eval";
      }
      {
        assertion = fleetPlan.worker-0.dependsOn == ["db"];
        message = "fleet replica dependencies should point at expanded node identities";
      }
      {
        assertion =
          prefixedFleet.planValue.order
          == [
            "tprefix-api"
            "tprefix-worker"
          ];
        message = "withNodePrefix should rename every node in the plan order";
      }
      {
        assertion = prefixedFleet.planValue.nodes.tprefix-worker.dependsOn == ["tprefix-api"];
        message = "withNodePrefix should rewrite dependsOn references so the prefixed graph stays connected";
      }
      {
        assertion = prefixedFleet.planValue.nodes.tprefix-worker.groups == ["tprefix-private-apps"];
        message = "withNodePrefix should rewrite east-west group names so scratch fleets do not collide";
      }
      {
        assertion =
          prefixedFleet.planValue.nodes.tprefix-api.replacementImage.destination == "tprefix-api:latest";
        message = "withNodePrefix should prefix the registry destination so scratch pushes cannot clobber the base tag";
      }
      {
        assertion = prefixedFleet.nodes.tprefix-api.networking.hostName == "api";
        message = "withNodePrefix is a plan-level rename: guest hostname and image name stay base-named so the prefixed fleet shares the base fleet's closures";
      }
      {
        assertion =
          prefixedFleet.planValue.nodes.tprefix-api.system
          == prefixedFleetBase.planValue.nodes.api.system
          && prefixedFleet.planValue.nodes.tprefix-api.replacementImage.sourceInstallable
          == ".#tprefix-api";
        message = "withNodePrefix must reuse the base fleet's system closure while re-deriving the replacement installable to the prefixed packages attr";
      }
      {
        assertion = prefixedFleet.nodes.tprefix-worker.environment.etc.api-host.text == "api";
        message = "nodes module-arg should resolve by the example's base name even when prefixed";
      }
      {
        assertion = prefixedFleet.planValue.nodes.tprefix-api.switch.sourceInstallable == ".#tprefix-api";
        message = "withNodePrefix should re-derive the default `.#<node>` installable to the prefixed attr so the native multi-VM `ix apply` names the prefixed VM";
      }
      {
        assertion =
          prefixedFleet.nixosConfigurations.tprefix-api.config.system.build.toplevel
          == prefixedFleetBase.nixosConfigurations.api.config.system.build.toplevel;
        message = "withNodePrefix should expose nixosConfigurations under the prefixed name while reusing the base closure (no second eval)";
      }
      {
        assertion = localBuildFleet.planValue.nodes.svc.switch.sourceInstallable == ".#svc-system";
        message = "a local-build node should default to the `.#<node>-system` package alias, since its plain `nix build` has no `ix apply` rewrite";
      }
      {
        assertion =
          (explicitInstallableFleet.withNodePrefix "tprefix-")
          .planValue.nodes.tprefix-svc.switch.sourceInstallable == ".#svc";
        message = "an explicit sourceInstallable equal to the default must survive withNodePrefix unchanged (prefixing keys on provenance, not the rendered string)";
      }
    ];
  };

  # --- Build-time checks ----------------------------------------------------

  buildScripts = {
    security-roots = ''
      root=${pkgs.hello}
      case "$root" in
        ${builtins.storeDir}/*) ;;
        *)
          echo "security root did not realize to a terminal store path: $root" >&2
          exit 1
          ;;
      esac
    '';
    factions = ''
      grep -q '^QuickShop-Hikari$' ${factionsExample.managed.dropins}/quickshop-hikari.jar.plugin-name
      grep -q '^Vault$' ${factionsExample.managed.dropins}/vaultunlocked.jar.plugin-name
      grep -q '^Essentials$' ${factionsExample.managed.dropins}/essentialsx.jar.plugin-name
      grep -q '^EssentialsSpawn$' ${factionsExample.managed.dropins}/essentialsx-spawn.jar.plugin-name
      grep -q '^CoreProtect$' ${factionsExample.managed.dropins}/coreprotect.jar.plugin-name
      grep -q '^EternalEconomy$' ${factionsExample.managed.dropins}/eternaleconomy.jar.plugin-name
      grep -q '^CombatLog$' ${factionsExample.managed.dropins}/combatlogplugin.jar.plugin-name
      grep -q '^voicechat$' ${factionsExample.managed.dropins}/simple-voice-chat.jar.plugin-name
      grep -q '^BlueMap$' ${factionsExample.managed.dropins}/bluemap.jar.plugin-name
      grep -q '^Skript$' ${factionsExample.managed.dropins}/skript.jar.plugin-name
      grep -q '^max-world-size=6000$' ${factionsExample.managed.serverFiles}/server.properties
      grep -q 'max-tnt-per-tick: -1' ${factionsExample.managed.serverFiles}/spigot.yml
      grep -q 'query-plugins: false' ${factionsExample.managed.serverFiles}/bukkit.yml
      grep -q '^port=24454$' ${factionsExample.managed.serverFiles}/plugins/voicechat/voicechat-server.properties
      grep -q '"port": 8100' ${factionsExample.managed.serverFiles}/plugins/BlueMap/webserver.conf
      grep -q '"accept-download": true' ${factionsExample.managed.serverFiles}/plugins/BlueMap/core.conf
      grep -q '"height": 4064' ${factionsExample.managed.datapacks}/max-height/data/minecraft/dimension_type/overworld.json
      grep -q '"height": 4064' ${factionsExample.managed.datapacks}/max-height/data/minecraft/dimension_type/the_end.json
      grep -q 'optimize-explosions: true' ${factionsExample.managed.config}/paper-world-defaults.yml
      grep -q 'allow-piston-duplication: true' ${factionsExample.managed.config}/paper-global.yml
      grep -q 'worldborder set 12000' ${factionsExample.service.serviceConfig.ExecStart}
    '';

    survival = ''
      test -L ${survivalExample.managed.velocityPlugins}/Geyser-Velocity.jar
      test -L ${survivalExample.managed.velocityPlugins}/floodgate-velocity.jar
      grep -q 'bind = "0.0.0.0:25565"' ${survivalExample.managed.velocityConfig}/velocity.toml
      grep -q 'player-info-forwarding-mode = "modern"' ${survivalExample.managed.velocityConfig}/velocity.toml
      grep -q 'survival = "127.0.0.1:25566"' ${survivalExample.managed.velocityConfig}/velocity.toml
      grep -q 'auth-type: floodgate' ${survivalExample.managed.velocityConfig}/plugins/geyser/config.yml
      grep -q 'port: 19132' ${survivalExample.managed.velocityConfig}/plugins/geyser/config.yml
      grep -q 'send-floodgate-data: false' ${survivalExample.managed.velocityConfig}/plugins/floodgate/proxy-config.yml
      grep -q 'enabled: true' ${survivalExample.managed.minecraftConfig}/paper-global.yml
      grep -q 'secret: ix-survival-example-forwarding-secret-change-me' ${survivalExample.managed.minecraftConfig}/paper-global.yml
      grep -q '^server-port=25566$' ${survivalExample.managed.minecraftServerFiles}/server.properties
      grep -q '^online-mode=false$' ${survivalExample.managed.minecraftServerFiles}/server.properties
    '';

    observability-stack = ''
      test -x ${observabilityStackExample.observability.queryTool}/bin/ix-observe
      grep -q '"uid": "ix-observability"' ${observabilityStackExample.observability.dashboardPath}/overview.json
      grep -q 'otel_traces' ${observabilityStackExample.observability.dashboardPath}/overview.json
      grep -q 'otel_logs' ${observabilityStackExample.observability.dashboardPath}/overview.json
    '';

    extended-attributes = ''
      rm -rf /build/ix-xattr-test
      mkdir -p /build/ix-xattr-probe
      if ${pkgs.attr}/bin/setfattr --name user.ix.probe --value yes -- /build/ix-xattr-probe; then
        ${extendedAttributes.activationScript}
        test -d /build/ix-xattr-test
        test "$(${pkgs.attr}/bin/getfattr --absolute-names --only-values -n user.ix.kind /build/ix-xattr-test)" = "test.path"
        test "$(${pkgs.attr}/bin/getfattr --absolute-names --only-values -n user.ix.owner /build/ix-xattr-test)" = "ix"
      else
        echo "xattrs are not supported by the Nix build sandbox filesystem; checked activation rendering by eval"
      fi
    '';

    vitest = lib.concatMapStringsSep "\n" (case: "test -d ${case}") vitestWorkspaceCases;

    minecraft = ''
      ! grep -R 'rcon.password' ${minecraft.rcon.managed.serverFiles}
      grep -q 'worldborder center 100 -50' ${minecraft.worldBorder.service.serviceConfig.ExecStart}
      grep -q 'worldborder set 8000' ${minecraft.worldBorder.service.serviceConfig.ExecStart}
      grep -q '^query.port=25565$' ${minecraft.nestedProperties.managed.serverFiles}/server.properties
      grep -q '^rcon.port=25575$' ${minecraft.nestedProperties.managed.serverFiles}/server.properties
      grep -q '^white-list=true$' ${minecraft.access.managed.serverFiles}/server.properties
      grep -q '^enforce-whitelist=true$' ${minecraft.access.managed.serverFiles}/server.properties
      grep -q 'factions_nether:' ${
        minecraft.paperPlugins.config.environment.etc."minecraft/managed-server-files".source
      }/bukkit.yml
      grep -q 'factions_the_end:' ${
        minecraft.paperPlugins.config.environment.etc."minecraft/managed-server-files".source
      }/bukkit.yml
      grep -q 'generator: TerraformGenerator' ${
        minecraft.paperPlugins.config.environment.etc."minecraft/managed-server-files".source
      }/bukkit.yml
      grep -q '"name": "Alice"' ${minecraft.access.managed.access}/whitelist.json
      grep -q '"name": "Bob"' ${minecraft.access.managed.access}/whitelist.json
      grep -q '"level": 3' ${minecraft.access.managed.access}/ops.json
      grep -q '"bypassesPlayerLimit": true' ${minecraft.access.managed.access}/ops.json

      rm -rf /build/minecraft-access-data /build/minecraft-managed-root
      mkdir -p /build/minecraft-access-data/.ix-managed-access /build/minecraft-managed-root
      ln -s ${minecraft.access.managed.access} /build/minecraft-managed-root/managed-access
      ln -s ${minecraft.access.managed.serverFiles} /build/minecraft-managed-root/managed-server-files
      cp ${minecraft.access.fixtures.whitelist.current} /build/minecraft-access-data/whitelist.json
      cp ${minecraft.access.fixtures.whitelist.previous} /build/minecraft-access-data/.ix-managed-access/whitelist.json
      cp ${minecraft.access.fixtures.operators.current} /build/minecraft-access-data/ops.json
      cp ${minecraft.access.fixtures.operators.previous} /build/minecraft-access-data/.ix-managed-access/ops.json

      ${lib.getExe minecraft.access.syncManaged}
      test ! -L /build/minecraft-access-data/whitelist.json
      test ! -L /build/minecraft-access-data/ops.json
      grep -q '"name": "Alice"' /build/minecraft-access-data/whitelist.json
      grep -q '"name": "Bob"' /build/minecraft-access-data/whitelist.json
      grep -q '"name": "Manual"' /build/minecraft-access-data/whitelist.json
      ! grep -q '"name": "Removed"' /build/minecraft-access-data/whitelist.json
      grep -q '"level": 3' /build/minecraft-access-data/ops.json
      grep -q '"bypassesPlayerLimit": true' /build/minecraft-access-data/ops.json
      grep -q '"name": "ManualOp"' /build/minecraft-access-data/ops.json
      ! grep -q '"name": "RemovedOp"' /build/minecraft-access-data/ops.json

      grep -q 'DataVersion: 4325' ${minecraft.nbt.managed.serverFiles}/generated/example.snbt
      grep -q 'Enabled: 1B' ${minecraft.nbt.managed.serverFiles}/generated/example.snbt
      grep -q 'Health: 20S' ${minecraft.nbt.managed.serverFiles}/generated/example.snbt
      grep -q 'Angle: 0.5F' ${minecraft.nbt.managed.serverFiles}/generated/example.snbt
      grep -q 'Precise: 12.25' ${minecraft.nbt.managed.serverFiles}/generated/example.snbt
      grep -q 'B;' ${minecraft.nbt.managed.serverFiles}/generated/example.snbt
      grep -q 'Dimension: "minecraft:overworld"' ${minecraft.nbt.managed.serverFiles}/generated/example.snbt
      grep -q 'Side: config' ${minecraft.nbt.managed.config}/generated/client.snbt
      test "$(od -An -tx1 -N5 ${minecraft.nbt.managed.serverFiles}/generated/example.nbt | tr -d ' \n')" = "0a00026978"
      test "$(od -An -tx1 -N2 ${minecraft.nbt.managed.serverFiles}/generated/example.nbt.gz | tr -d ' \n')" = "1f8b"

      grep -q '"max_format": 101' ${minecraft.datapacks.managed.datapacks}/max-height/pack.mcmeta
      grep -q '"min_y": -2032' ${minecraft.datapacks.managed.datapacks}/max-height/data/minecraft/dimension_type/overworld.json
      grep -q '"height": 4064' ${minecraft.datapacks.managed.datapacks}/max-height/data/minecraft/dimension_type/overworld.json

      rm -rf /build/minecraft-datapack-data /build/minecraft-datapack-managed-root
      mkdir -p /build/minecraft-datapack-managed-root
      ln -s ${minecraft.datapacks.managed.datapacks} /build/minecraft-datapack-managed-root/managed-datapacks

      ${lib.getExe minecraft.datapacks.syncManaged}
      test -L "/build/minecraft-datapack-data/My World/datapacks/max-height"
      grep -q '"logical_height": 4064' "/build/minecraft-datapack-data/My World/datapacks/max-height/data/minecraft/dimension_type/overworld.json"
    '';

    "minecraft_1.21.11-paper" = ''
      grep -q 'ignored-plugins' ${minecraft.paper.managed.serverFiles}/plugins/PlugManX/config.yml
      grep -q 'PlugManX' ${minecraft.paper.managed.serverFiles}/plugins/PlugManX/config.yml
      ! grep -R 'rcon.password' ${minecraft.paper.managed.serverFiles}
      grep -q '^almanac$' ${minecraft.paper.managed.dropins}/almanac.jar.plugin-name
      grep -q '^PlugManX$' ${minecraft.paper.managed.dropins}/PlugManX.jar.plugin-name
      grep -q -- '--password-file "/var/lib/minecraft/.ix-rcon-password"' ${minecraft.paper.service.config.ExecReload}
      grep -q 'plugman $row.action $row.plugin' ${minecraft.paper.service.config.ExecReload}
      ! grep -q 'reload all' ${minecraft.paper.service.config.ExecReload}
    '';
  };

  helperScript = ''
    # minecraft-blocks integration: committed fixtures -> ClickHouse local
    # spatial table -> bounding-box query. The derivation loads the fixture JSON
    # Lines into a ReplacingMergeTree table built from the one schema (Morton
    # ORDER BY, per-axis minmax skip indexes, small granule, signed-coordinate
    # offset), loads them TWICE to simulate the at-least-once restart re-send,
    # then asserts with FINAL that the replay was idempotent (the in-box count
    # and the total did not double), that the skip indexes prune (fewer granules
    # than the primary index alone), and the Morton round-trip. Realising it here
    # pulls the whole check into the `eval` aggregate. It also proves the Paper
    # plugin jar builds against the real API.
    test -f ${minecraftBlocksExample.packages.loadFixtures}/result
    grep -q 'in_box=512' ${minecraftBlocksExample.packages.loadFixtures}/result
    # The double-loaded total stays at the single-load row count: replay folded
    # the duplicates back to one row each, so the view is idempotent.
    grep -q 'idempotent_total=2977' ${minecraftBlocksExample.packages.loadFixtures}/result
    grep -qE 'pk_granules=[0-9]+ skip_granules=[0-9]+' ${minecraftBlocksExample.packages.loadFixtures}/result
    test -s ${minecraftBlocksExample.packages.loadFixtures}/events.jsonl
    test "$(wc -l < ${(paths.examples + "/minecraft/blocks/fixtures.jsonl")})" = "2977"
    grep -q '"block_type":"minecraft:stone"' ${(paths.examples + "/minecraft/blocks/fixtures.jsonl")}
    # The query tool reads with FINAL so counts are exact under the idempotent
    # ReplacingMergeTree (merge-time dedup forced at read), not only after a
    # background merge. Grep the rendered helper for the FINAL table reference.
    grep -q 'FROM block_events FINAL' ${
      minecraftBlocksExample.packages.mkQueryTool {
        host = "127.0.0.1";
        port = 9000;
      }
    }/bin/mc-blocks
    # The jar must contain only the plugin's own classes plus plugin.yml; no
    # leaked Paper/Bukkit API classes from the compile-time classpath.
    test -s ${minecraftBlocksExample.packages.plugin}
    ${lib.getExe' pkgs.unzip "unzip"} -l ${minecraftBlocksExample.packages.plugin} > mc-blocks-plugin-jar.list
    grep -q 'dev/ix/example/blockevents/BlockEventsPlugin.class' mc-blocks-plugin-jar.list
    grep -q 'plugin.yml' mc-blocks-plugin-jar.list
    ! grep -qE 'org/bukkit/|net/kyori/|com/google/' mc-blocks-plugin-jar.list

    ${lib.getExe pythonAppClosureProbe} > python-app-closure-probe.out
    grep -q 'python app source is in the runtime closure' python-app-closure-probe.out
    test -e ${processComposeApplication.passthru.tests.dryRun}
    ${lib.getExe bashApplicationProbe} > bash-application-probe.out
    grep -q 'Hello, world!' bash-application-probe.out

    ${lib.getExe zigApplication} > zig-app-fixture.out
    grep -q 'hello from zig app fixture' zig-app-fixture.out
    test -e ${zigApplication.passthru.tests.lib}/done
    test -e ${zigApplication.passthru.tests.exe}/done
    test -e ${zigDepsApplication}/bin/zig-deps-fixture
    test -e ${zigDepsApplication.passthru.tests.default}/done

    ${cargoUnitHello}/bin/cargo-unit-hello > cargo-unit-hello.out
    grep -q 'hello from cargo-unit' cargo-unit-hello.out
    ${cargoUnitScopeBinary}/bin/scope-alpha-cli > cargo-unit-scope-cli.out
    grep -q '^alpha:1$' cargo-unit-scope-cli.out
    sed 's|^${builtins.storeDir}/[a-z0-9]\{32\}-||' ${cargoUnitScopeBinaryReferences} \
      > cargo-unit-scope-references.txt
    test -s cargo-unit-scope-references.txt
    # Named, so a regression says which build-time dependency came back rather
    # than only that the count moved. Without the remap this binary retains
    # cargo-unit-source-scope-alpha (its own workspace rlib, whose `checked`
    # asserts), itoa (a vendored rlib), and the toolchain, which was 1.9 of
    # bedwars' 2.5 GiB on its own.
    for retained in cargo-unit-source-scope-alpha- itoa- ryu- \
      ${lib.escapeShellArg cargoUnitScopeRustToolchain.name}; do
      if grep -q "^$retained" cargo-unit-scope-references.txt; then
        echo "scope-alpha-cli retained its build-time dependency $retained:" >&2
        cat cargo-unit-scope-references.txt >&2
        exit 1
      fi
    done
    # The catch-all the named list cannot express: no source derivation of any
    # crate in the graph, under any name, may survive into a linked binary.
    if grep -q '^cargo-unit-source-' cargo-unit-scope-references.txt; then
      echo "scope-alpha-cli retained cargo-unit source derivations:" >&2
      cat cargo-unit-scope-references.txt >&2
      exit 1
    fi
    grep -q '^glibc-' cargo-unit-scope-references.txt
    ${cargoUnitCargoConfig}/bin/cargo-unit-cargo-config > cargo-unit-cargo-config.out
    grep -q 'cargo-config rustflags applied' cargo-unit-cargo-config.out
    ${cargoUnitBinaries.cargo-unit-goodbye}/bin/cargo-unit-goodbye > cargo-unit-goodbye.out
    grep -q 'goodbye from cargo-unit' cargo-unit-goodbye.out
    test -d ${cargoUnitWorkspace.targetSets.test.tests.cargo_unit_hello.all}
    test -d ${cargoUnitWorkspace.targetSets.test.tests.cargo_unit_hello.cases."tests::returns_greeting"}
    test -d ${
      cargoUnitWorkspace.targetSets.test.tests.cargo_unit_hello.cases."tests::package_test_env_and_path_are_available"
    }
    test -f ${cargoUnitWorkspace.nextestByTarget.cargo_unit_hello}/result
    test ! -e ${cargoUnitWorkspace.nextestByTarget.cargo_unit_hello}/junit.xml
    test -d ${(builtins.head (builtins.attrValues cargoUnitWorkspace.doctests)).all}
    test -d ${(builtins.head (builtins.attrValues (builtins.head (builtins.attrValues cargoUnitWorkspace.doctests)).cases))}
    test -s ${cargoUnitWorkspace.testPlan}/packages/cargo-unit-hello/test-binaries
    grep -q '/bin/cargo_unit_hello$' ${cargoUnitWorkspace.testPlan}/packages/cargo-unit-hello/test-binaries
    grep -qx '.' ${cargoUnitWorkspace.testPlan}/packages/cargo-unit-hello/package-root
    grep -q '^cargo-unit-source-cargo-unit-hello-0.1.0-.*	[.]$' ${cargoUnitWorkspace.testPlan}/source-roots.tsv
    test -s ${cargoUnitCoverageWorkspace.coverageReport}/lcov.info
    test -s ${cargoUnitCoverageWorkspace.coverageReport}/merged.profdata
    grep -q '^SF:src/lib.rs$' ${cargoUnitCoverageWorkspace.coverageReport}/lcov.info
    grep -q '^DA:' ${cargoUnitCoverageWorkspace.coverageReport}/lcov.info
    test -x ${cargoUnitWorkspace.benchmarkPlan}/packages/cargo-unit-hello/benchmarks/greeting
    grep -q '^cargo-unit-hello	greeting	.*/bin/greeting$' ${cargoUnitWorkspace.benchmarkPlan}/benchmarks.tsv
    test -e ${cargoUnitTangoComparison}/done
    grep -q '^cargo-unit-hello	greeting	' ${cargoUnitTangoComparison}/benchmarks.tsv
    grep -q '^greeting ' ${cargoUnitTangoComparison}/logs/cargo-unit-hello-greeting.log
    ${goUnitWorkspace.default}/bin/go-unit-hello > go-unit-hello.out
    grep -q 'hello from go-unit: Hello, world.' go-unit-hello.out
    test -e ${goUnitWorkspace.tests.root}/done
    ${goUnitNestedWorkspace.default}/bin/go-unit-nested > go-unit-nested.out
    grep -q 'hello from nested go-unit: Hello, world.' go-unit-nested.out
    test -e ${goUnitNestedWorkspace.tests.root}/done
    ${goUnitStdlibWorkspace.default}/bin/go-unit-stdlib > go-unit-stdlib.out
    grep -q 'HELLO FROM GO-UNIT STDLIB' go-unit-stdlib.out
    test -e ${goUnitStdlibWorkspace.tests.root}/done
    ${goUnitDerivedStdlibWorkspace.default}/bin/go-unit-stdlib > go-unit-stdlib-derived.out
    grep -q 'HELLO FROM GO-UNIT STDLIB' go-unit-stdlib-derived.out
    test -e ${goUnitDerivedStdlibWorkspace.tests.root}/done

    grep -q 'class="ix bun"' ${bunSite}/share/bun-site-fixture/index.html
    test -d ${bunSite.bunNodeModules}/node_modules/clsx
    test -x ${bunSite.bunNodeModules.nodeCompat}/bin/node
    grep -q 'class="ix npm"' ${npmSite}/share/npm-site-fixture/index.html
    grep -q 'class="ix svelte"' ${svelteSite}/share/npm-site-fixture/index.html
    test ! -L ${svelteSite}/share/npm-site-fixture
    test ! -L ${svelteSite}/share/npm-site-fixture/index.html
    grep -q -- '--route-prefix' ${svelteSite.passthru.serve}/bin/svelte-site-fixture
    grep -q -- '/fixture' ${svelteSite.passthru.serve}/bin/svelte-site-fixture
    test -x ${svelteSite}/bin/svelte-site-fixture
    grep -q -- "Svelte Site Fixture" ${svelteSite}/bin/svelte-site-fixture
    test -x ${svelteSite.passthru.devServer}/bin/npm-site-fixture-dev

    # wrapPackage wrapper contract: the operator's runtime PATH is preserved
    # (literal $PATH, not the baked build-sandbox PATH) and hostile env
    # literals survive both the build heredoc and the runtime sh parse.
    grep -qF 'export PATH="$PATH:' ${wrappedHello}/bin/hello
    grep -qF 'literal $HOME `code` "quoted"' ${wrappedHello}/bin/hello
    ${wrappedHello}/bin/hello > wrapped-hello.out
    grep -q 'Hello, world!' wrapped-hello.out

    ${uvApplication}/bin/uv-app-fixture > uv-app-fixture.out
    grep -q 'hello from uv app fixture' uv-app-fixture.out
    test -e ${uvApplication.uvWheelhouse}/click-8.1.7-py3-none-any.whl
  '';

  # The planner graph must not name a store path. Nix derives an output's
  # references by scanning its bytes for store hashes, so a single absolute
  # `src_path` makes this ~500 KB metadata file declare the whole vendor dir
  # as a runtime reference: hyperion's graph measured 618,148,280 bytes of
  # closure around a 488 KB file, of which ~258 MB was Windows crates no
  # target here compiles. `unitGraphJson` templates both roots out and the
  # render stage fills them back in, so what lands on disk is placeholders.
  # Reverting either `sed` in `lib/rust/cargo-unit.nix` fails this.
  cargoUnitGraphClosureScript = ''
    graph=${cargoUnitWorkspace.unitGraphJson}
    if grep -qm1 "${builtins.storeDir}/" "$graph"; then
      echo "cargo-unit-graph.json names a store path, so its closure now carries the vendor dir:" >&2
      grep -om3 "${builtins.storeDir}/[^\"]*" "$graph" >&2
      exit 1
    fi
    # The placeholders must actually be there: a graph with neither absolute
    # paths nor placeholders would pass the check above while meaning the
    # planner emitted nothing to template, which is a different bug.
    grep -q '@vendorRoot@' "$graph" || {
      echo "cargo-unit-graph.json has no @vendorRoot@ placeholder; the planner emitted no vendored source path" >&2
      exit 1
    }
    grep -q '@workspaceRoot@' "$graph" || {
      echo "cargo-unit-graph.json has no @workspaceRoot@ placeholder; the planner emitted no workspace source path" >&2
      exit 1
    }
  '';

  cargoUnitRealWorkspaceAssertions = [
    {
      assertion = builtins.hasAttr "serde_derive" cargoUnitRealWorkspaces.serde.buildWorkspace.libraries;
      message = "cargo-unit should build Serde's proc-macro workspace library";
    }
    {
      assertion = builtins.hasAttr "thiserror_impl" cargoUnitRealWorkspaces.thiserror.buildWorkspace.libraries;
      message = "cargo-unit should build Thiserror's derive implementation workspace member";
    }
    {
      assertion = builtins.hasAttr "indexmap" cargoUnitRealWorkspaces.indexmap.testWorkspace.tests;
      message = "cargo-unit should expose Indexmap's real workspace test binary";
    }
    {
      assertion = builtins.hasAttr "regex-cli" cargoUnitRealWorkspaces.regex.buildWorkspace.binaries;
      message = "cargo-unit should expose Regex's real workspace binary target";
    }
    {
      assertion = builtins.hasAttr "regex_syntax" cargoUnitRealWorkspaces.regex.testWorkspace.tests;
      message = "cargo-unit should expose Regex Syntax's real package tests";
    }
  ];

  cargoUnitRealWorkspaceScript = ''
    ${cargoUnitGraphClosureScript}
    test -d ${cargoUnitRealWorkspaces.serde.buildRoots}
    test -d ${cargoUnitRealWorkspaces.thiserror.buildRoots}
    test -d ${cargoUnitRealWorkspaces.indexmap.buildRoots}
    test -d ${cargoUnitRealWorkspaces.indexmap.testRoots}
    test -d ${cargoUnitRealWorkspaces.regex.buildRoots}
    test -d ${cargoUnitRealWorkspaces.regex.testRoots}
  '';

  # --- Prebuilt library injection seam -------------------------------------
  # Proves mkPrebuiltLibraryUnit + extraUnits/extraLibraries: a leaf library is
  # built from source, its rlib+rmeta and source-independent hash are captured,
  # and those artifacts are re-injected as a prebuilt unit that a downstream
  # consumer links with no library source in its own graph. The chain arm
  # proves the same for a prebuilt WITH a dep: only the mid prebuilt is passed
  # to extraUnits, and its recorded depUnits are auto-injected (ENG-2166).
  cargoUnitPrebuiltAssertions = [
    {
      # Source-independence: the variant lib (answer = 99) hashes to the SAME
      # unit key as the consumer's own from-source lib (answer = 42). This is the
      # property that lets a metadata-faithful prebuilt stand in for source.
      assertion = cargoUnitPrebuiltVariantLib.key == cargoUnitPrebuiltPlainLib.key;
      message = "a metadata-identical variant should produce the same unit key as the from-source lib";
    }
    {
      # Recursive source-independence: the mid unit's hash folds in the leaf
      # dep's hash, so it must also key identically across the variant and
      # from-source graphs. This is what makes auto-injected dep keys resolve.
      assertion = cargoUnitPrebuiltVariantMid.key == cargoUnitPrebuiltPlainMid.key;
      message = "a metadata-identical variant should produce the same unit key for a lib with a dep";
    }
    {
      # The injected prebuilt unit is a genuinely different derivation from the
      # variant's from-source compile unit.
      assertion = cargoUnitPrebuiltLibUnit.drvPath != cargoUnitPrebuiltVariantLib.unit.drvPath;
      message = "mkPrebuiltLibraryUnit should produce a distinct prebuilt derivation, not the from-source unit";
    }
    {
      # `extraUnits` merges over the generated `units` set under the unit key, so
      # the downstream consumer's `units.<key>` reference resolves to it.
      assertion =
        cargoUnitPrebuiltInjected.units.${cargoUnitPrebuiltVariantLib.key}.drvPath
        == cargoUnitPrebuiltLibUnit.drvPath;
      message = "extraUnits should override the generated units entry with the injected prebuilt unit";
    }
    {
      # `extraLibraries` surfaces the injected unit through `libraries`.
      assertion =
        cargoUnitPrebuiltInjected.libraries.prebuilt_lib.drvPath == cargoUnitPrebuiltLibUnit.drvPath;
      message = "extraLibraries should override the libraries entry with the injected prebuilt unit";
    }
    {
      assertion = cargoUnitPrebuiltLibUnit.passthru.unitKey == cargoUnitPrebuiltVariantLib.key;
      message = "mkPrebuiltLibraryUnit should expose the unit key it was injected under";
    }
    {
      assertion =
        cargoUnitPrebuiltChainInjected.units.${cargoUnitPrebuiltVariantMid.key}.drvPath
        == cargoUnitPrebuiltMidUnit.drvPath;
      message = "extraUnits should override the generated mid unit with the injected prebuilt";
    }
    {
      # ENG-2166: the leaf key was never passed to extraUnits; it must arrive
      # through the mid prebuilt's recorded depUnits.
      assertion =
        cargoUnitPrebuiltChainInjected.units.${cargoUnitPrebuiltVariantLib.key}.drvPath
        == cargoUnitPrebuiltLibUnit.drvPath;
      message = "buildWorkspace should auto-inject a prebuilt unit's recorded depUnits";
    }
    {
      # An explicit extraUnits entry for a dep key beats the recorded dep, and
      # the discarded dep's subtree is pruned: forcing this workspace at all
      # would fail C1 on the phantom dep's key if the traversal walked it.
      assertion =
        cargoUnitPrebuiltChainOverride.units.${cargoUnitPrebuiltVariantLib.key}.drvPath
        == cargoUnitPrebuiltLibUnitFromPlain.drvPath;
      message = "an explicit extraUnits entry should override an auto-injected dep unit and prune its subtree";
    }
    {
      # A toolchain id mismatch must be caught at eval, not at link time.
      assertion = !cargoUnitPrebuiltToolchainMismatchEval.success;
      message = "mkPrebuiltLibraryUnit should reject a toolchain id mismatch during eval";
    }
    {
      # C1: a mis-keyed injection (key absent from the generated graph) must fail
      # loud during eval rather than silently building from source.
      assertion = !cargoUnitPrebuiltMiskeyEval.success;
      message = "buildWorkspace should reject an extraUnits key absent from the generated graph";
    }
    {
      assertion = !cargoUnitPrebuiltBadDepEval.success;
      message = "mkPrebuiltLibraryUnit should reject depUnits entries without passthru.unitKey";
    }
    {
      # C4: two recorded prebuilts for one dep key with no explicit pin.
      assertion = !cargoUnitPrebuiltDepConflictEval.success;
      message = "buildWorkspace should reject conflicting recorded derivations for one dep unit key";
    }
    {
      # C3: the injection key must agree with the unit's own recorded unitKey.
      assertion = !cargoUnitPrebuiltKeyMismatchEval.success;
      message = "buildWorkspace should reject an extraUnits key that disagrees with the unit's recorded unitKey";
    }
  ];

  cargoUnitPrebuiltScript = ''
    # The injected unit's $out matches the unit contract: extern-path holds the
    # absolute path to the rlib (render.rs:1386-1398).
    test -f ${cargoUnitPrebuiltLibUnit}/lib/libprebuilt_lib-${cargoUnitPrebuiltVariantLib.hash}.rlib
    test -f ${cargoUnitPrebuiltLibUnit}/lib/libprebuilt_lib-${cargoUnitPrebuiltVariantLib.hash}.rmeta
    test -f ${cargoUnitPrebuiltLibUnit}/nix-support/extern-path
    grep -q '\.rlib$' ${cargoUnitPrebuiltLibUnit}/nix-support/extern-path

    # Provenance: the mid prebuilt records its leaf dep's store path.
    grep -qx '${cargoUnitPrebuiltLibUnit}' ${cargoUnitPrebuiltMidUnit}/nix-support/dependency-units

    # M1 (definitive source-less proof): the consumer's OWN source returns 42,
    # but it links the injected prebuilt rlib built from the variant (99). The
    # binary printing 99, not 42, can ONLY mean it linked the prebuilt artifact
    # and not its own from-source lib. A same-source rlib would be byte-identical
    # and could not distinguish the two; the distinct value makes the proof real.
    ${cargoUnitPrebuiltConsumer}/bin/prebuilt-consumer > cargo-unit-prebuilt.out
    cat cargo-unit-prebuilt.out
    grep -q 'prebuilt-lib:99 (answer=99)' cargo-unit-prebuilt.out
    if grep -q 'answer=42' cargo-unit-prebuilt.out; then
      echo "error: consumer used its own from-source lib (42), not the injected prebuilt (99)" >&2
      exit 1
    fi

    # ENG-2166 chained proof: the chain consumer's own sources answer 43
    # (42 + 1); the injected variant mid answers 100 (99 + 1) and its rlib
    # references the variant leaf's SVH, which only the auto-injected leaf
    # prebuilt satisfies. Linking at all, and printing 100, therefore proves
    # the recorded depUnits were injected without being passed to extraUnits.
    ${cargoUnitPrebuiltChainConsumer}/bin/prebuilt-chain-consumer > cargo-unit-prebuilt-chain.out
    cat cargo-unit-prebuilt-chain.out
    grep -q 'prebuilt-mid:100 (answer=100)' cargo-unit-prebuilt-chain.out
    if grep -q 'answer=43' cargo-unit-prebuilt-chain.out; then
      echo "error: chain consumer used from-source libs (43), not the injected prebuilts (100)" >&2
      exit 1
    fi
  '';

  # --- Test derivation builder ----------------------------------------------

  mkTest = name: assertions: extraScript: let
    failures = map (a: a.message) (lib.filter (a: !a.assertion) assertions);
  in
    assert lib.assertMsg (failures == []) (
      "ix-test-${name}:\n  " + lib.concatStringsSep "\n  " failures
    );
      pkgs.runCommand "ix-test-${name}" {nativeBuildInputs = [pkgs.gnugrep];} ''
        ${extraScript}
        mkdir -p "$out"
      '';

  groupTests = lib.mapAttrs (name: assertions: mkTest name assertions (buildScripts.${name} or "")) (
    removeAttrs groups ["fleet"]
  );

  fleetTest = mkTest "fleet" groups.fleet "";

  helperTest = pkgs.runCommand "ix-test-helpers" {nativeBuildInputs = [pkgs.gnugrep];} ''
    ${helperScript}
    mkdir -p "$out"
  '';

  cargoUnitRealWorkspacesTest =
    mkTest "cargo-unit-real-workspaces" cargoUnitRealWorkspaceAssertions
    cargoUnitRealWorkspaceScript;

  cargoUnitPrebuiltTest =
    mkTest "cargo-unit-prebuilt-library" cargoUnitPrebuiltAssertions
    cargoUnitPrebuiltScript;

  imageRegistryPinTest = mkTest "image-registry-pin" imageRegistryPinAssertions "";

  # Guard for the dev-profile fortify seam in lib/rust/cargo-unit.nix. Two
  # halves: `premiseStillHolds` compiles C and so is its own check;
  # `wiringReachesUnits` is eval-only and joins the aggregate below.
  devProfileFortifyTest = import ./dev-profile-fortify.nix {
    inherit lib pkgs ix;
  };

  # cargoUnit's dylib crate-type support (ENG-12078). Its own check rather than
  # part of the eval aggregate: it compiles a workspace, links it dynamically
  # and runs the result.
  cargoUnitDylibTest = import ./cargo-unit-dylib.nix {
    inherit lib pkgs ix;
  };
in {
  inherit
    groupTests
    groups
    cargoUnitRealWorkspaceAssertions
    cargoUnitPrebuiltAssertions
    ;
  cargoUnitRealWorkspaces = cargoUnitRealWorkspacesTest;
  cargoUnitPrebuiltLibrary = cargoUnitPrebuiltTest;
  # Validate the current R2 publication and local prebuilt-unit wrapper.
  sdkRustPrebuilt = sdkRust.artifactCheck;
  portableServices = portableServicesTest;
  provenance = provenanceTest;
  nixBuilder = nixBuilderTest;
  minecraftBlocksVm = minecraftBlocksVmTest;
  minestomSpleefVm = minestomSpleefVmTest;
  switchStopsAMountVm = switchStopsAMountVmTest;
  inherit baseImageNixDb;
  imageRegistryPin = imageRegistryPinTest;
  dev-profile-fortify = devProfileFortifyTest.premiseStillHolds;
  cargo-unit-dylib = cargoUnitDylibTest;

  # Aggregate. Pulls every group test into one derivation so
  # `nix flake check` covers the whole suite.
  eval = pkgs.linkFarmFromDrvs "ix-eval-tests" (
    lib.attrValues groupTests
    ++ [
      fleetTest
      helperTest
      portableServicesTest
      provenanceTest
      nixBuilderTest
      cargoUnitPrebuiltTest
      devProfileFortifyTest.wiringReachesUnits
    ]
  );
}
