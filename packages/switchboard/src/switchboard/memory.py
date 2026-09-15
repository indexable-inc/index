"""A fully functional in-memory adapter: the deterministic test double.

Stands in for any platform (pass the platform key it should impersonate).
``sent`` is the observable platform timeline -- everything the router
delivered, per binding -- and :meth:`inject` simulates a native message
arriving on the platform, exactly what a polling frontend would deliver.
"""

from __future__ import annotations

import itertools

from .adapter import Adapter, InboundMessage
from .ir import Message, RoomBinding


class InMemoryAdapter(Adapter):
    def __init__(self, platform: str = "memory") -> None:
        super().__init__(platform)
        self.sent: dict[str, list[tuple[str, Message]]] = {}
        self._seq = itertools.count(1)

    async def send(self, binding: RoomBinding, message: Message) -> str:
        platform_message_id = f"{self.platform}-{next(self._seq)}"
        self.sent.setdefault(binding.id, []).append((platform_message_id, message))
        return platform_message_id

    def sent_on(self, binding: RoomBinding) -> list[tuple[str, Message]]:
        return self.sent.get(binding.id, [])

    async def inject(
        self,
        binding: RoomBinding,
        sender_handle: str,
        body: str,
        *,
        platform_message_id: str | None = None,
        thread_key: str | None = None,
        internal_only: bool = False,
    ) -> InboundMessage:
        """Simulate a native platform message and deliver it to the subscriber.

        Passing an explicit ``platform_message_id`` simulates a poll echoing
        back something the router itself sent (the bounce case).
        """
        inbound = InboundMessage(
            platform=self.platform,
            binding_id=binding.id,
            platform_message_id=platform_message_id or f"{self.platform}-native-{next(self._seq)}",
            sender_handle=sender_handle,
            body=body,
            thread_key=thread_key,
            internal_only=internal_only,
        )
        await self._deliver(inbound)
        return inbound
