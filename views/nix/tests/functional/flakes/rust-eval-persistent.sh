#!/usr/bin/env bash

# A process must observe dependency edits and may reuse only validated answers.
source ./common.sh
requireGit

flakeDir="$TEST_ROOT/persistent-flake"
createGitRepo "$flakeDir" ""
echo '"before"' > "$flakeDir/value.nix"
cat > "$flakeDir/flake.nix" <<'NIX'
{
  outputs = { self }: {
    value = import ./value.nix;
    broken = throw "persistent request failed";
  };
}
NIX
git -C "$flakeDir" add flake.nix value.nix
git -C "$flakeDir" commit -m before
initialRev=$(git -C "$flakeDir" rev-parse HEAD)

evaluatorPid=""
cleanupEvaluator() {
    exec 3>&-
    if [[ -n "$evaluatorPid" ]]; then
        kill "$evaluatorPid" || true
        wait "$evaluatorPid" || true
    fi
}
trap cleanupEvaluator EXIT

startEvaluator() {
    local label=$1
    shift
    results="$TEST_ROOT/persistent-$label-results"
    errors="$TEST_ROOT/persistent-$label-errors"
    fifo="$TEST_ROOT/persistent-$label-fifo"
    mkfifo "$fifo"
    nix eval-persistent --builders '' --option eval-cache-dir "$TEST_ROOT/persistent-$label-cache" \
        --option eval-cache-verify-rate 0 --interactive "$@" < "$fifo" > "$results" 2> "$errors" &
    evaluatorPid=$!
    exec 3> "$fifo"
}

request() {
    local installable=$1 want=$2 waited=0
    echo "$installable" >&3
    while [[ "$(wc -l < "$results")" -lt "$want" ]]; do
        if ! kill -0 "$evaluatorPid"; then
            cat "$errors" >&2
            echo "persistent evaluator exited before result $want" >&2
            exit 1
        fi
        sleep 0.1
        waited=$((waited + 1))
        if [[ "$waited" -ge 600 ]]; then
            cat "$errors" >&2
            echo "timed out waiting for persistent result $want" >&2
            exit 1
        fi
    done
}

stopEvaluator() {
    exec 3>&-
    wait "$evaluatorPid"
    evaluatorPid=""
    rm "$fifo"
}

# A/A/B/B/A: the last request returns to the original immutable revision.
startEvaluator flake
request "$flakeDir#value" 1
request "$flakeDir#value" 2
echo '"after"' > "$flakeDir/value.nix"
git -C "$flakeDir" add value.nix
git -C "$flakeDir" commit -m after
request "$flakeDir#value" 3
request "$flakeDir#value" 4
git -C "$flakeDir" reset --hard "$initialRev"
request "$flakeDir#value" 5
stopEvaluator

jq -s -e 'map(.value) == ["before", "before", "after", "after", "before"]' "$results"
jq -s -e '
  all(.stats.countersEnabled == true) and
  (.[0].stats.compiles + .[0].stats.compileHits > 0) and
  .[0].stats.memoServed == 0 and .[2].stats.memoServed == 0 and
  .[1].stats.memoServed == 1 and .[3].stats.memoServed == 1 and .[4].stats.memoServed == 1 and
  all(has("thunks") == false and has("evalFileCalls") == false and has("evalFilePathHits") == false)
' "$results"

# Ambient --file sources need both read-set invalidation and host path/copy
# cache refresh. The root expression does not change when its dependencies do.
ambient="$TEST_ROOT/persistent-ambient"
mkdir -p "$ambient"
echo '"before"' > "$ambient/value.nix"
printf before > "$ambient/data"
cat > "$ambient/source.nix" <<'NIX'
{
  imported = import ./value.nix;
  copied = builtins.readFile "${./data}";
  exists = builtins.pathExists ./marker;
}
NIX
# This source is deliberately mutable: it has no immutable fetcher fingerprint.
# The evaluator must validate its read set rather than reuse a source hash.
_NIX_TEST_BARF_ON_UNCACHEABLE='' startEvaluator ambient --file "$ambient/source.nix"
request . 1
request . 2
echo '"after"' > "$ambient/value.nix"
printf after > "$ambient/data"
touch "$ambient/marker"
request . 3
request . 4
echo '"before"' > "$ambient/value.nix"
printf before > "$ambient/data"
rm "$ambient/marker"
request . 5
request . 6
stopEvaluator
jq -s -e '
  map(.value) == [
    {imported: "before", copied: "before", exists: false},
    {imported: "before", copied: "before", exists: false},
    {imported: "after", copied: "after", exists: true},
    {imported: "after", copied: "after", exists: true},
    {imported: "before", copied: "before", exists: false},
    {imported: "before", copied: "before", exists: false}
  ] and .[1].stats.memoServed == 1 and .[2].stats.memoServed == 0 and
  .[3].stats.memoServed == 1 and .[4].stats.memoServed == 1 and .[5].stats.memoServed == 1 and
  .[1].stats.witnessMemoryHits == 1 and .[1].stats.witnessDiskLoads == 0 and
  .[4].stats.witnessDiskLoads > 0 and
  .[5].stats.witnessMemoryHits == 1 and .[5].stats.witnessDiskLoads == 0 and
  all(.stats.witnessCacheEntries <= 64 and .stats.witnessCacheBytes <= 536870912)
' "$results"

# Disabling retained memory preserves disk memo reuse, but a second request
# must decode its witness again. This controls the memory-hit instrument.
_NIX_TEST_BARF_ON_UNCACHEABLE='' startEvaluator no-retention --memory-cache-size 0 --file "$ambient/source.nix"
request . 1
request . 2
stopEvaluator
jq -s -e '
  .[0].value == .[1].value and .[1].stats.memoServed == 1 and
  all(.stats.witnessMemoryHits == 0 and .stats.witnessCacheEntries == 0 and .stats.witnessCacheBytes == 0) and
  .[1].stats.witnessDiskLoads > 0
' "$results"

# No configured cache still gets real warm reuse in the standard user cache.
_NIX_TEST_BARF_ON_UNCACHEABLE='' XDG_CACHE_HOME="$TEST_ROOT/persistent-default-cache" \
    nix eval-persistent --builders '' --option eval-cache-dir '' --option eval-cache-verify-rate 0 \
    --file "$ambient/source.nix" . . > "$TEST_ROOT/persistent-default-results"
jq -s -e 'length == 2 and .[0].value == .[1].value and .[1].stats.memoServed == 1' \
    "$TEST_ROOT/persistent-default-results"
test -d "$TEST_ROOT/persistent-default-cache/nix/eval"

# getFlake reenters Rust through the host. Force substantial work before that
# reentry so a nested counter reset cannot disguise the earlier work as zero.
nested="$TEST_ROOT/persistent-nested"
mkdir -p "$nested"
cat > "$nested/source.nix" <<'NIX'
let before = builtins.foldl' (sum: file: sum + import file) 0 [
NIX
for ((i = 1; i <= 32; i++)); do
    echo "$i" > "$nested/work-$i.nix"
    printf ' ./work-%s.nix\n' "$i" >> "$nested/source.nix"
done
printf ']; in builtins.seq before (builtins.getFlake "%s").value\n' "$flakeDir" >> "$nested/source.nix"
NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$nested/stats.json" \
    nix eval-persistent --builders '' --option eval-cache-dir "$nested/cache" \
    --option eval-cache-verify-rate 0 --file "$nested/source.nix" . . > "$nested/results"
jq -s -e '
  map(.value) == ["before", "before"] and .[0].stats.compiles >= 33 and .[1].stats.memoServed >= 1
' "$nested/results"

# The process census must count each outer question once, including all nested
# work. Recording the cumulative Rust counters on every nested destructor
# would double-count even if nested resets themselves were removed.
jq -s -e --slurpfile census "$nested/stats.json" '
  ($census[0].evaluatorCalls.rust > 2) and
  ($census[0].rustEvalPerf.compiles == (map(.stats.compiles) | add)) and
  ($census[0].rustEvalPerf.compile_hits == (map(.stats.compileHits) | add)) and
  ($census[0].rustEvalPerf.questions == (map(.stats.hostQuestions) | add)) and
  ($census[0].rustEvalPerf.memo_served == (map(.stats.memoServed) | add))
' "$nested/results"

# The stream stops on the first failure and never emits a success-shaped row
# for that request or silently evaluates later requests.
if nix eval-persistent --builders '' "$flakeDir#value" "$flakeDir#broken" "$flakeDir#value" \
    > "$TEST_ROOT/persistent-failure-results" 2> "$TEST_ROOT/persistent-failure-errors"; then
    echo "persistent evaluator accepted a throwing request" >&2
    exit 1
fi
test "$(wc -l < "$TEST_ROOT/persistent-failure-results")" -eq 1
grep -F 'persistent request failed' "$TEST_ROOT/persistent-failure-errors"

if nix eval-persistent --builders '' --evict none "$flakeDir#value" \
    > "$TEST_ROOT/persistent-obsolete-results" 2> "$TEST_ROOT/persistent-obsolete-errors"; then
    echo "persistent evaluator accepted the stale-input mode" >&2
    exit 1
fi
grep -F 'unrecognised flag' "$TEST_ROOT/persistent-obsolete-errors"
