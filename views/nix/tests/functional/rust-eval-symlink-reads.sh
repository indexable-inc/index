#!/usr/bin/env bash


source common.sh
source rust-eval-lib.sh

clearStoreIfPossible


tree=$TEST_ROOT/symlinks
rm -rf "$tree"
mkdir -p "$tree/dir"
echo -n 'target contents' > "$tree/target"
echo -n 'other contents' > "$tree/other"
echo '1' > "$tree/dir/default.nix"
ln -s target "$tree/link"
ln -s dir "$tree/link-to-dir"
ln -s nowhere "$tree/dangling"

evaluate() { # EXPR
    NIX_CONFIG="$rustArm" nix-instantiate --eval --strict -E "$1"
}

checkFailure() { # EXPR NEEDLE
    expectStderr 1 env NIX_CONFIG="$rustArm" nix-instantiate --eval --strict -E "$1" | grepQuiet "$2"
}

# 1. Following, per primop. readFile and readDir resolve the leaf; pathExists
#    resolves ancestors only, which is why a dangling link exists; readFileType
#    resolves nothing at all.
[[ $(evaluate "builtins.readFile $tree/link") == '"target contents"' ]]
[[ $(evaluate "builtins.readDir $tree/link-to-dir") == '{ "default.nix" = "regular"; }' ]]
[[ $(evaluate "builtins.readFile $tree/link-to-dir/default.nix") == '"1\n"' ]]
[[ $(evaluate "builtins.pathExists $tree/dangling") == 'true' ]]
[[ $(evaluate "builtins.readFileType $tree/link") == '"symlink"' ]]
[[ $(evaluate "import $tree/link-to-dir") == '1' ]]

# 2. Refusing. A dangling link reports the missing TARGET, because the
#    resolution ran and then the read failed; readFileType reports the
#    ancestor as a symlink, because no resolution ran at all.
checkFailure "builtins.readFile $tree/dangling" "$tree/nowhere' does not exist"
checkFailure "builtins.readFileType $tree/link-to-dir/default.nix" "$tree/link-to-dir' is a symlink"

for setting in pure-eval restrict-eval; do
    for arm in "$rustArm"; do
        expectStderr 1 env NIX_CONFIG="$arm" nix-instantiate --eval --strict \
            --option "$setting" true -E "builtins.readFile $tree/link" \
            | grepQuiet "$tree/link' is forbidden"
        # The negative: naming the target would mean the resolution happened
        # somewhere the allow list is not.
        expectStderr 1 env NIX_CONFIG="$arm" nix-instantiate --eval --strict \
            --option "$setting" true -E "builtins.readFile $tree/link" \
            | grepQuietInverse "$tree/target"
    done
done

# 4. Invalidation, with the memo table on. Resolution means the answer to
#    "read $tree/link" comes from a file whose name is not in the question, so
#    an edit to EITHER end of the link has to change the answer. It does
#    because the witness replays the question rather than the recorded answer,
#    and replaying it resolves again -- but that is an argument, and this is
#    the measurement.
cacheDir=$TEST_ROOT/symlink-cache
rm -rf "$cacheDir"
cached() { # EXPR -> the rust arm's answer with the memo table on
    NIX_CONFIG="$rustArm"$'eval-cache-dir = '"$cacheDir"$'\n' \
        nix-instantiate --eval --strict -E "$1"
}

[[ $(cached "builtins.readFile $tree/link") == '"target contents"' ]]
# 4a. Edit the target the link points at. Same question, same link, new answer.
echo -n 'edited target' > "$tree/target"
[[ $(cached "builtins.readFile $tree/link") == '"edited target"' ]]
# 4b. Repoint the link, leaving both files alone. Same question again.
ln -sfn other "$tree/link"
[[ $(cached "builtins.readFile $tree/link") == '"other contents"' ]]
# 4c. And back, to show 4b was the link moving rather than a cache that never
#     hits: this returns to an answer the cache has already seen.
ln -sfn target "$tree/link"
[[ $(cached "builtins.readFile $tree/link") == '"edited target"' ]]

echo "rust-eval-symlink-reads: ok"
