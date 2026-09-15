"""SlackAdapter request construction and polling, over httpx.MockTransport.

No network: every test drives the adapter through an ``httpx.AsyncClient``
whose transport is a local handler, capturing the requests the adapter builds.
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import Callable, Iterator

import httpx
import pytest
from support import make_message

from switchboard import (
    AdapterSendError,
    ConfigError,
    InboundMessage,
    RoomBinding,
    SlackAdapter,
    ThreadRef,
)

BINDING = RoomBinding(id="b-slack", platform="slack", address="C42")


def make_adapter(
    handler: Callable[[httpx.Request], httpx.Response],
) -> tuple[SlackAdapter, list[httpx.Request]]:
    captured: list[httpx.Request] = []

    def capturing(request: httpx.Request) -> httpx.Response:
        captured.append(request)
        return handler(request)

    client = httpx.AsyncClient(
        base_url="https://slack.invalid/api", transport=httpx.MockTransport(capturing)
    )
    return SlackAdapter(http=client), captured


def test_send_builds_chat_post_message_request() -> None:
    adapter, captured = make_adapter(
        lambda _request: httpx.Response(200, json={"ok": True, "ts": "111.222"})
    )
    thread = ThreadRef(id="1700.1", platform_refs={"slack": "1700.1"})
    ts = asyncio.run(adapter.send(BINDING, make_message(thread=thread)))

    assert ts == "111.222"
    request = captured[0]
    assert request.method == "POST"
    assert request.url.path.endswith("/chat.postMessage")
    assert json.loads(request.content) == {
        "channel": "C42",
        "text": "[Alice] hi",  # the canonical sender travels in the body
        "thread_ts": "1700.1",
    }


def test_send_without_thread_omits_thread_ts() -> None:
    adapter, captured = make_adapter(
        lambda _request: httpx.Response(200, json={"ok": True, "ts": "1.2"})
    )
    asyncio.run(adapter.send(BINDING, make_message()))
    assert "thread_ts" not in json.loads(captured[0].content)


def test_send_surfaces_slack_error() -> None:
    adapter, _ = make_adapter(
        lambda _request: httpx.Response(200, json={"ok": False, "error": "channel_not_found"})
    )
    with pytest.raises(AdapterSendError, match="channel_not_found"):
        asyncio.run(adapter.send(BINDING, make_message()))


def test_poll_baselines_then_delivers_from_ts_cursor() -> None:
    pages: Iterator[dict[str, object]] = iter(
        [
            {
                "ok": True,
                "messages": [
                    {"ts": "100.2", "user": "U1", "text": "second"},
                    {"ts": "100.1", "user": "U1", "text": "first"},
                ],
            },
            {
                "ok": True,
                "messages": [
                    {"ts": "100.4", "user": "U2", "text": "new", "thread_ts": "100.2"},
                    {"ts": "100.3", "user": "U1", "text": "third"},
                ],
            },
        ]
    )
    adapter, captured = make_adapter(lambda _request: httpx.Response(200, json=next(pages)))
    received: list[InboundMessage] = []

    async def on_inbound(inbound: InboundMessage) -> None:
        received.append(inbound)

    adapter.subscribe(on_inbound)

    asyncio.run(adapter.poll_once([BINDING]))
    assert received == []  # first sweep baselines: history never replays
    assert "oldest" not in dict(captured[0].url.params)

    asyncio.run(adapter.poll_once([BINDING]))
    assert dict(captured[1].url.params)["oldest"] == "100.2"  # exclusive ts cursor
    assert [m.body for m in received] == ["third", "new"]  # delivered oldest-first
    assert received[1].sender_handle == "U2"
    assert received[1].platform_message_id == "100.4"
    assert received[1].thread_key == "100.2"


def test_poll_skips_non_user_rows_but_advances_cursor() -> None:
    pages: Iterator[dict[str, object]] = iter(
        [
            {"ok": True, "messages": []},
            {"ok": True, "messages": [{"ts": "5.0", "subtype": "channel_join"}]},
            {"ok": True, "messages": []},
        ]
    )
    adapter, captured = make_adapter(lambda _request: httpx.Response(200, json=next(pages)))
    received: list[InboundMessage] = []

    async def on_inbound(inbound: InboundMessage) -> None:
        received.append(inbound)

    adapter.subscribe(on_inbound)
    for _ in range(3):
        asyncio.run(adapter.poll_once([BINDING]))

    assert received == []
    assert dict(captured[1].url.params)["oldest"] == "0"  # empty channel baselined at 0
    assert dict(captured[2].url.params)["oldest"] == "5.0"  # join row advanced the cursor


def test_connect_requires_token_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("SWITCHBOARD_SLACK_TOKEN", raising=False)
    adapter = SlackAdapter()
    with pytest.raises(ConfigError, match="SWITCHBOARD_SLACK_TOKEN"):
        asyncio.run(adapter.connect())


def test_connect_reads_token_from_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SWITCHBOARD_SLACK_TOKEN", "xoxb-not-a-real-one")

    async def run() -> str:
        adapter = SlackAdapter()
        await adapter.connect()
        assert adapter._http is not None  # asserting wiring, not public API
        header = str(adapter._http.headers["Authorization"])
        await adapter.close()
        return header

    assert asyncio.run(run()) == "Bearer xoxb-not-a-real-one"


def test_send_before_connect_is_a_config_error() -> None:
    with pytest.raises(ConfigError, match="not connected"):
        asyncio.run(SlackAdapter().send(BINDING, make_message()))
