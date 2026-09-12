#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh


work=$TEST_ROOT/rust-eval-command-nix-instantiate
rm -rf "$work"
mkdir -p "$work"

runArm() { # CONFIG LABEL ARGS...
    local config=$1 label=$2
    shift 2
    NIX_CONFIG="$config" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$label.json" \
        nix-instantiate --eval "$@" > "$work/$label.out" 2> "$work/$label.err" \
        || armFailed "$label" "$work/$label.err"
}
runArmExpectingFailure() { # CONFIG LABEL ARGS...
    local config=$1 label=$2
    shift 2
    if NIX_CONFIG="$config" nix-instantiate --eval "$@" > "$work/$label.out" 2> "$work/$label.err"; then
        echo "the $label arm succeeded where a failure was owed" >&2
        exit 1
    fi
}
checkEvaluation() { # LABEL ARGS...
    local label=$1
    shift
    runArm "$rustArm" "rust-$label" "$@"
    assertRustServed "$work/rust-$label.json"
    grepQuietInverse -F 'rust-eval refusal' "$work/rust-$label.err"
}
checkFailure() { # LABEL EXPECTED-TEXT ARGS...
    local label=$1 expected=$2
    shift 2
    runArmExpectingFailure "$rustArm" "rust-$label" "$@"
    grepQuiet -F "$expected" "$work/rust-$label.err"
    grepQuietInverse -F 'rust-eval refusal' "$work/rust-$label.err"
}

# processExpr: the value reached is auto-called when there are arguments.
# The bug this pins: the served route printed <LAMBDA> with exit 0 here.
checkEvaluation arg --strict --expr '{ a }: a' --arg a 1
[[ $(cat "$work/rust-arg.out") == 1 ]]
checkEvaluation argstr --strict --expr '{ a }: a' --argstr a x
[[ $(cat "$work/rust-argstr.out") == '"x"' ]]
checkEvaluation uncalled --strict --expr '{ a ? 2 }: a'
[[ $(cat "$work/rust-uncalled.out") == '<LAMBDA>' ]]
# A default fills what --arg does not name; an ellipsis takes everything.
checkEvaluation default --strict --expr '{ a, b ? 10 }: a + b' --arg a 1
[[ $(cat "$work/rust-default.out") == 11 ]]
checkEvaluation ellipsis --strict --expr '{ ... } @ args: builtins.attrNames args' --arg a 1 --argstr b x
[[ $(cat "$work/rust-ellipsis.out") == '[ "a" "b" ]' ]]
# A set with a functor is applied to itself first.
checkEvaluation functor --strict --expr '{ __functor = self: { a }: a + self.k; k = 5; }' --arg a 1
[[ $(cat "$work/rust-functor.out") == 6 ]]
# An argument is a thunk: one that throws is fine as long as nothing reads
# it, and one that traces traces once however many formals take it.
checkEvaluation unused --strict --expr '{ a, b ? 0 }: b' --arg a 'throw "never forced"'
[[ $(cat "$work/rust-unused.out") == 0 ]]
checkEvaluation shared --strict --expr '{ a }: { x = ({ a }: a) { inherit a; }; y = a; }' --arg a 'builtins.trace "arg forced" 3'
[[ $(grep -c 'trace: arg forced' "$work/rust-shared.err") == 1 ]]
# The home-manager shape: a set printed with --strict from a file that takes
# two string arguments, then imported back.
cat > "$work/news.nix" <<'EOF'
{ newsJsonFile, newsReadIdsFile }:
{
  meta = { display = "notify"; numUnread = 2; ids = [ "a" "b" ]; };
  files = [ newsJsonFile newsReadIdsFile ];
}
EOF
checkEvaluation news --strict "$work/news.nix" --arg newsJsonFile '"/j"' --arg newsReadIdsFile '"/r"'
cp "$work/rust-news.out" "$work/news-value.nix"
checkEvaluation news-display --expr "(import $work/news-value.nix).meta.display"
[[ $(cat "$work/rust-news-display.out") == '"notify"' ]]

checkEvaluation path --strict --expr '{ f = { a ? 1 }: { y = a; }; }' -A f.y
[[ $(cat "$work/rust-path.out") == 1 ]]
checkEvaluation path-arg --strict --expr '{ f = { a ? 1 }: { y = a; }; }' -A f.y --arg a 7
[[ $(cat "$work/rust-path-arg.out") == 7 ]]
checkFailure path-fn "the expression selected by the selection path 'f.y' should be a set but is a function" \
    --expr '{ f = x: { y = x; }; }' -A f.y
checkFailure path-int "the expression selected by the selection path 'a.b' should be a set but is an integer" \
    --expr '{ a = 1; }' -A a.b
checkFailure missing "cannot evaluate a function that has an argument without a value ('b')" \
    --strict --expr '{ a, b }: a' --arg a 1
grepQuiet -F "passed explicitly with '--arg' or '--argstr'" "$work/rust-missing.err"
# An --arg that does not parse fails before anything is evaluated, on both
# arms (each in its own parser's words).
runArmExpectingFailure "$rustArm" rust-bad-arg --strict --expr '{ a }: a' --arg a '1 +'
grepQuietInverse -F 'rust-eval refusal' "$work/rust-bad-arg.err"

# nix eval: -f with --arg walks under the arguments; the installable's
# attribute path is auto-called per component, never the value at the end.
cat > "$work/lib.nix" <<'EOF'
{ n ? 1 }: { double = n * 2; }
EOF
[[ $(NIX_CONFIG=$rustArm NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/eval-f.json" \
    nix eval -f "$work/lib.nix" double --arg n 21) == 42 ]]
assertRustServed "$work/eval-f.json"

# The memo: the arguments are in the key, so a warm run of one command is
# served and a run with other arguments is not served the first answer.
keyArm=$(rustCachedArm key)
NIX_CONFIG="$keyArm" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/key-cold.json" \
    nix-instantiate --eval --strict --expr '{ a }: a' --arg a 1 > "$work/key-cold.out"
NIX_CONFIG="$keyArm" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/key-warm.json" \
    nix-instantiate --eval --strict --expr '{ a }: a' --arg a 1 > "$work/key-warm.out"
NIX_CONFIG="$keyArm" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/key-other.json" \
    nix-instantiate --eval --strict --expr '{ a }: a' --arg a 2 > "$work/key-other.out"
assertSame "$work/key-cold.out" "$work/key-warm.out"
assertMemoMissed "$work/key-cold.json"
assertMemoServed "$work/key-warm.json"
assertMemoMissed "$work/key-other.json"
[[ $(cat "$work/key-other.out") == 2 ]]

# --apply is in the key the same way: a cached unapplied answer is not
# served for an apply, and `--apply ''` is a parse error on a warm cache
# rather than the unapplied value.
applyArm=$(rustCachedArm apply)
[[ $(NIX_CONFIG="$applyArm" nix eval --expr '{ a = 1; }' a) == 1 ]]
[[ $(NIX_CONFIG="$applyArm" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/apply-warm.json" \
    nix eval --expr '{ a = 1; }' a) == 1 ]]
assertMemoServed "$work/apply-warm.json"
[[ $(NIX_CONFIG="$applyArm" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/apply-plus.json" \
    nix eval --expr '{ a = 1; }' a --apply 'x: x + 1') == 2 ]]
assertMemoMissed "$work/apply-plus.json"
expectStderr 1 env NIX_CONFIG="$applyArm" nix eval --expr '{ a = 1; }' a --apply '' > "$work/apply-empty.err"
expectStderr 1 env NIX_CONFIG="$rustArm" nix eval --expr 1 --apply '2' > "$work/apply-int.err"
grepQuiet -F 'attempt to call something which is not a function but an integer' "$work/apply-int.err"


expectStderr 1 env NIX_CONFIG="$rustArm" nix-instantiate --parse --expr '1 + 1' \
    | grepQuiet -F 'command-unsupported'

echo "rust-eval-command-nix-instantiate: ok"
