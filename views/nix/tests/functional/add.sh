#!/usr/bin/env bash

source common.sh

path1=$(nix-store --add ./dummy)
echo "$path1"

path2=$(nix-store --add-fixed sha256 --recursive ./dummy)
echo "$path2"

if test "$path1" != "$path2"; then
    echo "nix-store --add and --add-fixed mismatch"
    exit 1
fi

path3=$(nix-store --add-fixed sha256 ./dummy)
echo "$path3"
test "$path1" != "$path3" || exit 1

path4=$(nix-store --add-fixed sha1 --recursive ./dummy)
echo "$path4"
test "$path1" != "$path4" || exit 1

hash1=$(nix-store -q --hash "$path1")
echo "$hash1"

hash2=$(nix-hash --type sha256 --base32 ./dummy)
echo "$hash2"

test "$hash1" = "sha256:$hash2"

# The contents can be accessed through a symlink, and this symlink has no effect on the hash
# https://github.com/NixOS/nix/issues/11941
test_issue_11941() {
    local expected actual
    mkdir -p "$TEST_ROOT/foo/bar" && ln -s "$TEST_ROOT/foo" "$TEST_ROOT/foo-link"

    # legacy
    expected=$(nix-store --add-fixed --recursive sha256 "$TEST_ROOT/foo/bar")
    actual=$(nix-store --add-fixed --recursive sha256 "$TEST_ROOT/foo-link/bar")
    [[ "$expected" == "$actual" ]]
    actual=$(nix-store --add "$TEST_ROOT/foo-link/bar")
    [[ "$expected" == "$actual" ]]

    # nix store add
    actual=$(nix store add --hash-algo sha256 --mode nar "$TEST_ROOT/foo/bar")
    [[ "$expected" == "$actual" ]]

    # cleanup
    rm -r "$TEST_ROOT/foo" "$TEST_ROOT/foo-link"
}
test_issue_11941

# A symlink is added to the store as a symlink, not as a copy of the target
test_add_symlink() {
    ln -s /bin "$TEST_ROOT/my-bin"

    # legacy
    path=$(nix-store --add-fixed --recursive sha256 "$TEST_ROOT/my-bin")
    [[ "$(readlink "$path")" == /bin ]]
    path=$(nix-store --add "$TEST_ROOT/my-bin")
    [[ "$(readlink "$path")" == /bin ]]

    # nix store add
    path=$(nix store add --hash-algo sha256 --mode nar "$TEST_ROOT/my-bin")
    [[ "$(readlink "$path")" == /bin ]]

    # cleanup
    rm "$TEST_ROOT/my-bin"
}
test_add_symlink

#### New style commands

clearStoreIfPossible

(
    path1=$(nix store add ./dummy)
    path2=$(nix store add --mode nar ./dummy)
    path3=$(nix store add-path ./dummy)
    [[ "$path1" == "$path2" ]]
    [[ "$path1" == "$path3" ]]
    path4=$(nix store add --mode nar --hash-algo sha1 ./dummy)
)
(
    path1=$(nix store add --mode flat ./dummy)
    path2=$(nix store add-file ./dummy)
    [[ "$path1" == "$path2" ]]
    path4=$(nix store add --mode flat --hash-algo sha1 ./dummy)
)
(
    path1=$(nix store add --mode text ./dummy)
    path2=$(nix eval --impure --raw --expr 'builtins.toFile "dummy" (builtins.readFile ./dummy)')
    [[ "$path1" == "$path2" ]]
)

# Root publication belongs to the native add operation. These controls only
# collect the functional test's private store, never an installed system store.
needLocalStore "store add root controls need the private local GC"
[[ "$NIX_STORE_DIR" == "$TEST_ROOT/"* ]]
[[ "$NIX_STATE_DIR" == "$TEST_ROOT/"* ]]
rootWork=$TEST_ROOT/add-root
mkdir "$rootWork"
printf 'existing\n' > "$rootWork/existing"
printf 'new\n' > "$rootWork/new"
printf 'garbage\n' > "$rootWork/garbage"
existing=$(nix store add "$rootWork/existing")
garbage=$(nix store add "$rootWork/garbage")

# GC has already read its permanent and temporary root sets when it opens this
# FIFO. Keep the writer open while both an existing and a new path are rooted.
# Their safety therefore depends on registering new roots with the live GC.
mkfifo "$rootWork/gc-ready"
_NIX_TEST_GC_SYNC_2="$rootWork/gc-ready" nix-store --gc > "$rootWork/gc.log" 2>&1 &
gcPid=$!
exec 9> "$rootWork/gc-ready"
rootedExisting=$(nix store add --out-link "$rootWork/existing-root" "$rootWork/existing")
[[ "$rootedExisting" == "$existing" ]]
rootedNew=$(cd "$rootWork" && nix store add --out-link new-root ./new)
[[ $(readlink "$rootWork/existing-root") == "$existing" ]]
[[ $(readlink "$rootWork/new-root") == "$rootedNew" ]]
exec 9>&-
wait "$gcPid"
test -e "$existing"
test -e "$rootedNew"
test ! -e "$garbage"
[[ $(nix hash path "$rootWork/new") == $(nix hash path "$rootedNew") ]]

# After the producing process has exited, only the permanent links protect
# these paths. A second collection must keep them; removing the links must not.
nix-store --gc
test -e "$existing"
test -e "$rootedNew"
rm "$rootWork/existing-root" "$rootWork/new-root"
nix-store --gc
test ! -e "$existing"
test ! -e "$rootedNew"

# Pause a fresh process after addToStoreSlow has returned an ALREADY VALID
# path, before addPermRoot can register its own temporary root. Removing the
# early addTempRoot call must let this GC delete the path and fail the control.
printf 'handoff\n' > "$rootWork/handoff"
handoff=$(nix store add "$rootWork/handoff")
mkfifo "$rootWork/handoff-ready"
_NIX_TEST_STORE_ADD_ROOT_SYNC="$rootWork/handoff-ready" nix store add --out-link "$rootWork/handoff-root" "$rootWork/handoff" > "$rootWork/handoff.stdout" 2> "$rootWork/handoff.stderr" &
addPid=$!
exec 9> "$rootWork/handoff-ready"
nix-store --gc
survived=0
if test -e "$handoff"; then survived=1; fi
# Always release and reap the controlled process, including the negative mutant.
exec 9>&-
wait "$addPid"
[[ "$survived" == 1 ]]
[[ $(cat "$rootWork/handoff.stdout") == "$handoff" ]]
[[ $(readlink "$rootWork/handoff-root") == "$handoff" ]]
test -e "$handoff"

# Match build's root replacement semantics for an existing store symlink.
first=$(nix store add --out-link "$rootWork/result" "$rootWork/existing")
second=$(nix store add --out-link "$rootWork/result" "$rootWork/new")
[[ "$first" != "$second" ]]
[[ $(readlink "$rootWork/result") == "$second" ]]

# Failed publication must not print a successful store path or replace an
# unrelated file, directory or non-store symlink.
printf 'keep\n' > "$rootWork/occupied-file"
mkdir "$rootWork/occupied-directory"
ln -s "$rootWork/existing" "$rootWork/occupied-link"
for occupied in occupied-file occupied-directory occupied-link; do
    if nix store add --out-link "$rootWork/$occupied" "$rootWork/new" > "$rootWork/refused.stdout" 2> "$rootWork/refused.stderr"; then
        echo "store add accepted an occupied root: $occupied" >&2
        exit 1
    fi
    test ! -s "$rootWork/refused.stdout"
done
[[ $(cat "$rootWork/occupied-file") == keep ]]
test -d "$rootWork/occupied-directory"
[[ $(readlink "$rootWork/occupied-link") == "$rootWork/existing" ]]
if nix store add --dry-run --out-link "$rootWork/dry-root" "$rootWork/new" > "$rootWork/dry.stdout" 2> "$rootWork/dry.stderr"; then
    echo 'store add accepted --dry-run with --out-link' >&2
    exit 1
fi
test ! -s "$rootWork/dry.stdout"
test ! -e "$rootWork/dry-root"
