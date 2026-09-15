"""The canonical chat IR: identities, rooms, bindings, and messages.

LLVM-style middle layer: every platform frontend (Slack, email, the in-memory
test double) lowers what it receives into these types, and every backend is
handed them to render back out. Nothing in this module knows how any platform
works -- adapters own that -- so adding a platform never changes the IR or the
router.

Loop prevention lives in :class:`Provenance`: every message carries the origin
platform + message id and the list of bindings it has traversed (hops). The
router refuses to deliver a message to a binding already present in its
provenance, so a message can never ricochet between two bridged channels.
"""

from __future__ import annotations

import re
from collections.abc import Iterable
from datetime import UTC, datetime
from enum import StrEnum

from pydantic import BaseModel, ConfigDict, Field

# Body prefix that marks a message internal-only at lowering time: internal
# messages are delivered to internal bindings and agents but never to a
# guest-facing binding (e.g. the external-email leg of a room).
INTERNAL_MARKER = "[internal]"


class IdentityKind(StrEnum):
    PERSON = "person"
    AGENT = "agent"


class Identity(BaseModel):
    """One person or agent, stable across every platform they appear on.

    ``handles`` maps a platform key (the adapter's ``platform``) to that
    platform's native handle: a Slack user id, an email address, a Matrix
    mxid, an agent name. The canonical ``id`` is what the router stamps on
    forwarded messages, so readers on platform B see who spoke on platform A.
    """

    model_config = ConfigDict(frozen=True)

    id: str
    display_name: str
    kind: IdentityKind = IdentityKind.PERSON
    handles: dict[str, str] = Field(default_factory=dict)


# Guests are external people (typically reached over email): disclosure rules
# apply to the bindings that face them. Agents are AI participants and see
# everything internal members see.
class Role(StrEnum):
    """A member's standing in a room."""

    MEMBER = "member"
    GUEST = "guest"
    AGENT = "agent"


class Member(BaseModel):
    model_config = ConfigDict(frozen=True)

    identity: Identity
    role: Role = Role.MEMBER


# ``inbound`` lets platform messages into the room but never fans out to the
# platform; ``outbound`` is the mirror (a broadcast-only leg).
class Direction(StrEnum):
    """Which way a binding forwards."""

    BOTH = "both"
    INBOUND = "inbound"
    OUTBOUND = "outbound"


class ForwardingRules(BaseModel):
    """Per-binding forwarding policy, applied on delivery to that binding.

    ``allow``/``deny`` are regex patterns matched (``re.search``) against the
    message body. Deny wins; an empty allow list allows everything.
    """

    model_config = ConfigDict(frozen=True)

    direction: Direction = Direction.BOTH
    allow: tuple[str, ...] = ()
    deny: tuple[str, ...] = ()

    def allows(self, body: str) -> bool:
        if any(re.search(pattern, body) for pattern in self.deny):
            return False
        return not self.allow or any(re.search(pattern, body) for pattern in self.allow)


class RoomBinding(BaseModel):
    """One platform channel a room spans.

    ``address`` is the platform-native channel key: a Slack channel id, or a
    mailbox address (Postmoogle-style, one room binding = one mailbox).
    ``recipients`` is for delivery-list platforms (email): the addresses an
    outbound message is sent to. ``guest_facing`` marks the binding as visible
    to guests, so internal-only messages are withheld from it.
    """

    model_config = ConfigDict(frozen=True)

    id: str
    platform: str
    address: str
    rules: ForwardingRules = ForwardingRules()
    recipients: tuple[str, ...] = ()
    guest_facing: bool = False


class Room(BaseModel):
    model_config = ConfigDict(frozen=True)

    id: str
    name: str
    members: tuple[Member, ...] = ()
    bindings: tuple[RoomBinding, ...] = ()


class ThreadRef(BaseModel):
    """A canonical thread, with the per-platform keys that realize it.

    ``platform_refs`` maps platform key -> that platform's thread handle (a
    Slack ``thread_ts``, an email ``Message-ID``). v0 seeds only the origin
    platform's ref; a cross-platform thread mapping table is a follow-up.
    """

    model_config = ConfigDict(frozen=True)

    id: str
    platform_refs: dict[str, str] = Field(default_factory=dict)


class Attachment(BaseModel):
    """A named attachment; ``content_ref`` is an opaque reference to the bytes
    (path, URL, or store key) so the IR never carries payloads inline."""

    model_config = ConfigDict(frozen=True)

    name: str
    content_ref: str


class Reaction(BaseModel):
    model_config = ConfigDict(frozen=True)

    emoji: str
    by: str  # canonical Identity id


class Hop(BaseModel):
    """One binding a message has traversed (received on or delivered to)."""

    model_config = ConfigDict(frozen=True)

    binding_id: str
    platform: str
    platform_message_id: str | None = None


class Provenance(BaseModel):
    """Where a message came from and everywhere it has been.

    This is the echo-loop breaker: before delivering to a binding, the router
    checks :meth:`visited` and drops the delivery if the binding is already in
    the hop list.
    """

    model_config = ConfigDict(frozen=True)

    origin_platform: str
    origin_message_id: str
    hops: tuple[Hop, ...] = ()

    def visited(self, binding_id: str) -> bool:
        return any(hop.binding_id == binding_id for hop in self.hops)

    def with_hop(self, hop: Hop) -> Provenance:
        return self.model_copy(update={"hops": (*self.hops, hop)})


class Message(BaseModel):
    """One canonical message, fully resolved: sender is an Identity, not a
    platform handle. Immutable; the router derives per-delivery copies with
    :meth:`pydantic.BaseModel.model_copy` so provenance never aliases."""

    model_config = ConfigDict(frozen=True)

    id: str
    room_id: str
    sender: Identity
    body: str
    thread: ThreadRef | None = None
    attachments: tuple[Attachment, ...] = ()
    reactions: tuple[Reaction, ...] = ()
    internal_only: bool = False
    sent_at: datetime = Field(default_factory=lambda: datetime.now(UTC))
    provenance: Provenance


class Directory:
    """The identity registry: canonical id <-> per-platform handle mapping.

    :meth:`resolve` never fails: an unknown handle synthesizes a stable
    external identity (``ext:<platform>:<handle>``) and registers it, so a
    stranger emailing the room still gets one consistent identity everywhere.
    """

    def __init__(self, identities: Iterable[Identity] = ()) -> None:
        self._by_id: dict[str, Identity] = {}
        self._by_handle: dict[tuple[str, str], Identity] = {}
        for identity in identities:
            self.add(identity)

    def add(self, identity: Identity) -> None:
        if identity.id in self._by_id:
            raise ValueError(f"duplicate identity id {identity.id!r}")
        for platform, handle in identity.handles.items():
            key = (platform, handle)
            if key in self._by_handle:
                raise ValueError(f"handle {handle!r} on {platform!r} is already mapped")
            self._by_handle[key] = identity
        self._by_id[identity.id] = identity

    def get(self, canonical_id: str) -> Identity | None:
        return self._by_id.get(canonical_id)

    def find(self, platform: str, handle: str) -> Identity | None:
        return self._by_handle.get((platform, handle))

    def resolve(self, platform: str, handle: str) -> Identity:
        known = self.find(platform, handle)
        if known is not None:
            return known
        synthesized = Identity(
            id=f"ext:{platform}:{handle}",
            display_name=handle,
            kind=IdentityKind.PERSON,
            handles={platform: handle},
        )
        self.add(synthesized)
        return synthesized

    @staticmethod
    def handle_for(identity: Identity, platform: str) -> str | None:
        return identity.handles.get(platform)
