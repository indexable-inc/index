#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh

work=$TEST_ROOT/rust-eval-command-installables
rm -rf "$work"
mkdir -p "$work"
system=$(nix-instantiate --eval --strict -E builtins.currentSystem | tr -d '"')

jjFlakeDir "$work/flake"
cat > "$work/flake/flake.nix" <<EOF
{
  outputs = { self }:
    let
      # `outputs` spelled out: `derivation` exports it to the builder only when
      # given, and get-env.sh (print-dev-env) walks `\$outputs` to find where to
      # write.
      package = name: derivation {
        inherit name;
        system = "$system";
        builder = "$bash";
        outputs = [ "out" ];
        args = [ "-c" "echo \$name > \$out" ];
      };
    in {
      packages.$system.one = package "installables-one";
      packages.$system.two = package "installables-two";
      # Two outputs, and a default selection that leaves one of them out.
      packages.$system.multi = derivation {
        name = "installables-multi";
        system = "$system";
        builder = "$bash";
        outputs = [ "out" "dev" ];
        args = [ "-c" "echo out > \$out; echo dev > \$dev" ];
      } // { meta.outputsToInstall = [ "out" ]; };
    };
}
EOF
cat > "$work/default.nix" <<EOF
{
  one = derivation {
    name = "installables-file-one";
    system = builtins.currentSystem;
    builder = "$bash";
    args = [ "-c" "echo one > \$out" ];
  };
}
EOF
flake="jj+file://$work/flake"

cat > "$work/function.nix" <<EOF
{ name, suffix ? "default" }:
derivation {
  name = "installables-\${name}-\${suffix}";
  system = builtins.currentSystem;
  builder = "$bash";
  args = [ "-c" "echo function > \$out" ];
}
EOF

runArm() { # ARM LABEL ARGS...
    local arm=$1 label=$2
    shift 2
    NIX_CONFIG="$rustArm" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$arm-$label.json" \
        nix "$@" > "$work/$arm-$label.out" 2> "$work/$arm-$label.err" \
        || armFailed "$arm-$label" "$work/$arm-$label.err"
}

# Served alike: stdout identical, and the Rust arm evaluated on Rust alone.
checkServed() { # LABEL ARGS...
    local label=$1
    shift
    for arm in rust; do
        runArm "$arm" "$label" "$@"
    done
    assertRustServed "$work/rust-$label.json"
    grepQuietInverse -F 'rust-eval unimplemented' "$work/rust-$label.err"
}

checkServed derivation-show derivation show "$flake#one"
jq -e '.derivations | to_entries | length == 1 and (.[0].value.name == "installables-one")' < "$work/rust-derivation-show.out" > /dev/null
checkServed path-info path-info --derivation "$flake#one" "$flake#two"
[[ $(wc -l < "$work/rust-path-info.out") -eq 2 ]]
checkServed build-default build --no-link --print-out-paths "$flake#multi"
[[ $(wc -l < "$work/rust-build-default.out") -eq 1 ]]
grepQuiet -F -- '-installables-multi' "$work/rust-build-default.out"
grepQuietInverse -F -- '-installables-multi-dev' "$work/rust-build-default.out"
checkServed build-dev build --no-link --print-out-paths "$flake#multi^dev"
[[ $(wc -l < "$work/rust-build-dev.out") -eq 1 ]]
grepQuiet -F -- '-installables-multi-dev' "$work/rust-build-dev.out"
checkServed build-all build --no-link --print-out-paths "$flake#multi^*"
[[ $(wc -l < "$work/rust-build-all.out") -eq 2 ]]
checkServed file path-info --derivation --file "$work/default.nix" one
grepQuiet -F -- '-installables-file-one.drv' "$work/rust-file.out"
# The empty attribute path must auto-call a source function before checking
# that it is a derivation, using both supplied arguments and defaults.
checkServed function-default path-info --derivation --file "$work/function.nix" --argstr name called
grepQuiet -F -- '-installables-called-default.drv' "$work/rust-function-default.out"
checkServed function-arg build --no-link --print-out-paths --file "$work/function.nix" \
    --argstr name called --arg suffix '"explicit"'
grepQuiet -F -- '-installables-called-explicit' "$work/rust-function-arg.out"
expectStderr 1 env NIX_CONFIG="$rustArm" nix path-info --derivation --file "$work/function.nix" \
    | grepQuiet -F "cannot evaluate a function that has an argument without a value ('name')"
checkServed why-depends why-depends --derivation "$flake#one" "$flake#one"
grepQuiet -F -- '-installables-one.drv' "$work/rust-why-depends.out"
# `print-dev-env` builds the `-env` twin of the derivation the route hands it
# and evaluates nothing itself; the fixture's builder is bash, which get-env.sh
# needs. The witness is the twin's own `name` (the store path of `out` is
# rewritten to the outputs directory, so it never names the derivation).
checkServed print-dev-env print-dev-env "$flake#one"
grepQuiet -F "name='installables-one-env'" "$work/rust-print-dev-env.out"

drv=$(head -n 1 "$work/rust-path-info.out")
runArm rust store-path path-info --derivation "$drv"
[[ $(cat "$work/rust-store-path.out") == "$drv" ]]
# Opaque store paths bypass EvalState entirely. Its stats writer therefore
# never runs, even with NIX_SHOW_STATS enabled.
[[ ! -e "$work/rust-store-path.json" ]]
grepQuietInverse -F 'rust-eval unimplemented' "$work/rust-store-path.err"

# Search uses the bare reference's package roots without selecting a default.
searchStats="$work/rust-search.json"
NIX_CONFIG="$rustArm" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$searchStats" \
    nix search --json "$flake" one > "$work/search.out"
jq -e 'length > 0' "$work/search.out" > /dev/null
assertRustServed "$searchStats"

expectStderr 1 env NIX_CONFIG="$rustArm" nix profile add --profile "$work/profile" "$flake#one" \
    | grepQuiet -F 'rust-eval unimplemented: nix profile add'
jjFlakeDir "$work/broken"
echo '{ outputs = ' > "$work/broken/flake.nix"
broken="jj+file://$work/broken#one"
expectStderr 1 env NIX_CONFIG="$rustArm" nix profile add --profile "$work/profile" "$broken" \
    | grepQuiet -F 'rust-eval unimplemented: nix profile add'
[[ ! -e $work/profile ]]

echo "rust-eval-command-installables: ok"
