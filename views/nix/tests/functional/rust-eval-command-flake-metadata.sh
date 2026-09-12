#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh

work=$TEST_ROOT/rust-eval-command-flake-metadata
rm -rf "$work"
mkdir -p "$work"

jjFlakeDir "$work/dep"
cat > "$work/dep/flake.nix" <<EOF
{ outputs = args: throw "dep outputs forced by a lock-only command"; }
EOF

# One fixture per arm: `nix flake lock` and `update` write into the flake they
# lock.
for arm in rust; do
    jjFlakeDir "$work/$arm"
    cat > "$work/$arm/flake.nix" <<EOF
{
  description = "lock-only metadata";
  inputs.dep.url = "jj+file://$work/dep";
  outputs = args: throw "outputs forced by a lock-only command";
}
EOF
done

lockOnly() { # ARM LABEL ARGS...
    local arm=$1 label=$2
    shift 2
    NIX_CONFIG="$rustArm" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$arm-$label.json" \
        nix "$@" > "$work/$arm-$label.out" 2> "$work/$arm-$label.err" \
        || armFailed "$arm-$label" "$work/$arm-$label.err"
    # The refusal marker, not the bare word: the fixture's own path carries
    # `rust-eval-command-flake-metadata`, and `nix flake lock` prints it.
    grepQuietInverse -F 'rust-eval unimplemented' "$work/$arm-$label.err"
    assertRustServed "$work/$arm-$label.json"
}

labels="lock metadata update archive prefetch-inputs"
for arm in rust; do
    lockOnly "$arm" lock flake lock "$work/$arm"
    [[ -f "$work/$arm/flake.lock" ]] || fail "$arm arm wrote no lock file"
    lockOnly "$arm" metadata flake metadata --json "$work/$arm"
    # The page is the real one: it names the input just locked.
    jq -e '.locks.nodes.dep.locked.type == "jj"' < "$work/$arm-metadata.out" > /dev/null
    lockOnly "$arm" update flake update --flake "$work/$arm"
    lockOnly "$arm" archive flake archive --dry-run --json "$work/$arm"
    # The dry run still walks the lock: the input is named with the store path
    # its tree hash derives (`Input::computeStorePath`), in this test's store.
    jq -e --arg store "$NIX_STORE_DIR" '.inputs.dep.path | startswith($store)' \
        < "$work/$arm-archive.out" > /dev/null
    lockOnly "$arm" prefetch-inputs flake prefetch-inputs "$work/$arm"
done

# Checking a flake evaluates its outputs, so this same fixture must fail at
# its throw. The successful commands above only inspect the declaration and lock.
expectStderr 1 env NIX_CONFIG="$rustArm" nix flake check --no-build "$work/rust" \
    | grepQuiet -F 'outputs forced by a lock-only command'

echo "rust-eval-command-flake-metadata: ok"
