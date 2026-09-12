# Config validation for andrewgazelka's tracked dotfiles (#3898), imported by
# lib/per-system.nix into the per-system check catalog. The zellij settings
# render against the owner's real XDG_CONFIG_HOME (the rendered config embeds
# absolute paths into that home), so the check validates exactly what
# deploys; that personal constant lives here, not in the shared aggregator.
{
  lib,
  pkgs,
  ix,
  mkCheck,
  nushell,
}: let
  fs = lib.fileset;
  zellij = import ./config/zellij {
    configRoot = ./config;
    inherit (pkgs) lib stdenvNoCC zellijPlugins;
    xdgConfigHome = "/Users/andrewgazelka/.config";
  };
  zellijConfig = pkgs.writeText "andrewgazelka-zellij.kdl" (ix.kdl.render zellij.settings);
  # Only the tracked nushell config tree: the check's subject, without
  # whatever untracked state sits next to it in a working checkout.
  nushellConfig = fs.toSource {
    root = ./config/nushell;
    fileset = ./config/nushell;
  };
in {
  zellij-config = mkCheck "zellij-config" {
    nativeBuildInputs = [pkgs.zellij];
    script = ''
      export HOME="$TMPDIR/home"
      mkdir -p "$HOME" "$out"
      zellij --config ${zellijConfig} setup --check >"$out/check.txt"
    '';
  };
  nushell-config = mkCheck "nushell-config" {
    nativeBuildInputs = [
      pkgs.binutils
      pkgs.jq
      # The fork package, not pkgs.nushell: the deployed shell that
      # executes this config is newer than the repo's nixpkgs pin
      # (0.114 names vs 0.113.1), so the check must run a
      # repo-controlled interpreter that tracks upstream (#3428).
      nushell
    ];
    script = ''
      export HOME="$TMPDIR/home"
      config_dir=$(nu --no-config-file -c '$nu.default-config-dir')
      mkdir -p "$(dirname "$config_dir")"
      cp -R ${nushellConfig} "$config_dir"
      cd "$config_dir"
      diagnostics="$TMPDIR/diagnostics.jsonl"
      nu --no-config-file --ide-check 100 config.nu > "$diagnostics"
      if ! jq -s -e 'map(select(.type == "diagnostic")) | length == 0' "$diagnostics" >/dev/null; then
        jq -s 'map(select(.type == "diagnostic"))' "$diagnostics" >&2
        exit 1
      fi
      if [[ $(nu --no-config-file -c 'nu-check functions/infra/status.nu') != true ]]; then
        echo "infra status probe did not parse" >&2
        exit 1
      fi
      for test in tests/test_*.nu; do
        nu --no-config-file "$test"
      done
    '';
  };
}
