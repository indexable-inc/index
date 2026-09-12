# Rule self-test for the astlog lint rules (nix.astlog + rust.astlog):
# every (lint ...) declaration must have a committed fixture pair and
# fire exactly on the violating one, driven through the same `astlog
# scan --json` surface the lint gate uses. A lint that never fires in
# tests is unproven (its query may have silently stopped matching), so
# a missing or non-firing fixture fails the build, as does a rule
# without a lint declaration (it would silently drop out of the gate).
# Fixtures are stored as `.fixture` (not `.nix`/`.rs`) so the repo lint
# stages (alejandra / statix / deadnix / astlog itself) never scan the
# deliberately-violating snippets; the check stages each back to its
# ruleset's extension (`.nix` for nix.astlog, `.rs` for rust.astlog --
# astlog selects the grammar by file extension) before running the
# binary. `scan` exits nonzero on the violating fixture by design, so
# the jq pipelines deliberately take the JSON regardless of exit code.
{
  lib,
  pkgs,
  mkCheck,
  astlog,
}: let
  fs = lib.fileset;
  # Just the rules files plus their fixture pairs (this self-test excluded),
  # so the check only rebuilds when the rules or fixtures change, not on
  # every tracked-file edit the way a whole-tree source would.
  rulesSource = fs.toSource {
    root = ./.;
    fileset = fs.difference ./. ./checks.nix;
  };
in {
  astlog-rules = mkCheck "astlog-rules" {
    nativeBuildInputs = [
      astlog
      pkgs.jq
    ];
    script = ''
      root=${rulesSource}
      tests="$root/tests"
      fail=0
      # Each ruleset paired with the source extension its fixtures take.
      check_ruleset() {
        rules="$1"
        ext="$2"
        # Rules without a (lint ...) are legitimate helper relations
        # (joins/negation need intermediate relations), so they are not
        # required to back a lint. The meaningful checks remain: every
        # lint has a good/bad fixture pair that fires/stays-clean, and
        # every fixture dir backs some lint. `astlog` itself rejects a
        # lint that names an undefined relation at parse time.
        for rule in $(sed -n 's/^(lint \([a-z0-9-]*\).*/\1/p' "$rules" | sort -u); do
          dir="$tests/$rule"
          if [ ! -f "$dir/bad.fixture" ] || [ ! -f "$dir/good.fixture" ]; then
            echo "lint $rule has no fixture pair under astlog-rules/tests/$rule" >&2
            fail=1
            continue
          fi
          work=$(mktemp -d)
          cp "$dir/bad.fixture" "$work/bad.$ext"
          cp "$dir/good.fixture" "$work/good.$ext"
          # `astlog scan` exits nonzero on a violating fixture by
          # design; capture its JSON (`|| true` so the by-design exit
          # does not abort the `set -o pipefail` build) and count
          # separately, rather than piping straight into jq.
          bad_json=$(astlog scan "$rules" "$work/bad.$ext" --json || true)
          good_json=$(astlog scan "$rules" "$work/good.$ext" --json || true)
          bad=$(jq --arg r "$rule" '[.[] | select(.rule == $r)] | length' <<<"$bad_json")
          good=$(jq --arg r "$rule" '[.[] | select(.rule == $r)] | length' <<<"$good_json")
          if [ "$bad" = 0 ]; then
            echo "lint $rule did not fire on its violating fixture" >&2
            fail=1
          fi
          if [ "$good" != 0 ]; then
            echo "lint $rule fired $good finding(s) on its valid fixture" >&2
            fail=1
          fi
        done
      }
      check_ruleset "$root/nix.astlog" nix
      check_ruleset "$root/rust.astlog" rs
      check_ruleset "$root/cargo.astlog" toml
      check_ruleset "$root/elixir.astlog" ex
      check_ruleset "$root/shell-fence.astlog" nix
      # Every fixture dir must back a lint in one of the rulesets.
      for dir in "$tests"/*/; do
        rule=$(basename "$dir")
        if ! grep -q "^(lint $rule " "$root/nix.astlog" "$root/rust.astlog" "$root/cargo.astlog" "$root/elixir.astlog" "$root/shell-fence.astlog"; then
          echo "fixture dir astlog-rules/tests/$rule matches no lint" >&2
          fail=1
        fi
      done
      if [ "$fail" != 0 ]; then
        exit 1
      fi
    '';
  };
}
