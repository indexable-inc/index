#!/usr/bin/env bash


source common.sh
source rust-eval-lib.sh

clearStoreIfPossible


srcDir=$TEST_ROOT/interp
mkdir -p "$srcDir"
echo -n 'the contents decide the store path' > "$srcDir/f"

evaluate() { # EXPR [OPTIONS]
    local expr=$1
    shift
    NIX_CONFIG="$rustArm" nix-instantiate --eval --strict "$@" -E "$expr"
}

out=$(evaluate "\"\${$srcDir/f}\"" --read-write-mode)
[[ $out =~ ^\"$NIX_STORE_DIR/[0-9a-z]{32}-f\"$ ]] || {
    echo "not a store path: $out" >&2
    exit 1
}
# The negative that the cheap fix would pass: it must not be the source path.
[[ $out != "\"$srcDir/f\"" ]]

# 2. The file is really in the store under that name, so this is a copy and
#    not a computed string.
storePath=${out%\"}
storePath=${storePath#\"}
[[ $(cat "$storePath") == 'the contents decide the store path' ]]

# 3. The store path is content-addressed: edit the file, get a different one.
echo -n 'different contents' > "$srcDir/f"
other=$(evaluate "\"\${$srcDir/f}\"" --read-write-mode)
[[ $other != "$out" ]]

# 4. Read-only mode (the default for --eval, and what the corpus runs under)
#    answers with the same path without copying. Same expression, fresh store.
clearStoreIfPossible
readOnly=$(evaluate "\"\${$srcDir/f}\"")
[[ $readOnly == "$other" ]]
[[ ! -e ${readOnly//\"/} ]]

plain=$(evaluate "builtins.toString $srcDir/f")
[[ $plain == "\"$srcDir/f\"" ]]

# 6. A path on the LEFT of + stays a path and copies nothing; a string on the
#    left makes the right-hand path a store copy, exactly as interpolation does.
[[ $(evaluate "$srcDir/f + \"/g\"") == "$srcDir/f/g" ]]
[[ $(evaluate "\"\" + $srcDir/f") == "$other" ]]

expectStderr 1 env NIX_CONFIG="$rustArm" nix-instantiate --eval --strict -E "\"\${$srcDir/nope}\"" \
    | grepQuiet "does not exist"

echo "rust-eval-path-to-store: ok"
