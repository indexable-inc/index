#!/usr/bin/env bash

source common.sh

clearStoreIfPossible
clearCache

(( $(nix search -f search.nix '' hello | wc -l) > 0 ))

# Check descriptions are searched
(( $(nix search -f search.nix '' broken | wc -l) > 0 ))

# Check search that matches nothing
(( $(nix search -f search.nix '' nosuchpackageexists | wc -l) == 0 ))

# Search for multiple arguments
(( $(nix search -f search.nix '' hello empty | wc -l) == 2 ))

# Multiple arguments will not exist
(( $(nix search -f search.nix '' hello broken | wc -l) == 0 ))

# No regex should return an error
(( $(nix search -f search.nix '' | wc -l) == 0 ))

## Search expressions

# Check that empty search string matches all
nix search -f search.nix '' ^ | grepQuiet foo
nix search -f search.nix '' ^ | grepQuiet bar
nix search -f search.nix '' ^ | grepQuiet hello

## Multiple regexes still select once when matches overlap. The Rust renderer
## unit tests cover highlight spans; redirected command output remains plain.
[[ $(nix search -f search.nix '' 'oo' 'foo' 'oo' | grep -c '^\* foo ') == 1 ]]
[[ $(nix search -f search.nix '' 'broken b' 'en bar' | grep -c 'broken bar') == 1 ]]
[[ $(nix search -f search.nix '' 'o' | grep -c '^\* ') == 3 ]]
[[ $(nix search -f search.nix '' 'b' | grep -c '^\* ') == 1 ]]
! nix search -f search.nix '' '^' | grep -q $'\x1b'

## Tests for --exclude
(( $(nix search -f search.nix ^ -e hello | grep -c hello) == 0 ))

(( $(nix search -f search.nix foo ^ --exclude 'foo|bar' | grep -Ec 'foo|bar') == 0 ))
(( $(nix search -f search.nix foo ^ -e foo --exclude bar | grep -Ec 'foo|bar') == 0 ))
[[ $(nix search -f search.nix '' ^ -e bar --json | jq -c 'keys') == '["foo","hello"]' ]]
