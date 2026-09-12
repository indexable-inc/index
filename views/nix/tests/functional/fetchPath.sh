#!/usr/bin/env bash

source common.sh

# `path:` serves store objects and nothing else (src/libfetchers/path.cc). A
# store object is fixed once it is registered, so two reads of one input see
# one tree. A directory on the filesystem has no such identity: nothing holds
# it still while an evaluation reads it, and the copy that used to paper over
# that cost a full read of the tree per evaluation without closing the race.
# So every other absolute path is refused, in the fetcher, where the fix can
# be named.

# A store object fetches, and has no last-modified time of its own (every file
# in the store is stamped at the epoch), so `lastModified` is 0 unless the
# caller supplies one.
touch "$TEST_ROOT/foo" -t 202211111111
storePath=$(nix store add-path "$TEST_ROOT/foo")

[[ "$(nix eval --impure --expr "(builtins.fetchTree \"path://$storePath\").lastModified")" = 0 ]]

# Check that we can override lastModified for "path:" inputs.
[[ "$(nix eval --impure --expr "(builtins.fetchTree { type = \"path\"; path = \"$storePath\"; lastModified = 123; }).lastModified")" = 123 ]]

# Anything else has no identity to lock to and is refused rather than copied.
# Every match below is a short fragment: these messages are prose, and pinning
# a whole sentence would break on a rewording that changes no behaviour.
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchTree \"path://$TEST_ROOT/foo\").outPath" \
    | grepQuiet "has no identity a lock file can name"

# The refusal says what it was handed. `$TEST_ROOT/foo` is a file; a directory
# is the case that used to work and is the one people will hit.
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchTree \"path://$TEST_ROOT/foo\").outPath" \
    | grepQuiet "is a mutable path"

mkdir -p "$TEST_ROOT/plain"
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchTree \"path://$TEST_ROOT/plain\").outPath" \
    | grepQuiet "is a mutable directory"

# ...and it names the fix for the directory in front of it. Which advice comes
# out is decided by the presence of `.git` or `.jj` and by nothing else, so
# these fixtures are the whole input to that decision. That `jj init` really
# produces a `.jj` is fetchJj.sh's subject; keeping it out of here is what lets
# this file run on a machine with neither tool installed.
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchTree \"path://$TEST_ROOT/plain\").outPath" \
    | grepQuiet "Give it an identity first"

mkdir -p "$TEST_ROOT/gitdir/.git"
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchTree \"path://$TEST_ROOT/gitdir\").outPath" \
    | grepQuiet "It is a Git working tree"

mkdir -p "$TEST_ROOT/jjdir/.jj"
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchTree \"path://$TEST_ROOT/jjdir\").outPath" \
    | grepQuiet "It is a Jujutsu workspace"

# A colocated repository has both directories and is Git-backed, so `.git`
# wins. That is also the precedence a bare path gets from flakeref.cc, and the
# jj fetcher refuses such a repository naming `git+file` itself.
mkdir -p "$TEST_ROOT/colocated/.git" "$TEST_ROOT/colocated/.jj"
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchTree \"path://$TEST_ROOT/colocated\").outPath" \
    | grepQuiet "It is a Git working tree"

# A path that is not there says so, rather than describing what it would have
# been had it existed.
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchTree \"path://$TEST_ROOT/nonexistent\").outPath" \
    | grepQuiet "does not exist"

# A relative path is refused before any of that. It names a directory inside
# another tree, so it resolves only as a flake input, through the parent flake
# that declares it (libflake/flake.cc, resolveRelativePath); anything that
# reaches this scheme has no parent to resolve against.
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchTree { type = \"path\"; path = \"./plain\"; }).outPath" \
    | grepQuiet "it uses a relative path"
