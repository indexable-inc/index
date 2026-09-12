#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh


work=$TEST_ROOT/rust-eval-command-run
rm -rf "$work"
mkdir -p "$work"
cat > "$work/package.nix" <<EOF
let
  package = derivation {
    name = "rust-command-run";
    system = builtins.currentSystem;
    builder = "$bash";
    PATH = "$coreutils";
    args = [ "-c" ''mkdir -p \$out/bin; cat > \$out/bin/rust-command-run <<'SCRIPT'
#!$bash
printf 'rust-run-served\\n'
SCRIPT
chmod +x \$out/bin/rust-command-run'' ];
  };
in {
  # A derivation, not an app-typed set: with `--file` cppnix resolves the
  # installable to a root cursor with no attribute path, and outside a flake's
  # `apps` it expects a derivation (`expectedAppType`), whose program is
  # `bin/<name>`.
  inherit package;
}
EOF

runArm() {
    local config=$1 label=$2
    NIX_CONFIG="$config" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$label.json" \
        nix run --file "$work/package.nix" 'package^out' > "$work/$label.out" 2> "$work/$label.err" || armFailed "$label" "$work/$label.err"
}

runArm "$rustArm" rust
grepQuiet -Fx 'rust-run-served' "$work/rust.out"
assertRustServed "$work/rust.json"
grepQuietInverse -F 'rust-eval refusal' "$work/rust.err"

# Baked into every flake below: `builtins.currentSystem` does not exist under
# a flake's pure evaluation.
system=$(nix-instantiate --eval --strict -E builtins.currentSystem | tr -d '"')
jjFlakeDir "$work/flake"
cat > "$work/flake/flake.nix" <<EOF
{
  outputs = { self }:
    let
      package = { drvName, mainProgram, executable, text }: (derivation {
        name = drvName;
        system = "$system";
        builder = "$bash";
        PATH = "$coreutils";
        args = [ "-c" ''
          mkdir -p \$out/bin
          cat > \$out/bin/\${executable} <<'SCRIPT'
#!$bash
printf '\${text}\\n'
SCRIPT
          chmod +x \$out/bin/\${executable}
          ln -s \${executable} \$out/bin/\${mainProgram}
        '' ];
      }) // { meta.mainProgram = mainProgram; };
      appPackage = package {
        drvName = "apps-derivation";
        mainProgram = "apps-launcher";
        executable = "apps-payload";
        text = "apps-wins";
      };
    in {
      apps.${system}.default = { type = "app"; program = "\${appPackage}/bin/apps-payload"; };
      packages.${system}.default = package {
        drvName = "packages-derivation";
        mainProgram = "packages-launcher";
        executable = "packages-payload";
        text = "packages-loses";
      };
      packages.${system}.packageOnly = package {
        drvName = "package-only-derivation";
        mainProgram = "selected-main-program";
        executable = "payload-program";
        text = "main-program-wins";
      };
    };
}
EOF

runFlake() {
    local config=$1 label=$2 installable=$3
    NIX_CONFIG="$config" nix run "$installable" > "$work/$label.out" 2> "$work/$label.err" || armFailed "$label" "$work/$label.err"
}

for installable in "jj+file://$work/flake" "jj+file://$work/flake#packageOnly"; do
    suffix=precedence
    [[ $installable == *'#'* ]] && suffix=main-program
    runFlake "$rustArm" "rust-$suffix" "$installable"
done
grepQuiet -Fx 'apps-wins' "$work/rust-precedence.out"
grepQuiet -Fx 'main-program-wins' "$work/rust-main-program.out"

expectStderr 1 env NIX_CONFIG="$rustArm" nix run "jj+file://$work/flake#missing" \
    | grepQuiet -F 'missing'
expectStderr 1 env NIX_CONFIG="$rustArm" nix run --file "$work/does-not-exist.nix" . \
    | grepQuiet -F 'does-not-exist.nix'

echo "rust-eval-command-run: ok"
