#!/usr/bin/env bash

source common.sh
source ./rust-eval-lib.sh

work="$TEST_ROOT/rust-import-cache"
mkdir -p "$work"
cacheConfig=$(rustCachedArm imported-scalars)

writeRoot() {
    cat > "$work/root.nix" <<'NIX'
{
  changing = builtins.readFile ./changing;
  stable = import ./stable.nix;
}
NIX
}

runCase() {
    local label=$1
    local expected=$2
    local hits=$3
    local forces=$4
    shift 4
    # Every request differs at a dependency outside the imported module. A
    # whole-question hit cannot explain any imported-entry reuse below.
    printf '%s' "$label" > "$work/changing"
    NIX_CONFIG="$cacheConfig" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$label.stats" \
        nix eval --json --file "$work/root.nix" --option eval-cache-verify-rate 0 "$@" \
        > "$work/$label.out" 2> "$work/$label.err"
    assertRustServed "$work/$label.stats"
    jq -e --arg changing "$label" --argjson stable "$expected" \
        '.changing == $changing and .stable == $stable' "$work/$label.out"
    # No //0 defaults: a missing instrument must fail the test.
    jq -e --argjson hits "$hits" --argjson forces "$forces" \
        '.rustEvalPerf.memo_served == 0 and
         .rustEvalPerf.subtreeMemoHits == $hits and
         .rustEvalPerf.subtreeEntryForces == $forces' "$work/$label.stats"
}

writeRoot
for round in 0 1 2 3 4; do
    case "$round" in
        0) n=30; expected=465; hits=0; forces=1 ;;
        1) n=30; expected=465; hits=1; forces=0 ;;
        2) n=31; expected=496; hits=0; forces=1 ;;
        3) n=31; expected=496; hits=1; forces=0 ;;
        4) n=30; expected=465; hits=1; forces=0 ;;
    esac
    cat > "$work/stable.nix" <<NIX
let sum = n: if n == 0 then 0 else n + sum (n - 1); in sum $n
NIX
    # Each call launches a new process: this requires persisted evaluated
    # values, not retained VM cells or the existing compiled-module cache.
    runCase "history-$round" "$expected" "$hits" "$forces"
    runCase "disabled-$round" "$expected" 0 1 --option eval-cache-dir ''
done

# Establish a whole-question hit with no dependency changes, then ask its
# verifier to reproduce the answer. The verifier must execute the imported
# entry even though both the whole answer and imported scalar are cached.
runCase verifier-prime 465 1 0
for rate in 0 1; do
    NIX_CONFIG="$cacheConfig" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/verifier-$rate.stats" \
        nix eval --json --file "$work/root.nix" --option eval-cache-verify-rate "$rate" \
        > "$work/verifier-$rate.out" 2> "$work/verifier-$rate.err"
    cmp "$work/verifier-prime.out" "$work/verifier-$rate.out"
done
jq -e '.rustEvalPerf.memo_served == 1 and
       .rustEvalPerf.subtreeMemoHits == 0 and
       .rustEvalPerf.subtreeEntryForces == 0' "$work/verifier-0.stats"
jq -e '.rustEvalPerf.memo_served == 0 and
       .rustEvalPerf.subtreeMemoHits == 0 and
       .rustEvalPerf.subtreeEntryForces == 1' "$work/verifier-1.stats"

# Trace emission disqualifies a module even though its result is scalar.
printf '%s\n' 'builtins.trace "import-cache-effect" 42' > "$work/stable.nix"
for round in 0 1; do
    runCase "effect-$round" 42 0 1
    grep -F 'import-cache-effect' "$work/effect-$round.err"
done

# A container must not be deep-forced to turn it into a cacheable scalar.
printf '%s\n' '{ x = 42; unused = assert false; 0; }' > "$work/stable.nix"
cat > "$work/root.nix" <<'NIX'
{ changing = builtins.readFile ./changing; stable = (import ./stable.nix).x; }
NIX
runCase container-0 42 0 1
runCase container-1 42 0 1

printf '%s\n' 'x: x' > "$work/stable.nix"
cat > "$work/root.nix" <<'NIX'
{ changing = builtins.readFile ./changing; stable = (import ./stable.nix) 42; }
NIX
runCase closure-0 42 0 1
runCase closure-1 42 0 1

# A result obtained with ample call-depth budget cannot be served to a
# caller that would overflow while evaluating the same imported module.
printf '%s\n' 'let f = n: if n == 0 then 42 else f (n - 1); in f 20' > "$work/stable.nix"
cat > "$work/root.nix" <<'NIX'
{ changing = builtins.readFile ./changing; stable = import ./stable.nix; }
NIX
runCase depth-shallow 42 0 1 --option max-call-depth 32
cat > "$work/root.nix" <<'NIX'
let f = n: if n == 0 then import ./stable.nix else f (n - 1); in f 20
NIX
for mode in cached disabled; do
    extra=()
    if test "$mode" = disabled; then extra=(--option eval-cache-dir ''); fi
    if NIX_CONFIG="$cacheConfig" nix eval --json --file "$work/root.nix" \
        --option max-call-depth 32 "${extra[@]}" > "$work/depth-$mode.out" 2> "$work/depth-$mode.err"; then
        echo "import cache bypassed the caller's call-depth limit ($mode)" >&2
        exit 1
    fi
    grep -F 'max-call-depth exceeded' "$work/depth-$mode.err"
done

writeRoot
printf '%s\n' 'assert false; 42' > "$work/stable.nix"
for round in 0 1; do
    printf '%s' "failure-$round" > "$work/changing"
    if NIX_CONFIG="$cacheConfig" nix eval --json --file "$work/root.nix" \
        > "$work/failure-$round.out" 2> "$work/failure-$round.err"; then
        echo 'failing imported evaluation was served as a success' >&2
        exit 1
    fi
    grep -i 'assertion' "$work/failure-$round.err"
done
