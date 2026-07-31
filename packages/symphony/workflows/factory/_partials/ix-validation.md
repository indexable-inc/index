## ix validation contract

- Required CI check is **`build`** = `scripts/ci/run-required-ci-checks.sh`
  (which runs `nix build .#required-ci-checks`). That is the only gate the merge
  queue enforces.
- In the inner edit loop, do NOT run the full gate every time - it is slow.
  Validate only the crates you touched:
  `nix build '.#affected { packages = [<changed crates>] }'`.
  The scope/plan names the affected crates; trust it.
- Merge strategy is **squash via the merge queue (ALLGREEN)**. A push to `main`
  auto-deploys, so a red `build` after merge is a real incident - be confident
  before you mark a PR mergeable.
- Auto-merge guardrails (inherited from `ix-playbook-agent`): NEVER edit
  `.github/**` or `CODEOWNERS` in an auto-mergeable PR, and honor a
  `manual-merge` label as a human override - if present, stop and escalate.
- House style: Conventional Commits for the PR title and commits.
