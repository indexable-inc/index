{
  config,
  ix,
  lib,
  pkgs,
  ...
}: let
  inherit
    (lib)
    mkEnableOption
    mkIf
    mkOption
    types
    ;

  cfg = config.services.resource-monitor;
  fs = lib.fileset;
  runtimeDirectory = lib.removePrefix "/run/" cfg.runtimeDirectory;
  runtimeDirectoryIsSafe =
    lib.hasPrefix "/run/" cfg.runtimeDirectory && ix.relativePath.isSafe runtimeDirectory;

  metricOptions = {
    server = {
      vcpu = {
        type = types.ints.positive;
        default = 64;
        description = "Advertised total vCPU capacity shown by the monitor.";
      };
      memoryGiB = {
        type = types.ints.positive;
        default = 256;
        description = "Advertised total memory capacity in GiB.";
      };
      storageTiB = {
        type = types.ints.positive;
        default = 1024;
        description = "Advertised total storage capacity in TiB.";
      };
    };

    billing = {
      cpuUsdPerVcpuMonth = {
        type = types.number;
        default = 20;
        description = "CPU billing rate in USD per vCPU-month.";
      };
      memoryUsdPerGibHour = {
        type = types.number;
        default = 0.005;
        description = "Memory billing rate in USD per GiB-hour.";
      };
      storageUsdPerTibHour = {
        type = types.number;
        default = 0.0031;
        description = "Storage billing rate in USD per TiB-hour.";
      };
      marginMultiplier = {
        type = types.number;
        default = 2;
        description = "Multiplier applied to memory and storage billing rates.";
      };
    };
  };

  metricOptionAttrs = lib.mapAttrs (_: mkOption) (lib.mergeAttrsList (lib.attrValues metricOptions));

  metricConfig = lib.mapAttrs (_: group: builtins.intersectAttrs group cfg) metricOptions;
  metricValues = lib.mergeAttrsList (lib.attrValues metricConfig);

  siteSrc = fs.toSource {
    root = ./site;
    fileset = fs.unions [
      ./site/index.html
      ./site/package.json
      ./site/package-lock.json
      ./site/eslint.config.js
      ./site/tsconfig.json
      ./site/src
      ./site/vite.config.js
    ];
  };

  site = ix.buildSvelteSite pkgs {
    src = siteSrc;
    preBuild = "cp ${
      (pkgs.formats.json {}).generate "resource-monitor-vm-config.json" metricConfig
    } src/lib/vm-config.json";
    serve = {
      enable = false;
    };
    devServer = {
      enable = false;
    };
  };

  statsWriter = ix.cargoUnit.selectBinaryWithTests ix.rustWorkspace.units {
    binary = "resource-monitor-stats-writer";
    meta.mainProgram = "resource-monitor-stats-writer";
  };

  statsWriterSettings = {
    output-dir = cfg.runtimeDirectory;
    interval-seconds = cfg.intervalSeconds;
    df = lib.getExe' pkgs.coreutils "df";
    total-cores = metricValues.vcpu;
    total-memory-gib = metricValues.memoryGiB;
    total-storage-tib = metricValues.storageTiB;
    cpu-usd-per-vcpu-month = metricValues.cpuUsdPerVcpuMonth;
    memory-usd-per-gib-hour = metricValues.memoryUsdPerGibHour;
    storage-usd-per-tib-hour = metricValues.storageUsdPerTibHour;
    margin-multiplier = metricValues.marginMultiplier;
  };

  statsWriterArgs = lib.concatLists (
    lib.mapAttrsToList (name: value: [
      "--${name}"
      (toString value)
    ])
    statsWriterSettings
  );
in {
  options.services.resource-monitor =
    metricOptionAttrs
    // {
      enable = mkEnableOption "browser-accessible VM resource monitor";

      port = mkOption {
        type = types.port;
        default = 80;
        description = "TCP port served by nginx.";
      };

      openFirewall = mkOption {
        type = types.bool;
        default = true;
        description = "Whether to open the nginx port in the in-guest firewall.";
      };

      intervalSeconds = mkOption {
        type = types.ints.positive;
        default = 1;
        description = "Seconds between stats samples.";
      };

      runtimeDirectory = mkOption {
        type = types.str;
        default = "/run/resource-monitor";
        description = "Directory where the generated stats JSON is written.";
      };
    };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = runtimeDirectoryIsSafe;
        message = "services.resource-monitor.runtimeDirectory must be a managed /run subdirectory with safe relative segments";
      }
    ];

    ix.networking.portClaims.resource-monitor = {
      protocol = "tcp";
      inherit (cfg) port;
      address = "0.0.0.0";
      description = "resource monitor nginx";
    };

    systemd.services.resource-monitor = {
      description = "VM resource monitor stats";
      wantedBy = ["multi-user.target"];
      serviceConfig =
        ix.systemdHardening
        // {
          Type = "simple";
          DynamicUser = true;
          RuntimeDirectory = runtimeDirectory;
          RuntimeDirectoryMode = "0755";
          ExecStart = lib.escapeShellArgs ([(lib.getExe statsWriter)] ++ statsWriterArgs);
          Restart = "on-failure";
        };
    };

    services.nginx = {
      enable = true;
      virtualHosts.resource-monitor = {
        default = true;
        listen = [
          {
            addr = "0.0.0.0";
            inherit (cfg) port;
          }
        ];
        root = "${site}/share/resource-monitor-site";
        locations."/stats.json".root = cfg.runtimeDirectory;
        locations."/".tryFiles = "$uri $uri/ /index.html";
      };
    };

    networking.firewall.allowedTCPPorts = lib.optional cfg.openFirewall cfg.port;
  };
}
