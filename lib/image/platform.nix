# Baseline platform applied to every image.
{
  config,
  lib,
  pkgs,
  ...
}: let
  # `ix.healthChecks.<name>.unit` sugar: probe a systemd unit with
  # `systemctl is-active`. A bare name gets the `.service` suffix; pass an
  # explicit `foo.socket`/`foo.timer` to probe another unit type.
  unitName = unit:
    if lib.hasInfix "." unit
    then unit
    else "${unit}.service";
  mkUnitCommand = unit: [
    (lib.getExe' config.systemd.package "systemctl")
    "is-active"
    "--quiet"
    (unitName unit)
  ];

  # `ix.healthChecks.<name>.http` sugar: an in-guest HTTP readiness probe.
  # `--fail` maps curl's exit code onto the check result the same way a
  # Kubernetes httpGet probe treats a >= 400 status as unhealthy.
  mkHttpCommand = http: [
    (lib.getExe pkgs.curl)
    "--fail"
    "--silent"
    "--show-error"
    "http://${http.host}:${toString http.port}${http.path}"
  ];

  # `ix.healthChecks.<name>.tcp` sugar: an in-guest TCP connect probe,
  # the analog of a Kubernetes tcpSocket probe.
  mkTcpCommand = tcp: [
    (lib.getExe' pkgs.netcat-openbsd "nc")
    "-z"
    tcp.host
    (toString tcp.port)
  ];

  httpProbeType = lib.types.submodule {
    options = {
      port = lib.mkOption {
        type = lib.types.port;
        description = "Port the HTTP listener answers on.";
      };

      path = lib.mkOption {
        type = lib.types.str;
        default = "/";
        example = "/healthz";
        description = "Request path; give the service a cheap dedicated readiness route where you can.";
      };

      host = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1";
        description = "Host to connect to from inside the guest.";
      };
    };
  };

  tcpProbeType = lib.types.submodule {
    options = {
      port = lib.mkOption {
        type = lib.types.port;
        description = "Port to open a TCP connection to.";
      };

      host = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1";
        description = "Host to connect to from inside the guest.";
      };
    };
  };

  healthCheckType = lib.types.submodule (
    {
      name,
      config,
      ...
    }: {
      options = {
        description = lib.mkOption {
          type = lib.types.str;
          default = name;
          description = "Human-readable check name shown by fleet health commands.";
        };

        unit = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          example = "nginx";
          description = ''
            A systemd unit to probe with `systemctl is-active --quiet`.

            Sugar for the overwhelmingly common "is this unit running?" check:
            setting `unit` derives `command` for you, so
            `ix.healthChecks.nginx.unit = "nginx";` replaces the full
            `systemctl is-active` argv. A bare name gets the `.service` suffix;
            pass `foo.socket` or `foo.timer` to probe another unit type.

            Mutually exclusive with `command`: set one or the other, not both.
          '';
        };

        http = lib.mkOption {
          type = lib.types.nullOr httpProbeType;
          default = null;
          example = lib.literalExpression ''{ port = 8080; path = "/healthz"; }'';
          description = ''
            An in-guest HTTP readiness probe, the analog of a Kubernetes
            `httpGet` probe: setting `http` derives a curl `command` that
            succeeds on 2xx/3xx and fails on any status >= 400 (curl
            `--fail`). The probe binary is pinned into the image closure
            automatically, so no `environment.systemPackages` bookkeeping
            is needed.

            Sugar for `command`; set at most one of `unit`, `http`, `tcp`,
            or an explicit `command`.
          '';
        };

        tcp = lib.mkOption {
          type = lib.types.nullOr tcpProbeType;
          default = null;
          example = lib.literalExpression ''{ port = 5432; }'';
          description = ''
            An in-guest TCP connect probe, the analog of a Kubernetes
            `tcpSocket` probe: setting `tcp` derives an `nc -z` `command`
            that succeeds once the port accepts a connection. The probe
            binary is pinned into the image closure automatically.

            Sugar for `command`; set at most one of `unit`, `http`, `tcp`,
            or an explicit `command`.
          '';
        };

        from = lib.mkOption {
          type = lib.types.enum [
            "guest"
            "host"
          ];
          default = "guest";
          description = ''
            Where the command runs.

            `guest` execs through `ix shell <node> -- <command>` inside the VM.
            `host` execs `<command>` directly on the operator's machine and
            exports `IX_NODE` plus any fields returned by `ix ls` as
            `IX_NODE_<KEY>` env vars, so the command can probe the node from
            outside the VM (firewall, public IPv4, gateway path).
          '';
        };

        command = lib.mkOption {
          type = lib.types.nonEmptyListOf lib.types.str;
          description = ''
            Command argv. For `from = "guest"` it runs in the VM through
            `ix shell`. For `from = "host"` it runs directly with the
            `IX_NODE*` env vars described above; tools must be on the
            operator's PATH.

            When `unit` is set this defaults to a `systemctl is-active` probe of
            that unit, so most checks only set `unit`. Set `command` for a real
            readiness probe (an HTTP request, a query) rather than a bare unit
            liveness check; set one of `unit` or `command`.
          '';
        };

        timeoutSec = lib.mkOption {
          type = lib.types.ints.positive;
          default = 30;
          description = "Per-attempt timeout in seconds.";
        };

        attempts = lib.mkOption {
          type = lib.types.ints.positive;
          default = 30;
          description = "Maximum number of attempts before the check fails.";
        };

        intervalSec = lib.mkOption {
          type = lib.types.ints.unsigned;
          default = 2;
          description = "Seconds to wait between failed attempts.";
        };

        requiresIpv4 = lib.mkOption {
          type = lib.types.bool;
          default = false;
          description = ''
            Whether this check needs `IX_NODE_IPV4` from `ix ls`.

            Use this for host-side public reachability probes that connect to
            the node's assigned IPv4 address. Fleet evaluation rejects nodes
            with this requirement unless `deployment.ipv4 = true`.
          '';
        };
      };

      # Probe sugar lives here, not in `command`'s default: a public option's
      # default must be a self-contained literal (repo astlog rule), so the
      # sugar -> command branch is seeded in config as an mkDefault a real
      # `command` (priority 100) still overrides. One definition site (first
      # set sugar wins) rather than one mkIf per sugar, so setting two sugars
      # reaches the readable platform-level assertion instead of a module
      # "conflicting definition values" error.
      config = lib.mkIf (config.unit != null || config.http != null || config.tcp != null) {
        command = lib.mkDefault (
          if config.unit != null
          then mkUnitCommand config.unit
          else if config.http != null
          then mkHttpCommand config.http
          else mkTcpCommand config.tcp
        );
      };
    }
  );

  portClaimType = lib.types.submodule (
    {name, ...}: {
      options = {
        protocol = lib.mkOption {
          type = lib.types.enum [
            "tcp"
            "udp"
          ];
          description = "Transport protocol claimed by this listener.";
        };

        port = lib.mkOption {
          type = lib.types.port;
          description = "Port claimed by this listener.";
        };

        address = lib.mkOption {
          type = lib.types.str;
          default = "*";
          description = "Bind address. Use * when the service binds every address or the bind behavior is implicit.";
        };

        namespace = lib.mkOption {
          type = lib.types.str;
          default = "default";
          description = "Network namespace for this listener. Ordinary image services use the default namespace.";
        };

        description = lib.mkOption {
          type = lib.types.str;
          default = name;
          description = "Human-readable listener owner used in collision errors.";
        };
      };
    }
  );

  portClaims =
    lib.mapAttrsToList (
      name: claim: claim // {inherit name;}
    )
    config.ix.networking.portClaims;
  claimKey = claim: "${claim.namespace}/${claim.protocol}/${toString claim.port}";
  portClaimGroups = builtins.groupBy claimKey portClaims;
  isIpv4Address = address: lib.hasInfix "." address;
  isIpv6Address = address: lib.hasInfix ":" address;
  addressOverlaps = left: right:
    left
    == "*"
    || right == "*"
    || left == right
    || (left == "0.0.0.0" && !(isIpv6Address right))
    || (right == "0.0.0.0" && !(isIpv6Address left))
    || (left == "::" && !(isIpv4Address right))
    || (right == "::" && !(isIpv4Address left));
  groupConflicts = claims:
    lib.any (
      left: lib.any (right: left.name != right.name && addressOverlaps left.address right.address) claims
    )
    claims;
  conflictingPortClaimGroups = lib.filterAttrs (_: groupConflicts) portClaimGroups;
  renderPortClaim = claim: "${claim.name} (${claim.address}, ${claim.description})";
  renderPortClaimConflict = key: claims: "${key}: ${lib.concatMapStringsSep ", " renderPortClaim claims}";
  ipv4GuestHealthChecks =
    lib.filterAttrs (
      _name: check: check.requiresIpv4 && check.from != "host"
    )
    config.ix.healthChecks;

  # The declarative probe sugars (`unit`, `http`, `tcp`) each derive `command`,
  # so they are mutually exclusive with each other and with an explicit
  # `command`: a check that sets two sources of truth would silently ignore
  # one. `probeSugars` mirrors the submodule's derivation order.
  probeSugars = check: lib.filter (sugar: check.${sugar} != null) ["unit" "http" "tcp"];
  sugarCommand = check:
    if check.unit != null
    then mkUnitCommand check.unit
    else if check.http != null
    then mkHttpCommand check.http
    else mkTcpCommand check.tcp;

  multiSugarHealthChecks =
    lib.filterAttrs (
      _name: check: builtins.length (probeSugars check) > 1
    )
    config.ix.healthChecks;

  # Health checks that set a probe sugar must not also override `command`: the
  # whole point of the sugar is that it derives the command, so a custom
  # command means the sugar is silently ignored. Flag it instead of letting
  # them disagree.
  overSpecifiedHealthChecks =
    lib.filterAttrs (
      _name: check: probeSugars check != [] && check.command != sugarCommand check
    )
    config.ix.healthChecks;

  # `http`/`tcp` probes exec pinned in-guest store paths (curl, nc), which do
  # not exist on the operator's machine; a host-side reachability probe needs
  # an explicit `command` with tools from the operator's PATH.
  hostProbeSugarHealthChecks =
    lib.filterAttrs (
      _name: check: check.from == "host" && (check.http != null || check.tcp != null)
    )
    config.ix.healthChecks;

  # Probe binaries ride the system closure: the plan strips string context
  # from check argv (fleet.nix `planHealthChecks`), so nothing else retains
  # curl/nc for a check that is the only reference to them.
  healthCheckValues = lib.attrValues config.ix.healthChecks;
  healthProbePackages =
    lib.optional (lib.any (check: check.http != null) healthCheckValues) pkgs.curl
    ++ lib.optional (lib.any (check: check.tcp != null) healthCheckValues) pkgs.netcat-openbsd;

  # `ix.networking.expose.<name>` is the one declaration for "this image listens
  # here": it registers the port in the claim registry (so collisions are caught
  # at eval time) and, by default, opens the in-guest firewall for it. It also
  # makes the listener discoverable across the fleet via `ix.endpointOf`.
  exposeType = lib.types.submodule (
    {name, ...}: {
      options = {
        port = lib.mkOption {
          type = lib.types.port;
          description = "Port this image listens on.";
        };

        protocol = lib.mkOption {
          type = lib.types.enum [
            "tcp"
            "udp"
          ];
          default = "tcp";
          description = "Transport protocol of this listener.";
        };

        address = lib.mkOption {
          type = lib.types.str;
          default = "*";
          description = "Bind address. Use * when the service binds every address or the bind behavior is implicit.";
        };

        namespace = lib.mkOption {
          type = lib.types.str;
          default = "default";
          description = "Network namespace for this listener. Ordinary image services use the default namespace.";
        };

        description = lib.mkOption {
          type = lib.types.str;
          default = name;
          description = "Human-readable listener owner, used in collision errors and health output.";
        };

        firewall = lib.mkOption {
          type = lib.types.bool;
          default = true;
          description = ''
            Open the in-guest firewall for this port. Leave it on for the normal
            case (this image owns the listener). Set it false when another
            mechanism already opens the port (a service's own
            `openFirewall = true`) and you only want the registry entry and
            cross-node discovery.
          '';
        };
      };
    }
  );

  exposeList = lib.attrValues config.ix.networking.expose;
  exposePortClaims =
    lib.mapAttrs (_name: e: {
      inherit
        (e)
        protocol
        port
        address
        namespace
        description
        ;
    })
    config.ix.networking.expose;
  exposeFirewallPorts = proto: map (e: e.port) (lib.filter (e: e.firewall && e.protocol == proto) exposeList);

  # --- account-store secret attachments --------------------------------------
  #
  # Which stored secrets a VM is created with, declared in the image so the
  # answer travels with the definition rather than with whoever typed the
  # command. `deployment.secrets` in a fleet spec lands here (see
  # `identityModule` in fleet.nix), and both readers -- the fleet plan
  # ix-fleet consumes and the `fleet.resolve` evaluator `ix apply` reads --
  # take `ix.secretAttachments`, so there is one normalization rather than two
  # that drift.

  # The account-store key. Lower snake_case is ix's own constraint on a secret
  # name, checked here so a typo fails the eval instead of the create RPC.
  isSecretName = name: builtins.match "[a-z][a-z0-9_]*" name != null;

  secretType = lib.types.submodule {
    options = {
      env = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "GH_TOKEN";
        description = ''
          Environment variable name the stored value is injected as. Mutually
          exclusive with `file`.
        '';
      };

      file = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "github/token";
        description = ''
          Guest-relative path the stored value is written to, under
          `/run/secrets`. Mutually exclusive with `env`.
        '';
      };

      owner = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "nginx";
        description = ''
          Guest unix user that owns the delivered file. File targets only;
          `null` keeps the root-owned default.
        '';
      };

      mode = lib.mkOption {
        # A quoted octal string, never a Nix integer. `mode = 0400` is the
        # decimal 400 to Nix, which is 0620 as permission bits -- nothing
        # anyone means, and unrecoverable once it is a number. ix-fleet's plan
        # model still accepts `str | int` for the same key; this refuses the
        # int half at eval rather than carrying the ambiguity into the create
        # RPC, which parses the string as octal exactly like `--secret-file`.
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "0400";
        description = ''
          Permission bits for the delivered file, as a quoted octal string
          between `"0001"` and `"0777"`. File targets only; `null` keeps the
          0600 default.
        '';
      };
    };
  };

  # One `ix.secrets` entry as the create RPC's `SecretAttachment`. `owner` and
  # `mode` are omitted rather than sent as null, because the server tells
  # "absent, keep the default" from "explicitly set" by the key's presence.
  secretAttachment = sourceName: secret:
    assert lib.assertMsg (isSecretName sourceName)
    "secret key '${sourceName}' must be lower snake_case: [a-z][a-z0-9_]*";
    assert lib.assertMsg (!(secret.env != null && secret.file != null))
    "secret '${sourceName}' cannot set both env and file";
    assert lib.assertMsg (secret.env != null || secret.file != null)
    "secret '${sourceName}' must set either env or file";
      if secret.env != null
      then {
        name = sourceName;
        target = {
          name = secret.env;
          injectAs = "env";
        };
      }
      else {
        name = sourceName;
        target =
          {
            name = secret.file;
            injectAs = "file";
          }
          // lib.optionalAttrs (secret.owner != null) {inherit (secret) owner;}
          // lib.optionalAttrs (secret.mode != null) {inherit (secret) mode;};
      };

  # Every tmpfs this image sizes against RAM, as `{mountpoint, size}` with the
  # declared spec verbatim ("50%", "2G").
  #
  # A tmpfs `size=N%` is resolved exactly once, when the filesystem is mounted,
  # against `totalram_pages()`, and stored as a fixed block count (mm/shmem.c,
  # `shmem_parse_one`). The fraction does not survive: /proc/self/mountinfo
  # carries the donor's already-resolved byte count and nothing that says which
  # fraction produced it. A golden restore never re-mounts, so a clone keeps
  # the caps the DONOR's RAM produced. Publishing the declaration is what lets
  # ix-vm-guest recompute them for the machine it was actually restored onto
  # (ENG-12403; the reader is `handler/configure/tmpfs_sizing.rs`).
  #
  # Read from the evaluated config rather than hand-listed. `boot.runSize`,
  # `boot.devShmSize` and `boot.tmp.tmpfsSize` are ordinary options an image
  # may override, so a fixed list here would eventually hand the guest a
  # fraction its mounts were never built from -- the disagreement this is
  # meant to make impossible.
  declaredTmpfsSize = options: let
    sized = builtins.filter (option: lib.hasPrefix "size=" option) options;
  in
    if sized == []
    then null
    else lib.removePrefix "size=" (lib.last sized);

  ramSizedMount = mountPoint: fsType: options: let
    size = declaredTmpfsSize options;
  in
    lib.optional (fsType == "tmpfs" && size != null) {
      mountpoint = mountPoint;
      inherit size;
    };

  ramSizedMountsIn = fileSystemAttrs:
    lib.concatLists (
      lib.mapAttrsToList (
        mountPoint: fs: ramSizedMount mountPoint fs.fsType fs.options
      )
      fileSystemAttrs
    );

  # `boot.specialFileSystems` is where /run and /dev/shm get their fractions,
  # `fileSystems` is any tmpfs the image declares itself, and `systemd.mounts`
  # is where `boot.tmp.useTmpfs = true` lands /tmp.
  ramSizedTmpfsMounts =
    ramSizedMountsIn config.boot.specialFileSystems
    ++ ramSizedMountsIn config.fileSystems
    ++ lib.concatLists (
      map (
        mount:
          ramSizedMount mount.where mount.type (
            lib.splitString "," (mount.options or "")
            ++ lib.splitString "," (mount.mountConfig.Options or "")
          )
      )
      config.systemd.mounts
    );
in {
  options.ix = {
    cpus = lib.mkOption {
      type = lib.types.nullOr (lib.types.enum [2 4 8 16 32 64]);
      default = null;
      example = 8;
      description = ''
        How many vCPUs this image's VM boots with, or `null` for the platform
        default.

        The default is the full autoscale ceiling, which is what every machine
        gets and what almost every machine should keep: a VM boots with the
        whole shape and costs nothing extra for it, because machines are billed
        on what they consume rather than on the size they were created at.
        Setting this is an opt-DOWN, and it is worth doing when a workload is
        known to be small and you would rather fit more machines per node --
        placement admits against this number, not against the ceiling.

        The guest genuinely sees this many vCPUs; it is the boot topology, not
        a scheduler quota on a larger machine. Hotplug headroom is unaffected:
        the machine can still grow back to the platform ceiling without being
        recreated.

        The allowed values are a menu rather than a range, and the reason is
        the warm-boot cache. A machine's restore seed is staged per hardware
        class, whose first component is the boot vCPU count, and each new class
        pays a cold first boot before it is warm. Six sizes stay cheap to
        pre-warm; an open range would let one configuration mint an unbounded
        set of shapes that the whole fleet then carries.

        Read at CREATE time only. Like `ix.networking.ipv4`, the count is
        stamped on the machine when it is created, so changing it for a VM that
        already exists is not converged by `ix apply` -- `ix rm` and re-apply to
        resize. Declared here rather than passed as `ix new --cpus` so the shape
        travels with the definition instead of with whoever ran the command.
      '';
    };

    secrets = lib.mkOption {
      type = lib.types.attrsOf secretType;
      default = {};
      example = lib.literalExpression ''
        {
          github_token = {
            file = "github/token";
            owner = "root";
            mode = "0400";
          };
        }
      '';
      description = ''
        Account-store secrets delivered into this image's VM, keyed by the
        name they were stored under with `ix secret set`.

        Declared in the image so the VM's secret needs travel with its
        definition: a fleet's `deployment.secrets` merges into this option, and
        a module that needs a credential can ask for it directly instead of
        relying on whoever runs the deploy to pass `--secret-file`.

        Delivery happens once, when the VM is created. Adding an entry for a VM
        that already exists is refused by `ix apply` with the recreate spelled
        out rather than applied half way, because nothing copies a stored value
        into a live VM (ENG-12214).
      '';
    };

    secretAttachments = lib.mkOption {
      type = lib.types.listOf (lib.types.attrsOf lib.types.anything);
      internal = true;
      readOnly = true;
      description = ''
        [`ix.secrets`](#opt-ix.secrets) in the shape both consumers want: the
        create RPC's `SecretAttachment` list, ordered by source key.

        Read by the fleet plan (`ix-fleet`) and by the `fleet.resolve`
        evaluator (`ix apply`). Not an input; set `ix.secrets`.
      '';
    };

    healthChecks = lib.mkOption {
      type = lib.types.attrsOf healthCheckType;
      default = {};
      description = ''
        Commands that prove this image's important services are ready.

        Each check declares whether it runs from inside the VM (`from = "guest"`)
        or from the operator host (`from = "host"`); host checks are how you
        prove public reachability, firewall correctness, and external routing,
        not just that systemd thinks the unit is active. Fleet plans expose
        these so `ix-fleet health` and the post-deploy waits in `up`,
        `replace`, and `switch` can use them.
      '';
    };

    networking = {
      portClaims = lib.mkOption {
        type = lib.types.attrsOf portClaimType;
        default = {};
        description = ''
          Sockets claimed by repo-owned service modules inside this image.

          The registry catches same-namespace listener collisions at eval time.
          Use separate fleet nodes or an explicit alternate port when two services
          need the same public protocol port.
        '';
      };

      expose = lib.mkOption {
        type = lib.types.attrsOf exposeType;
        default = {};
        example = lib.literalExpression ''
          {
            http = {
              port = 8080;
              description = "public HTTP API";
            };
          }
        '';
        description = ''
          Listeners this image exposes, declared once. Each entry registers a
          port claim (so same-namespace collisions fail at eval time), opens the
          in-guest firewall for the port (unless `firewall = false`), and becomes
          discoverable from sibling nodes via `ix.endpointOf nodes.<node> "<name>"`.

          This is the one source of truth for a port: prefer it over hand-pairing
          `networking.firewall.allowed*Ports` with `ix.networking.portClaims`,
          which is the lower-level primitive `expose` desugars to.
        '';
      };

      # Networking policy (per-port filtering, L7, WAF, rate limiting, gateway
      # behavior) belongs to the image, not to ix. ix exposes two primitives:
      # east-west group membership (which VMs can reach each other) and
      # north-south on/off (whether the VM has internet ingress / egress).
      # Anything finer lives in `networking.firewall.*` inside the image, in a
      # sidecar, or behind a user-built gateway VM. `eastWest.hostName` stays
      # here because it is a name, not a policy.
      eastWest.hostName = lib.mkOption {
        type = lib.types.str;
        default = config.networking.hostName;
      };

      groups = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [];
        example = lib.literalExpression ''[ "shared-db" ]'';
        description = ''
          East-west group slugs this image's VM joins at creation.

          Declared in the image so the network identity travels with the
          definition: the fleet plan unions these with the fleet-level
          `nodes.<name>.groups`, and the create path get-or-creates each
          slug under the deploying user before first boot. VMs sharing a
          slug reach each other privately as `<eastWest.hostName>.ix.internal`;
          a VM outside the group has no route in.

          Slugs are scoped per owner and limited to `[a-z0-9_-]`, max 63
          chars (the DNS label limit); the fleet eval rejects anything else
          before any RPC runs.
        '';
      };

      ipv4 = lib.mkOption {
        type = lib.types.bool;
        default = false;
        example = true;
        description = ''
          Whether this image's VM is allocated a public IPv4 address at
          creation, so it can serve the internet on its own address rather
          than only through a share or the L7 proxy.

          This costs a real address out of the region's IPv4 ingress block,
          and only a region that has such a block configured can serve one
          (today that is `us-west-1`): creating there with this set fails the
          create RPC with "no IPv4 address available in the regional pool"
          rather than coming up quietly unreachable. Leave it off unless the
          VM is a public entrypoint.

          The allocation happens once, at create: `ix apply` cannot add an
          address to a VM that already exists (there is no `ix vm set --ipv4`),
          so turning this on for a live VM is refused with the recreate step
          spelled out. Turning it back off likewise never revokes an address
          the VM already holds, because the option's `false` default cannot be
          told apart from a config that never had an opinion; use `ix rm` and
          re-apply to drop one.
        '';
      };
    };
  };

  config = {
    ix.secretAttachments = lib.mapAttrsToList secretAttachment config.ix.secrets;

    ix.networking.portClaims =
      exposePortClaims
      // {
        ix-console = {
          protocol = "tcp";
          port = 5001;
          address = "*";
          description = "ix-console shell and terminal snapshot listener";
        };

        ix-agent = {
          protocol = "udp";
          port = 8443;
          address = "*";
          description = "ix-agent WebTransport direct-connect endpoint";
        };
      };

    assertions = [
      {
        assertion = conflictingPortClaimGroups == {};
        message = ''
          ix.networking.portClaims has same-namespace port collisions:
            ${lib.concatMapAttrsStringSep "\n  " renderPortClaimConflict conflictingPortClaimGroups}

          Put services that need the same public protocol port in separate fleet nodes/VMs, or choose an explicit alternate port when same-image co-location is intentional.
        '';
      }
      {
        assertion = ipv4GuestHealthChecks == {};
        message = ''
          ix.healthChecks can only set requiresIpv4 on host checks:
            ${lib.concatStringsSep ", " (lib.attrNames ipv4GuestHealthChecks)}
        '';
      }
      {
        assertion = multiSugarHealthChecks == {};
        message = ''
          ix.healthChecks set more than one of `unit`, `http`, and `tcp`, which
          conflict (each derives the check's command):
            ${
            lib.concatMapAttrsStringSep ", " (
              name: check: "${name} (${lib.concatStringsSep " + " (probeSugars check)})"
            )
            multiSugarHealthChecks
          }

          Pick the one probe that proves readiness, or write an explicit
          `command` when a single probe is not enough.
        '';
      }
      {
        assertion = overSpecifiedHealthChecks == {};
        message = ''
          ix.healthChecks set both a probe sugar (`unit`, `http`, or `tcp`) and
          a custom `command`, which conflict (a custom command makes the sugar
          a no-op):
            ${lib.concatStringsSep ", " (lib.attrNames overSpecifiedHealthChecks)}

          Set `unit` for a `systemctl is-active` probe, `http`/`tcp` for a
          readiness probe, or `command` for an explicit argv -- not both.
        '';
      }
      {
        assertion = hostProbeSugarHealthChecks == {};
        message = ''
          ix.healthChecks can only use `http`/`tcp` probes on guest checks
          (their probe binaries are pinned inside the image, not on the
          operator's machine):
            ${lib.concatStringsSep ", " (lib.attrNames hostProbeSugarHealthChecks)}

          Keep `from = "guest"`, or write an explicit host `command` using
          tools from the operator's PATH.
        '';
      }
    ];

    # Keep declared `http`/`tcp` probe binaries in the image closure (and on
    # PATH for interactive debugging): the fleet plan strips string context
    # from check argv, so nothing else would retain them.
    environment.systemPackages = healthProbePackages;

    # The host platform (x86_64-linux) and the YourKit-only unfree predicate
    # live on the shared `imagePkgs` instantiation in `default.nix`: every
    # image shares ONE nixpkgs instance via `nixpkgs.pkgs`, and the nixpkgs
    # module ignores `hostPlatform`/`config` once `pkgs` is set.

    boot = {
      isContainer = true;

      # `isContainer` makes nixpkgs' container-config.nix turn the modprobe
      # machinery off ("containers don't have a kernel"), but ix guests DO
      # have one: the host direct-kernel-boots linux-ix and injects that
      # kernel's full module tree at /lib/modules/<kver> in every rootfs.
      # Without this re-enable the guest has no module loader at all:
      # /proc/sys/kernel/modprobe keeps the kernel's compiled-in default
      # /sbin/modprobe (which does not exist here), so request_module()
      # fails and every =m kernel feature is dead -- nftables.service dies
      # with "Unable to initialize Netlink socket: Protocol not supported"
      # on every boot and every switch exits 4 (ix#8408). Re-enabling the
      # upstream module (nixos/modules/system/boot/modprobe.nix) puts
      # pkgs.kmod on PATH and points the sysctl at its modprobe from an
      # activation snippet, which runs at boot AND during a switch, so the
      # fix reaches an already-booted VM through a plain `ix apply`.
      #
      # Version skew is safe: nixpkgs kmod probes
      # /run/{booted,current}-system/kernel-modules first but only takes a
      # prefix whose <uname -r> subdir exists. ix toplevels never ship the
      # kernel-modules link (boot.kernel.enable = false under isContainer),
      # and a user toplevel carrying modules for some other kernel fails
      # the <uname -r> probe, so resolution always lands on the injected
      # /lib/modules tree of the RUNNING kernel.
      # astlog-ignore: no-mkforce container-config.nix sets this unconditionally for isContainer; ix#8408
      modprobeConfig.enable = lib.mkForce true;

      # /tmp lives on the rootfs, like a normal machine. It used to be a
      # tmpfs on the theory that an ix VM has RAM to spare and an on-disk
      # /tmp grows without bound. Both halves of that theory are wrong
      # here.
      #
      # `size=50%` is not half of the VM's memory. The kernel resolves the
      # percentage exactly once, when the filesystem is mounted, against
      # `totalram_pages()` (mm/shmem.c, the `*rest == '%'` branch of
      # `shmem_parse_one`), and stores the answer as a fixed block count.
      # Memory that shows up afterwards never raises it. An ix guest
      # mounts /tmp while only the unpluggable virtio-mem base is present
      # (`VIRTIO_MEM_BOOT_BASE_MIB`, 3 GiB), well before the host's
      # post-health-check resize, so the cap is taken against a ~3 GiB
      # total and then frozen for the life of the VM. Golden restore no
      # longer inherits that answer -- `ix/tmpfs-sizing.json` below hands
      # the declared fraction to ix-vm-guest, which re-resolves it against
      # the restored machine's own memory (ENG-12403) -- but the boot-time
      # freeze against the 3 GiB base is still what a first boot gets.
      #
      # Measured on two live hil guests (2026-07-29): /tmp mounted
      # `size=1491832k` -- an absolute 1.42 GiB, not a percentage -- while
      # the guest reported MemTotal 256 GiB. That is 0.55% of the VM's
      # RAM, and one of the two guests had that /tmp 100% full. /dev/shm
      # on the same guests resolved its own 50% against a slightly larger
      # total (1749884k), so the mount-time freeze is visible twice in one
      # mount table.
      #
      # A build whose scratch exceeds the cap dies ENOSPC on a machine
      # advertising far more memory (index#4332) -- the exact failure
      # nixpkgs warns about in the `boot.tmp.useTmpfs` description.
      # Raising `tmpfsSize` does not fix it: the cap is bounded by the
      # 3 GiB boot base either way, and spending a larger fraction of that
      # unpluggable floor only trades the ENOSPC for an OOM kill.
      #
      # An on-disk /tmp is not unbounded either. NixOS ships systemd's own
      # `q /tmp 1777 root root 10d` tmpfiles rule and enables
      # systemd-tmpfiles-clean.timer, so idle scratch ages out on its own.
      # What it does cost is disk: temp files now occupy the rootfs and
      # ride along in snapshots until they are deleted. An image that
      # genuinely wants RAM-backed scratch sets `boot.tmp.useTmpfs = true`
      # for itself; mkDefault here restates the nixpkgs default so a
      # future upstream flip cannot silently reintroduce the cap.
      tmp = {
        useTmpfs = lib.mkDefault false;
        # Do NOT set cleanOnBoot here. nixpkgs
        # nixos/modules/system/boot/tmp.nix turns it into an unconditional
        # `D! /tmp 1777 root root` tmpfiles rule, so
        # systemd-tmpfiles-setup.service deletes the CONTENTS of /tmp
        # partway through boot. ix guests accept exec/shell before systemd
        # activation settles (ENG-5440), so early workload writes to /tmp
        # were destroyed mid-run: SQLite "disk I/O error" + ENOENT on a
        # near-empty /tmp (ix#7905, ix#7908). Now that /tmp survives a
        # reboot the same rule would also destroy scratch written by an
        # earlier boot. The 10d age rule above reclaims the same space
        # without racing the workload.
      };
    };

    # The mount-time RAM fractions this image declared, published for the guest
    # daemon's re-personalization step: the kernel throws the fraction away at
    # mount time, so a restored clone cannot recompute its own caps without
    # being told what they were meant to be. Path is duplicated in
    # `MANIFEST_PATH` in crates/vm/guest/daemon/src/handler/configure/
    # tmpfs_sizing.rs; a rename on either side shows up as "image declares no
    # RAM-derived tmpfs caps" at debug in the guest journal.
    environment.etc."ix/tmpfs-sizing.json".source = (pkgs.formats.json {}).generate "ix-tmpfs-sizing.json" {
      mounts = ramSizedTmpfsMounts;
    };

    # Many ix VMs are SSH'd into and used as interactive dev machines, where
    # operators run unpatched prebuilt binaries (npm-installed CLIs, LSPs,
    # downloaded toolchains) that expect a standard FHS dynamic linker. Off
    # by default for the rare image that is genuinely a sealed appliance and
    # wants to drop the stub from its closure.
    programs.nix-ld.enable = lib.mkDefault true;

    networking = {
      # ix provisions the guest address, route, and DNS before systemd reaches
      # normal service startup. Leaving NixOS DHCP enabled makes dhcpcd wait
      # for a lease that will never arrive, which keeps network-online.target
      # pending and blocks services such as minecraft.
      useDHCP = false;

      # ix-vm-guest writes the runtime-provided nameservers to /etc/resolv.conf
      # before starting the image. NixOS 26.05 enables resolvconf by default;
      # with no NixOS-owned nameservers, stage 2 then replaces that file with an
      # empty one and breaks DNS in an otherwise connected VM.
      resolvconf.enable = lib.mkDefault false;

      # In-guest firewall is the NixOS nftables backend, enforcing each
      # module's `services.*.openFirewall` and `networking.firewall.allowed*`
      # declarations. ix VMs are `boot.isContainer = true` and share the
      # host's linux-ix kernel (CONFIG_NF_TABLES); nft rules run in this
      # container's own net namespace.
      #
      # This is the primary mechanism for port-level policy. ix provides only
      # the coarse primitives (east-west group membership, north-south
      # on/off); per-port allowlists, L7, WAF, rate limiting, etc. live here
      # in the image or in a user-built gateway VM. The "primitives only"
      # rule is recorded in `ix/AGENTS.md` under "Architecture that must not
      # drift". Tracking the ix-side north-south primitive in
      # https://github.com/indexable-inc/index/issues/41.
      nftables.enable = true;
      firewall = {
        enable = lib.mkDefault true;
        allowedTCPPorts =
          [
            5001 # ix-console shell and terminal snapshot listener.
          ]
          ++ exposeFirewallPorts "tcp";
        allowedUDPPorts =
          [
            8443 # ix-agent WebTransport direct-connect endpoint.
          ]
          ++ exposeFirewallPorts "udp";
      };
    };

    services = {
      # Bound the journal so a long-running VM that catches one tcpdump-style
      # spam burst does not fill its disk with rotated journal files.
      # Override per image when an operator actually needs the historical
      # depth.
      journald.extraConfig = lib.mkDefault ''
        SystemMaxUse=1G
      '';

      # Varlink multiplexer for JSON user/group records. On-demand activated,
      # so the cost when nothing queries it is negligible. Having it on means
      # operator-side `userdbctl user/group` works out of the box, services
      # that adopt `DynamicUser=true` get proper cgroup accounting through a
      # synthesized record, and the eventual NFTSet=/cgroup-id integration
      # over the nftables backend already enabled here can hook into real
      # user/group identities without another platform-wide flip.
      userdbd = {
        enable = true;
        # macOS Nix installs (and other multi-user setups) ship `nixbld*`
        # build users above 1000, which trips the systemd-userdb "regular
        # users have system-range UIDs" warning at eval time. The build
        # users do not exist inside ix VMs at all — `boot.isContainer =
        # true` means the guest has its own user namespace — so the
        # warning is noise from the host nixpkgs evaluator rather than a
        # runtime concern. Silencing here keeps `nix run .#health-checks`
        # and other repo evals scannable.
        silenceHighSystemUsers = true;
      };
    };

    # No serial login prompt. systemd-getty-generator instantiates a serial
    # getty for every console in /sys/class/tty/console/active plus the first
    # virtio console it finds, so an ix guest gets serial-getty@ttyS0 (the
    # host's DEFAULT_CMDLINE sets `console=ttyS0`,
    # crates/vm/host/vmm/kvm/src/cmdline.rs) and serial-getty@hvc0 (the
    # generator probes /sys/class/tty/hvc0, which the virtio-console driver
    # registers whether or not a host end is attached). Neither device can
    # carry a login session:
    #
    #   * hvc0 is output only. The host drains the transmitq into a per-VM log
    #     file and deliberately never drains the receiveq, so no keystroke can
    #     reach a getty there (crates/vm/host/vmm/device/src/virtio/console.rs).
    #   * ttyS0 carries kernel printk, is captured to a log file and ingested
    #     into ClickHouse `vm_serial_logs`, and `ix serial` claims it as a
    #     single-holder attach. A getty would only write its login banner into
    #     that log.
    #
    # Interactive access is ix-console over vsock (port 5001, claimed above),
    # which allocates its own PTY and wants no getty at all.
    #
    # Left in place they do not idle, they fail every switch: serial-getty@
    # carries `BindsTo=dev-%i.device`, and no .device unit EVER activates in an
    # ix guest, because `boot.isContainer = true` drops both
    # systemd-udev-trigger.service and every udev rule, so nothing is ever
    # tagged `systemd` (ENG-11064 has the measurements; dev-vda.device, the
    # root disk, reads `tentative` for the same reason). Each job waits out its
    # 90s JobTimeoutSec and fails, the getty is dependency-failed, and
    # switch-to-configuration exits 4, so EVERY `ix apply` of a guest reported
    # failure while having actually switched cleanly (ENG-11063). Same shape as
    # the modprobe/nftables case in ix#8408.
    #
    # `enable = false` renders each instance as a /dev/null symlink in
    # /etc/systemd/system, which outranks the generator's /run/systemd/generator
    # copy in systemd's unit load path, so the generator stays free to keep
    # inventing them. Masking the `serial-getty@.service` template instead
    # would not work: the generator symlinks straight at the unit file in the
    # systemd store path, so the instances would still load.
    systemd.services = {
      "serial-getty@ttyS0".enable = false;
      "serial-getty@hvc0".enable = false;

      # The rescue prompt the masked instances above cannot be. Masking is
      # still correct -- the generator's instances are unfixable, because the
      # `BindsTo=dev-%i.device` is baked into the systemd store path and no
      # .device unit ever activates under `boot.isContainer` -- but masking on
      # its own left ix guests with exactly one interactive path: ix-console
      # over vsock. That path runs inside the guest as a process scheduled
      # alongside the workload, so the failure that most needs a rescue shell
      # is the failure that takes the rescue shell with it. On the 2026-08-17
      # dev fleet a guest that pegged its single entitled core starved the
      # ix-agent QUIC endpoint until it stopped answering, and there was no
      # second way in: the VM was wedged with a live kernel, a live serial
      # console carrying printk, and nothing on the far end of it to log into.
      #
      # This unit is that second way in. It is deliberately NOT an instance of
      # `serial-getty@`: it carries no `BindsTo=`, no `After=dev-ttyS0.device`,
      # and no `Requires=` on anything a container-mode guest cannot activate,
      # so it starts on the same boot every generator instance would have hung
      # on. `ConditionPathExists` is the whole device check -- devtmpfs has
      # /dev/ttyS0 long before multi-user.target, and a guest booted without a
      # serial port simply skips the unit as a satisfied no-op rather than
      # failing it.
      #
      # `--autologin root`, deliberately, because the alternative is a prompt
      # nobody can answer. This image sets no root credential anywhere:
      # `users.defaultUserShell` above is the only `users.*` fact it states, so
      # nixpkgs' update-users-groups.pl falls through to `my $hashedPassword =
      # "!"` and the guest ships `root:!:` in /etc/shadow, which pam_unix can
      # never authenticate against. There is no sshd in the image and no
      # authorized_keys injection on the ix side either. A bare agetty here
      # would answer every rescue attempt with "Login incorrect" -- a rescue
      # path that cannot be entered is not one. The alternative, baking a
      # password hash into a shared base image, would put one guessable
      # long-lived secret in front of every guest in the fleet; a per-VM
      # injected credential is real control-plane machinery (mint, store,
      # deliver, rotate) and is not what this unit is.
      #
      # Autologin grants no authority the caller lacks. `ix serial` and
      # `ix shell` mint their connect tokens through the same op-level gate,
      # `vm:exec`, behind the same owner-or-platform-admin check; ix's
      # per-audience escalation table lifts only the `Switch` audience, and the
      # one non-owner mint path is hardcoded to port-forward. Every principal
      # who can reach this getty already has root in the guest via `ix shell`,
      # and the serial bridge refuses any attach without a signed
      # Serial-audience JWT.
      #
      # Known cost, accepted: the emulated UART is teed into the capture file
      # backing the `kernel` log stream, and that stream reads at `vm:read` --
      # weaker than the `vm:exec` an attach needs. A rescue session's prompt and
      # echoed commands therefore land in `ix logs --stream kernel` and
      # ClickHouse `vm_serial_logs` alongside kernel printk. The tee is a
      # property of using serial at all rather than of autologin, but it does
      # mean this console is not the place to type secrets. `--noclear` keeps a
      # respawn from also emitting a screen-clear escape sequence into that log.
      ix-serial-getty = {
        description = "Rescue serial login prompt on ttyS0";
        documentation = ["man:agetty(8)"];
        wantedBy = ["multi-user.target"];
        # Ordering only, never a dependency: a rescue shell that refuses to
        # start because a normal-boot unit failed is not a rescue shell.
        after = ["systemd-user-sessions.service"];
        # A switch must not kill the session an operator is using to debug the
        # switch. Same reasoning nixpkgs applies to its own gettys.
        restartIfChanged = false;
        unitConfig.ConditionPathExists = "/dev/ttyS0";
        serviceConfig = {
          ExecStart = "${pkgs.util-linux}/sbin/agetty --keep-baud --noclear --autologin root ttyS0 115200 vt100";
          Type = "idle";
          Restart = "always";
          RestartSec = 5;
          UtmpIdentifier = "ttyS0";
          TTYPath = "/dev/ttyS0";
          TTYReset = true;
          TTYVHangup = true;
          KillMode = "process";
          IgnoreSIGPIPE = false;
          SendSIGHUP = true;
          StandardInput = "tty";
          StandardOutput = "tty";
        };
      };
    };

    # Capture native crashes (JVM segfault, Rust panics in extern, anything
    # that takes SIGSEGV/SIGABRT) into /var/lib/systemd/coredump where
    # `coredumpctl list/info/gdb` can find them. Without this, a crashed
    # service shows `signal=SEGV` in the journal and the dump goes to
    # /dev/null. MaxUse caps a crash-looping service rather than saving
    # disk — disk autoscales to ~1 PiB, the cap is just defence in depth.
    systemd.coredump = {
      enable = true;
      settings.Coredump = {
        Storage = "external";
        Compress = "yes";
        MaxUse = "5G";
      };
    };

    # Modern Nix configuration for any in-VM nix invocation.
    #
    # experimental-features: nix CLI, flakes, the `|>` pipe operator,
    # fetchClosure, content-addressed derivations, dynamic derivations,
    # and git tree hashing. List-typed, so per-image additions concatenate
    # rather than override.
    #
    # allow-import-from-derivation: IFD is allowed by upstream default;
    # setting it explicitly pins repo policy ("IFD is allowed when it
    # removes a fake Nix layer", AGENTS.md) against a future upstream flip.
    #
    # gc + optimise: long-lived dev VMs accumulate roots from `nix run` /
    # `nix shell` and never get a /nix/store sweep otherwise. 30 days
    # keeps recent results for repeat invocations and bounds disk growth.
    # Optimise hardlinks duplicate store paths so the savings compound.
    nix = {
      # The system flake registry pin for `nixpkgs` lives in
      # lib/image/default.nix (an inline module next to `nixpkgs.pkgs`),
      # because locking it needs the flake input's `narHash`, which only
      # that scope has.
      #
      # NIX_PATH so legacy nix-shell / <nixpkgs> imports resolve without a
      # channel subscription or network fetch.
      nixPath = ["nixpkgs=${pkgs.path}"];
      settings = {
        experimental-features = [
          "nix-command"
          "flakes"
          "pipe-operators"
          "fetch-closure"
          "fetch-tree"
          "ca-derivations"
          # Rust units address their output with blake3 (ADR 0003); parsing
          # `outputHashAlgo = "blake3"` is gated on this feature, so an in-VM
          # `nix build` of a rendered unit needs it.
          "blake3-hashes"
          "dynamic-derivations"
          "git-hashing"
          # `.ix` imports convert in-eval via `builtins.wasm` over the
          # `ix2nix-wasm` package output (importIxWasm; IFD as of 2026-07-25,
          # replacing the committed artifact of #4136). The
          # in-VM client is the wasm-enabled nix-ix, which gates the builtin
          # behind this feature, so plain `nix build` / `nix eval` in a VM
          # can load `.ix` modules without per-command flags.
          "wasm-builtin"
        ];
        allow-import-from-derivation = lib.mkDefault true;
        warn-dirty = false;
        # nixpkgs' nix-daemon module bakes `sandbox-fallback = false` as a
        # "legacy configuration conversion" (nixos/modules/services/system/
        # nix-daemon.nix), which turns a build FATAL the moment the kernel
        # lacks the namespaces sandboxing needs. ix guests deliberately run
        # without user namespaces (hardening sets `allowNamespaces = false`)
        # and the VM is itself the isolation boundary, so a build that cannot
        # be sandboxed should degrade to an unsandboxed build with a warning,
        # not kill `ix apply`. Restore Nix's own upstream default of `true`.
        # mkForce because the nixpkgs assignment is unconditional, not a
        # default. See indexable-inc/index#2453.
        # astlog-ignore: no-mkforce nixpkgs sets this unconditionally; #2453 owns the fix.
        sandbox-fallback = lib.mkForce true;
      };
      gc = {
        automatic = true;
        options = "--delete-older-than 30d";
      };
      optimise.automatic = true;
      # Keep ad-hoc nix work an operator runs over SSH from starving the
      # service workload (Minecraft tick rate, Postgres queries). The
      # daemon still makes progress; the kernel just deprioritizes it
      # whenever the real service wants CPU or disk.
      daemonCPUSchedPolicy = "idle";
      daemonIOSchedClass = "idle";
    };

    # /bin/sh and /usr/bin/env are baked into every image root at build time
    # (oci-layer.nix systemRoot here; the CAS systemRoot on the ix side), and
    # the CAS-booted guest reaches /bin and /usr through symlinks into the
    # read-only store, so the stock recreate-and-rename these snippets run
    # can never succeed there: both fail on every boot and would fail the
    # Activating phase of a switch (ix#8307). Blank them; the attrs stay
    # defined so snippet dependency resolution is unaffected. mkForce because
    # nixpkgs assigns both snippets unconditionally, not as defaults.
    system.activationScripts = {
      # astlog-ignore: no-mkforce nixpkgs sets this unconditionally; the image bakes /bin/sh (ix#8307)
      binsh = lib.mkForce "";
      # astlog-ignore: no-mkforce nixpkgs sets this unconditionally; the image bakes /usr/bin/env (ix#8307)
      usrbinenv = lib.mkForce "";

      # Heal the FHS compat entries when they dangle. An image root can bake
      # /bin, /sbin and /usr as absolute symlinks into the closure that built
      # the IMAGE rather than into anything stable. Nothing repoints them on a
      # switch, and the guest's own nix-gc then collects that closure, so on
      # any VM that has switched once all three dangle at the same time.
      # Measured on hyperion-game (2026-07-29), which had switched away from
      # its image closure and GC'd it:
      #
      #   /usr -> /nix/store/fna5ylak...-nixos-system-nixos-26.11.../sw  [DANGLING]
      #
      # The consequences are not subtle. /bin/sh stops existing, so every
      # `#!/bin/sh` script in the guest is broken; systemd-update-done dies on
      # `Failed to stat /usr/`; and logrotate dies at step NAMESPACE building
      # its mount namespace. The last two exit a switch 4, which an apply
      # reports as a failed target (ENG-11063).
      #
      # /run/current-system/sw is the stable target: activation repoints it on
      # every switch and it is itself a GC root, so it cannot go the same way.
      # Only a symlink that fails to resolve is touched, so a root laid out
      # with real /bin and /usr directories (oci-layer.nix since ix#8307) is
      # left exactly as the image built it. /lib is deliberately not in the
      # list: the host injects the running kernel's module tree at
      # /lib/modules/<kver> in the rootfs, and a symlink into the system
      # closure would not carry it (ix toplevels ship no kernel-modules link).
      #
      # Activation runs at boot AND during a switch, so an already-booted VM
      # heals through a plain `ix apply` with no image rebuild and no recreate.
      fhsCompatLinks = ''
        heal() {
          # $1 = FHS entry, $2 = the path it should point at instead.
          if [ -L "$1" ] && [ ! -e "$1" ]; then
            echo "platform: $1 dangles, repointing at $2"
            ln -sfn "$2" "$1"
          fi
        }
        heal /bin /run/current-system/sw/bin
        heal /sbin /run/current-system/sw/sbin
        heal /usr /run/current-system/sw
      '';
    };

    system.stateVersion = "25.05";
  };
}
