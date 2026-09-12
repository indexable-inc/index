#!/usr/bin/env bash

source ./common.sh

flakeFollowsA=$TEST_ROOT/follows/flakeA
flakeFollowsB=$TEST_ROOT/follows/flakeA/flakeB
flakeFollowsC=$TEST_ROOT/follows/flakeA/flakeB/flakeC
flakeFollowsD=$TEST_ROOT/follows/flakeA/flakeD
flakeFollowsE=$TEST_ROOT/follows/flakeA/flakeE

# flakeA is the only workspace: B, C, D and E are directories inside its tree,
# so an absolute reference to one of them names flakeA and a `dir=`. The three
# flakes that name flakeE all name it the same way, which is what makes them
# share one lock node.
urlFollowsB="jj+file://$flakeFollowsA?dir=flakeB"
urlFollowsC="jj+file://$flakeFollowsA?dir=flakeB/flakeC"
urlFollowsD="jj+file://$flakeFollowsA?dir=flakeD"
urlFollowsE="jj+file://$flakeFollowsA?dir=flakeE"

# Test following path flakerefs.
jjFlakeDir "$flakeFollowsA"
mkdir -p "$flakeFollowsB"
mkdir -p "$flakeFollowsC"
mkdir -p "$flakeFollowsD"
mkdir -p "$flakeFollowsE"

cat > "$flakeFollowsA"/flake.nix <<EOF
{
    description = "Flake A";
    inputs = {
        B = {
            url = "path:./flakeB";
            inputs.foobar.follows = "foobar";
        };

        foobar.url = "$urlFollowsE";
    };
    outputs = { ... }: {};
}
EOF

cat > "$flakeFollowsB"/flake.nix <<EOF
{
    description = "Flake B";
    inputs = {
        foobar.url = "$urlFollowsE";
        goodoo.follows = "C/goodoo";
        C = {
            url = "path:./flakeC";
            inputs.foobar.follows = "foobar";
        };
    };
    outputs = { ... }: {};
}
EOF

cat > "$flakeFollowsC"/flake.nix <<EOF
{
    description = "Flake C";
    inputs = {
        foobar.url = "$urlFollowsE";
        goodoo.follows = "foobar";
    };
    outputs = { ... }: {};
}
EOF

cat > "$flakeFollowsD"/flake.nix <<EOF
{
    description = "Flake D";
    inputs = {};
    outputs = { ... }: {};
}
EOF

cat > "$flakeFollowsE"/flake.nix <<EOF
{
    description = "Flake E";
    inputs = {};
    outputs = { ... }: {};
}
EOF

nix flake metadata "$flakeFollowsA"

nix flake update --flake "$flakeFollowsA"

nix flake lock "$flakeFollowsA"

oldLock="$(cat "$flakeFollowsA/flake.lock")"

# Ensure that locking twice doesn't change anything

nix flake lock "$flakeFollowsA"

newLock="$(cat "$flakeFollowsA/flake.lock")"

diff <(echo "$newLock") <(echo "$oldLock")

[[ $(jq -c .nodes.B.inputs.C "$flakeFollowsA"/flake.lock) = '"C"' ]]
[[ $(jq -c .nodes.B.inputs.foobar "$flakeFollowsA"/flake.lock) = '["foobar"]' ]]
[[ $(jq -c .nodes.C.inputs.foobar "$flakeFollowsA"/flake.lock) = '["B","foobar"]' ]]

# Ensure removing follows from flake.nix removes them from the lockfile

cat > "$flakeFollowsA"/flake.nix <<EOF
{
    description = "Flake A";
    inputs = {
        B = {
            url = "path:./flakeB";
        };
        D.url = "path:./flakeD";
    };
    outputs = { ... }: {};
}
EOF

nix flake lock "$flakeFollowsA"

[[ $(jq -c .nodes.B.inputs.foobar "$flakeFollowsA"/flake.lock) = '"foobar"' ]]
jq -r -c '.nodes | keys | .[]' "$flakeFollowsA"/flake.lock | grep "^foobar$"

# Check that path: inputs cannot escape from their root. A relative path is a
# directory of the flake's own tree; one that climbs above the tree's root is
# refused by name, and pure and impure evaluation agree, because nothing was
# read from the filesystem to find that out.
cat > "$flakeFollowsA"/flake.nix <<EOF
{
    description = "Flake A";
    inputs = {
        B.url = "path:../flakeB";
    };
    outputs = { ... }: {};
}
EOF

expect 1 nix flake lock "$flakeFollowsA" 2>&1 | grep "input 'B'.*'../flakeB'.*is outside the tree of that flake"
expect 1 nix flake lock --impure "$flakeFollowsA" 2>&1 | grep "input 'B'.*'../flakeB'.*is outside the tree of that flake"

# Test relative non-flake inputs.
cat > "$flakeFollowsA"/flake.nix <<EOF
{
    description = "Flake A";
    inputs = {
        E.flake = false;
        E.url = "./foo.nix"; # test relative paths without 'path:'
    };
    outputs = { E, ... }: { e = import E; };
}
EOF

echo 123 > "$flakeFollowsA"/foo.nix

nix flake lock "$flakeFollowsA"

[[ $(nix eval --json "$flakeFollowsA"#e) = 123 ]]

# Non-existant follows should print a warning.
cat >"$flakeFollowsA"/flake.nix <<EOF
{
    description = "Flake A";
    inputs.B = {
        url = "path:./flakeB";
        inputs.invalid.follows = "D";
        inputs.invalid2.url = "path:./flakeD";
    };
    inputs.D.url = "path:./flakeD";
    outputs = { ... }: {};
}
EOF

nix flake lock "$flakeFollowsA" 2>&1 | grep "warning: input 'B' has an override for a non-existent input 'invalid'"
nix flake lock "$flakeFollowsA" 2>&1 | grep "warning: input 'B' has an override for a non-existent input 'invalid2'"

# Now test follow path overloading
# This tests a lockfile checking regression https://github.com/NixOS/nix/pull/8819
#
# We construct the following graph, where p->q means p has input q.
# A double edge means that the edge gets overridden using `follows`.
#
#      A
#     / \
#    /   \
#   v     v
#   B ==> C   --- follows declared in A
#    \\  /
#     \\/     --- follows declared in B
#      v
#      D
#
# The message was
#    error: input 'B/D' follows a non-existent input 'B/C/D'
#
# Note that for `B` to resolve its follow for `D`, it needs `C/D`, for which it needs to resolve the follow on `C` first.
flakeFollowsOverloadA="$TEST_ROOT/follows/overload/flakeA"
flakeFollowsOverloadB="$TEST_ROOT/follows/overload/flakeA/flakeB"
flakeFollowsOverloadC="$TEST_ROOT/follows/overload/flakeA/flakeB/flakeC"
flakeFollowsOverloadD="$TEST_ROOT/follows/overload/flakeA/flakeB/flakeC/flakeD"

# Test following path flakerefs.
jjFlakeDir "$flakeFollowsOverloadA"
mkdir -p "$flakeFollowsOverloadB"
mkdir -p "$flakeFollowsOverloadC"
mkdir -p "$flakeFollowsOverloadD"

cat > "$flakeFollowsOverloadD/flake.nix" <<EOF
{
    description = "Flake D";
    inputs = {};
    outputs = { ... }: {};
}
EOF

cat > "$flakeFollowsOverloadC/flake.nix" <<EOF
{
    description = "Flake C";
    inputs.D.url = "path:./flakeD";
    outputs = { ... }: {};
}
EOF

cat > "$flakeFollowsOverloadB/flake.nix" <<EOF
{
    description = "Flake B";
    inputs = {
        C = {
            url = "path:./flakeC";
        };
        D.follows = "C/D";
    };
    outputs = { ... }: {};
}
EOF

# input B/D should be able to be found...
cat > "$flakeFollowsOverloadA/flake.nix" <<EOF
{
    description = "Flake A";
    inputs = {
        B = {
            url = "path:./flakeB";
            inputs.C.follows = "C";
        };
        C.url = "path:./flakeB/flakeC";
    };
    outputs = { ... }: {};
}
EOF

nix flake metadata "$flakeFollowsOverloadA"
nix flake update --flake "$flakeFollowsOverloadA"
nix flake lock "$flakeFollowsOverloadA"

# Now test follow cycle detection
# We construct the following follows graph:
#
#    foo
#    / ^
#   /   \
#  v     \
# bar -> baz
# The message was
#     error: follow cycle detected: [baz -> foo -> bar -> baz]
flakeFollowCycle="$TEST_ROOT/follows/followCycle"

# Test following path flakerefs.
jjFlakeDir "$flakeFollowCycle"

cat > "$flakeFollowCycle"/flake.nix <<EOF
{
    description = "Flake A";
    inputs = {
        foo.follows = "bar";
        bar.follows = "baz";
        baz.follows = "foo";
    };
    outputs = { ... }: {};
}
EOF

# shellcheck disable=SC2015
checkRes=$(nix flake lock "$flakeFollowCycle" 2>&1 && fail "nix flake lock should have failed." || true)
echo "$checkRes" | grep -F "error: follow cycle detected: [baz -> foo -> bar -> baz]"


# Test transitive input url locking
# This tests the following lockfile issue: https://github.com/NixOS/nix/issues/9143
#
# We construct the following graph, where p->q means p has input q.
#
# A -> B -> C
#
# And override B/C to flake D, first in A's flake.nix and then with --override-input.
#
# A -> B -> D
flakeFollowsCustomUrlA="$TEST_ROOT/follows/custom-url/flakeA"
flakeFollowsCustomUrlB="$TEST_ROOT/follows/custom-url/flakeA/flakeB"
flakeFollowsCustomUrlC="$TEST_ROOT/follows/custom-url/flakeA/flakeB/flakeC"
flakeFollowsCustomUrlD="$TEST_ROOT/follows/custom-url/flakeA/flakeB/flakeD"


jjFlakeDir "$flakeFollowsCustomUrlA"
mkdir -p "$flakeFollowsCustomUrlB"
mkdir -p "$flakeFollowsCustomUrlC"
mkdir -p "$flakeFollowsCustomUrlD"

cat > "$flakeFollowsCustomUrlD/flake.nix" <<EOF
{
    description = "Flake D";
    inputs = {};
    outputs = { ... }: {};
}
EOF

cat > "$flakeFollowsCustomUrlC/flake.nix" <<EOF
{
    description = "Flake C";
    inputs = {};
    outputs = { ... }: {};
}
EOF

cat > "$flakeFollowsCustomUrlB/flake.nix" <<EOF
{
    description = "Flake B";
    inputs = {
        C = {
            url = "path:./flakeC";
        };
    };
    outputs = { ... }: {};
}
EOF

cat > "$flakeFollowsCustomUrlA/flake.nix" <<EOF
{
    description = "Flake A";
    inputs = {
        B = {
            url = "path:./flakeB";
            inputs.C.url = "path:./flakeB/flakeD";
        };
    };
    outputs = { ... }: {};
}
EOF

# lock "original" entry should contain overridden url
json=$(nix flake metadata "$flakeFollowsCustomUrlA" --json)
[[ $(echo "$json" | jq -r .locks.nodes.C.original.path) = './flakeB/flakeD' ]]
rm "$flakeFollowsCustomUrlA"/flake.lock

# if override-input is specified, lock "original" entry should contain original url
json=$(nix flake metadata "$flakeFollowsCustomUrlA" --override-input B/C "$flakeFollowsCustomUrlD" --json)
echo "$json" | jq .locks.nodes.C.original
[[ $(echo "$json" | jq -r .locks.nodes.C.original.path) = './flakeC' ]]

# Test deep overrides, e.g. `inputs.B.inputs.C.inputs.D.follows = ...`.

cat <<EOF > "$flakeFollowsD"/flake.nix
{ outputs = _: {}; }
EOF
cat <<EOF > "$flakeFollowsC"/flake.nix
{
  inputs.D.url = "path:nosuchflake";
  outputs = _: {};
}
EOF
cat <<EOF > "$flakeFollowsB"/flake.nix
{
  inputs.C.url = "$urlFollowsC";
  outputs = _: {};
}
EOF
cat <<EOF > "$flakeFollowsA"/flake.nix
{
  inputs.B.url = "$urlFollowsB";
  inputs.D.url = "$urlFollowsD";
  inputs.B.inputs.C.inputs.D.follows = "D";
  outputs = _: {};
}
EOF

nix flake lock "$flakeFollowsA"

[[ $(jq -c .nodes.C.inputs.D "$flakeFollowsA"/flake.lock) = '["D"]' ]]

# Test overlapping flake follows: B has D follow C/D, while A has B/C follow C

cat <<EOF > "$flakeFollowsC"/flake.nix
{
  inputs.D.url = "$urlFollowsD";
  outputs = _: {};
}
EOF
cat <<EOF > "$flakeFollowsB"/flake.nix
{
  inputs.C.url = "path:nosuchflake";
  inputs.D.follows = "C/D";
  outputs = _: {};
}
EOF
cat <<EOF > "$flakeFollowsA"/flake.nix
{
  inputs.B.url = "$urlFollowsB";
  inputs.C.url = "$urlFollowsC";
  inputs.B.inputs.C.follows = "C";
  outputs = _: {};
}
EOF

# bug was not triggered without recreating the lockfile
nix flake lock "$flakeFollowsA" --recreate-lock-file

[[ $(jq -c .nodes.B.inputs.D "$flakeFollowsA"/flake.lock) = '["B","C","D"]' ]]

# Check that you can't have both a flakeref and a follows attribute on an input.
cat <<EOF > "$flakeFollowsB"/flake.nix
{
  inputs.C.url = "path:nosuchflake";
  inputs.D.url = "path:nosuchflake";
  inputs.D.follows = "C/D";
  outputs = _: {};
}
EOF

expectStderr 1 nix flake lock "$flakeFollowsA" --recreate-lock-file | grepQuiet "flake input has both a flake reference and a follows attribute"
