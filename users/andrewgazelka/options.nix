{
  config,
  lib,
  ...
}: {
  options.users.andrewgazelka = {
    blockedHosts = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      description = "Domains whose browser cookies are cleared on activation.";
    };

    browserBlockedHosts = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      description = "Domains redirected by the workstation browser-tab guard.";
    };

    infra.hosts = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule {
        options = {
          sshAlias = lib.mkOption {
            type = lib.types.nonEmptyStr;
            description = "SSH alias used to query the host.";
          };
        };
      });
      default = {};
      description = "NixOS infrastructure hosts queried by `infra ls`.";
    };

    configurationRevision = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Revision of the consuming host configuration for provenance.";
    };

    packages = {
      ix = lib.mkOption {
        type = lib.types.nullOr lib.types.package;
        default = null;
        description = "Host-native ix CLI supplied by the consuming flake.";
      };
      lifelog = lib.mkOption {
        type = lib.types.nullOr lib.types.package;
        default = null;
        description = "Host-native lifelog package supplied by the consuming flake.";
      };
      jj = lib.mkOption {
        type = lib.types.nullOr lib.types.package;
        default = null;
        description = ''
          Host-native jj supplied by the consuming flake, installed as `jj`.
          Must be ix's native client (`packages/jj-ix` in the ix repository),
          not nixpkgs' jujutsu and not the vendored fork's own binary: it is
          the only one that opens an ix-backed repo and the only one that
          carries `jj view`, which `vcs-prompt` spawns on every prompt.

          Supplied by the host rather than taken from `indexPackages` because
          the client is a package of the ix ROOT flake, whose `packages/` this
          index-side profile cannot reach (index's flake source root is
          `./index`).
        '';
      };
      mercuryCli = lib.mkOption {
        type = lib.types.nullOr lib.types.package;
        default = null;
        description = "Host-native Mercury CLI supplied by the consuming flake.";
      };
      typenix = lib.mkOption {
        type = lib.types.nullOr lib.types.package;
        default = null;
        description = "Host-native typenix package supplied by the consuming flake.";
      };
      weave = lib.mkOption {
        type = lib.types.nullOr lib.types.package;
        default = null;
        description = "Host-native weave binary (indexable-inc/weave) supplied by the consuming flake; backs the agent memory store.";
      };
    };

    paths = {
      indexCheckout = lib.mkOption {
        type = lib.types.str;
        default = "${config.home.homeDirectory}/Projects/indexable-inc/index";
        defaultText = lib.literalExpression ''"''${config.home.homeDirectory}/Projects/indexable-inc/index"'';
        description = "Mutable index checkout used by workstation-only development services.";
      };
      ixCheckout = lib.mkOption {
        type = lib.types.str;
        default = "${config.home.homeDirectory}/Projects/indexable-inc/ix";
        defaultText = lib.literalExpression ''"''${config.home.homeDirectory}/Projects/indexable-inc/ix"'';
        description = "Mutable ix checkout used by workstation development tools.";
      };
      privateConfigDirectory = lib.mkOption {
        type = lib.types.str;
        default = "${config.home.homeDirectory}/.config/nix";
        defaultText = lib.literalExpression ''"''${config.home.homeDirectory}/.config/nix"'';
        description = "Private runtime configuration directory containing secret files.";
      };
      vscodeIslands = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "Source tree for the VS Code Islands extension.";
      };
    };

    ixPinRepo = lib.mkOption {
      type = lib.types.str;
      description = ''
        ix checkout the Starship prompt asks for the pin's commit epoch at
        prompt time. A path, not an epoch: `path:./ix` carries no
        `lastModified` while that checkout is dirty, so baking one at eval
        time broke `home-manager switch` outright.
      '';
    };

    sshSigningPublicKey = lib.mkOption {
      type = lib.types.str;
      default = builtins.head (
        builtins.filter (line: lib.hasInfix "signing" (lib.toLower line)) (
          lib.splitString "\n" (builtins.readFile ./config/ssh-keys/andrewgazelka.pub)
        )
      );
      defaultText = lib.literalExpression "the public key labelled signing in users/andrewgazelka/config/ssh-keys/andrewgazelka.pub";
      description = "Public SSH signing key line; never a private key or credential.";
    };

    ssh = {
      matchBlocks = lib.mkOption {
        type = lib.types.attrsOf (lib.types.attrsOf lib.types.anything);
        default = {};
        description = "Private, host-specific SSH match blocks supplied by the consumer.";
      };
      knownHosts = lib.mkOption {
        type = lib.types.nullOr lib.types.lines;
        default = null;
        description = "Private fleet known-host inventory supplied by the consumer.";
      };
    };

    agent = {
      extraSkills = lib.mkOption {
        type = lib.types.attrsOf lib.types.path;
        default = {};
        description = ''
          Consumer-local Claude Code / Codex skills merged into the shared
          index catalog. Each value is a skill directory holding a SKILL.md.
          Names must not collide with a shared skill.
        '';
      };
    };

    rbw = {
      email = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Private Vaultwarden account name supplied by the consumer.";
      };
      baseUrl = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Private Vaultwarden endpoint supplied by the consumer.";
      };
    };
  };
}
