---
name: upstream-contribution
description: "Opening a pull request against a third-party upstream: their CONTRIBUTING and AI policy, contribution gates, AI disclosure, commit conventions, draft until CI is green, recording a rejection. Use before opening a PR against a repo we do not own."
---

## Upstream contribution

This covers a PR whose target is someone else's repo. For an owned repository,
follow that repository's local workflow instructions.

### Read their rules before writing anything

A PR that ignores a project's stated rules gets closed without review, whatever
the diff does. Read these on the upstream's default branch first:

- `CONTRIBUTING.md`, `.github/CONTRIBUTING.md`, or `docs/contributing.md`
- `AI_POLICY.md`, or whatever file the project uses for its AI or LLM policy
- `CODE_OF_CONDUCT.md`
- `.github/PULL_REQUEST_TEMPLATE.md`, plus any file under
  `.github/PULL_REQUEST_TEMPLATE/`

Their rules beat any habit from this repo. Where the two disagree, follow the
upstream.

### Check the gates that close a PR before anyone reads the diff

Each of these rejects a PR on process, so check them before writing code:

- Sign-off. A CLA to sign, or a DCO `Signed-off-by` trailer on every commit.
- A required issue, feature request, or design proposal ahead of a feature PR.
  btop wants a feature request the maintainer has accepted.
- A vouch or sponsorship step. ghostty auto-closes a first-time contributor's
  PR until a maintainer comments `!vouch` on a Vouch Request discussion.
- Whether unsolicited PRs are accepted at all. openai/codex closes them without
  review and points contributors at issues.
- Where a change is submitted. Mesa takes GitLab merge requests, and some
  projects take patches on a mailing list. `gh` opens neither.

A gate that needs a human conversation is not yours to clear. Stop and hand it
to the operator.

### Disclose AI assistance in the format the upstream asks for

The disclosure format is the upstream's choice, not ours. btop requires the PR
to carry an `[AI generated]` tag and blocks accounts that hide it. nix requires
an `Assisted-by:` trailer on the commit and a human writing the PR
communication. Mesa requires `Assisted-by:` or `Generated-by:` and bans
autonomous submissions. ghostty states its terms in `AI_POLICY.md`. Copy the
wording the project asks for.

When a project states no policy, disclose in the PR body anyway.

### Match their commit conventions

Read `git log --oneline -20` on the default branch and copy what you see:
subject prefix, subject length, and whether the project wraps the body. The
body says why the change is needed; the diff already says what it does. Add
tests wherever the project tests that area.

### One logical change per PR

Split unrelated work into separate PRs. If landing the change also needs a
dependency bump or a refactor, say so in the body and name the commit that
carries it.

### Open as a draft, then make their checks pass

A red PR sitting in a maintainer's queue reads as abandoned work. Open it with
`--draft`, watch the checks, fix what fails, then mark it ready:

```sh
gh pr checks <n> --repo <owner>/<repo> --watch
gh pr ready <n> --repo <owner>/<repo>
```

Upstream CI often enforces things ours does not, such as a formatter config or
a changelog entry. Fix those on the branch rather than asking a maintainer to
waive them.

### Answer the review, and record a rejection

Read review comments and reply to each one, including the ones you disagree
with. If the PR is closed unmerged, write down that it was rejected and why,
where the next agent will find it. Reopening or resubmitting the same change
without that record wastes the maintainer's time twice.

### In this repo

Maintained forks are jj views. The root `views.toml` owns each view's path,
transport, upstream remote, ref, and the recorded import. Inspect the local
series before choosing a contribution:

```sh
jj view status <view>
jj view patches <view> -r @ --output <empty dir> --archive <path>.tar.zst
```

(`view` is a verb of `jj` itself: every host installs ix's native client
under that name.)

The export is a `git format-patch`-shaped series with paths relative to the
subtree, applicable with `git am` onto the upstream commit named as
`upstream_commit` in its `manifest.json`. An upstream PR is an outward-facing
third-party action, so get the operator's approval for that PR, then push the
series to a fork with git tooling outside this repository; `jj view` has no
push leg by design.

When a PR is rejected or closed unmerged, record the upstream response and PR
URL in the issue that authorized the contribution. Link that issue from the
view commit description before doing more work on the change.
