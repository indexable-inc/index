"""IR-level units: forwarding rules, provenance, the identity directory,
and the agent participant contract."""

from __future__ import annotations

import asyncio

import pytest

from switchboard import (
    AgentParticipant,
    Directory,
    ForwardingRules,
    Hop,
    Identity,
    IdentityKind,
    Message,
    Provenance,
    echo_responder,
    llm_responder,
)


def test_forwarding_rules_default_allows_everything() -> None:
    assert ForwardingRules().allows("anything at all")


def test_forwarding_rules_deny_wins_over_allow() -> None:
    rules = ForwardingRules(allow=("plan",), deny=("secret",))
    assert rules.allows("the plan")
    assert not rules.allows("the secret plan")


def test_forwarding_rules_allow_list_is_exhaustive() -> None:
    rules = ForwardingRules(allow=("deploy", "release"))
    assert rules.allows("release notes")
    assert not rules.allows("lunch?")


def test_provenance_visited_and_with_hop() -> None:
    provenance = Provenance(origin_platform="slack", origin_message_id="1.0")
    assert not provenance.visited("b-email")
    hopped = provenance.with_hop(Hop(binding_id="b-email", platform="email"))
    assert hopped.visited("b-email")
    assert not provenance.visited("b-email")  # with_hop derives, never mutates


def test_directory_rejects_duplicate_handles() -> None:
    directory = Directory([Identity(id="a", display_name="A", handles={"slack": "U1"})])
    with pytest.raises(ValueError, match="already mapped"):
        directory.add(Identity(id="b", display_name="B", handles={"slack": "U1"}))


def test_directory_resolve_synthesizes_and_registers() -> None:
    directory = Directory()
    first = directory.resolve("email", "stranger@ext.example")
    assert first.id == "ext:email:stranger@ext.example"
    assert directory.resolve("email", "stranger@ext.example") is first
    assert directory.find("email", "stranger@ext.example") is first


def test_agent_participant_requires_agent_identity() -> None:
    person = Identity(id="p", display_name="P")
    with pytest.raises(ValueError, match="not an agent"):
        AgentParticipant(person)


def test_echo_responder_is_deterministic() -> None:
    message = Message(
        id="sb-1",
        room_id="eng",
        sender=Identity(id="alice", display_name="Alice"),
        body="ship it",
        provenance=Provenance(origin_platform="slack", origin_message_id="1.0"),
    )
    reply = asyncio.run(echo_responder(message))
    assert reply == "noted: Alice said 'ship it'"


def test_llm_responder_is_an_unimplemented_seam() -> None:
    with pytest.raises(NotImplementedError, match="ENG-7479"):
        llm_responder()
