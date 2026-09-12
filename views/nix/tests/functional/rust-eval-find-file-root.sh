#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh

for ext in so dylib; do
    plugin="${_NIX_TEST_BUILD_DIR}/plugins/libpathrootlookup.$ext"
    [[ -f "$plugin" ]] && break
done


expr='import (builtins.findFile [ { prefix = ""; path = "pathroot-test:fixture"; } ] "main.nix")'
NIX_CONFIG="${rustArm}plugin-files = $plugin" \
    nix-instantiate --eval --strict -E "$expr" > "$TEST_ROOT/find-file-root-rust"

[[ $(cat "$TEST_ROOT/find-file-root-rust") == 42 ]]

# A canonical mount key below a store object is still not a valid rooted wire
# identity. The plugin makes that otherwise-unreachable key resolve to the same
# fixture, so deleting the complete-store-object guard makes this evaluate to
# 42 instead of refusing at the boundary.
incompleteExpr='import (builtins.findFile [ { prefix = ""; path = "pathroot-incomplete:fixture"; } ] "main.nix")'
if NIX_CONFIG="${rustArm}plugin-files = $plugin" \
    nix-instantiate --eval --strict -E "$incompleteExpr" \
    > "$TEST_ROOT/find-file-incomplete-out" \
    2> "$TEST_ROOT/find-file-incomplete-err"; then
    echo "the Rust evaluator accepted an incomplete store-path mount key" >&2
    exit 1
fi
grep -F "not a complete store path" "$TEST_ROOT/find-file-incomplete-err" > /dev/null || {
    echo "the incomplete mount key failed for the wrong reason" >&2
    cat "$TEST_ROOT/find-file-incomplete-err" >&2
    exit 1
}
