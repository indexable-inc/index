# shellcheck shell=bash

# Shared fixture for the jj-tree suite: a jj repository on jj's native
# (ix-local) store, the only kind whose trees carry the blake3 ids these tests
# are about. Every fixture is created and edited with the jj CLI; the fetcher
# under test never runs it.

source ../common.sh

TODO_NixOS

requireJj

clearStoreIfPossible

# `jj`, `jjConfig` and `jjInit` come from common/functions.sh: one fixture
# for every jj test.
jjConfig

# $1: fetchTree URL; $2: extra fetchTree fields; $3: attribute to read.
#
# `unsafeDiscardStringContext`, deliberately. `outPath` is a string carrying
# its store path as context, and `nix eval` forces every lazy path in its
# result's context into the store before it exits (`ensureLazyPathsCopied`):
# printing an outPath is, by itself, a copy. These tests are about the mount
# staying lazy until something forces it, so the probe reads the path's
# NAME without making that promise; the forcing is done by the derivations
# the tests write for it, where it can be seen.
fetchAttr() {
    nix eval --extra-experimental-features fetch-tree --impure --raw --expr \
        "builtins.unsafeDiscardStringContext (toString (builtins.fetchTree { type = \"jj\"; url = \"$1\"$2; }).$3)"
}

# The tree id of the working copy at jj URL $1, as SRI (`blake3-...`), the form
# `treeHash` takes in a lock file. The jj CLI exposes no tree id template, so
# this comes from the fetcher; the independent oracle for the id is the
# own-repo equality in relative.sh (two fetch roads, one store path).
treeIdOf() {
    fetchAttr "$1" "" treeHash
}

# The `ca` field of a store path's registered info, as `method` and SRI `hash`.
caOf() {
    nix path-info --json --json-format 2 "$1" | jq -r '.info[].ca | "\(.method) \(.hash)"'
}
