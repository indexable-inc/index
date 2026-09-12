#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh


work=$TEST_ROOT/rust-eval-command-flake-show
rm -rf "$work"
# Baked into every flake below: `builtins.currentSystem` does not exist under
# a flake's pure evaluation.
system=$(nix-instantiate --eval --strict -E builtins.currentSystem | tr -d '"')
# jj workspaces, not plain directories: `path:` serves store objects only and
# refuses a mutable directory (see `jjFlakeDir`).
for flakeDir in flake nameless template wrong-name wrong-description; do
    jjFlakeDir "$work/$flakeDir"
done
cat > "$work/flake/flake.nix" <<EOF
{
  outputs = { self }:
    let
      package = name: derivation {
        inherit name;
        system = "$system";
        builder = "$bash";
        args = [ "-c" "touch \$out" ];
      };
      ifdSource = derivation {
        name = "flake-show-ifd-source";
        system = "$system";
        builder = "$bash";
        args = [ "-c" "echo 'builtins.currentSystem' > \$out" ];
      };
    in {
      packages.${system} = {
        good = package "flake-show-good";
        scalar = 42;
        "bad.name" = 42;
        empty = { };
        ifd = import ifdSource;
        metadataIfd = (package "flake-show-metadata-ifd") // {
          meta.description = import ifdSource;
        };
      };
      packages.emptySystem = { };
      packages.someOtherSystem.hidden = package "flake-show-hidden";
      legacyPackages.${system} = {
        package = package "flake-show-legacy";
        nested = { ignored = package "flake-show-legacy-nested"; };
        broken = throw "legacy output failed";
      };
      hydraJobs.nested.job = package "flake-show-nested";
      templates.example = {
        path = ./.;
        description = "required template description";
      };
    };
}
EOF

cat > "$work/nameless/flake.nix" <<EOF
{
  outputs = { self }: {
    packages.${system}.nameless = { type = "derivation"; };
  };
}
EOF

cat > "$work/template/flake.nix" <<EOF
{
  outputs = { self }: {
    templates.nameless = { path = ./.; };
  };
}
EOF

cat > "$work/wrong-name/flake.nix" <<EOF
{
  outputs = { self }: {
    packages.${system}."bad.name" = { type = "derivation"; name = 42; };
  };
}
EOF

cat > "$work/wrong-description/flake.nix" <<EOF
{
  outputs = { self }: {
    packages.${system}.badDescription = {
      type = "derivation";
      name = "bad-description";
      meta.description = 42;
    };
  };
}
EOF

runJSON() {
    local config=$1 label=$2
    NIX_CONFIG="$config" NO_COLOR=1 NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$label.json" \
        nix flake show --json --legacy --all-systems "jj+file://$work/flake" \
        > "$work/$label.out" 2> "$work/$label.err" || armFailed "$label" "$work/$label.err"
}

runText() {
    local config=$1 label=$2
    NIX_CONFIG="$config" NO_COLOR=1 NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$label.text.json" \
        nix flake show --legacy "jj+file://$work/flake" \
        > "$work/$label.text" 2> "$work/$label.text.err"
}

runHiddenJSON() {
    local config=$1 label=$2
    NIX_CONFIG="$config" NO_COLOR=1 nix flake show --json --legacy "jj+file://$work/flake" \
        > "$work/$label.hidden.out" 2> "$work/$label.hidden.err"
}

runDefaultText() {
    local config=$1 label=$2
    NIX_CONFIG="$config" NO_COLOR=1 nix flake show "jj+file://$work/flake" \
        > "$work/$label.default.text" 2> "$work/$label.default.text.err"
}

runJSON "$rustArm" rust
warningsOf() { # ERR-FILE -> path of its flake-show warnings
    grep -E 'not a derivation|omitted' "$1" > "$1.warnings" || true
    echo "$1.warnings"
}
jq -e --arg system "$system" '
  .packages[$system].good.name == "flake-show-good" and
  .packages[$system].scalar == {} and
  .packages[$system]["bad.name"] == {} and
  .packages[$system].empty == {} and
  .packages[$system].ifd == {} and
  .packages[$system].metadataIfd == {} and
  (.packages | has("emptySystem") | not) and
  .legacyPackages[$system].package.name == "flake-show-legacy" and
  .legacyPackages[$system].nested == {} and
  .legacyPackages[$system].broken == {} and
  .hydraJobs.nested.job.name == "flake-show-nested" and
  .templates.example.description == "required template description"
' < "$work/rust.out" > /dev/null
assertRustServed "$work/rust.json"
grepQuiet -F 'scalar.name is not a derivation' "$work/rust.err"
grepQuiet -Fx "warning: packages.$system.\"bad.name\".name is not a derivation" "$work/rust.err"
grepQuiet -F 'empty.name is not a derivation' "$work/rust.err"
grepQuiet -Fx "warning: packages.$system.ifd omitted due to use of import from derivation" "$work/rust.err"

runHiddenJSON "$rustArm" rust
grepQuiet -F "omitted (use '--all-systems' to show)" "$work/rust.hidden.err"

runText "$rustArm" rust
plain() { # FILE -> its text without ANSI escapes
    sed $'s/\e\\[[0-9;]*m//g' "$1"
}
plain "$work/rust.text" > "$work/rust.text.plain"
rustIfdNode=$(grep -F 'ifd omitted due to use of import from derivation' "$work/rust.text.plain")
grepQuiet -F "omitted (use '--all-systems' to show)" "$work/rust.text.plain"
grepQuiet -F "metadataIfd: package 'flake-show-metadata-ifd'" "$work/rust.text.plain"

runDefaultText "$rustArm" rust
plain "$work/rust.default.text" > "$work/rust.default.text.plain"
grepQuiet -F "omitted (use '--legacy' to show)" "$work/rust.default.text.plain"

# Both render modes must replay the same document and warnings from the
# persistent question cache, without depending on evaluator handles.
cachedArm=$(rustCachedArm flake-show-document)
runJSON "$cachedArm" json-cold
runJSON "$cachedArm" json-warm
cmp "$work/json-cold.out" "$work/json-warm.out"
cmp "$(warningsOf "$work/json-cold.err")" "$(warningsOf "$work/json-warm.err")"
assertMemoServed "$work/json-warm.json"
runText "$cachedArm" text-cold
runText "$cachedArm" text-warm
cmp "$work/text-cold.text" "$work/text-warm.text"
cmp "$(warningsOf "$work/text-cold.text.err")" "$(warningsOf "$work/text-warm.text.err")"
assertMemoServed "$work/text-warm.text.json"

compareFailure() {
    local flake=$1 expected=$2
    for arm in rust; do
        [[ $arm == rust ]] && config=$rustArm
        err="$work/$flake.$arm.err"
        expectStderr 1 env NIX_CONFIG="$config" NO_COLOR=1 \
            nix flake show --json "jj+file://$work/$flake" > "$err"
        grep -F "$expected" "$err" > "$work/$flake.$arm.line"
    done
}

compareFailure nameless "attribute 'packages.$system.nameless.name' does not exist"
compareFailure template "attribute 'templates.nameless.description' does not exist"
compareFailure wrong-name "'packages.$system.\"bad.name\".name' is not a string but an integer"
compareFailure wrong-description "'packages.$system.badDescription.meta.description' is not a string but an integer"

echo "rust-eval-command-flake-show: ok"
