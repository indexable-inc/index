# Shared evaluator configuration and cache assertions.
rustArm=$'extra-experimental-features = nix-command flakes\n'

# The rust arm with a persistent memo under $TEST_ROOT, for the claims that
# are about warm runs: what a second run of the same command is served, and
# what a different command is NOT served. Call `rustCachedArm <name>` per
# claim; one directory per name, so claims cannot warm each other.
rustCachedArm() { # NAME
    printf '%s\neval-cache-dir = %s/rust-eval-cache-%s\n' "$rustArm" "$TEST_ROOT" "$1"
}

# The command's answer came out of the persistent memo: at least one
# question was served without evaluating (`memo_served`, counted by the
# evaluator's own census, not inferred from timing).
assertMemoServed() { # STATS
    jq -e '(.rustEvalPerf.memo_served // 0) >= 1' < "$1" > /dev/null || {
        echo "not served from the memo ($1):" >&2
        jq -c '{evaluator, rustEvalPerf: (.rustEvalPerf // {} | {memo_served, compiles})}' < "$1" >&2
        return 1
    }
}

# The command evaluated: nothing was served from the memo.
# The served run wrote no derivation. A hit verifies that every derivation
# the witness expects is still in the store (one queryValidPaths for the
# whole witness) instead of writing each again; `drvWrites` is the bridge's
# own count of derivations it streamed to the store (`drvFlushes` batches), so
# a served run that wrote one replayed a write it owed nobody.
assertNoDrvWrites() { # STATS
    jq -e '(.rustEvalPerf.drvWrites // 0) == 0' < "$1" > /dev/null || {
        echo "a served run wrote derivations ($1):" >&2
        jq -c '{evaluator, rustEvalPerf: (.rustEvalPerf // {} | {memo_served, drvWrites, drvFlushes})}' < "$1" >&2
        return 1
    }
}

# The control for assertNoDrvWrites: a fresh evaluation of a derivation
# writes at least one, or the counter could never have fired.
assertDrvWrites() { # STATS
    jq -e '(.rustEvalPerf.drvWrites // 0) >= 1' < "$1" > /dev/null || {
        echo "a fresh evaluation wrote no derivation ($1):" >&2
        jq -c '{evaluator, rustEvalPerf: (.rustEvalPerf // {} | {memo_served, drvWrites, drvFlushes})}' < "$1" >&2
        return 1
    }
}

assertMemoMissed() { # STATS
    jq -e '(.rustEvalPerf.memo_served // 0) == 0' < "$1" > /dev/null || {
        echo "served from the memo where a fresh evaluation was owed ($1):" >&2
        jq -c '{evaluator, rustEvalPerf: (.rustEvalPerf // {} | {memo_served})}' < "$1" >&2
        return 1
    }
}

# The command's evaluation was answered by the Rust evaluator alone: at least
# one Rust call, no C++ evaluation, no refusal.
assertRustServed() {
    jq -e '.evaluator == "rust" and .evaluatorCalls.rust >= 1 and .refusals.total == 0' \
        < "$1" > /dev/null || {
        echo "not served by the rust evaluator ($1):" >&2
        jq -c '{evaluator, evaluatorCalls, refusals}' < "$1" >&2
        return 1
    }
}

# An arm whose command failed says why before the test dies: the stderr
# the arm captured for later assertions is otherwise never seen, and a
# failing `nix` invocation under `set -e` would leave only its command line
# in the trace.
armFailed() { # LABEL ERR-FILE
    echo "the $1 arm failed; its stderr:" >&2
    cat "$2" >&2
    exit 1
}

# `NIX_SHOW_STATS` makes the evaluator run a full Boehm collection before it
# prints, and warn when more than 1 KiB has been allocated by the time it
# looks (`EvalState::fullGC`): a race with the process's other threads, seen
# once in about sixty gate runs, on the rust arm, with the same binary then
# passing on every rerun (2026-09-03). It says nothing about either arm's
# evaluation, so the arms are compared without it. `grep -v` exits 1 on an
# empty result, which an empty stderr is.
withoutGcRaceWarning() { # FILE, to stdout
    grep -v -F 'failed to perform a full GC before reporting stats' "$1" || true
}

# Two arms' outputs are byte-identical, and when they are not the diff is
# the diagnostic: a bare `cmp` failure under `set -e` names the files and
# nothing else. The files themselves are left as captured; the comparison
# reads them through `withoutGcRaceWarning`.
assertSame() { # FILE-A FILE-B
    withoutGcRaceWarning "$1" > "$1.compared"
    withoutGcRaceWarning "$2" > "$2.compared"
    cmp -s "$1.compared" "$2.compared" && return 0
    echo "$1 and $2 differ:" >&2
    diff "$1.compared" "$2.compared" | head -60 >&2
    # The first differing bytes, for a difference the eye cannot see.
    cmp -l "$1.compared" "$2.compared" 2>&1 | head -5 >&2
    exit 1
}
