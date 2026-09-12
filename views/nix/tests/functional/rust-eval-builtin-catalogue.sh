#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh

work=$TEST_ROOT/rust-eval-builtin-catalogue
rm -rf "$work"
mkdir -p "$work"

expression='{
  answer = builtins.add 2 3;
  wasm = builtins ? wasm;
  fetchTree = builtins ? fetchTree;
  getFlake = builtins ? getFlake;
  internal = builtins ? fetchFinalTree;
  native = builtins ? importNative;
}'

checkCatalogue() { # LABEL FEATURES EXPECTED-JSON CACHE-STATE
    local label=$1 features=$2 expected=$3 cacheState=$4 config
    config=$(printf 'experimental-features = nix-command %s\nextra-experimental-features =\neval-cache-dir = %s/cache\n' "$features" "$work")
    NIX_CONFIG="$config" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$label.stats" \
        nix eval --json --expr "$expression" > "$work/$label.json"
    jq -e --argjson expected "$expected" '. == $expected' "$work/$label.json" > /dev/null
    assertRustServed "$work/$label.stats"
    "$cacheState" "$work/$label.stats"
}

disabled='{"answer":5,"wasm":false,"fetchTree":false,"getFlake":false,"internal":false,"native":false}'
enabled='{"answer":5,"wasm":true,"fetchTree":true,"getFlake":false,"internal":false,"native":false}'
flakes='{"answer":5,"wasm":false,"fetchTree":true,"getFlake":true,"internal":false,"native":false}'

checkCatalogue disabled '' "$disabled" assertMemoMissed
checkCatalogue enabled 'fetch-tree wasm-builtin' "$enabled" assertMemoMissed
checkCatalogue enabled-warm 'fetch-tree wasm-builtin' "$enabled" assertMemoServed
checkCatalogue disabled-again '' "$disabled" assertMemoServed
checkCatalogue flakes flakes "$flakes" assertMemoMissed

echo 'rust-eval-builtin-catalogue: ok'
