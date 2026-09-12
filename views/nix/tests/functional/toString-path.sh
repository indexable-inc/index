#!/usr/bin/env bash

source common.sh

requireJj
jjConfig

mkdir -p "$TEST_ROOT"/foo
echo bla > "$TEST_ROOT"/foo/bar

# A plain directory has no identity a lock file can name: the `path` scheme
# serves store objects only and refuses it, naming the fix.
expectStderr 1 nix eval --raw --impure --expr "builtins.toString (builtins.fetchTree { type = \"path\"; path = \"$TEST_ROOT/foo\"; })" \
    | grepQuiet "mutable directory"

# Given one, `toString` of the fetched tree is a path that reads through the
# mount: the file and the directory listing, with the repository's own
# metadata (`.jj`) absent from the tree.
jjInit "$TEST_ROOT"/foo

[[ $(nix eval --raw --impure --expr "builtins.readFile (builtins.toString (builtins.fetchTree { type = \"jj\"; url = \"file://$TEST_ROOT/foo\"; } + \"/bar\"))") = bla ]]

[[ $(nix eval --json --impure --expr "builtins.readDir (builtins.toString (builtins.fetchTree { type = \"jj\"; url = \"file://$TEST_ROOT/foo\"; }))") = '{"bar":"regular"}' ]]
