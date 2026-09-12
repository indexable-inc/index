#!/usr/bin/env bash

source common.sh

flakeDir="$TEST_HOME/flake"
# `path:` serves store objects only, so a plain directory is no longer a flake
# source (src/libfetchers/path.cc). jj supplies the identity, and the fixture
# goes on writing files exactly as before: every fetch snapshots the disk.
jjFlakeDir "${flakeDir}"
cp flake.nix "${_NIX_TEST_BUILD_DIR}/ca/config.nix" content-addressed.nix "${flakeDir}"

nix run --no-write-lock-file "jj+file://${flakeDir}#runnable"
