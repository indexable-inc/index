# factory pack

Autonomous SDLC factory for **ix**, implementing [ENG-2921](https://linear.app/indexable/issue/ENG-2921/agent-workflow).
Design doc: [`../../docs/factory-workflow.md`](../../docs/factory-workflow.md).

Five agent roles + two continuous feeders take an idea to a green, auto-merged
PR on `main`, with a bounded self-correction loop and human escape hatches.

## Run it

```sh
SYMPHONY_PACK_DIR=packages/symphony/workflows/factory nix run .#symphony
```

Requires (see `.env.example`): `LINEAR_API_KEY` + `LINEAR_WEBHOOK_SECRET`
(trigger + issue creation), GitHub App creds for the `ix-playbook-agent` bot
(PRs + auto-merge), and an authenticated `codex` on PATH.

## Shape

- `workflows/idea-generator.sym` - cron (daily). Files one deduped idea issue.
- `workflows/cleaner.sym` - cron (every 4h). Files one deduped cleanup issue.
- `workflows/task-pipeline.sym` - `on linear label "factory:scope-needed"`.
  Researcher -> Worker -> bounded review/fix cascade -> auto-merge. One run per
  issue. The Worker<->Reviewer loop is an UNROLLED `when` cascade
  (`MAX_ROUNDS = 3`) - Symphony is acyclic, so there is no recursion; the
  nesting depth is the hard convergence cap.
- `skills/*.md` - one per role, each returning a structured JSON decision that
  the `when` gates read.
- `_partials/` - shared `decision-contract` and `ix-validation` text.

## Label protocol (the wire)

| Label | Set by | Effect |
|---|---|---|
| `factory:scope-needed` | feeders / human | triggers `task-pipeline` |
| `factory:in-review` | Worker (PR open) | marks issue under review |
| `factory:needs-human` | escalate | human takeover |
| `factory:denied` | Researcher / Worker | terminal reject |

## Verify on first boot (runtime-dependent)

- Agent final-message JSON is parsed into the fields `when ${node.field}` reads.
- The Linear tool name (`linear_graphql`) and `web_search` match what the
  engine exposes; adjust skill frontmatter `tools:` if not.
- The reviewer's auto-merge action uses the existing `ix-playbook-agent` path;
  that ix-side workflow may need its scope widened from playbook-only to code.
