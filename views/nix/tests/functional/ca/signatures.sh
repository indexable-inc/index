#!/usr/bin/env bash

source common.sh

clearStore
clearCache

nix-store --generate-binary-cache-key cache1.example.org "$TEST_ROOT/sk1" "$TEST_ROOT/pk1"
pk1=$(cat "$TEST_ROOT/pk1")

export REMOTE_STORE_DIR="$TEST_ROOT/remote_store"
export REMOTE_STORE="file://$REMOTE_STORE_DIR"

ensureCorrectlyCopied () {
    attrPath="$1"
    nix build --store "$REMOTE_STORE" --file ./content-addressed.nix "$attrPath"
}

testOneCopy () {
    clearStore
    rm -rf "$REMOTE_STORE_DIR"

    attrPath="$1"
    nix copy -vvvv --to "$REMOTE_STORE" "$attrPath" --file ./content-addressed.nix \
        --secret-key-files "$TEST_ROOT/sk1" --show-trace

    ensureCorrectlyCopied "$attrPath"

    # Ensure that we can copy back what we put in the store
    clearStore
    nix copy --from "$REMOTE_STORE" --eval-store local \
        --file ./content-addressed.nix "$attrPath" \
        --trusted-public-keys "$pk1"
}

for attrPath in rootCA dependentCA transitivelyDependentCA dependentNonCA dependentFixedOutput; do
    testOneCopy "$attrPath"
done

# Signing an existing build must retain its realisation, not merely its NAR.
clearStore
rm -rf "$REMOTE_STORE_DIR"
nix-store --generate-binary-cache-key wrong.example.org "$TEST_ROOT/wrong-sk" "$TEST_ROOT/wrong-pk"
wrongPk=$(cat "$TEST_ROOT/wrong-pk")
nix build --no-link --file ./content-addressed.nix rootCA
drv=$(nix eval --raw --file ./content-addressed.nix rootCA.drvPath)
out=$(nix path-info "$drv^out")
nix copy --to "$REMOTE_STORE" "$drv" "$drv^out"
nix store sign --recursive --store "$REMOTE_STORE" --key-file "$TEST_ROOT/sk1" "$drv^out"
# A repeat is idempotent; an unknown output is not a mapping to invent.
nix store sign --recursive --store "$REMOTE_STORE" --key-file "$TEST_ROOT/sk1" "$drv^out" 2>&1 | grep '0 realisation signatures'
expectStderr 1 nix store sign --store "$REMOTE_STORE" --key-file "$TEST_ROOT/sk1" "$drv^missing" | grep -E 'output|realisation'
clearStore
# Admit the bytes with the right key first: the negative control must reach
# the realisation signature, rather than fail on an input-addressed reference.
nix copy --from "$REMOTE_STORE" "$out" --trusted-public-keys "$pk1"
expectStderr 1 nix copy --from "$REMOTE_STORE" "$drv^out" --trusted-public-keys "$wrongPk" | grep 'cannot register realisation.*lacks a signature by a trusted key'
nix copy --from "$REMOTE_STORE" "$drv" "$drv^out" --trusted-public-keys "$pk1"
nix realisation info --json "$drv^out" | jq -e 'length == 1 and (.[0].signatures | length > 0)'
