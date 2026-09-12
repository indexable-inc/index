#!/usr/bin/env bash

# Unsupported entry points emit one attributable refusal census row. Invalid
# command options are ordinary user errors and must not enter that census.
# Supported commands provide the success control. Flake check has its own
# rust-eval-flake-check fixture.

source common.sh
source rust-eval-lib.sh
rustArm+=$'builders =\n'

drv='derivation { name = "eng12711"; builder = "/bin/sh"; system = "x86_64-linux"; }'

# The census line, as journald sees it. The `<4>` is the syslog priority that
# makes it selectable by severity; asserting it here is what stops the prefix
# being dropped by someone tidying the output, which would leave the line in
# the journal at `info` where no census query looks for it.
censusLine() { # TOKEN DETAIL
    printf '<4>rust-eval refusal token=%s detail=%s' "$1" "$2"
}

assertRefusalCensus() { # TOKEN DIAGNOSTIC ERR-FILE
    local token=$1 diagnostic=$2 refusalErr=$3
    local rows row detail prefix
    rows=$(grep -c -F 'rust-eval refusal token=' "$refusalErr" || true)
    if [[ $rows -ne 1 ]]; then
        echo "expected exactly one refusal census line, found $rows:" >&2
        cat "$refusalErr" >&2
        exit 1
    fi
    row=$(grep -F 'rust-eval refusal token=' "$refusalErr")
    prefix=$(censusLine "$token" '')
    [[ $row == "$prefix"* ]] || fail "unexpected refusal census row: $row"
    detail=${row#"$prefix"}
    [[ -n $detail && $detail == *"$diagnostic"* ]] || fail "unexpected refusal detail: $detail"
    # The diagnostic and census must describe the same failure. Explanatory
    # prose can change without changing the token or the rejected operation.
    grepQuiet -F "rust-eval unimplemented: $detail" "$refusalErr"
}

assertRefusal() { # LABEL TOKEN DIAGNOSTIC COMMAND...
    local label=$1 token=$2 diagnostic=$3
    shift 3
    local refusalErr="$TEST_ROOT/refusal-$label.err"
    expectStderr 1 env NIX_CONFIG="$rustArm" "$@" > "$refusalErr"
    assertRefusalCensus "$token" "$diagnostic" "$refusalErr"
}

# nix-shell remains unsupported and names the legacy entry point exactly.
assertRefusal nix-shell command-unsupported nix-shell \
    nix-shell --run true -E "$drv"
grepQuiet -Fx "$(censusLine command-unsupported nix-shell)" "$TEST_ROOT/refusal-nix-shell.err"

assertUserError() { # LABEL DIAGNOSTIC COMMAND...
    local label=$1 diagnostic=$2
    shift 2
    local errorFile="$TEST_ROOT/user-error-$label.err"
    expectStderr 1 env NIX_CONFIG="$rustArm" "$@" > "$errorFile"
    grepQuiet -F -- "$diagnostic" "$errorFile"
    grepQuietInverse -F 'rust-eval refusal token=' "$errorFile"
    grepQuietInverse -F 'rust-eval unimplemented:' "$errorFile"
}

# Supported value and build commands succeed; command-specific fixtures
# cover execution and flake presentation.
echo 1 > "$TEST_ROOT/one.nix"
[[ "$(NIX_CONFIG=$rustArm nix eval --expr 1)" == 1 ]]
[[ "$(NIX_CONFIG=$rustArm nix eval --file "$TEST_ROOT/one.nix")" == 1 ]]
# Strict instantiation also uses the Rust evaluator.
[[ "$(NIX_CONFIG=$rustArm nix-instantiate --eval --strict -E 1)" == 1 ]]
# Dry runs evaluate and write the derivation without executing its builder.
echo "$drv" > "$TEST_ROOT/drv.nix"
NIX_CONFIG=$rustArm nix build --dry-run --impure --expr "$drv"
NIX_CONFIG=$rustArm nix build --dry-run --impure --file "$TEST_ROOT/drv.nix"
# And the `.drv` is really in the store afterwards, which is the whole
# difference between `nix build` being served and `nix eval` being served: a
# computed path is not a store object, and every gate that compares printed
# paths is blind to which one it has (ENG-12799).
builtDrv=$(NIX_CONFIG=$rustArm nix eval --raw --impure --expr "($drv).drvPath")
[[ -f "$builtDrv" ]] || { echo "the rust arm reported $builtDrv and did not write it"; exit 1; }

# Output selection is valid for a build and invalid for a value query.
NIX_CONFIG=$rustArm nix build --dry-run --impure --expr "$drv" '.^out'
assertUserError eval-output "derivation output selection is not supported by nix eval" \
    nix eval --impure --expr "$drv" '.^out'
# `--apply` is served: the evaluator applies the expression, keyed with the
# question, so two applies of one value are two memo rows rather than one
# answer served twice.
applyStats=$TEST_ROOT/apply-stats.json
[[ "$(NIX_CONFIG=$rustArm NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH=$applyStats \
    nix eval --expr '{ a = 1; }' a --apply 'x: x + 1')" == 2 ]]
assertRustServed "$applyStats"
[[ "$(NIX_CONFIG=$rustArm nix eval --expr '{ a = 1; }' a --apply 'x: x + 1')" == 2 ]]
[[ "$(NIX_CONFIG=$rustArm nix eval --expr '{ a = 1; }' a --apply 'x: x + 2')" == 3 ]]
assertUserError eval-write 'nix eval --write-to is not supported' \
    nix eval --expr 1 --write-to "$TEST_ROOT/write-result"
assertUserError conflicting-render '--raw and --json are mutually exclusive' \
    nix eval --raw --json --expr 1
assertUserError conflicting-source "'--file' and '--expr' are exclusive" \
    nix eval --file "$TEST_ROOT/one.nix" --expr 1
mkdir -p "$TEST_ROOT/redirect-result"
assertRefusal develop-redirect command-unsupported \
    'nix develop --redirect resolves each redirect with SourceExprCommand::parseInstallable after the development derivation has been selected' \
    nix develop --impure --expr "$drv" --redirect "$builtDrv" "$TEST_ROOT/redirect-result" --command true
assertRefusal file-ref command-file "--file '<nixpkgs>' (only a plain path)" \
    nix eval --file '<nixpkgs>'
storeInstallable=$(nix store add-path --name refusal-store-installable "$TEST_ROOT/one.nix")
assertRefusal store-installable command-installable \
    "the store-path installable '$storeInstallable' (this backend evaluates a flake, an '--expr' or a '--file'; a store path names something already built)" \
    nix eval "$storeInstallable"

drvLet='let d = derivation { name = "refusal-shape"; system = builtins.currentSystem; builder = "'$bash'"; args = [ "-c" "touch $out" ]; }; in '
assertRefusal string-installable command-not-a-derivation \
    'an installable that is not a derivation' \
    nix build --dry-run --impure --expr '"/nix/store/not-a-real-output"'
assertRefusal recursive-set command-not-a-derivation \
    'an attribute set that is not a derivation' \
    nix build --dry-run --impure --expr "${drvLet}{ recurseForDerivations = true; child = d; }"
assertRefusal outputs-type command-not-a-derivation "the 'outputs' attribute is not a list" \
    nix build --dry-run --impure --expr "${drvLet}d // { outputs = \"out\"; }"
assertRefusal outputs-element command-not-a-derivation \
    "an element of the 'outputs' list is not a string" \
    nix build --dry-run --impure --expr "${drvLet}d // { outputs = [ 1 ]; }"
assertRefusal output-specified-type command-outputs-to-install "'outputSpecified' is not a boolean" \
    nix build --dry-run --impure --expr "${drvLet}d // { outputSpecified = \"yes\"; }"
assertRefusal output-specified command-outputs-to-install \
    "'outputSpecified = true', which selects a single output by name" \
    nix build --dry-run --impure --expr "${drvLet}d // { outputSpecified = true; outputName = \"out\"; }"
assertRefusal meta-type command-outputs-to-install "'meta' is not an attribute set" \
    nix build --dry-run --impure --expr "${drvLet}d // { meta = 1; }"
assertRefusal outputs-to-install-type command-outputs-to-install "'meta.outputsToInstall' is not a list" \
    nix build --dry-run --impure --expr "${drvLet}d // { meta.outputsToInstall = \"out\"; }"
assertRefusal outputs-to-install-element command-not-a-derivation \
    "an element of 'meta.outputsToInstall' is not a string" \
    nix build --dry-run --impure --expr "${drvLet}d // { meta.outputsToInstall = [ 1 ]; }"
assertRefusal outputs-to-install-name command-outputs-to-install \
    "'meta.outputsToInstall' names 'dev', which is not one of this derivation's outputs" \
    nix build --dry-run --impure --expr "${drvLet}d // { outputs = [ \"out\" ]; meta.outputsToInstall = [ \"dev\" ]; }"
assertRefusal outputs-to-install-empty command-outputs-to-install "'meta.outputsToInstall' is empty" \
    nix build --dry-run --impure --expr "${drvLet}d // { meta.outputsToInstall = [ ]; }"
assertRefusal develop-value command-not-a-derivation \
    'the value selected for nix develop is not an attribute set' \
    nix develop --impure --expr '1' --command true
assertRefusal develop-set command-not-a-derivation \
    'the value selected for nix develop is not a derivation' \
    nix develop --impure --expr '{}' --command true
# Instantiation is a Rust derivation-set question.
[[ $(NIX_CONFIG="$rustArm" nix-instantiate -E "$drv") == "$builtDrv" ]]

# Without `--strict`, a value with no children is served (lazy and strict
# printing are one answer for it -- home-manager's news probes are three of
# these); a value with children is the evaluator's refusal, by name.
[[ "$(NIX_CONFIG=$rustArm nix-instantiate --eval -E '1 + 1')" == 2 ]]
[[ "$(NIX_CONFIG=$rustArm nix-instantiate --eval -E '"s"')" == '"s"' ]]
assertRefusal instantiate-lazy lazy-print 'lazy top-level printing of a list (run with --strict)' \
    nix-instantiate --eval -E '[ 1 ]'
assertRefusal instantiate-xml command-xml-output '--xml with source locations (run with --no-location)' \
    nix-instantiate --eval --strict --xml -E '1'
assertRefusal nested-function unsupported-render \
    'printing a function' \
    nix eval --expr '{ nested = x: x; }'

jjFlakeDir "$TEST_ROOT/flake"
echo '{ outputs = { self }: { value = 1; }; }' > "$TEST_ROOT/flake/flake.nix"
readSetRustArm="$rustArm"$'read-set-trace-file = '"$TEST_ROOT"$'/refusal-read-set-rust.jsonl\n'
readSetDetail="a flake installable while the read-set tracker is on"
expectStderr 1 env NIX_CONFIG="$readSetRustArm" nix eval "jj+file://$TEST_ROOT/flake#value" \
    > "$TEST_ROOT/refusal-flake-read-set.err"
assertRefusalCensus command-unsupported "$readSetDetail" "$TEST_ROOT/refusal-flake-read-set.err"

getFlakeDetail="builtins.getFlake while the read-set tracker is on"
expectStderr 1 env NIX_CONFIG="$readSetRustArm" nix eval --impure --expr \
    "(builtins.getFlake \"jj+file://$TEST_ROOT/flake\").value" > "$TEST_ROOT/refusal-get-flake-read-set.err"
assertRefusalCensus unimplemented-builtin "$getFlakeDetail" "$TEST_ROOT/refusal-get-flake-read-set.err"

stdinErr="$TEST_ROOT/refusal-stdin.err"
printf '1\n' | expectStderr 1 env NIX_CONFIG="$rustArm" nix eval --file - > "$stdinErr"
grepQuiet -Fx "$(censusLine command-stdin 'reading the expression from stdin')" "$stdinErr"

# And a served command emits no refusal at all, so the greps above are matching
# this mechanism rather than something the harness prints on every invocation.
err=$TEST_ROOT/served.err
NIX_CONFIG=$rustArm nix eval --expr 1 2> "$err" > /dev/null
grepQuietInverse -F 'rust-eval refusal' "$err"

echo "rust-eval-refusal-token: ok"
