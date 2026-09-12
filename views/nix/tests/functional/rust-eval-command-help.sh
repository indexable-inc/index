#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh

work=$TEST_ROOT/rust-eval-command-help
rm -rf "$work"
mkdir -p "$work"

# Language documentation comes from the Rust catalogue and needs no store.
NIX_CONFIG="$rustArm"$'store = unsupported-language-doc-test://\n' \
    nix __dump-language > "$work/language.json"
jq -e '
    type == "object" and
    (.add.args | length) == 2 and
    (.readFile.doc | type) == "string" and
    (.true.type | type) == "string" and
    all(.[]; (.doc | type) == "string")
' "$work/language.json" > /dev/null

checkPage() { # LABEL PHRASE ARGS...
    local label=$1 phrase=$2
    shift 2
    NIX_CONFIG="$rustArm" NIX_PAGER=cat NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/rust-$label.json" \
        nix "$@" > "$work/rust-$label.out" 2> "$work/rust-$label.err" \
        || armFailed "rust-$label" "$work/rust-$label.err"
    grepQuiet -F "$phrase" "$work/rust-$label.out"
    grepQuietInverse -F 'rust-eval' "$work/rust-$label.err"
    assertRustServed "$work/rust-$label.json"
}

checkPage hash-convert 'nix hash convert' hash convert --help
checkPage toplevel 'Create a new flake' --help
checkPage help-subcommand 'nix flake metadata' help flake metadata

# The generated markdown attribute is internal. A typed missing-selection
# error must reach show's command-name handler instead of leaking that path.
expect 1 env NIX_CONFIG="$rustArm" NIX_PAGER=cat nix help no-such-command \
    > "$work/missing.out" 2> "$work/missing.err"
grepQuiet -F "Nix has no subcommand 'no-such-command'" "$work/missing.err"
grepQuietInverse -F 'nix3-no-such-command.md' "$work/missing.err"
expect 1 env NIX_CONFIG="$rustArm" NIX_PAGER=cat nix help flake no-such-command \
    > "$work/missing-nested.out" 2> "$work/missing-nested.err"
grepQuiet -F "Nix has no subcommand 'flake no-such-command'" "$work/missing-nested.err"
grepQuietInverse -F 'nix3-flake-no-such-command.md' "$work/missing-nested.err"

echo "rust-eval-command-help: ok"
