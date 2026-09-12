#!/usr/bin/env bash

source common.sh

for ext in so dylib; do
    plugin="${_NIX_TEST_BUILD_DIR}/plugins/libplugintest.$ext"
    [[ -f "$plugin" ]] && break
done

res=$(nix --option plugin-files "$plugin" config show setting-set)
[[ "$res" == false ]]

res=$(nix --option setting-set true --option plugin-files "$plugin" config show setting-set)
[[ "$res" == true ]]
