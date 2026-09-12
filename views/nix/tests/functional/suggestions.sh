#!/usr/bin/env bash

source common.sh

clearStoreIfPossible

# A flake source needs an identity a lock file can name: `path:` serves store
# objects only (src/libfetchers/path.cc), so a plain directory is refused.
flakeDir=$TEST_HOME/flake
jjFlakeDir "$flakeDir"

cd "$flakeDir"

cat <<EOF > flake.nix
{
    outputs = a: {
       packages.$system = {
         foo = 1;
         fo1 = 1;
         fo2 = 1;
         fooo = 1;
         foooo = 1;
         fooooo = 1;
         fooooo1 = 1;
         fooooo2 = 1;
         fooooo3 = 1;
         fooooo4 = 1;
         fooooo5 = 1;
         fooooo6 = 1;
       };
    };
}
EOF

# Probable typo in the requested attribute path. Suggest some close possibilities
NIX_BUILD_STDERR_WITH_SUGGESTIONS=$(! nix build .\#fob 2>&1 1>/dev/null)
[[ "$NIX_BUILD_STDERR_WITH_SUGGESTIONS" =~ "Did you mean one of fo1, fo2, foo or fooo?" ]] || \
    fail "The nix build stderr should suggest the three closest possiblities"

# None of the possible attributes is close to `bar`, so shouldn’t suggest anything
NIX_BUILD_STDERR_WITH_NO_CLOSE_SUGGESTION=$(! nix build .\#bar 2>&1 1>/dev/null)
[[ ! "$NIX_BUILD_STDERR_WITH_NO_CLOSE_SUGGESTION" =~ "Did you mean" ]] || \
    fail "The nix build stderr shouldn’t suggest anything if there’s nothing relevant to suggest"

# `builtins.getFlake` parses its argument with no base directory
# (src/libflake/flake-primops.cc), so a bare path stays a `path:` input instead
# of being routed by what the directory holds. Hence the flake ref is spelled
# out; the suggestion is what this line asserts.
NIX_EVAL_STDERR_WITH_SUGGESTIONS=$(! nix build --impure --expr "(builtins.getFlake \"jj+file://$flakeDir\").packages.$system.fob" 2>&1 1>/dev/null)
[[ "$NIX_EVAL_STDERR_WITH_SUGGESTIONS" =~ "Did you mean one of fo1, fo2, foo or fooo?" ]] || \
    fail "The evaluator should suggest the three closest possiblities"

NIX_EVAL_STDERR_WITH_SUGGESTIONS=$(! nix build --impure --expr '({ foo }: foo) { foo = 1; fob = 2; }' 2>&1 1>/dev/null)
[[ "$NIX_EVAL_STDERR_WITH_SUGGESTIONS" =~ "Did you mean foo?" ]] || \
    fail "The evaluator should suggest the three closest possiblities"
