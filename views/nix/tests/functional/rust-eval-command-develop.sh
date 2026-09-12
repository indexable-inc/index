#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh


work=$TEST_ROOT/rust-eval-command-develop
rm -rf "$work"
mkdir -p "$work"
# Baked into every flake below: `builtins.currentSystem` does not exist under
# a flake's pure evaluation.
system=$(nix-instantiate --eval --strict -E builtins.currentSystem | tr -d '"')
# jj workspaces, not plain directories: `path:` serves store objects only and
# refuses a mutable directory (see `jjFlakeDir`).
jjFlakeDir "$work/nixpkgs"
jjFlakeDir "$work/develop"
cat > "$work/nixpkgs/flake.nix" <<EOF
{
  outputs = { self }: {
    legacyPackages.$system.bashInteractive = derivation {
      name = "rust-command-bash-interactive";
      system = "$system";
      # `nix develop` dumps the environment per name in `$outputs`, which
      # only a derivation given an `outputs` attribute carries (stdenv always
      # does); a bare derivation without it produces no environment at all.
      outputs = [ "out" ];
      builder = "$bash";
      PATH = "$coreutils";
      args = [ "-c" "mkdir -p \$out/bin; ln -s $bash \$out/bin/bash" ];
    };
  };
}
EOF
cat > "$work/develop/flake.nix" <<EOF
{
  inputs.nixpkgs.url = "jj+file://$work/nixpkgs";
  outputs = { self, nixpkgs }: {
    devShells.$system.default = derivation {
      name = "rust-command-develop";
      system = "$system";
      # `nix develop` dumps the environment per name in `$outputs`, which
      # only a derivation given an `outputs` attribute carries (stdenv always
      # does); a bare derivation without it produces no environment at all.
      outputs = [ "out" ];
      builder = "$bash";
      PATH = "$coreutils";
      args = [ "-c" "touch \$out" ];
      RUST_DEVELOP_MARKER = "rust-develop-served";
      RUST_DEVELOP_SECOND = "second-value";
      buildPhase = ''printf 'phase:%s\\n' "\$PWD"'';
    };
  };
}
EOF

runArm() {
    local config=$1 label=$2
    NIX_CONFIG="$config" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$label.json" \
        nix develop --ignore-env "jj+file://$work/develop^out" \
        --command "$bash" -c 'printf "%s\n" "$RUST_DEVELOP_MARKER" "$RUST_DEVELOP_SECOND" "$SHELL" "$PATH"' \
        > "$work/$label.out" 2> "$work/$label.err" || armFailed "$label" "$work/$label.err"
}

runArm "$rustArm" rust
grepQuiet -Fx 'rust-develop-served' "$work/rust.out"
grepQuiet -Fx 'second-value' "$work/rust.out"
selectedShell=$(sed -n '3p' "$work/rust.out")
selectedPath=$(sed -n '4p' "$work/rust.out")
if ! [[ $selectedShell == "$NIX_STORE_DIR/"*/bin/bash ]]; then
    echo "the develop shell was not the nixpkgs input's bashInteractive but '$selectedShell'; the arms' stderr:" >&2
    cat "$work/rust.err" >&2
    exit 1
fi
[[ $selectedPath == "${selectedShell%/bash}" || $selectedPath == "${selectedShell%/bash}:"* ]]
assertRustServed "$work/rust.json"
grepQuietInverse -F 'rust-eval refusal' "$work/rust.err"

expectStderr 1 env NIX_CONFIG="$rustArm" nix develop "jj+file://$work/develop#missing" \
    | grepQuiet -F 'missing'

multiExpr='let mk = name: derivation { inherit name; outputs = [ "out" ]; system = "'$system'"; builder = "'$bash'"; PATH = "'$coreutils'"; args = [ "-c" "touch $out" ]; }; in { recurseForDerivations = true; a = mk "a"; b = mk "b"; }'
expectStderr 1 env NIX_CONFIG="$rustArm" nix develop --impure --expr "$multiExpr" \
    | grepQuiet -F 'command-not-a-derivation'

for kind in missing error; do
    jjFlakeDir "$work/nixpkgs-$kind"
    jjFlakeDir "$work/develop-$kind"
done
cat > "$work/nixpkgs-missing/flake.nix" <<EOF
{
  outputs = { self }: { legacyPackages.$system = { }; };
}
EOF
cat > "$work/nixpkgs-error/flake.nix" <<EOF
{
  outputs = { self }: {
    legacyPackages.$system.bashInteractive = throw "bashInteractive evaluation failed";
  };
}
EOF
for kind in missing error; do
    cat > "$work/develop-$kind/flake.nix" <<EOF
{
  inputs.nixpkgs.url = "jj+file://$work/nixpkgs-$kind";
  outputs = { self, nixpkgs }: {
    devShells.$system.default = derivation {
      name = "develop-fallback-$kind";
      system = "$system";
      # `nix develop` dumps the environment per name in `$outputs`, which
      # only a derivation given an `outputs` attribute carries (stdenv always
      # does); a bare derivation without it produces no environment at all.
      outputs = [ "out" ];
      builder = "$bash";
      PATH = "$coreutils";
      args = [ "-c" "touch \$out" ];
    };
  };
}
EOF
    for arm in rust; do
        [[ $arm == rust ]] && config=$rustArm
        NIX_CONFIG="$config" SHELL="$bash" nix develop "jj+file://$work/develop-$kind" \
            --command "$bash" -c 'printf "%s\n" "$SHELL"' \
            > "$work/fallback-$kind-$arm.out" 2> "$work/fallback-$kind-$arm.err"
    done
    grepQuiet -Fx 'bash' "$work/fallback-$kind-rust.out"
done

expectStderr 1 env NIX_CONFIG="$rustArm" _NIX_TEST_RUST_EVAL_DEVELOP_BASH_REFUSAL=1 \
    nix develop --ignore-env "jj+file://$work/develop" --command true > "$work/bash-refusal.err"
grepQuiet -Fx '<4>rust-eval refusal token=command-unsupported detail=test-only refusal resolving nixpkgs#bashInteractive' \
    "$work/bash-refusal.err"
grepQuiet -F 'rust-eval unimplemented: test-only refusal resolving nixpkgs#bashInteractive' \
    "$work/bash-refusal.err"

expectStderr 1 env NIX_CONFIG="$rustArm" nix develop "jj+file://$work/develop^" --command true \
    | grepQuiet -F 'output'

echo "rust-eval-command-develop: ok"
