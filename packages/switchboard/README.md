# switchboard

Cross-platform chatrooms: one room spanning Slack, email, and AI agents, over
one canonical message IR. Design record: [ENG-7479](https://linear.app/indexable/issue/ENG-7479/switchboard-cross-platform-chatrooms-slack-email-imessage-ai-with).

## Architecture

LLVM-shaped: N frontends lower into one IR; a router applies room policy; N
backends render back out. Adding a platform means writing one adapter --
nothing else changes.

```
 Slack ──┐                                              ┌── Slack
 email ──┤  frontends lower to IR   ┌────────┐  fan-out ├── email
 memory ─┤ ───────────────────────▶ │ router │ ────────▶├── memory
 (next: matrix, imessage, ...)      └───┬────┘          └── ...
                                        │
                                 agent participants
                              (reply re-enters fan-out)
```

- **IR** (`ir.py`): `Identity` (canonical id + per-platform handles), `Room`
  (members with roles member/guest/agent; bindings to platform channels with
  per-binding forwarding rules), `Message` (body, resolved sender, thread ref,
  attachments, reactions, **provenance**: origin platform/message id + hop
  list).
- **Adapters** (`adapter.py`): `connect`/`close` lifecycle, `send(binding,
  message) -> platform_msg_id`, inbound delivery via an async callback the
  router subscribes. `memory.py` is the deterministic test double; `slack.py`
  posts via `chat.postMessage` and polls `conversations.history` with a ts
  cursor; `email.py` sends over SMTP and polls IMAP with a UID cursor, one
  room binding = one mailbox, threading via
  `Message-ID`/`In-Reply-To`/`References` and a `[sb:<room>]` subject tag.
- **Agents** (`agent.py`): an `Identity` of kind agent plus a pluggable async
  responder (deterministic echo stub by default; the model-backed responder is
  a `llm_responder` seam, deliberately unimplemented in v0).
- **Router** (`router.py`): lowers inbound to IR, resolves the sender
  identity, applies direction + allow/deny rules, fans out to every other
  binding and to agent members, stamps provenance hops. Echo loops are
  impossible twice over: provenance blocks re-delivery to any visited binding,
  and a sent-id registry drops polled copies of our own messages. Messages
  marked internal (`[internal]` body prefix) never reach guest-facing
  bindings.

## Environment

No secrets in code; everything is env-configured and only read when the real
transports are used (tests inject fakes, so CI never touches the network).

| Variable | Purpose |
| --- | --- |
| `SWITCHBOARD_SLACK_TOKEN` | Slack bot token (`chat.postMessage`, `conversations.history`) |
| `SWITCHBOARD_SMTP_HOST` / `SWITCHBOARD_SMTP_PORT` | outbound mail relay (default port 587, STARTTLS) |
| `SWITCHBOARD_SMTP_USERNAME` / `SWITCHBOARD_SMTP_PASSWORD` | optional SMTP auth |
| `SWITCHBOARD_IMAP_HOST` / `SWITCHBOARD_IMAP_PORT` | inbound mailbox (default port 993, SSL) |
| `SWITCHBOARD_IMAP_USERNAME` / `SWITCHBOARD_IMAP_PASSWORD` | optional IMAP auth |

## Tests

Hermetic (in-memory adapters, `httpx.MockTransport`, fake SMTP/IMAP):

```sh
python -m pytest packages/switchboard/tests -q     # locally, with pydantic+httpx+pytest
nix build .#switchboard.tests.pytest               # the CI-shaped run
nix build .#switchboard.tests.typecheck            # zuban --strict + ruff ANN
```
