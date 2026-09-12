#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh

work=$TEST_ROOT/rust-eval-flake-document
rm -rf "$work"
mkdir -p "$work"

# The input every fixture points at, and its own input. `dotted` takes dep
# with `flake = false` (a Boolean attribute; dep's flake.nix is never read);
# `dynamic` takes it as a flake, so dep's flake.nix is read as a document too
# and its `other` input is followed. Both `outputs` throw: a lock-only command
# that reached either would be evaluating outputs, which no document read does.
jjFlakeDir "$work/other"
cat > "$work/other/flake.nix" <<EOF
{ outputs = args: throw "other outputs forced by a document read"; }
EOF
jjFlakeDir "$work/dep"
cat > "$work/dep/flake.nix" <<EOF
{
  inputs.other.url = "jj+file://$work/other";
  outputs = { self, other }: throw "dep outputs forced by a document read";
}
EOF

fixture() { # NAME <<flake.nix
    jjFlakeDir "$work/$1"
    cat > "$work/$1/flake.nix"
}

metadata() { # ARM NAME -> $work/ARM-NAME.{out,err,json}; the test dies if the arm fails
    local arm=$1 name=$2
    NIX_CONFIG="$rustArm" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$arm-$name.json" \
        nix flake metadata --json "$work/$name" > "$work/$arm-$name.out" 2> "$work/$arm-$name.err" \
        || armFailed "$arm-$name" "$work/$arm-$name.err"
    grepQuietInverse -F 'rust-eval unimplemented' "$work/$arm-$name.err"
}

checkDocument() { # NAME DESCRIPTION
    local name=$1 description=$2
    metadata rust "$name"
    assertRustServed "$work/rust-$name.json"
    jq -e --arg description "$description" '.description == $description and .locks.nodes.dep.locked.type == "jj"' \
        < "$work/rust-$name.out" > /dev/null
}

# Invalid declarations fail before fetch or outputs evaluation.
refuse() { # NAME PHRASE
    local name=$1 phrase=$2 arm
    for arm in rust; do
        expectStderr 1 env NIX_CONFIG="$rustArm" nix flake metadata --json "$work/$name" \
            | grepQuiet -F -- "$phrase" || {
            echo "$name: the $arm arm did not refuse with '$phrase'" >&2
            exit 1
        }
    done
}

# 1. Dotted implicit sets (`inputs.dep.url` is `inputs = { dep = { url = ..; }; }`
#    to the parser and an assembled set to the compiler), a Boolean input
#    attribute, and nixConfig in every scalar kind plus a list.
fixture dotted <<EOF
{
  description = "dotted sets and nixConfig";
  inputs.dep.url = "jj+file://$work/dep";
  inputs.dep.flake = (false);
  nixConfig.extra-substituters = [ "https://example.invalid/cache" ];
  nixConfig.max-jobs = 4;
  nixConfig.allow-import-from-derivation = true;
  nixConfig.bash-prompt = "flake> ";
  outputs = { self, dep }: throw "outputs forced by a document read";
}
EOF
checkDocument dotted "dotted sets and nixConfig"

# 2. A computed name at the root, an
#    explicit nested set with a nested `inputs` (a follows), an input that is
#    a flake and so is read as a document itself, and a path in nixConfig,
#    which the host copies to the store after Rust validates it.
fixture dynamic <<EOF
{
  \${"descr" + "iption"} = "a dynamic name at the root";
  inputs = {
    dep = {
      url = "jj+file://$work/dep";
      inputs.other.follows = "dep";
    };
  };
  nixConfig.flake-registry = ./registry.json;
  outputs = { self, dep, ... }: throw "outputs forced by a document read";
}
EOF
echo '{ "version": 2, "flakes": [] }' > "$work/dynamic/registry.json"
checkDocument dynamic "a dynamic name at the root"

# A symlinked document evaluates relative paths beside its target, while
# document paths remain relative to the flake directory. The dynamic name
# reads through the mounted input, including the first document read.
fixture rooted <<EOF
{
  \${builtins.readFile ./name} = "rooted document";
  inputs.dep = rec { url = "jj+file://$work/dep"; flake = (false); };
  nixConfig.flake-registry = ./registry.json;
  outputs = { self, dep }: throw "outputs forced by a document read";
}
EOF
mkdir "$work/rooted/config"
mv "$work/rooted/flake.nix" "$work/rooted/config/definition.nix"
ln -s config/definition.nix "$work/rooted/flake.nix"
printf description > "$work/rooted/config/name"
echo '{ "version": 2, "flakes": [] }' > "$work/rooted/config/registry.json"
checkDocument rooted "rooted document"

fixture unsupported <<EOF
{ foo = 1; outputs = args: throw "no"; }
EOF
refuse unsupported "unsupported attribute 'foo'"
fixture outputs-not-a-function <<EOF
{ outputs = 1; }
EOF
refuse outputs-not-a-function "expected a function but got an integer"
fixture no-outputs <<EOF
{ description = "no outputs"; }
EOF
refuse no-outputs "lacks attribute 'outputs'"
fixture float-url <<EOF
{ inputs.a.url = 1.5; outputs = args: throw "no"; }
EOF
refuse float-url "a float"

fixture thunk-description <<EOF
{ description = builtins.readFile ./missing; outputs = args: throw "no"; }
EOF
refuse thunk-description "missing"
fixture thunk-url <<EOF
{ inputs.a.url = builtins.readFile ./missing; outputs = args: throw "no"; }
EOF
refuse thunk-url "missing"

fixture computed <<EOF
let description = builtins.readFile ./description; in {
  inherit description;
  inputs.dep.url = "jj+file://$work/dep";
  outputs = args: throw "outputs forced";
}
EOF
printf 'computed metadata' > "$work/computed/description"
checkDocument computed "computed metadata"

# Declaration policy lives in Rust and is exercised through the production CLI.
fixture ambiguous-reference <<EOF
{ inputs.a = { url = "jj+file://$work/dep"; follows = "dep"; }; outputs = args: {}; }
EOF
refuse ambiguous-reference "both a flake reference and a follows attribute"
fixture invalid-follows <<EOF
{ inputs.a.follows = "dep//other"; outputs = args: {}; }
EOF
refuse invalid-follows "invalid follows path component"
fixture nested-self <<EOF
{ inputs.dep.inputs.self.lfs = true; outputs = args: {}; }
EOF
refuse nested-self "'self' input attribute not allowed"
fixture negative-input-integer <<EOF
{ inputs.dep = { type = "git"; revCount = -1; }; outputs = args: {}; }
EOF
refuse negative-input-integer "negative value given for flake input attribute"

# Existing corrupt lock data must not be interpreted as a request to resolve
# new pins. The graph parser distinguishes an absent file from an empty one.
fixture empty-lock <<EOF
{ outputs = args: {}; }
EOF
: > "$work/empty-lock/flake.lock"
refuse empty-lock "invalid lock JSON"

fixture cyclic-lock-follows <<EOF
{ outputs = args: {}; }
EOF
cat > "$work/cyclic-lock-follows/flake.lock" <<EOF
{ "version": 7, "root": "root", "nodes": { "root": { "inputs": { "a": ["b"], "b": ["a"] } } } }
EOF
refuse cyclic-lock-follows "follow cycle detected"

# Prefetch uses the mounted source identities already chosen by evaluation.
# A relative subtree can have its own jj tree object; a relative file can be
# a path inside another object's store root. Neither is a standalone path
# fetch request. The follows alias must resolve to the same child identity.
jjFlakeDir "$work/prefetch-parent"
mkdir "$work/prefetch-parent/child"
cat > "$work/prefetch-parent/flake.nix" <<'NIX'
{
  inputs.child.url = "path:./child";
  inputs.blob = { url = "path:./blob.txt"; flake = false; };
  outputs = { self, child, blob }: {
    expected = {
      parent = builtins.unsafeDiscardStringContext self.outPath;
      child = builtins.unsafeDiscardStringContext child.outPath;
      file = builtins.unsafeDiscardStringContext blob.outPath;
    };
  };
}
NIX
cat > "$work/prefetch-parent/child/flake.nix" <<'NIX'
{ outputs = args: {}; }
NIX
printf 'relative jj subtree payload\n' > "$work/prefetch-parent/child/payload.txt"
printf 'relative jj file payload\n' > "$work/prefetch-parent/blob.txt"
fixture prefetch-relative <<EOF
{
  inputs.parent.url = "jj+file://$work/prefetch-parent";
  inputs.alias.follows = "parent/child";
  outputs = { self, parent, alias }: {
    expected = parent.expected // {
      root = builtins.unsafeDiscardStringContext self.outPath;
      alias = builtins.unsafeDiscardStringContext alias.outPath;
    };
  };
}
EOF

# Avoid lock writes, which would change the jj tree between the measurement
# and prefetch. Discarding string context prevents the measurement from
# requesting materialization; the absence checks below verify that control.
NIX_CONFIG="$rustArm" nix flake metadata --builders '' --no-write-lock-file --json \
    "$work/prefetch-relative" > "$work/prefetch-relative-metadata.json" \
    2> "$work/prefetch-relative-metadata.err" \
    || armFailed prefetch-relative-metadata "$work/prefetch-relative-metadata.err"
jq -e '
    .locks as $lock |
    $lock.nodes[$lock.root] as $root |
    $lock.nodes[$root.inputs.parent] as $parent |
    $parent.locked.type == "jj" and
    $lock.nodes[$parent.inputs.child].locked.type == "path" and
    $lock.nodes[$parent.inputs.child].parent == ["parent"] and
    $lock.nodes[$parent.inputs.blob].flake == false and
    $lock.nodes[$parent.inputs.blob].parent == ["parent"] and
    $root.inputs.alias == ["parent", "child"]
' "$work/prefetch-relative-metadata.json" > /dev/null
NIX_CONFIG="$rustArm" nix eval --builders '' --no-write-lock-file --json \
    "$work/prefetch-relative#expected" > "$work/prefetch-relative-expected.json" \
    2> "$work/prefetch-relative-eval.err" \
    || armFailed prefetch-relative-eval "$work/prefetch-relative-eval.err"
jq -e --arg store "$NIX_STORE_DIR/" '
    (.child == .alias) and (length == 5) and all(.[]; startswith($store))
' "$work/prefetch-relative-expected.json" > /dev/null

# Check containing store roots, not merely an absent descendant. Otherwise
# an already copied parent could make this test pass without any prefetch.
jq -r '.[]' "$work/prefetch-relative-expected.json" > "$work/prefetch-relative-paths"
while IFS= read -r expectedPath; do
    relativePath=${expectedPath#"$NIX_STORE_DIR/"}
    printf '%s/%s\n' "$NIX_STORE_DIR" "${relativePath%%/*}"
done < "$work/prefetch-relative-paths" | sort -u > "$work/prefetch-relative-store-roots"
while IFS= read -r expectedRoot; do
    [[ ! -e "$expectedRoot" ]] || fail "measurement already materialized $expectedRoot"
done < "$work/prefetch-relative-store-roots"

NIX_CONFIG="$rustArm" nix flake prefetch-inputs --builders '' --no-write-lock-file \
    "$work/prefetch-relative" > "$work/prefetch-relative.out" \
    2> "$work/prefetch-relative.err" \
    || armFailed prefetch-relative "$work/prefetch-relative.err"
while IFS= read -r expectedRoot; do
    [[ -e "$expectedRoot" ]] || fail "prefetch did not materialize expected identity $expectedRoot"
    nix-store --builders '' --check-validity "$expectedRoot"
done < "$work/prefetch-relative-store-roots"
parentPath=$(jq -r '.parent' "$work/prefetch-relative-expected.json")
childPath=$(jq -r '.child' "$work/prefetch-relative-expected.json")
filePath=$(jq -r '.file' "$work/prefetch-relative-expected.json")
rootPath=$(jq -r '.root' "$work/prefetch-relative-expected.json")
cmp "$work/prefetch-relative/flake.nix" "$rootPath/flake.nix"
cmp "$work/prefetch-parent/flake.nix" "$parentPath/flake.nix"
cmp "$work/prefetch-parent/child/flake.nix" "$childPath/flake.nix"
cmp "$work/prefetch-parent/child/payload.txt" "$childPath/payload.txt"
cmp "$work/prefetch-parent/blob.txt" "$filePath"

# A pinned absolute path inside another input's store object needs that
# parent copied before the child fetch starts. Unlike a relative input, the
# path fetcher cannot read this child through the evaluator's mount table.
jjFlakeDir "$work/prefetch-absolute-parent"
mkdir "$work/prefetch-absolute-parent/child"
cat > "$work/prefetch-absolute-parent/flake.nix" <<'NIX'
{
  inputs.child.url = "path:" + builtins.toString ./child;
  outputs = { self, child }: {
    expected = {
      parent = builtins.unsafeDiscardStringContext self.outPath;
      child = builtins.unsafeDiscardStringContext child.outPath;
    };
  };
}
NIX
cat > "$work/prefetch-absolute-parent/child/flake.nix" <<'NIX'
{ outputs = args: {}; }
NIX
printf 'absolute prefetch parent owned by this fixture\n' \
    > "$work/prefetch-absolute-parent/owner.txt"
printf 'absolute prefetch child owned by this fixture\n' \
    > "$work/prefetch-absolute-parent/child/owner.txt"

# Read the parent as data for bootstrap: its absolute child cannot be
# locked until this parent object exists in the isolated test store.
fixture prefetch-absolute-bootstrap <<EOF
{
  inputs.parent = { url = "jj+file://$work/prefetch-absolute-parent"; flake = false; };
  outputs = args: {};
}
EOF
NIX_CONFIG="$rustArm" nix flake prefetch-inputs --builders '' --no-write-lock-file \
    "$work/prefetch-absolute-bootstrap" > "$work/prefetch-absolute-bootstrap.out" \
    2> "$work/prefetch-absolute-bootstrap.err" \
    || armFailed prefetch-absolute-bootstrap "$work/prefetch-absolute-bootstrap.err"
fixture prefetch-absolute <<EOF
{
  inputs.parent.url = "jj+file://$work/prefetch-absolute-parent";
  outputs = { self, parent }: { inherit (parent) expected; };
}
EOF
NIX_CONFIG="$rustArm" nix flake lock --builders '' "$work/prefetch-absolute" \
    > "$work/prefetch-absolute-lock.out" 2> "$work/prefetch-absolute-lock.err" \
    || armFailed prefetch-absolute-lock "$work/prefetch-absolute-lock.err"
cp "$work/prefetch-absolute/flake.lock" "$work/prefetch-absolute-lock-before.json"
NIX_CONFIG="$rustArm" nix eval --builders '' --no-update-lock-file --json \
    "$work/prefetch-absolute#expected" > "$work/prefetch-absolute-expected.json" \
    2> "$work/prefetch-absolute-eval.err" \
    || armFailed prefetch-absolute-eval "$work/prefetch-absolute-eval.err"
absoluteParent=$(jq -r '.parent' "$work/prefetch-absolute-expected.json")
absoluteChild=$(jq -r '.child' "$work/prefetch-absolute-expected.json")
jq -e --arg parent "$absoluteParent" '
    .nodes[.root].inputs.parent as $parentId |
    .nodes[$parentId] as $parentNode |
    .nodes[$parentNode.inputs.child] as $childNode |
    $parentNode.locked.type == "jj" and
    $childNode.locked.type == "path" and
    $childNode.locked.path == ($parent + "/child") and
    ($childNode | has("parent") | not)
' "$work/prefetch-absolute/flake.lock" > /dev/null
NIX_CONFIG="$rustArm" nix flake prefetch-inputs --builders '' --no-update-lock-file \
    "$work/prefetch-absolute" > "$work/prefetch-absolute-warm.out" \
    2> "$work/prefetch-absolute-warm.err" \
    || armFailed prefetch-absolute-warm "$work/prefetch-absolute-warm.err"

# Only these two fixture objects are removed. Require full store roots,
# distinct identities, and our own marker bytes before performing deletion.
[[ "$absoluteParent" != "$absoluteChild" ]] || fail "absolute child reused parent identity"
for ownedPath in "$absoluteParent" "$absoluteChild"; do
    [[ "$ownedPath" == "$NIX_STORE_DIR/"* ]] || fail "fixture path outside test store: $ownedPath"
    ownedName=${ownedPath#"$NIX_STORE_DIR/"}
    [[ -n "$ownedName" && "$ownedName" != */* ]] || fail "fixture path is not a store root: $ownedPath"
    nix-store --builders '' --check-validity "$ownedPath"
done
cmp "$work/prefetch-absolute-parent/owner.txt" "$absoluteParent/owner.txt"
cmp "$work/prefetch-absolute-parent/child/owner.txt" "$absoluteChild/owner.txt"
nix-store --builders '' --delete "$absoluteChild" "$absoluteParent"
[[ ! -e "$absoluteParent" && ! -e "$absoluteChild" ]] \
    || fail "absolute fixture was not cold after deletion"

NIX_CONFIG="$rustArm" nix flake prefetch-inputs --builders '' --no-update-lock-file \
    "$work/prefetch-absolute" > "$work/prefetch-absolute-cold.out" \
    2> "$work/prefetch-absolute-cold.err" \
    || armFailed prefetch-absolute-cold "$work/prefetch-absolute-cold.err"
for ownedPath in "$absoluteParent" "$absoluteChild"; do
    [[ -e "$ownedPath" ]] || fail "cold prefetch did not restore $ownedPath"
    nix-store --builders '' --check-validity "$ownedPath"
done
cmp "$work/prefetch-absolute-parent/flake.nix" "$absoluteParent/flake.nix"
cmp "$work/prefetch-absolute-parent/owner.txt" "$absoluteParent/owner.txt"
cmp "$work/prefetch-absolute-parent/child/flake.nix" "$absoluteChild/flake.nix"
cmp "$work/prefetch-absolute-parent/child/owner.txt" "$absoluteChild/owner.txt"
cmp "$work/prefetch-absolute-lock-before.json" "$work/prefetch-absolute/flake.lock"
