"""AI agents as first-class room members.

An :class:`AgentParticipant` is an :class:`~switchboard.ir.Identity` of kind
``agent`` plus a pluggable async responder. The router delivers every room
message to the agents among the room's members and fans any reply back out to
every platform binding -- an agent is a peer, not a bot bolted onto one
platform.
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable

from .ir import Identity, IdentityKind, Message

# The responder contract: the room message in, the reply body out (or None to
# stay silent). Async so a real implementation can call out to a model.
Responder = Callable[[Message], Awaitable[str | None]]


async def echo_responder(message: Message) -> str:
    """The default stub: a deterministic one-line acknowledgement."""
    return f"noted: {message.sender.display_name} said {message.body!r}"


def llm_responder() -> Responder:
    """Seam for a real model-backed responder.

    TODO(ENG-7479): implement against a model API, gated on
    ``SWITCHBOARD_AGENT_MODEL``; v0 deliberately ships only the seam so no
    test or build ever depends on a network model call.
    """
    raise NotImplementedError("model-backed responder is a v1 follow-up (ENG-7479)")


class AgentParticipant:
    def __init__(self, identity: Identity, responder: Responder | None = None) -> None:
        if identity.kind is not IdentityKind.AGENT:
            raise ValueError(f"identity {identity.id!r} is not an agent")
        self.identity = identity
        self._responder = responder if responder is not None else echo_responder

    async def respond(self, message: Message) -> str | None:
        return await self._responder(message)
