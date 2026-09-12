#!/usr/bin/env bash
source common.sh

# Evaluation only: the builder does not exist and no outputs are realized.
# The same fixture is a C++ oracle for output paths, placeholders and contexts.
nix eval --offline --option max-jobs 0 --option builders '' --option substituters '' \
    --extra-experimental-features ca-derivations --impure --json \
    --file ./rust-eval-import-drv.nix > "$TEST_ROOT/import-drv.json"
jq -e '. == {inputAddressed:true, fixedOutput:true, floatingCA:true, deferred:true}' \
    "$TEST_ROOT/import-drv.json"

# A suffix alone does not make a local file a store derivation.
printf '%s\n' '{ answer = 42; }' > "$TEST_ROOT/ordinary.drv"
nix eval --impure --json --expr "(import $TEST_ROOT/ordinary.drv).answer" \
    > "$TEST_ROOT/ordinary-drv.json"
jq -e '. == 42' "$TEST_ROOT/ordinary-drv.json"
