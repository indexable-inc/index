"""The router: lower inbound events to IR, apply room policy, fan out.

One router owns a set of rooms and one adapter per platform. Inbound flow:

  1. an adapter delivers an :class:`~switchboard.adapter.InboundMessage`;
  2. the router drops it if it is a bounce (a message the router itself sent,
     echoed back by the platform's poll) or the binding is inbound-disabled;
  3. the sender handle is resolved to a canonical Identity and the event is
     lowered to a :class:`~switchboard.ir.Message` with origin provenance;
  4. the message fans out to every *other* binding that its provenance has not
     visited and whose rules (direction, allow/deny, guest disclosure) admit
     it, stamping a hop per delivery;
  5. agents among the room's members see the message; a reply re-enters the
     fan-out as a first-class message (but is not offered to agents again, so
     two agents can never ping-pong).

Echo-loop prevention is therefore two independent gates: provenance hops stop
re-delivery to any binding a message has traversed, and the sent-id registry
stops polled copies of our own outbound messages from re-entering at all.
"""

from __future__ import annotations

import itertools
from collections.abc import Iterable

from .adapter import Adapter, InboundMessage
from .agent import AgentParticipant
from .ir import (
    INTERNAL_MARKER,
    Direction,
    Directory,
    Hop,
    Message,
    Provenance,
    Role,
    Room,
    RoomBinding,
    ThreadRef,
)

AGENT_ORIGIN_PLATFORM = "agent"


class Router:
    def __init__(self, directory: Directory, rooms: Iterable[Room]) -> None:
        self.directory = directory
        self._rooms: dict[str, Room] = {}
        self._bindings: dict[str, tuple[Room, RoomBinding]] = {}
        for room in rooms:
            if room.id in self._rooms:
                raise ValueError(f"duplicate room id {room.id!r}")
            self._rooms[room.id] = room
            for binding in room.bindings:
                if binding.id in self._bindings:
                    raise ValueError(f"duplicate binding id {binding.id!r}")
                self._bindings[binding.id] = (room, binding)
        self._adapters: dict[str, Adapter] = {}
        self._agents: dict[str, AgentParticipant] = {}
        # Every (binding, platform message id) this router produced; inbound
        # events matching one are our own messages coming back around.
        self._sent: set[tuple[str, str]] = set()
        self._seq = itertools.count(1)

    def attach(self, adapter: Adapter) -> None:
        if adapter.platform in self._adapters:
            raise ValueError(f"an adapter for {adapter.platform!r} is already attached")
        adapter.subscribe(self._on_inbound)
        self._adapters[adapter.platform] = adapter

    def register_agent(self, agent: AgentParticipant) -> None:
        if agent.identity.id in self._agents:
            raise ValueError(f"agent {agent.identity.id!r} is already registered")
        self._agents[agent.identity.id] = agent

    async def connect(self) -> None:
        for adapter in self._adapters.values():
            await adapter.connect()

    async def close(self) -> None:
        for adapter in self._adapters.values():
            await adapter.close()

    def _next_message_id(self, room: Room) -> str:
        return f"sb-{room.id}-{next(self._seq)}"

    async def _on_inbound(self, inbound: InboundMessage) -> None:
        entry = self._bindings.get(inbound.binding_id)
        if entry is None:
            return  # a binding this router does not own: not ours to route
        room, binding = entry
        if (binding.id, inbound.platform_message_id) in self._sent:
            return  # bounce: our own outbound message, polled back
        if binding.rules.direction is Direction.OUTBOUND:
            return  # broadcast-only leg: platform messages stay on the platform
        message = self._lower(room, binding, inbound)
        await self._dispatch(room, message, origin=binding, to_agents=True)

    def _lower(self, room: Room, binding: RoomBinding, inbound: InboundMessage) -> Message:
        sender = self.directory.resolve(binding.platform, inbound.sender_handle)
        thread = (
            ThreadRef(id=inbound.thread_key, platform_refs={binding.platform: inbound.thread_key})
            if inbound.thread_key is not None
            else None
        )
        return Message(
            id=self._next_message_id(room),
            room_id=room.id,
            sender=sender,
            body=inbound.body,
            thread=thread,
            attachments=inbound.attachments,
            internal_only=inbound.internal_only or inbound.body.startswith(INTERNAL_MARKER),
            provenance=Provenance(
                origin_platform=binding.platform,
                origin_message_id=inbound.platform_message_id,
                hops=(
                    Hop(
                        binding_id=binding.id,
                        platform=binding.platform,
                        platform_message_id=inbound.platform_message_id,
                    ),
                ),
            ),
        )

    async def _dispatch(
        self,
        room: Room,
        message: Message,
        *,
        origin: RoomBinding | None,
        to_agents: bool,
    ) -> None:
        for binding in room.bindings:
            if origin is not None and binding.id == origin.id:
                continue
            if message.provenance.visited(binding.id):
                continue  # loop prevention: already traversed this binding
            if binding.rules.direction is Direction.INBOUND:
                continue  # listen-only leg: nothing fans out to it
            if message.internal_only and binding.guest_facing:
                continue  # disclosure: internal notes never reach guests
            if not binding.rules.allows(message.body):
                continue
            adapter = self._adapters.get(binding.platform)
            if adapter is None:
                continue  # platform not attached (e.g. a partial deployment)
            outbound = message.model_copy(
                update={
                    "provenance": message.provenance.with_hop(
                        Hop(binding_id=binding.id, platform=binding.platform)
                    )
                }
            )
            platform_message_id = await adapter.send(binding, outbound)
            self._sent.add((binding.id, platform_message_id))
        if not to_agents:
            return
        for member in room.members:
            if member.role is not Role.AGENT:
                continue
            agent = self._agents.get(member.identity.id)
            if agent is None or agent.identity.id == message.sender.id:
                continue
            reply_body = await agent.respond(message)
            if not reply_body:
                continue
            reply_id = self._next_message_id(room)
            reply = Message(
                id=reply_id,
                room_id=room.id,
                sender=agent.identity,
                body=reply_body,
                thread=message.thread,
                # A reply to an internal note quotes it, so it stays internal.
                internal_only=message.internal_only,
                provenance=Provenance(
                    origin_platform=AGENT_ORIGIN_PLATFORM,
                    origin_message_id=reply_id,
                ),
            )
            # Agent replies reach every binding (origin included) but never
            # other agents: one agent round per human message, no ping-pong.
            await self._dispatch(room, reply, origin=None, to_agents=False)
