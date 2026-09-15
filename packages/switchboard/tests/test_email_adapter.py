"""EmailAdapter: outbound construction (threading headers), inbound parsing,
and IMAP UID-cursor polling -- all against fake transports, no network."""

from __future__ import annotations

import asyncio
from email.message import EmailMessage

import pytest
from support import make_message

from switchboard import (
    ConfigError,
    EmailAdapter,
    InboundMessage,
    RoomBinding,
    SmtpConfig,
    ThreadRef,
    build_email,
    parse_inbound,
)

BINDING = RoomBinding(
    id="b-mail",
    platform="email",
    address="room@corp.example",
    recipients=("gwen@ext.example",),
    guest_facing=True,
)


# ---------------------------------------------------------------------------
# Outbound construction
# ---------------------------------------------------------------------------


def test_build_email_room_tag_recipients_and_deterministic_message_id() -> None:
    mail = build_email(BINDING, make_message(body="status update\nsecond line"))
    assert mail["Subject"] == "[sb:eng] status update"  # room tag + first-line preview
    assert mail["From"] == "room@corp.example"
    assert mail["To"] == "gwen@ext.example"
    assert mail["Message-ID"] == "<sb-eng-1.b-mail@corp.example>"
    assert "In-Reply-To" not in mail
    assert "References" not in mail
    assert "[Alice] status update" in mail.get_content()


def test_build_email_reply_sets_threading_headers() -> None:
    origin = "<sb-eng-0.b-mail@corp.example>"
    thread = ThreadRef(id=origin, platform_refs={"email": origin})
    mail = build_email(BINDING, make_message(body="following up", thread=thread))
    assert mail["In-Reply-To"] == origin
    assert mail["References"] == origin
    assert mail["Subject"] == "Re: [sb:eng] following up"


def test_build_email_thread_without_email_ref_starts_fresh() -> None:
    # A thread born on Slack has no email Message-ID yet: first mail in the
    # thread goes out unthreaded (the reply chain starts on the email side).
    thread = ThreadRef(id="1700.1", platform_refs={"slack": "1700.1"})
    mail = build_email(BINDING, make_message(thread=thread))
    assert "In-Reply-To" not in mail


class StubSmtp:
    def __init__(self) -> None:
        self.outbox: list[EmailMessage] = []
        self.quit_calls = 0

    def send_message(self, msg: EmailMessage) -> object:
        self.outbox.append(msg)
        return {}

    def quit(self) -> object:
        self.quit_calls += 1
        return (221, b"bye")


def test_send_uses_injected_smtp_transport_and_returns_message_id() -> None:
    stub = StubSmtp()
    adapter = EmailAdapter(smtp_factory=lambda: stub)
    message_id = asyncio.run(adapter.send(BINDING, make_message()))
    assert message_id == "<sb-eng-1.b-mail@corp.example>"
    assert [str(mail["To"]) for mail in stub.outbox] == ["gwen@ext.example"]
    assert stub.quit_calls == 1  # the connection is released even on success


# ---------------------------------------------------------------------------
# Inbound parsing
# ---------------------------------------------------------------------------


def _raw_mail(
    *,
    body: str = "Sounds good",
    in_reply_to: str | None = None,
    references: str | None = None,
    message_id: str | None = "<abc@ext.example>",
) -> bytes:
    mail = EmailMessage()
    mail["From"] = "Gwen <gwen@ext.example>"
    mail["To"] = "room@corp.example"
    mail["Subject"] = "Re: [sb:eng] status update"
    if message_id is not None:
        mail["Message-ID"] = message_id
    if in_reply_to is not None:
        mail["In-Reply-To"] = in_reply_to
    if references is not None:
        mail["References"] = references
    mail.set_content(body)
    return mail.as_bytes()


def test_parse_inbound_maps_sender_thread_and_body() -> None:
    origin = "<sb-eng-1.b-mail@corp.example>"
    inbound = parse_inbound(_raw_mail(in_reply_to=origin), BINDING)
    assert inbound.platform == "email"
    assert inbound.binding_id == "b-mail"
    assert inbound.sender_handle == "gwen@ext.example"  # bare address, not display form
    assert inbound.thread_key == origin
    assert inbound.body == "Sounds good"
    assert inbound.platform_message_id == "<abc@ext.example>"


def test_parse_inbound_falls_back_to_first_reference() -> None:
    inbound = parse_inbound(
        _raw_mail(references="<first@corp.example> <second@corp.example>"), BINDING
    )
    assert inbound.thread_key == "<first@corp.example>"


def test_parse_inbound_synthesizes_missing_message_id() -> None:
    inbound = parse_inbound(_raw_mail(message_id=None), BINDING)
    assert inbound.platform_message_id == "<unknown@room@corp.example>"


# ---------------------------------------------------------------------------
# IMAP polling with a UID cursor
# ---------------------------------------------------------------------------


class FakeImap:
    """Just enough of the imaplib uid() surface for the polling contract."""

    def __init__(self, store: dict[int, bytes]) -> None:
        self.store = store
        self.searches: list[str] = []

    def login(self, user: str, password: str) -> object:
        return ("OK", [])

    def select(self, mailbox: str) -> object:
        return ("OK", [])

    def uid(self, command: str, *args: str) -> tuple[str, list[object]]:
        if command == "search":
            self.searches.append(args[-1])
            # Like a real server, an N:* search always matches at least the
            # newest message, even when its UID is below N.
            listing = " ".join(str(uid) for uid in sorted(self.store))
            return ("OK", [listing.encode()])
        uid = int(args[0])
        return ("OK", [(b"1 (RFC822 {0}", self.store[uid]), b")"])

    def logout(self) -> object:
        return ("BYE", [])


def test_poll_once_fetches_then_only_new_uids() -> None:
    store = {
        1: _raw_mail(body="first", message_id="<m1@ext.example>"),
        2: _raw_mail(body="second", message_id="<m2@ext.example>"),
    }
    fake = FakeImap(store)
    adapter = EmailAdapter(imap_factory=lambda: fake)
    received: list[InboundMessage] = []

    async def on_inbound(inbound: InboundMessage) -> None:
        received.append(inbound)

    adapter.subscribe(on_inbound)

    asyncio.run(adapter.poll_once([BINDING]))
    assert [m.body for m in received] == ["first", "second"]

    asyncio.run(adapter.poll_once([BINDING]))  # nothing new: the newest UID re-matches
    assert len(received) == 2

    store[3] = _raw_mail(body="third", message_id="<m3@ext.example>")
    asyncio.run(adapter.poll_once([BINDING]))
    assert [m.body for m in received] == ["first", "second", "third"]
    assert fake.searches == ["1:*", "3:*", "3:*"]  # cursor-driven search ranges


def test_poll_once_skips_mail_for_unknown_mailboxes() -> None:
    other = EmailMessage()
    other["From"] = "someone@ext.example"
    other["To"] = "unrelated@corp.example"
    other["Message-ID"] = "<x@ext.example>"
    other.set_content("not for this room")
    fake = FakeImap({1: other.as_bytes()})
    adapter = EmailAdapter(imap_factory=lambda: fake)
    received: list[InboundMessage] = []

    async def on_inbound(inbound: InboundMessage) -> None:
        received.append(inbound)

    adapter.subscribe(on_inbound)
    asyncio.run(adapter.poll_once([BINDING]))
    assert received == []


# ---------------------------------------------------------------------------
# Env configuration
# ---------------------------------------------------------------------------


def test_smtp_config_from_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SWITCHBOARD_SMTP_HOST", "smtp.corp.example")
    monkeypatch.setenv("SWITCHBOARD_SMTP_PORT", "2525")
    monkeypatch.setenv("SWITCHBOARD_SMTP_USERNAME", "switchboard")
    monkeypatch.setenv("SWITCHBOARD_SMTP_PASSWORD", "hunter2")
    config = SmtpConfig.from_env()
    assert config.host == "smtp.corp.example"
    assert config.port == 2525
    assert (config.username, config.password) == ("switchboard", "hunter2")


def test_smtp_config_requires_host(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("SWITCHBOARD_SMTP_HOST", raising=False)
    with pytest.raises(ConfigError, match="SWITCHBOARD_SMTP_HOST"):
        SmtpConfig.from_env()
