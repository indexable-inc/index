"""The platform adapter interface: how frontends and backends plug in.

An adapter owns exactly one platform. Outbound, the router calls
:meth:`Adapter.send` with a canonical :class:`~switchboard.ir.Message` and the
binding to deliver it on; the adapter renders and returns the platform's
message id (which the router records so the message is dropped if it comes
back around via polling). Inbound, the adapter lowers each platform event to
an :class:`InboundMessage` -- platform-shaped but normalized, sender still a
platform handle -- and hands it to the callback the router registered with
:meth:`Adapter.subscribe`; the router does identity resolution and provenance
stamping, so adapters stay dumb about rooms and members.
"""

from __future__ import annotations

import abc
from collections.abc import Awaitable, Callable

from pydantic import BaseModel, ConfigDict

from .ir import Attachment, Message, RoomBinding


class ConfigError(RuntimeError):
    """A required credential or endpoint is missing from the environment."""


class AdapterSendError(RuntimeError):
    """The platform rejected an outbound message."""


class InboundMessage(BaseModel):
    """One platform message, normalized but not yet resolved to identities."""

    model_config = ConfigDict(frozen=True)

    platform: str
    binding_id: str
    platform_message_id: str
    sender_handle: str
    body: str
    thread_key: str | None = None
    attachments: tuple[Attachment, ...] = ()
    internal_only: bool = False


OnInbound = Callable[[InboundMessage], Awaitable[None]]


def render_for_relay(message: Message) -> str:
    """The cross-platform rendering of a forwarded message.

    The relaying bot/mailbox is the platform-level author, so the canonical
    sender travels in the body -- that is the identity mapping a reader on the
    far platform actually sees.
    """
    return f"[{message.sender.display_name}] {message.body}"


class Adapter(abc.ABC):
    """Base class every platform adapter implements.

    ``platform`` is the key that ties the adapter to bindings
    (``RoomBinding.platform``) and identity handles (``Identity.handles``).
    It is an instance attribute (not a ClassVar) so test doubles can stand in
    for any platform.
    """

    def __init__(self, platform: str) -> None:
        self.platform = platform
        self._on_inbound: OnInbound | None = None

    def subscribe(self, handler: OnInbound) -> None:
        if self._on_inbound is not None:
            raise ValueError(f"adapter {self.platform!r} already has a subscriber")
        self._on_inbound = handler

    async def connect(self) -> None:  # noqa: B027 -- optional lifecycle hook, default no-op
        """Acquire platform resources (clients, sessions). Default: nothing."""

    async def close(self) -> None:  # noqa: B027 -- optional lifecycle hook, default no-op
        """Release platform resources. Default: nothing."""

    @abc.abstractmethod
    async def send(self, binding: RoomBinding, message: Message) -> str:
        """Deliver ``message`` on ``binding``; return the platform message id."""

    async def _deliver(self, inbound: InboundMessage) -> None:
        # No subscriber means no router is attached yet; dropping (rather than
        # buffering) keeps a half-wired adapter from replaying stale events.
        if self._on_inbound is not None:
            await self._on_inbound(inbound)
