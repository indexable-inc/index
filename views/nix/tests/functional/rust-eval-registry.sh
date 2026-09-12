#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh

work=$TEST_ROOT/rust-eval-registry
mkdir -p "$work"
registry=$work/registry.json
printf '{"version":2,"flakes":[]}\n' > "$registry"

registryNix() {
    NIX_CONFIG="$rustArm" nix --builders '' --option flake-registry "$registry" "$@"
}

jjFlakeDir "$work/target"
mkdir "$work/target/sub"
cat > "$work/target/flake.nix" <<'NIX'
{ outputs = args: { marker = "registry root"; }; }
NIX
cat > "$work/target/sub/flake.nix" <<'NIX'
{ outputs = args: { marker = "registry subdirectory"; }; }
NIX
jjFlakeDir "$work/alternate"
cat > "$work/alternate/flake.nix" <<'NIX'
{ outputs = args: { marker = "registry replacement"; }; }
NIX
target="jj+file://$work/target"
alternate="jj+file://$work/alternate"

# Mutation is visible through both enumeration and resolution. Replacing an
# existing alias must remove the earlier entry rather than hide a duplicate.
registryNix registry add --registry "$registry" registrySmoke "$target"
[[ $(registryNix registry resolve registrySmoke) == "$target" ]]
[[ $(registryNix eval --no-write-lock-file --raw registrySmoke#marker) == 'registry root' ]]
registryNix registry list > "$work/list-before"
grepQuiet -F "flake:registrySmoke $target" "$work/list-before"
registryNix registry add --registry "$registry" registrySmoke "$alternate"
jq -e '[.flakes[] | select(.from.id == "registrySmoke")] | length == 1' "$registry" > /dev/null
[[ $(registryNix registry resolve registrySmoke) == "$alternate" ]]
[[ $(registryNix eval --no-write-lock-file --raw registrySmoke#marker) == 'registry replacement' ]]
registryNix registry remove --registry "$registry" registrySmoke
jq -e '.flakes | length == 0' "$registry" > /dev/null
expectStderr 1 registryNix registry resolve registrySmoke | grepQuiet -F 'cannot find flake'
registryNix registry list > "$work/list-after"
grepQuietInverse -F 'flake:registrySmoke ' "$work/list-after"

# Directory data survives the persisted schema and the resolved reference.
registryNix registry add --registry "$registry" registrySub "$target?dir=sub"
jq -e '.flakes[] | select(.from.id == "registrySub") | .to.dir == "sub"' "$registry" > /dev/null
[[ $(registryNix registry resolve registrySub) == "$target?dir=sub" ]]
[[ $(registryNix eval --no-write-lock-file --raw registrySub#marker) == 'registry subdirectory' ]]

# --inputs-from installs a flag registry from a locked flake. The second
# evaluation must preserve the subdirectory even when input fetches are warm.
jjFlakeDir "$work/catalog"
cat > "$work/catalog/flake.nix" <<EOF
{
  inputs.catalogSub.url = "$target?dir=sub";
  outputs = args: {};
}
EOF
registryNix flake lock "$work/catalog"
registryNix eval --no-write-lock-file --inputs-from "$work/catalog" --raw catalogSub#marker > "$work/catalog-cold"
registryNix eval --no-write-lock-file --inputs-from "$work/catalog" --raw catalogSub#marker > "$work/catalog-warm"
printf 'registry subdirectory' > "$work/catalog-expected"
cmp "$work/catalog-expected" "$work/catalog-cold"
cmp "$work/catalog-expected" "$work/catalog-warm"
expectStderr 1 registryNix registry resolve catalogSub | grepQuiet -F 'cannot find flake'

# An exact entry cannot match a qualified alias. Fuzzy matching carries the
# requested ref to the jj target; resolving does not fetch that bookmark.
jq -n --arg url "file://$work/target" '{version:2, flakes:[{
    from:{type:"indirect",id:"registryExact"}, to:{type:"jj",url:$url}, exact:true
}]}' > "$registry"
[[ $(registryNix registry resolve registryExact) == "$target" ]]
expectStderr 1 registryNix registry resolve registryExact/topic | grepQuiet -F 'cannot find flake'
jq '.flakes[0].exact = false' "$registry" > "$registry.next"
mv "$registry.next" "$registry"
[[ $(registryNix registry resolve registryExact/topic) == "$target?ref=topic" ]]

# First declaration wins even if a later candidate is an exact match. Once
# reordered, the exact entry wins and consumes its explicitly matched ref.
jq -n --arg first "file://$work/target" --arg second "file://$work/alternate" '{version:2, flakes:[
    {from:{type:"indirect",id:"registryOrder"},to:{type:"jj",url:$first}},
    {from:{type:"indirect",id:"registryOrder",ref:"topic"},to:{type:"jj",url:$second},exact:true}
]}' > "$registry"
[[ $(registryNix registry resolve registryOrder/topic) == "$target?ref=topic" ]]
jq '.flakes |= reverse' "$registry" > "$registry.next"
mv "$registry.next" "$registry"
[[ $(registryNix registry resolve registryOrder/topic) == "$alternate" ]]

# This valid chain crosses the retired 100-rewrite ceiling. Every hop stays
# local and the terminal reference names the real fixture checked above.
jq -n --arg url "file://$work/target" '{version:2, flakes:(
    [range(0;256) | {from:{type:"indirect",id:("registryChain" + tostring)},
        to:{type:"indirect",id:("registryChain" + (. + 1 | tostring))}}] +
    [{from:{type:"indirect",id:"registryChain256"},to:{type:"jj",url:$url}}]
)}' > "$registry"
jq -e '.flakes | length == 257' "$registry" > /dev/null
[[ $(registryNix registry resolve registryChain0) == "$target" ]]
[[ $(registryNix eval --no-write-lock-file --raw registryChain0#marker) == 'registry root' ]]

# A real cycle fails by repeated identity, including a self alias. These
# failures are followed below by a valid registry to verify recovery.
jq '.flakes[-1].to = {type:"indirect",id:"registryChain0"}' "$registry" > "$registry.next"
mv "$registry.next" "$registry"
expectStderr 1 registryNix registry resolve registryChain0 | grepQuiet -F 'cycle detected in flake registry'
jq -n '{version:2,flakes:[{
    from:{type:"indirect",id:"registrySelf"},to:{type:"indirect",id:"registrySelf"}
}]}' > "$registry"
expectStderr 1 registryNix registry resolve registrySelf | grepQuiet -F 'cycle detected in flake registry'

# A valid first entry followed by a malformed one must reject the complete
# file, including resolution of that first entry and enumeration. Older
# partial parsing could silently retain that first alias after an error.
jq -n --arg url "file://$work/target" '{version:2,flakes:[
    {from:{type:"indirect",id:"registryHealthy"},to:{type:"jj",url:$url}},
    {from:{type:"indirect",id:"registryBroken"}}
]}' > "$registry"
expectStderr 1 registryNix registry resolve registryHealthy | grepQuiet -F "registry entry requires 'to'"
if registryNix registry list > "$work/malformed-list.out" 2> "$work/malformed-list.err"; then
    fail 'registry list accepted a malformed registry'
fi
grepQuiet -F "registry entry requires 'to'" "$work/malformed-list.err"
grepQuietInverse -F 'registryHealthy' "$work/malformed-list.out"
jq '.flakes |= .[:1]' "$registry" > "$registry.next"
mv "$registry.next" "$registry"
[[ $(registryNix registry resolve registryHealthy) == "$target" ]]

# Reject schema and scalar-type errors instead of treating corrupt data as
# an empty registry. Each mutation begins with the same valid document.
cp "$registry" "$work/valid-registry.json"
jq '.version = 1' "$work/valid-registry.json" > "$registry"
expectStderr 1 registryNix registry resolve registryHealthy | grepQuiet -F 'schema version 2'
jq '.flakes[0].exact = 1' "$work/valid-registry.json" > "$registry"
expectStderr 1 registryNix registry resolve registryHealthy | grepQuiet -F "'exact' must be a boolean"
jq '.flakes[0].to.dir = false' "$work/valid-registry.json" > "$registry"
expectStderr 1 registryNix registry resolve registryHealthy | grepQuiet -F "only a string 'dir'"
cp "$work/valid-registry.json" "$registry"
[[ $(registryNix eval --no-write-lock-file --raw registryHealthy#marker) == 'registry root' ]]

echo 'rust-eval-registry: ok'
