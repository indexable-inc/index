#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh


work=$TEST_ROOT/rust-eval-command-shell
rm -rf "$work"
mkdir -p "$work"
cat > "$work/package.nix" <<EOF
derivation {
  name = "rust-command-shell";
  system = builtins.currentSystem;
  builder = "$bash";
  PATH = "$coreutils";
  args = [ "-c" ''mkdir -p \$dev/bin; cat > \$dev/bin/rust-shell-probe <<'SCRIPT'
#!$bash
printf 'rust-shell-served\\n'
SCRIPT
chmod +x \$dev/bin/rust-shell-probe
touch \$out'' ];
  outputs = [ "out" "dev" ];
}
EOF

runArm() {
    local config=$1 label=$2
    NIX_CONFIG="$config" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$label.json" \
        nix shell --ignore-env --file "$work/package.nix" '.^dev' --command rust-shell-probe \
        > "$work/$label.out" 2> "$work/$label.err" || armFailed "$label" "$work/$label.err"
}

runArm "$rustArm" rust
grepQuiet -Fx 'rust-shell-served' "$work/rust.out"
assertRustServed "$work/rust.json"
grepQuietInverse -F 'rust-eval refusal' "$work/rust.err"

runInteractive() {
    local config=$1 label=$2
    NIX_CONFIG="$config" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$label.interactive.json" \
        SHELL="$bash" nix shell --ignore-env --file "$work/package.nix" '.^dev' \
        > "$work/$label.interactive.out" 2> "$work/$label.interactive.err" <<'EOF'
rust-shell-probe
EOF
}

runInteractive "$rustArm" rust
grepQuiet -Fx 'rust-shell-served' "$work/rust.interactive.out"
assertRustServed "$work/rust.interactive.json"

expectStderr 1 env NIX_CONFIG="$rustArm" nix shell --file "$work/does-not-exist.nix" . --command true \
    | grepQuiet -F 'does-not-exist.nix'

echo "rust-eval-command-shell: ok"
