# Factory: autonomous SDLC workflow (ENG-2921)

Status: design draft. Source: [ENG-2921](https://linear.app/indexable/issue/ENG-2921/agent-workflow).

A self-driving software development loop built as a Symphony pack. Five agent
roles plus two continuous feeders take an idea from conception to a
green, auto-merged PR on `main`, with bounded self-correction loops and
human escape hatches.

```
Idea generator (code . exa . prior art)
        | new
        v
   Researcher  <---- issues ---- Cleaner (slop . CI . tests . bugs)  (runs continuously)
 (deep research . scope)
        | scoped              ^
        v                     | new issues
     Worker --------------------+
 (execute . bench . test)
   | completed      | denied -> Denied
   v
 Reviewer / judge  <-- suggestions (bounded loop) -- Worker
 (checks code + metrics)
   | merged (green)
   v
 Merged to main (auto-deploy follows)
```

## Locked decisions

| Decision | Choice | Rationale |
|---|---|---|
| Runtime | Symphony (`packages/symphony`) | Already a durable DAG runtime with Linear/GitHub/cron triggers, worktrees, dashboard. We write a pack, not a runtime. |
| Target repo | `ix` (`indexable-inc/ix`) | Primary product; has a working bot-PR + auto-merge precedent. |
| Merge policy | Auto-merge if green | Reuse the proven `ix-playbook-agent[bot]` path, generalized from playbook-only to code. |
| First build | Full skeleton, thin | All five roles present end-to-end on one trivial issue, then deepen each. |
| Loop model | All in one run | Per-issue pipeline = one Symphony run; Worker<->Reviewer is a bounded, unrolled `when`-cascade (NOT recursion - see below). Feeders are separate cron runs. |
| Bot identity | Reuse `ix-playbook-agent` | Auto-merge infra already works; widen its scope. |

## Two clocks

1. **Continuous feeders** (Idea generator, Cleaner): `on cron` workflows. Each
   run inspects ix + prior art, dedups against open AND closed Linear issues,
   and creates at most one new issue tagged `factory:scope-needed`.
2. **Per-issue pipeline** (Researcher -> Worker <-> Reviewer -> merge): one
   Symphony run per issue, triggered `on linear { label: "factory:scope-needed" }`.
   The Worker<->Reviewer correction loop is bounded recursion inside that run.

Linear is the durable message bus. Labels are the wire (`on linear` triggers on
a label, not a state). We keep human-readable workflow **states** in sync for
the board, but the **label** is the machine trigger.

### Label protocol (the wire)

| Label | Set by | Triggers |
|---|---|---|
| `factory:scope-needed` | Idea gen / Cleaner | Researcher (pipeline run) |
| `factory:ready-for-dev` | Researcher | Worker (within same run) |
| `factory:in-review` | Worker (PR opened) | Reviewer (within same run) |
| `factory:needs-human` | any role on cap/escalation | human |
| `factory:denied` | Worker / Researcher | terminal |

## Role -> skill mapping

Each role is a Symphony **skill** (markdown + YAML frontmatter) invoked by an
`agent` node. Every skill returns a **structured decision** (an enum + payload)
as its final output so downstream `when` gates are deterministic.

| Role | Engine/effort | Permissions | Key tools | Decision output |
|---|---|---|---|---|
| Idea generator | high | workspace read | exa/web, code read | `{created: bool, issue_id?}` |
| Cleaner | medium | workspace read | code read, CI/log read | `{created: bool, issue_id?}` |
| Researcher | high | workspace read | explore, deep-research, linear_graphql | `{decision: scoped\|rejected, plan, affected_crates, bench_cmd?}` |
| Worker | high | workspace write | edit, bash, git worktree, gh | `{decision: completed\|denied\|delegate, pr_url?, new_issues?}` |
| Reviewer/judge | high | workspace write | code review, bench, gh | `{decision: approved\|needs_changes, findings[], metrics?}` |

## How loops work (Symphony is acyclic - this is the crux)

Symphony has **no backward edges, ever**. There is no loop or recursion
primitive. Loop-like behavior comes from two forward-only mechanisms:

1. **Lazy gate re-expansion.** A `when ${x.flag} { ... }` emits a `:gate`
   placeholder; when the gating agent output arrives, the `Materializer`
   re-expands the AST and emits the body nodes then, with stable
   content-derived ids (`interpreter.ex` `expand_effect :when`,
   `materializer.ex` `expand_dynamic`). The graph grows forward; nothing
   points back.
2. **`subrun`** spawns a whole nested child run (a fresh DAG). It is NOT a
   self-loop: `subrun_runner.ex` has a cycle guard that rejects any child
   whose name is already on the ancestor chain (self-subrun is rejected
   first) and a depth ceiling (`SYMPHONY_SUBRUN_MAX_DEPTH`, default 8). Use
   it for distinct child workflows, never for the correction loop.

### Bounded correction loop = unrolled `when`-cascade (one run)

The tight Worker<->Reviewer loop is **statically unrolled** to `MAX_ROUNDS`
nested `when` blocks. Later rounds only materialize if earlier reviews asked
for changes, so we pay only for rounds we use. The nesting depth IS the cap -
it cannot run away.

```
workflow "task-pipeline" on linear { label: "factory:ready-for-dev" } {
  impl    <- agent { prompt: skill "worker" }
  review1 <- agent { prompt: skill "reviewer"; inputs: { pr: impl.pr } }
  r1 <- when ${review1.needs_changes} {
    fix1    <- agent { prompt: skill "worker-fix"; inputs: { findings: review1.findings } }
    review2 <- agent { prompt: skill "reviewer"; inputs: { pr: fix1.pr } }
    r2 <- when ${review2.needs_changes} {
      fix2    <- agent { prompt: skill "worker-fix"; inputs: { findings: review2.findings } }
      review3 <- agent { prompt: skill "reviewer"; inputs: { pr: fix2.pr } }
      r3 <- when ${review3.needs_changes} {
        escalate <- agent { prompt: skill "escalate" }   # MAX_ROUNDS hit -> factory:needs-human
      }
    }
  }
}
```

`MAX_ROUNDS` (start at 4) is the convergence guarantee. Keep it DRY with a
`_partials/review-round.md` template. Determinism holds: each round has
distinct inputs, so distinct stable node ids.

### Open-ended macro loops = cross-run re-triggering

The genuinely open-ended arrows (Worker -> delegate back to Researcher,
re-validation after new issues) are NOT one run. Each is a Linear **label
flip that starts a fresh top-level run**. The loop counter lives in Linear (a
round-count comment/field); each run reads it and bails to
`factory:needs-human` past a threshold. Structurally unbounded, so the
Linear-stored counter is the bound.

## ix validation contract (what Worker/Reviewer must satisfy)

- **Required check**: `build` = `scripts/ci/run-required-ci-checks.sh` -> `nix build .#required-ci-checks`.
- **Inner-loop validation** (cheap): `nix build '.#affected { packages = [<changed crates>] }'` so the Worker does not pay the full 180-min gate on every iteration. Researcher declares `affected_crates`.
- **Merge**: squash via merge queue (ALLGREEN). `main` push auto-deploys (`auto-deploy.yml`).
- **Auto-merge guardrails** (inherit from playbook-agent): never touch `.github/**` or CODEOWNERS in an auto-mergeable PR; honor a `manual-merge` label as human override.
- **Metrics**: ix has no standardized CI bench suite. The Researcher declares an optional `bench_cmd` per issue; Reviewer reports `metrics: n/a` when absent. (Do not block the skeleton on a metrics harness.)

## Safety rails (non-negotiable)

- Every loop has a hard depth/iteration cap + `factory:needs-human` escalation.
- Per-issue token/time budget; exceed -> escalate.
- Dedup at the source (Idea gen + Cleaner) against open and closed issues. Steal the pattern from ix's `antithesis-triage-linear` skill.
- Atomic claim: Worker sets assignee + label, then re-reads to confirm it won (Symphony runs are parallel).
- Auto-merge blast radius limited by the playbook guardrails above.
- All nondeterminism lives in `agent` nodes; workflow topology and loop bounds are static.

## Pack layout

```
packages/symphony/workflows/factory/
  repositories.yaml          # primary: ix
  workflows/
    idea-generator.sym       # on cron
    cleaner.sym              # on cron
    task-pipeline.sym        # on linear { label: factory:ready-for-dev }; unrolled when-cascade
  skills/
    idea-generator.md
    cleaner.md
    researcher.md
    worker.md
    worker-fix.md
    reviewer.md
    escalate.md
  _partials/
    decision-contract.md     # shared "return this JSON shape" instructions
    ix-validation.md         # shared validation commands
```

Point the runtime at it with `SYMPHONY_PACK_DIR=packages/symphony/workflows/factory`.

## Thin-skeleton build sequence

1. Scaffold the pack dir + `repositories.yaml` (ix) + shared partials.
2. Write the five skill stubs with real envelopes and the decision contract.
3. `task-pipeline.sym`: Researcher -> Worker -> `review-round` subrun -> squash-merge.
4. `idea-generator.sym` + `cleaner.sym` cron stubs that each create one deduped issue.
5. Wire triggers + reuse the playbook-agent bot for PR/auto-merge.
6. Prove end-to-end on one trivial ix issue (idea -> issue -> scope -> tiny change -> PR -> green -> merge), then deepen each skill.

## Open questions

- `MAX_ROUNDS` and per-issue budget starting values (proposed: 4 rounds).
- Cron cadence for feeders (proposed: idea gen daily, cleaner every few hours).
- Whether Researcher auto-promotes to `ready-for-dev` or pauses for human sign-off on scope in v1.
- RESOLVED: self-subrun is forbidden by the cycle guard; the loop is an unrolled `when`-cascade instead.
