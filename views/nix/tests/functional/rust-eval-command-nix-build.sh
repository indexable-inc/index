#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh


work=$TEST_ROOT/rust-eval-command-nix-build
rm -rf "$work"
mkdir -p "$work"
cat > "$work/default.nix" <<EOF
let
  mk = name: derivation {
    inherit name;
    system = builtins.currentSystem;
    builder = "$bash";
    PATH = "$coreutils";
    args = [ "-c" "echo \$name > \$out" ];
  };
in {
  one = mk "one";
  nested = { recurseForDerivations = true; two = mk "two"; skipped = { three = mk "three"; }; };
  list = [ (mk "four") { recurseForDerivations = true; five = mk "five"; } ];
  scalar = 1;
}
EOF

runArm() { # CONFIG LABEL ARGS...
    local config=$1 label=$2
    shift 2
    NIX_CONFIG="$config" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$label.json" \
        nix-build --dry-run --no-out-link "$@" > "$work/$label.out" 2> "$work/$label.err" \
        || armFailed "$label" "$work/$label.err"
}
runArm "$rustArm" rust "$work/default.nix"
assertRustServed "$work/rust.json"
grepQuietInverse -F 'rust-eval refusal' "$work/rust.err"
# Every derivation the walk reaches from the root set: one, then nested.two
# (skipped carries no flag). A list attribute is not descended into --
# getDerivations walks lists only as the value itself -- and neither is
# a scalar.
for name in one two; do
    grepQuiet -F -- "-$name.drv" "$work/rust.err"
done
for name in three four five; do
    grepQuietInverse -F -- "-$name.drv" "$work/rust.err"
done

# The list as the value: its derivations, and the set inside it that asks to
# be descended into.
runArm "$rustArm" rust-list "$work/default.nix" -A list
for name in four five; do
    grepQuiet -F -- "-$name.drv" "$work/rust-list.err"
done

# An attribute path, and an expression: the two spellings home-manager's
# activation and ordinary scripts use.
runArm "$rustArm" rust-attr "$work/default.nix" -A nested
grepQuiet -F -- "-two.drv" "$work/rust-attr.err"

runArm "$rustArm" rust-empty --expr '{}'
[[ ! -s "$work/rust-empty.out" ]]

NIX_CONFIG="$rustArm" nix-build --no-out-link "$work/default.nix" -A one > "$work/built-rust.out"
[[ $(cat "$(cat "$work/built-rust.out")") == one ]]
[[ $(cat "$work/built-rust.out") == "$NIX_STORE_DIR/"*-one ]]

runArmExpectingFailure() { # CONFIG LABEL ARGS...
    local config=$1 label=$2
    shift 2
    if NIX_CONFIG="$config" nix-build --dry-run --no-out-link "$@" > "$work/$label.out" 2> "$work/$label.err"; then
        echo "the $label arm succeeded where a failure was owed" >&2
        exit 1
    fi
}

# Run an arm and record its stdout, stderr, and exit code without judging the
# outcome: for a claim about two runs agreeing (a memo round-trip), whether
# they both succeed or both fail is the arm's business, and the test asserts
# only that the two agree.
runArmCapture() { # CONFIG LABEL ARGS...
    local config=$1 label=$2
    shift 2
    local status=0
    NIX_CONFIG="$config" nix-build --dry-run --no-out-link "$@" > "$work/$label.out" 2> "$work/$label.err" || status=$?
    echo "$status" > "$work/$label.rc"
}
runArmExpectingFailure "$rustArm" rust-fn --expr '{ x ? 1 }: x'
grepQuiet -F 'expression does not evaluate to a derivation (or a set or list of those)' "$work/rust-fn.err"
grepQuietInverse -F 'rust-eval refusal' "$work/rust-fn.err"

# The everyday shape: a file that takes arguments as a formal with a default,
# overridden by --arg and --argstr, each of which selects what gets built.
# Fresh names (nothing above built them), so `--dry-run` still reports "will
# be built" for each -- an already-built derivation prints nothing.
cat > "$work/function.nix" <<EOF
{ which ? "alpha", suffix ? "" }:
let
  mk = name: derivation {
    inherit name;
    system = builtins.currentSystem;
    builder = "$bash";
    PATH = "$coreutils";
    args = [ "-c" "echo \$name > \$out" ];
  };
in {
  chosen = mk (which + suffix);
}
EOF
runArm "$rustArm" rust-default "$work/function.nix"
assertRustServed "$work/rust-default.json"
grepQuiet -F -- "-alpha.drv" "$work/rust-default.err"
runArm "$rustArm" rust-arg "$work/function.nix" --arg which '"beta"' --argstr suffix z
assertRustServed "$work/rust-arg.json"
grepQuiet -F -- "-betaz.drv" "$work/rust-arg.err"
grepQuietInverse -F -- "-alpha.drv" "$work/rust-arg.err"

# Three rules of getDerivations that a walk written from its summary misses:
# a derivation reached under two names is built once; a derivation whose
# name fails fails the walk; a set carrying _combineChannels is entered
# without recurseForDerivations. Fresh names throughout.
cat > "$work/rules.nix" <<EOF
let
  mk = name: derivation {
    inherit name;
    system = builtins.currentSystem;
    builder = "$bash";
    PATH = "$coreutils";
    args = [ "-c" "echo \$name > \$out" ];
  };
  d = mk "gamma";
in {
  alias = { a = d; b = d; };
  badName = mk "delta" // { name = throw "the name is forced"; };
  channels = { _combineChannels = true; group = { inner = mk "epsilon"; }; };
}
EOF
runArm "$rustArm" rust-alias "$work/rules.nix" -A alias
[[ $(grep -c -F -- "-gamma.drv" "$work/rust-alias.err") == 1 ]]
runArmExpectingFailure "$rustArm" rust-badname "$work/rules.nix" -A badName
grepQuiet -F 'the name is forced' "$work/rust-badname.err"
runArmExpectingFailure "$rustArm" rust-channels "$work/rules.nix" -A channels
combineErr='expression does not evaluate to a derivation (or a set or list of those)'
grepQuiet -F "$combineErr" "$work/rust-channels.err"

cat > "$work/multi.nix" <<EOF
builtins.trace "root evaluated" (
let
  mk = name: derivation {
    inherit name;
    system = builtins.currentSystem;
    builder = "$bash";
    PATH = "$coreutils";
    args = [ "-c" "echo \$name > \$out" ];
  };
in {
  a = mk "zeta";
  b = { c = mk "eta"; };
})
EOF
runArm "$rustArm" rust-multi "$work/multi.nix" -A a -A b.c
[[ $(grep -c -F 'trace: root evaluated' "$work/rust-multi.err") == 1 ]]
grepQuiet -F -- "-zeta.drv" "$work/rust-multi.err"
grepQuiet -F -- "-eta.drv" "$work/rust-multi.err"

# The answer's codec is injective: a derivation-shaped set whose output name
# carries a newline and a tab is one record, however it is spelled, so a
# warm run decodes exactly what the cold run filed rather than two
# derivations. Both runs fail the same way, naming the one output that does
# not exist.
realDrv=$(NIX_CONFIG="$rustArm" nix-instantiate "$work/default.nix" -A one 2> /dev/null)
cat > "$work/newline.nix" <<EOF
{
  type = "derivation";
  name = "newline";
  drvPath = "$realDrv";
  outputName = "out\n$realDrv\tout";
}
EOF
# The cold run walks and files the record; the warm run is served the filed
# bytes. A codec that split on the newline would decode a different record set
# on the warm run, so the two runs would diverge. They must not: same stdout,
# same stderr, same exit code, whatever that outcome is (the fake output name
# is rejected by some stores and ignored by others under --dry-run, and the
# claim is injectivity, not the outcome).
codecArm=$(rustCachedArm codec)
runArmCapture "$codecArm" rust-newline-cold "$work/newline.nix"
runArmCapture "$codecArm" rust-newline-warm "$work/newline.nix"
assertSame "$work/rust-newline-cold.out" "$work/rust-newline-warm.out"
assertSame "$work/rust-newline-cold.err" "$work/rust-newline-warm.err"
[[ "$(cat "$work/rust-newline-cold.rc")" == "$(cat "$work/rust-newline-warm.rc")" ]]
# The warm run really came from the memo (else "same bytes" proves nothing):
# the cold run filed the record, so a second walk is served without evaluating.
NIX_CONFIG="$codecArm" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/newline-served.json" \
    nix-build --dry-run --no-out-link "$work/newline.nix" > "$work/newline-served.out" 2>&1 || true
assertMemoServed "$work/newline-served.json"

# A warm run of an ordinary set is served from the memo with the same answer
# as the cold one; a cold run is not.
warmArm=$(rustCachedArm warm)
runArm "$warmArm" rust-cold "$work/function.nix"
runArm "$warmArm" rust-warm "$work/function.nix"
assertSame "$work/rust-cold.err" "$work/rust-warm.err"
assertMemoMissed "$work/rust-cold.json"
assertMemoServed "$work/rust-warm.json"
# The hit checked its derivations instead of writing them again; the cold run
# is the control that the counter fires.
assertDrvWrites "$work/rust-cold.json"
assertNoDrvWrites "$work/rust-warm.json"

# A .drv path is a direct build request; no expression evaluation is needed.
NIX_CONFIG="$rustArm" nix-build --no-out-link "$realDrv" > "$work/direct-drv.out"
assertSame "$work/built-rust.out" "$work/direct-drv.out"

echo "rust-eval-command-nix-build: ok"
