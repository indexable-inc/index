#!/usr/bin/env bash
source common.sh
source rust-eval-lib.sh

work=$TEST_ROOT/rust-eval-command-search
rm -rf "$work"
jjFlakeDir "$work/flake"
system=$(nix-instantiate --eval --strict -E builtins.currentSystem | tr -d '"')
cat > "$work/flake/flake.nix" <<EOF_FLAKE
{
  outputs = { self }: let p = name: {
    type = "derivation";
    inherit name;
    drvPath = throw "search forced drvPath";
    meta.description = "Search fixture description";
  }; in {
    packages.$system = {
      alpha = p "alpha-1";
      ignored = { recurseForDerivations = true; hidden = p "hidden-1"; };
    };
    legacyPackages.$system = {
      beta = p "beta-2";
      group = { recurseForDerivations = true; deep = p "deep-3"; };
      ignored.hidden = throw "unmarked nested subtree was forced";
    };
  };
}
EOF_FLAKE
cached=$(rustCachedArm search-catalogue)
runSearch() {
    local label=$1 pattern=$2
    NIX_CONFIG="$cached" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$label.stats" \
        nix search --json "jj+file://$work/flake" "$pattern" > "$work/$label.json"
}
runSearch cold alpha
jq -e --arg path "packages.$system.alpha" 'keys == [$path] and .[$path].pname == "alpha"' "$work/cold.json"
runSearch changed-regex beta
assertMemoServed "$work/changed-regex.stats"
jq -e --arg path "legacyPackages.$system.beta" 'keys == [$path]' "$work/changed-regex.json"
runSearch all '^'
assertMemoServed "$work/all.stats"
jq -e --arg system "$system" 'keys == ["legacyPackages.\($system).beta", "legacyPackages.\($system).group.deep", "packages.\($system).alpha"]' "$work/all.json"
NIX_CONFIG="$cached" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/text.stats" \
    nix search "jj+file://$work/flake" '^' -e beta > "$work/text.out"
assertMemoServed "$work/text.stats"
! grep -q $'\x1b' "$work/text.out"
for colorVariable in NO_COLOR NOCOLOR; do
    env "$colorVariable=1" NIX_CONFIG="$cached" nix search "jj+file://$work/flake" '^' > "$work/$colorVariable.out"
    ! grep -q $'\x1b' "$work/$colorVariable.out"
done
grepQuiet -F alpha "$work/text.out"
! grep -q beta "$work/text.out"
NIX_CONFIG="$cached" nix search --json "jj+file://$work/flake#legacyPackages.$system.group" '^' \
    | jq -e --arg path "legacyPackages.$system.group.deep" 'keys == [$path]'

cat > "$work/broken.nix" <<'EOF_NIX'
{
  good = { type = "derivation"; name = "good-1"; };
  bad = { type = "derivation"; name = "bad-1"; meta.description = throw "search metadata failed"; };
}
EOF_NIX
for attempt in cold repeat; do
    expectStderr 1 env NIX_CONFIG="$cached" nix search --json -f "$work/broken.nix" '' '^' \
        | grepQuiet -F 'search metadata failed'
done
cat > "$work/flag.nix" <<'EOF_NIX'
{ nested = { recurseForDerivations = "true"; }; }
EOF_NIX
expectStderr 1 env NIX_CONFIG="$cached" nix search -f "$work/flag.nix" '' '^' \
    | grepQuiet -F 'recurseForDerivations must be a Boolean'
expectStderr 1 env NIX_CONFIG="$cached" nix search --expr 'throw "must not evaluate"' '' '[' \
    | grepQuiet -F 'invalid search regular expression'
echo "rust-eval-command-search: ok"
