#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh

work=$TEST_ROOT/rust-eval-command-edit
mkdir -p "$work/first" "$work/second"
cat > "$work/first/package.nix" <<'NIX'
let
  attrs = { name = "source-position-marker"; };
  pos = builtins.unsafeGetAttrPos "name" attrs;
in {
  selected = {
    meta.position = "${pos.file}:${toString pos.line}";
    drvPath = throw "editing must not evaluate derivation paths";
  };
  missing = {};
  malformed.meta.position = "/package.nix:4294967296";
  zero.meta.position = "/package.nix:0";
  failed.meta.position = throw "selected metadata failure";
  unused = throw "editing must not evaluate other packages";
}
NIX
cp "$work/first/package.nix" "$work/second/package.nix"

# The editor observes the exact line argument and requires a readable source.
# Including vim in its name enables the normal editorFor line-number argument.
cat > "$work/editor-vim" <<EOF_EDITOR
#!$bash
set -eu
printf '%s\\n' "\$@"
for sourceFile; do :; done
test -r "\$sourceFile"
if test -n "\${EDIT_EXPECT_FILE-}"; then
    test "\$sourceFile" = "\$EDIT_EXPECT_FILE"
fi
grep -F 'source-position-marker' "\$sourceFile"
if test -n "\${EDIT_APPEND-}"; then
    test -w "\$sourceFile"
    printf '%s\\n' "\$EDIT_APPEND" >> "\$sourceFile"
fi
EOF_EDITOR
chmod +x "$work/editor-vim"
export EDITOR="$work/editor-vim"
editConfig=$(rustCachedArm edit)

editArm() {
    local label=$1
    shift
    NIX_CONFIG="$editConfig" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$label.json" \
        nix edit "$@" > "$work/$label.out" 2> "$work/$label.err" || armFailed "$label" "$work/$label.err"
    assertRustServed "$work/$label.json"
    grep -Fx -- '+2' "$work/$label.out"
    grep -F 'source-position-marker' "$work/$label.out"
}

# Identical source bytes at different paths must keep their own real origins.
editArm first --file "$work/first/package.nix" selected
editArm first-warm --file "$work/first/package.nix" selected
editArm second --file "$work/second/package.nix" selected
editArm first-again --file "$work/first/package.nix" selected
assertMemoMissed "$work/first.json"
assertMemoServed "$work/first-warm.json"
assertMemoMissed "$work/second.json"
assertMemoServed "$work/first-again.json"
grep -Fx "$work/first/package.nix" "$work/first.out"
grep -Fx "$work/second/package.nix" "$work/second.out"
cmp "$work/first.out" "$work/first-again.out"

# Raw expressions use the same metadata question without inventing a source
# origin for the expression itself. Distinct selections have distinct answers.
expression="{ first.meta.position = \"$work/first/package.nix:2\"; second.meta.position = \"$work/second/package.nix:2\"; }"
editArm expr-first --expr "$expression" first
editArm expr-second --expr "$expression" second
editArm expr-first-warm --expr "$expression" first
assertMemoMissed "$work/expr-first.json"
assertMemoMissed "$work/expr-second.json"
assertMemoServed "$work/expr-first-warm.json"
grep -Fx "$work/first/package.nix" "$work/expr-first.out"
grep -Fx "$work/second/package.nix" "$work/expr-second.out"

# Opening an editor is a host action after the metadata question. It must not
# grant the evaluator permission to read an unrelated ambient file itself.
readExpression="{ selected.meta.position = builtins.readFile \"$work/first/package.nix\"; }"
expect 1 env NIX_CONFIG="$editConfig" nix edit --expr "$readExpression" selected \
    > "$work/pure-read.out" 2> "$work/pure-read.err"
[[ ! -s "$work/pure-read.out" ]]
grep -F 'forbidden in pure evaluation mode' "$work/pure-read.err"

for attribute in missing malformed zero failed; do
    expect 1 nix edit --file "$work/first/package.nix" "$attribute" > "$work/$attribute.out" 2> "$work/$attribute.err"
    [[ ! -s "$work/$attribute.out" ]]
done
grep -F 'missing' "$work/missing.err"
grep -F 'invalid meta.position' "$work/malformed.err"
grep -F 'invalid meta.position' "$work/zero.err"
grep -F 'selected metadata failure' "$work/failed.err"

# A flake answer names immutable source files. Editor targets must instead
# be the exact local checkout paths on both cold and warm runs.
jjFlakeDir "$work/flake"
cp "$work/first/package.nix" "$work/flake/one.nix"
cp "$work/first/package.nix" "$work/flake/two.nix"
cat > "$work/flake/flake.nix" <<EOF_FLAKE
{
  outputs = _: {
    packages.$system = {
      selected = (import ./one.nix).selected;
      other = (import ./two.nix).selected;
      broken = {};
    };
    legacyPackages.$system.broken = (import ./two.nix).selected;
  };
}
EOF_FLAKE
editArm flake-first "$work/flake#selected"
editArm flake-first-warm "$work/flake#selected"
editArm flake-other "$work/flake#other"
assertMemoServed "$work/flake-first-warm.json"
[[ "$(sed -n '2p' "$work/flake-first.out")" = "$work/flake/one.nix" ]]
[[ "$(sed -n '2p' "$work/flake-other.out")" = "$work/flake/two.nix" ]]
cmp "$work/flake-first.out" "$work/flake-first-warm.out"

# Existing packages with missing metadata cannot fall through to another root.
expect 1 nix edit "$work/flake#broken" > "$work/broken.out" 2> "$work/broken.err"
[[ ! -s "$work/broken.out" ]]
grep -F 'missing' "$work/broken.err"

# A real edit must land in the writable checkout, not its store copy.
EDIT_EXPECT_FILE="$work/flake/one.nix" EDIT_APPEND='# edited-local-checkout' \
    editArm flake-write "$work/flake#selected"
grep -Fx '# edited-local-checkout' "$work/flake/one.nix"

# Subflakes retain the workspace input root. Append the source suffix once;
# neither dropping nor repeating the subdirectory identifies the same file.
mkdir "$work/flake/subflake"
cp "$work/first/package.nix" "$work/flake/subflake/one.nix"
cat > "$work/flake/subflake/flake.nix" <<EOF_SUBFLAKE
{
  outputs = _: { packages.$system.selected = (import ./one.nix).selected; };
}
EOF_SUBFLAKE
EDIT_EXPECT_FILE="$work/flake/subflake/one.nix" EDIT_APPEND='# edited-subflake' \
    editArm subflake-write "$work/flake/subflake#selected"
grep -Fx '# edited-subflake' "$work/flake/subflake/one.nix"

# Same committed source in two checkouts shares the immutable location memo.
# The second invocation must choose its own checkout after receiving that row.
requireGit
createGitRepo "$work/checkout-one" ""
cp "$work/first/package.nix" "$work/checkout-one/one.nix"
cp "$work/flake/subflake/flake.nix" "$work/checkout-one/flake.nix"
git -C "$work/checkout-one" add flake.nix one.nix
git -C "$work/checkout-one" commit -m 'edit source'
git clone "$work/checkout-one" "$work/checkout-two"
[[ "$(git -C "$work/checkout-one" rev-parse HEAD)" = "$(git -C "$work/checkout-two" rev-parse HEAD)" ]]
editConfig=$(rustCachedArm edit-checkouts)
editArm checkout-one "git+file://$work/checkout-one#selected"
editArm checkout-one-warm "git+file://$work/checkout-one#selected"
EDIT_EXPECT_FILE="$work/checkout-two/one.nix" EDIT_APPEND='# edited-second-checkout' \
    editArm checkout-two "git+file://$work/checkout-two#selected"
assertMemoServed "$work/checkout-two.json"
# Every Rust session in this command was served, including the selected
# position question. Warming only the flake document cannot pass this check.
jq -e '.evaluatorCalls.rust >= 2 and .rustEvalPerf.memo_served == .evaluatorCalls.rust and .rustEvalPerf.compiles == 0' \
    "$work/checkout-two.json"
[[ "$(sed -n '2p' "$work/checkout-one.out")" = "$work/checkout-one/one.nix" ]]
[[ "$(sed -n '2p' "$work/checkout-two.out")" = "$work/checkout-two/one.nix" ]]
grep -Fx '# edited-second-checkout' "$work/checkout-two/one.nix"
grepQuietInverse -F '# edited-second-checkout' "$work/checkout-one/one.nix"

# Explicit old revisions and branch refs describe historical source, even
# when their local repository has a different current checkout layout.
oldRevision=$(git -C "$work/checkout-one" rev-parse HEAD)
git -C "$work/checkout-one" branch historical "$oldRevision"
cp "$work/checkout-one/one.nix" "$work/historical-source.nix"
{ printf '# current checkout layout\n\n\n'; cat "$work/historical-source.nix"; } > "$work/checkout-one/one.nix"
git -C "$work/checkout-one" add one.nix
git -C "$work/checkout-one" commit -m 'move source lines'
cp "$work/checkout-one/one.nix" "$work/current-source.nix"
for pin in "rev=$oldRevision" 'ref=historical'; do
    label="historical-${pin%%=*}"
    editArm "$label" "git+file://$work/checkout-one?$pin#selected"
    historicalFile=$(sed -n '2p' "$work/$label.out")
    [[ "$historicalFile" = "$NIX_STORE_DIR/"*/one.nix ]]
    [[ "$historicalFile" != "$work/checkout-one/one.nix" ]]
    cmp "$work/historical-source.nix" "$historicalFile"
    cmp "$work/current-source.nix" "$work/checkout-one/one.nix"
done

# Dependency inputs have their own source roots and must never be relabelled
# as files in the root checkout, even when filenames match.
jjFlakeDir "$work/dependency"
cp "$work/first/package.nix" "$work/dependency/one.nix"
cp "$work/flake/subflake/flake.nix" "$work/dependency/flake.nix"
jjFlakeDir "$work/dependent"
cp "$work/first/package.nix" "$work/dependent/one.nix"
cat > "$work/dependent/flake.nix" <<EOF_DEPENDENT
{
  inputs.dependency.url = "jj+file://$work/dependency";
  outputs = { dependency, ... }: {
    packages.$system.selected = dependency.packages.$system.selected;
  };
}
EOF_DEPENDENT
editArm dependency "$work/dependent#selected"
dependencyFile=$(sed -n '2p' "$work/dependency.out")
[[ "$dependencyFile" = "$NIX_STORE_DIR/"*/one.nix ]]
[[ "$dependencyFile" != "$work/dependent/one.nix" ]]
[[ "$dependencyFile" != "$work/dependency/one.nix" ]]
