#!/usr/bin/env bash

source common.sh

store_uri="ssh://localhost?remote-store=$TEST_ROOT/other-store"

# Check that store info trusted doesn't yet work with ssh://
nix --store "$store_uri" store info --json | jq -e 'has("trusted") | not'

# Suppress grumpiness about multiple nixes on PATH
(nix --store "$store_uri" doctor || true) 2>&1 | grep "doesn't have a notion of trusted user"

# Opaque paths must not construct an evaluator against the legacy SSH store.
# That store can transfer NARs but does not implement an evaluation filesystem.
drvPath=$(nix-instantiate simple.nix)
nix copy --derivation --to "$store_uri" "$drvPath"
[[ $(nix path-info --derivation --store "$store_uri" "$drvPath") == "$drvPath" ]]
nix copy --derivation --from "$store_uri" "$drvPath"
