"""The Slack frontend/backend: chat.postMessage out, conversations.history in.

Deliberately minimal: a bot token over plain HTTPS (httpx), no Socket Mode, no
Events API. Inbound is a per-binding poll of ``conversations.history`` with an
``oldest`` ts cursor, so the adapter needs nothing but the token and the
channel ids in the room's bindings. The token comes from
``SWITCHBOARD_SLACK_TOKEN`` at :meth:`SlackAdapter.connect`; tests inject an
``httpx.AsyncClient`` over a mock transport instead, so nothing here ever
touches the network in CI.
"""

from __future__ import annotations

import asyncio
import os
from collections.abc import Sequence

import httpx

from .adapter import Adapter, AdapterSendError, ConfigError, InboundMessage, render_for_relay
from .ir import Message, RoomBinding

_API_BASE = "https://slack.com/api"


class SlackAdapter(Adapter):
    def __init__(self, *, http: httpx.AsyncClient | None = None) -> None:
        super().__init__("slack")
        self._http = http
        self._owns_http = False
        # Per-binding ts of the newest message seen, exclusive lower bound for
        # the next conversations.history poll.
        self._cursors: dict[str, str] = {}

    async def connect(self) -> None:
        if self._http is not None:
            return
        credential = os.environ.get("SWITCHBOARD_SLACK_TOKEN")
        if not credential:
            raise ConfigError("SWITCHBOARD_SLACK_TOKEN is not set (Slack bot token)")
        self._http = httpx.AsyncClient(
            base_url=_API_BASE,
            headers={"Authorization": f"Bearer {credential}"},
        )
        self._owns_http = True

    async def close(self) -> None:
        if self._http is not None and self._owns_http:
            await self._http.aclose()
            self._http = None
            self._owns_http = False

    def _client(self) -> httpx.AsyncClient:
        if self._http is None:
            raise ConfigError("SlackAdapter is not connected (call connect() first)")
        return self._http

    async def send(self, binding: RoomBinding, message: Message) -> str:
        payload: dict[str, str] = {
            "channel": binding.address,
            "text": render_for_relay(message),
        }
        thread_ts = message.thread.platform_refs.get("slack") if message.thread else None
        if thread_ts is not None:
            payload["thread_ts"] = thread_ts
        response = await self._client().post("/chat.postMessage", json=payload)
        response.raise_for_status()
        data = response.json()
        if not data.get("ok"):
            raise AdapterSendError(f"slack chat.postMessage failed: {data.get('error', 'unknown')}")
        return str(data["ts"])

    async def poll_once(self, bindings: Sequence[RoomBinding]) -> None:
        """One conversations.history sweep over ``bindings``, delivering new rows.

        The first sweep for a binding only baselines its cursor at the
        channel's current newest ts -- history never replays into the room.
        """
        for binding in bindings:
            baseline = binding.id not in self._cursors
            params: dict[str, str] = {"channel": binding.address, "limit": "200"}
            cursor = self._cursors.get(binding.id)
            if cursor is not None:
                params["oldest"] = cursor  # exclusive: strictly newer than the last seen ts
            response = await self._client().get("/conversations.history", params=params)
            response.raise_for_status()
            data = response.json()
            if not data.get("ok"):
                raise AdapterSendError(
                    f"slack conversations.history failed: {data.get('error', 'unknown')}"
                )
            newest = cursor
            # The API returns newest-first; deliver oldest-first so ordering
            # on the far side matches the channel.
            for row in reversed(data.get("messages", [])):
                ts = str(row.get("ts", ""))
                if not ts:
                    continue
                if newest is None or float(ts) > float(newest):
                    newest = ts
                if baseline or "user" not in row or "text" not in row:
                    continue  # baselining, or joins/topic changes -- still advance the cursor
                await self._deliver(
                    InboundMessage(
                        platform=self.platform,
                        binding_id=binding.id,
                        platform_message_id=ts,
                        sender_handle=str(row["user"]),
                        body=str(row["text"]),
                        thread_key=row.get("thread_ts"),
                    )
                )
            # An empty channel baselines at 0 so everything after is new.
            self._cursors[binding.id] = newest if newest is not None else "0"

    async def run(self, bindings: Sequence[RoomBinding], *, interval: float = 2.0) -> None:
        """Poll forever (baseline sweep first, then deliveries)."""
        while True:
            await self.poll_once(bindings)
            await asyncio.sleep(interval)
