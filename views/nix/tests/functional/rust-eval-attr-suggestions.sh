#!/usr/bin/env bash


source common.sh
source rust-eval-lib.sh


checkMissing() { # LABEL EXPR ATTR
    local label=$1 expr=$2 attr=$3
    local out=$TEST_ROOT/$label.rust.out err=$TEST_ROOT/$label.rust.err
    if env NIX_CONFIG="$rustArm" nix-instantiate --eval --strict -E "$expr" -A "$attr" > "$out" 2> "$err"; then
        fail "$label unexpectedly evaluated"
    fi
}

ordered='{ fo = 1; fooo = 2; fox = 3; xoo = 4; fooba = 5; unrelated = 6; }'
checkMissing ordered "$ordered" foo

grepQuiet -F 'Did you mean one of fo, fooo, fox, xoo or fooba?' "$TEST_ROOT/ordered.rust.err"

checkMissing nomatch '{ alpha = 1; beta = 2; gamma = 3; }' zzzznotarealname
grepQuietInverse -F 'Did you mean' "$TEST_ROOT/nomatch.rust.err"

# The Rust selector ranks 5,000 names without per-name FFI or retaining a
# distance table proportional to the number or length of candidates.
big='builtins.listToAttrs (builtins.genList (i: { name = "attr" + toString i; value = i; }) 5000)'
checkMissing big "$big" attr9999x
grepQuiet -F "attribute 'attr9999x' in selection path 'attr9999x' not found" "$TEST_ROOT/big.rust.err"
# A near miss on the same large set, so the scale case covers a rendered
# suggestion list and not only the empty one.
checkMissing bignear "$big" attr499x
grepQuiet -F 'Did you mean' "$TEST_ROOT/bignear.rust.err"

# Awkward names remain intact; controls are escaped for terminal output.
awkward='{ "" = 1; "a b" = 2; "c\nd" = 3; "é" = 4; }'
checkMissing awkward "$awkward" ab
grepQuiet -F 'a b' "$TEST_ROOT/awkward.rust.err"

checkMissing lazy '{ foo = throw "suggestion was forced"; }' foa
grepQuiet -F 'Did you mean foo?' "$TEST_ROOT/lazy.rust.err"
grepQuietInverse -F 'suggestion was forced' "$TEST_ROOT/lazy.rust.err"
checkMissing escaped '{ "c\nd" = throw "suggestion was forced"; }' cd
grepQuiet -F 'c\nd' "$TEST_ROOT/escaped.rust.err"

echo "rust-eval-attr-suggestions: ok"
