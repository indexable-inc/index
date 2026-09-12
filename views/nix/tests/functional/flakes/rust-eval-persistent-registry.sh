#!/usr/bin/env bash

source ./common.sh
requireGit

work="$TEST_ROOT/persistent-registry"
mkdir -p "$work"
flakeDir="$work/flake"
createGitRepo "$flakeDir" "--initial-branch=registry-test"
echo '{ outputs = { self }: { value = "A"; }; }' > "$flakeDir/flake.nix"
git -C "$flakeDir" add flake.nix
git -C "$flakeDir" commit -m A
revA=$(git -C "$flakeDir" rev-parse HEAD)
echo '{ outputs = { self }: { value = "B"; }; }' > "$flakeDir/flake.nix"
git -C "$flakeDir" commit -am B
revB=$(git -C "$flakeDir" rev-parse HEAD)

evaluatorPid=""
cleanupEvaluator() {
    exec 3>&-
    if [[ -n "$evaluatorPid" ]]; then
        kill "$evaluatorPid" || true
        wait "$evaluatorPid" || true
    fi
}
trap cleanupEvaluator EXIT

writeRegistry() {
    local revision=$1
    jq -n --arg url "file://$flakeDir" --arg rev "$revision" '{
      version: 2,
      flakes: [{from: {type: "indirect", id: "persistentAlias"},
                to: {type: "git", url: $url, ref: "registry-test", rev: $rev}}]
    }' > "$registryFile.tmp"
    mv "$registryFile.tmp" "$registryFile"
}

request() {
    local count=$1 waited=0
    echo 'persistentAlias#value' >&3
    while [[ "$(wc -l < "$results")" -lt "$count" ]]; do
        if ! kill -0 "$evaluatorPid"; then
            cat "$errors" >&2
            echo "persistent evaluator exited before result $count" >&2
            exit 1
        fi
        sleep 0.1
        waited=$((waited + 1))
        if [[ "$waited" -ge 600 ]]; then
            cat "$errors" >&2
            echo "timed out waiting for persistent result $count" >&2
            exit 1
        fi
    done
}

# Each file-backed registry must resolve the unchanged alias again between
# requests. Both target revisions stay immutable throughout these changes.
for registryKind in global user system flag; do
    configDir="$work/$registryKind-config"
    mkdir -p "$configDir"
    case "$registryKind" in
        user) registryFile="$configDir/registry.json" ;;
        system) registryFile="$NIX_CONF_DIR/registry.json" ;;
        *) registryFile="$work/$registryKind-registry.json" ;;
    esac
    writeRegistry "$revA"
    results="$work/$registryKind-results"
    errors="$work/$registryKind-errors"
    fifo="$work/$registryKind-fifo"
    mkfifo "$fifo"
    extraArgs=()
    if [[ "$registryKind" == flag ]]; then
        extraArgs+=(--override-flake persistentAlias "git+file://$flakeDir?ref=registry-test&rev=$revA")
    fi
    NIX_CONFIG_HOME="$configDir" nix eval-persistent --builders '' \
        --option flake-registry "$registryFile" \
        --option eval-cache-dir "$work/$registryKind-cache" \
        --option eval-cache-verify-rate 0 --interactive "${extraArgs[@]}" \
        < "$fifo" > "$results" 2> "$errors" &
    evaluatorPid=$!
    exec 3> "$fifo"
    request 1
    request 2
    writeRegistry "$revB"
    request 3
    request 4
    writeRegistry "$revA"
    request 5
    exec 3>&-
    wait "$evaluatorPid"
    evaluatorPid=""
    rm "$fifo"

    if [[ "$registryKind" == flag ]]; then
        jq -s -e 'map(.value) == ["A", "A", "A", "A", "A"]' "$results"
    else
        jq -s -e 'map(.value) == ["A", "A", "B", "B", "A"]' "$results"
    fi
    jq -s -e 'length == 5 and .[1].stats.memoServed == 1 and .[3].stats.memoServed == 1' "$results"
    rm "$registryFile"
done
