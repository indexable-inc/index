# Gate for flake surfaces that need `packages.<system>.nix-ix`. Fork-only syntax
# (underscore digit separators, patch 0014) fails at parse time, before any
# eval-time check can run, so the gate sits at the import seam of each
# fork-syntax island, and this file itself must stay parseable by every
# evaluator: frozen syntax only, the same trick as nixpkgs' `lib/minver.nix`.
#
# Detection reads the `+ix` fork suffix (optionally followed by a revision)
# out of `builtins.nixVersion` instead of
# feature-detecting a fork-added builtin: a new builtin exists only in fork
# builds the fleet is not running yet, so a `builtins ? ixVersion` gate would
# reject every currently deployed evaluator until a nix upgrade rolls out
# (index#3635).
{
  /**
  `require surface value` returns `value` when the evaluator is the indexable
  Nix fork and throws an actionable install message otherwise. Wrap the
  *import* of a fork-syntax island (`ix.evaluatorGate.require "tests" (import
  paths.tests {...})`) so stock Nix hits the message when the surface is
  forced, not a bare parse error: function application only forces `value`
  after the check passes.
  */
  require = surface: value:
    if builtins ? nixVersion && builtins.match ".*[+]ix([.].+)?" builtins.nixVersion != null
    then value
    else
      throw ''
        the `${surface}` surface of this flake requires the indexable Nix
        fork: it uses language patches (packages/nix/patches, e.g.
        underscore digit separators) that this evaluator
        (Nix ${builtins.nixVersion or "unknown"}) cannot parse.

        Stock Nix can still build the fork, but this flake cannot hand it to
        you: the jj tree ABI archive the fork links lives in the ix
        repository, which is not public. With ix repository access, install
        it from there:

            nix profile install github:indexable-inc/ix/warmed-main#nix-ix

        Without that access there is no install route for the fork today, so
        the surfaces behind this gate are team-only until one exists.

        The bootstrap surface stays stock-parseable by policy, enforced by
        `checks.<system>.stock-nix-parse-*` (index#3635).
      '';
}
