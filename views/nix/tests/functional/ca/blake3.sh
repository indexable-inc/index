#!/usr/bin/env bash

# A BLAKE3 content-addressed output may carry references, including a
# reference to itself, and survives a round trip through a binary cache.

source common.sh

# shellcheck disable=SC1111
needLocalStore "“--no-require-sigs” can’t be used with the daemon"

enableFeatures "blake3-hashes"
restartDaemon

clearStore

REMOTE_STORE_DIR="$TEST_ROOT/blake3_cache"
REMOTE_STORE="file://$REMOTE_STORE_DIR"
rm -rf "$REMOTE_STORE_DIR"

buildAttr () {
    nix build --file ./blake3.nix -L --no-link --print-out-paths "$@"
}

caOf () {
    nix path-info --json --json-format 1 "$1" | jq -r '.[].ca'
}

# A reference-free BLAKE3 output is addressed by the BLAKE3 hash of its NAR,
# and nothing else. Comparing against an independently computed digest keeps
# the store path scheme honest about which bytes it is naming.
plainOut=$(buildAttr plain)
plainCa=$(caOf "$plainOut")
[[ $plainCa == fixed:r:blake3:* ]]
plainCaBase16=$(nix hash convert --hash-algo blake3 --from nix32 --to base16 "${plainCa#fixed:r:blake3:}")
plainNarBase16=$(nix hash path --mode nar --algo blake3 --base16 "$plainOut")
[[ $plainCaBase16 == "$plainNarBase16" ]]

# An output that refers to another store path and to itself. Both kinds of
# reference are recorded, rather than rejected as they were when only SHA-256
# could reach the reference-bearing store path scheme.
selfRefOut=$(buildAttr selfRef)
[[ $(caOf "$selfRefOut") == fixed:r:blake3:* ]]
nix path-info --json --json-format 1 "$selfRefOut" \
    | jq -e --arg out "$selfRefOut" '.[].references | index($out) != null'
nix path-info --json --json-format 1 "$selfRefOut" \
    | jq -e '.[].references | length >= 2'

nix store verify "$selfRefOut"

# A dependent BLAKE3 output resolves through the self-referential one.
dependentOut=$(buildAttr dependent)
[[ $(caOf "$dependentOut") == fixed:r:blake3:* ]]

# The whole thing round trips through a binary cache: copy out, wipe the
# store, then realise with building forbidden so only substitution can win.
nix copy --to "$REMOTE_STORE" --file ./blake3.nix plain selfRef dependent

clearStore
substitutedOut=$(buildAttr --substitute --substituters "$REMOTE_STORE" --no-require-sigs -j0 dependent)
[[ $substitutedOut == "$dependentOut" ]]
[[ $(caOf "$dependentOut") == fixed:r:blake3:* ]]
[[ $(caOf "$selfRefOut") == fixed:r:blake3:* ]]

# Verify the admitted CA itself as well as the transported NAR.
nix store verify "$selfRefOut" "$dependentOut"
