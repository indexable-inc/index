"""End-to-end routing over in-memory adapters: one room, two platforms, one agent.

Two :class:`InMemoryAdapter` instances stand in for Slack and email (the room
sees only platform keys, so the router's behavior is identical to the real
adapters); the agent is a recording participant. Hermetic: no network, no env.
"""

from __future__ import annotations

import asyncio

from switchboard import (
    AgentParticipant,
    Direction,
    Directory,
    ForwardingRules,
    Identity,
    IdentityKind,
    InMemoryAdapter,
    Member,
    Message,
    Role,
    Room,
    RoomBinding,
    Router,
)
from switchboard.router import AGENT_ORIGIN_PLATFORM

SLACK_SIM = "sim-slack"
EMAIL_SIM = "sim-email"


def _identities() -> tuple[Identity, Identity, Identity]:
    alice = Identity(
        id="alice",
        display_name="Alice",
        handles={SLACK_SIM: "U1", EMAIL_SIM: "alice@corp.example"},
    )
    gwen = Identity(
        id="gwen",
        display_name="Gwen",
        handles={EMAIL_SIM: "gwen@ext.example"},
    )
    hermes = Identity(id="hermes", display_name="Hermes", kind=IdentityKind.AGENT)
    return alice, gwen, hermes


class Harness:
    """One wired room: a Slack-sim binding, a guest-facing email-sim binding,
    and an agent member whose responder records what it sees."""

    def __init__(
        self,
        *,
        email_rules: ForwardingRules | None = None,
        agent_reply: str | None = None,
        extra_bindings: tuple[RoomBinding, ...] = (),
    ) -> None:
        self.alice, self.gwen, self.hermes = _identities()
        self.slack_binding = RoomBinding(id="b-slack", platform=SLACK_SIM, address="C123")
        self.email_binding = RoomBinding(
            id="b-email",
            platform=EMAIL_SIM,
            address="room@corp.example",
            recipients=("gwen@ext.example",),
            guest_facing=True,
            rules=email_rules if email_rules is not None else ForwardingRules(),
        )
        self.room = Room(
            id="eng",
            name="Engineering",
            members=(
                Member(identity=self.alice),
                Member(identity=self.gwen, role=Role.GUEST),
                Member(identity=self.hermes, role=Role.AGENT),
            ),
            bindings=(self.slack_binding, self.email_binding, *extra_bindings),
        )
        self.router = Router(Directory([self.alice, self.gwen, self.hermes]), [self.room])
        self.slack = InMemoryAdapter(SLACK_SIM)
        self.email = InMemoryAdapter(EMAIL_SIM)
        self.router.attach(self.slack)
        self.router.attach(self.email)
        self.agent_saw: list[Message] = []

        async def responder(message: Message) -> str | None:
            self.agent_saw.append(message)
            return agent_reply

        self.router.register_agent(AgentParticipant(self.hermes, responder))


def test_message_crosses_platforms_with_mapped_identity_and_provenance() -> None:
    h = Harness()
    asyncio.run(h.slack.inject(h.slack_binding, "U1", "hello from slack"))

    deliveries = h.email.sent_on(h.email_binding)
    assert len(deliveries) == 1
    _, message = deliveries[0]
    assert message.sender == h.alice  # platform handle U1 -> canonical identity
    assert message.body == "hello from slack"
    assert message.provenance.origin_platform == SLACK_SIM
    assert [hop.binding_id for hop in message.provenance.hops] == ["b-slack", "b-email"]
    # The agent, a first-class member, saw it too.
    assert [m.body for m in h.agent_saw] == ["hello from slack"]


def test_agent_reply_fans_out_to_both_platforms() -> None:
    h = Harness(agent_reply="on it")
    asyncio.run(h.slack.inject(h.slack_binding, "U1", "please deploy"))

    slack_deliveries = h.slack.sent_on(h.slack_binding)
    assert [(m.sender.id, m.body) for _, m in slack_deliveries] == [("hermes", "on it")]
    email_deliveries = h.email.sent_on(h.email_binding)
    assert [(m.sender.id, m.body) for _, m in email_deliveries] == [
        ("alice", "please deploy"),
        ("hermes", "on it"),
    ]
    assert email_deliveries[1][1].provenance.origin_platform == AGENT_ORIGIN_PLATFORM
    # The agent never sees its own reply (one agent round per human message).
    assert len(h.agent_saw) == 1


def test_no_redelivery_to_origin_platform() -> None:
    h = Harness()
    asyncio.run(h.slack.inject(h.slack_binding, "U1", "hello"))
    assert h.slack.sent_on(h.slack_binding) == []  # A's message never re-delivered to A


def test_bounced_message_is_dropped() -> None:
    h = Harness()
    asyncio.run(h.slack.inject(h.slack_binding, "U1", "hello"))
    platform_message_id, _ = h.email.sent_on(h.email_binding)[0]

    # The email platform's poll now "sees" the message the router itself sent.
    asyncio.run(
        h.email.inject(
            h.email_binding,
            "room@corp.example",
            "[Alice] hello",
            platform_message_id=platform_message_id,
        )
    )
    assert h.slack.sent_on(h.slack_binding) == []  # not re-forwarded
    assert len(h.email.sent_on(h.email_binding)) == 1  # nothing new anywhere
    assert len(h.agent_saw) == 1  # the agent saw the original only


def test_internal_only_withheld_from_guest_binding() -> None:
    notes = RoomBinding(id="b-notes", platform=SLACK_SIM, address="C-notes")
    h = Harness(extra_bindings=(notes,))
    asyncio.run(h.slack.inject(h.slack_binding, "U1", "[internal] rotating the deploy key"))

    assert h.email.sent_on(h.email_binding) == []  # the guest-facing leg gets nothing
    internal_deliveries = h.slack.sent_on(notes)
    assert len(internal_deliveries) == 1  # internal bindings still get it
    assert internal_deliveries[0][1].internal_only
    assert len(h.agent_saw) == 1  # agents are internal members and see it


def test_agent_reply_to_internal_note_stays_internal() -> None:
    notes = RoomBinding(id="b-notes", platform=SLACK_SIM, address="C-notes")
    h = Harness(agent_reply="ack, watching the rotation", extra_bindings=(notes,))
    asyncio.run(h.slack.inject(h.slack_binding, "U1", "[internal] rotating the deploy key"))

    assert h.email.sent_on(h.email_binding) == []  # neither the note nor the quote leaks
    assert [m.internal_only for _, m in h.slack.sent_on(notes)] == [True, True]


def test_deny_pattern_respected() -> None:
    h = Harness(email_rules=ForwardingRules(deny=("secret",)))
    asyncio.run(h.slack.inject(h.slack_binding, "U1", "the secret plan"))
    assert h.email.sent_on(h.email_binding) == []

    asyncio.run(h.slack.inject(h.slack_binding, "U1", "public update"))
    assert [m.body for _, m in h.email.sent_on(h.email_binding)] == ["public update"]


def test_allow_patterns_gate_when_present() -> None:
    h = Harness(email_rules=ForwardingRules(allow=(r"\bdeploy\b",)))
    asyncio.run(h.slack.inject(h.slack_binding, "U1", "lunch anyone?"))
    asyncio.run(h.slack.inject(h.slack_binding, "U1", "deploy is done"))
    assert [m.body for _, m in h.email.sent_on(h.email_binding)] == ["deploy is done"]


def test_inbound_only_binding_never_receives_fanout() -> None:
    listen_only = RoomBinding(
        id="b-listen",
        platform=SLACK_SIM,
        address="C-listen",
        rules=ForwardingRules(direction=Direction.INBOUND),
    )
    h = Harness(extra_bindings=(listen_only,))
    asyncio.run(h.slack.inject(h.slack_binding, "U1", "hello"))
    assert h.slack.sent_on(listen_only) == []

    # ...but messages arriving on it still enter the room.
    asyncio.run(h.slack.inject(listen_only, "U1", "from the listen-only leg"))
    assert [m.body for _, m in h.email.sent_on(h.email_binding)] == [
        "hello",
        "from the listen-only leg",
    ]


def test_outbound_only_binding_ignores_platform_messages() -> None:
    broadcast = RoomBinding(
        id="b-cast",
        platform=SLACK_SIM,
        address="C-cast",
        rules=ForwardingRules(direction=Direction.OUTBOUND),
    )
    h = Harness(extra_bindings=(broadcast,))
    asyncio.run(h.slack.inject(broadcast, "U1", "shout into the void"))
    assert h.email.sent_on(h.email_binding) == []
    assert h.agent_saw == []

    # Outbound still works: a room message fans out to the broadcast leg.
    asyncio.run(h.slack.inject(h.slack_binding, "U1", "hello"))
    assert [m.body for _, m in h.slack.sent_on(broadcast)] == ["hello"]


def test_unknown_sender_synthesizes_one_stable_identity() -> None:
    h = Harness()
    asyncio.run(h.slack.inject(h.slack_binding, "U9", "who am I?"))
    asyncio.run(h.slack.inject(h.slack_binding, "U9", "still me"))

    senders = [m.sender for _, m in h.email.sent_on(h.email_binding)]
    assert [s.id for s in senders] == [f"ext:{SLACK_SIM}:U9", f"ext:{SLACK_SIM}:U9"]
    assert senders[0] == senders[1]


def test_thread_ref_carries_origin_platform_key() -> None:
    h = Harness()
    asyncio.run(h.slack.inject(h.slack_binding, "U1", "in a thread", thread_key="1700.42"))
    _, message = h.email.sent_on(h.email_binding)[0]
    assert message.thread is not None
    assert message.thread.platform_refs == {SLACK_SIM: "1700.42"}
