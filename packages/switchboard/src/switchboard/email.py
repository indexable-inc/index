"""The email frontend/backend: SMTP out, IMAP UID polling in.

Postmoogle-style: one room binding = one mailbox address (the binding's
``address`` is both the From of outbound mail and the To that routes inbound
mail to the room). Guests need nothing but a normal mail client.

Threading rides the standard headers: outbound mail carries a deterministic
``Message-ID`` derived from the canonical message id, the subject carries the
room tag (``[sb:<room>]``), and a reply to a threaded message sets
``In-Reply-To``/``References`` from the thread's email ref, so guest mail
clients stack the conversation correctly.

Both transports are injectable (:class:`SmtpTransport` / :class:`ImapTransport`
protocols), so tests exercise message construction and the UID cursor with
fakes; real credentials come from ``SWITCHBOARD_SMTP_*`` / ``SWITCHBOARD_IMAP_*``
env vars and are only read when the default factories are used.
"""

from __future__ import annotations

import asyncio
import email.policy
import imaplib
import os
import smtplib
from collections.abc import Callable, Sequence
from email.message import EmailMessage
from email.utils import parseaddr
from typing import Protocol

from pydantic import BaseModel, ConfigDict

from .adapter import Adapter, ConfigError, InboundMessage, render_for_relay
from .ir import Message, RoomBinding

_SUBJECT_TAG = "sb"
_BODY_PREVIEW_CHARS = 60


def _require_env(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        raise ConfigError(f"{name} is not set")
    return value


class SmtpConfig(BaseModel):
    model_config = ConfigDict(frozen=True)

    host: str
    port: int = 587
    username: str | None = None
    password: str | None = None
    use_starttls: bool = True

    @classmethod
    def from_env(cls) -> SmtpConfig:
        return cls(
            host=_require_env("SWITCHBOARD_SMTP_HOST"),
            port=int(os.environ.get("SWITCHBOARD_SMTP_PORT", "587")),
            username=os.environ.get("SWITCHBOARD_SMTP_USERNAME"),
            password=os.environ.get("SWITCHBOARD_SMTP_PASSWORD"),
        )


class ImapConfig(BaseModel):
    model_config = ConfigDict(frozen=True)

    host: str
    port: int = 993
    username: str | None = None
    password: str | None = None
    folder: str = "INBOX"

    @classmethod
    def from_env(cls) -> ImapConfig:
        return cls(
            host=_require_env("SWITCHBOARD_IMAP_HOST"),
            port=int(os.environ.get("SWITCHBOARD_IMAP_PORT", "993")),
            username=os.environ.get("SWITCHBOARD_IMAP_USERNAME"),
            password=os.environ.get("SWITCHBOARD_IMAP_PASSWORD"),
        )


class SmtpTransport(Protocol):
    def send_message(self, msg: EmailMessage) -> object: ...
    def quit(self) -> object: ...


class ImapTransport(Protocol):
    def login(self, user: str, password: str) -> object: ...
    def select(self, mailbox: str) -> object: ...
    def uid(self, command: str, *args: str) -> tuple[str, list[object]]: ...
    def logout(self) -> object: ...


def message_id_for(message: Message, binding: RoomBinding) -> str:
    """The deterministic RFC 5322 Message-ID for one delivery of a message."""
    domain = binding.address.rsplit("@", 1)[-1]
    return f"<{message.id}.{binding.id}@{domain}>"


def build_email(binding: RoomBinding, message: Message) -> EmailMessage:
    """Render one canonical message as the outbound mail for ``binding``."""
    mail = EmailMessage()
    mail["From"] = binding.address
    mail["To"] = ", ".join(binding.recipients) if binding.recipients else binding.address
    mail["Message-ID"] = message_id_for(message, binding)
    preview = message.body.splitlines()[0][:_BODY_PREVIEW_CHARS] if message.body else ""
    subject = f"[{_SUBJECT_TAG}:{message.room_id}] {preview}".rstrip()
    email_thread_ref = message.thread.platform_refs.get("email") if message.thread else None
    if email_thread_ref is not None:
        mail["Subject"] = f"Re: {subject}"
        mail["In-Reply-To"] = email_thread_ref
        mail["References"] = email_thread_ref
    else:
        mail["Subject"] = subject
    mail.set_content(render_for_relay(message))
    return mail


def parse_inbound(raw: bytes, binding: RoomBinding) -> InboundMessage:
    """Lower one raw RFC 5322 message into an :class:`InboundMessage`."""
    parsed = email.message_from_bytes(raw, policy=email.policy.default)
    sender = parseaddr(str(parsed.get("From", "")))[1]
    thread_key: str | None = None
    in_reply_to = parsed.get("In-Reply-To")
    references = parsed.get("References")
    if in_reply_to:
        thread_key = str(in_reply_to).strip()
    elif references:
        thread_key = str(references).split()[0]
    body = ""
    part = parsed.get_body(preferencelist=("plain",))
    if part is not None:
        body = str(part.get_content()).strip()
    platform_message_id = str(parsed.get("Message-ID", "")).strip()
    return InboundMessage(
        platform="email",
        binding_id=binding.id,
        platform_message_id=platform_message_id or f"<unknown@{binding.address}>",
        sender_handle=sender,
        body=body,
        thread_key=thread_key,
    )


class EmailAdapter(Adapter):
    def __init__(
        self,
        *,
        smtp: SmtpConfig | None = None,
        imap: ImapConfig | None = None,
        smtp_factory: Callable[[], SmtpTransport] | None = None,
        imap_factory: Callable[[], ImapTransport] | None = None,
    ) -> None:
        super().__init__("email")
        self._smtp = smtp
        self._imap = imap
        self._smtp_factory = smtp_factory
        self._imap_factory = imap_factory
        # UID cursor: only mail with a strictly larger IMAP UID is fetched, so
        # nothing replays across polls (UIDs are ascending within a mailbox).
        self._uid_cursor = 0

    async def connect(self) -> None:
        # Config is resolved lazily so a send-only (or receive-only) deployment
        # needs only its half of the environment.
        if self._smtp is None and self._smtp_factory is None and "SWITCHBOARD_SMTP_HOST" in os.environ:
            self._smtp = SmtpConfig.from_env()
        if self._imap is None and self._imap_factory is None and "SWITCHBOARD_IMAP_HOST" in os.environ:
            self._imap = ImapConfig.from_env()

    def _open_smtp(self) -> SmtpTransport:
        if self._smtp_factory is not None:
            return self._smtp_factory()
        if self._smtp is None:
            self._smtp = SmtpConfig.from_env()
        client = smtplib.SMTP(self._smtp.host, self._smtp.port, timeout=30)
        if self._smtp.use_starttls:
            client.starttls()
        if self._smtp.username is not None:
            client.login(self._smtp.username, self._smtp.password or "")
        return client

    def _open_imap(self) -> ImapTransport:
        if self._imap_factory is not None:
            return self._imap_factory()
        if self._imap is None:
            self._imap = ImapConfig.from_env()
        return imaplib.IMAP4_SSL(self._imap.host, self._imap.port)

    def _smtp_send(self, mail: EmailMessage) -> None:
        client = self._open_smtp()
        try:
            client.send_message(mail)
        finally:
            client.quit()

    async def send(self, binding: RoomBinding, message: Message) -> str:
        mail = build_email(binding, message)
        # smtplib is blocking; a worker thread keeps the router's loop live.
        await asyncio.to_thread(self._smtp_send, mail)
        return str(mail["Message-ID"])

    def _imap_fetch(self) -> list[bytes]:
        client = self._open_imap()
        try:
            config = self._imap
            if config is not None and config.username is not None:
                client.login(config.username, config.password or "")
            client.select(config.folder if config is not None else "INBOX")
            status, data = client.uid("search", "UID", f"{self._uid_cursor + 1}:*")
            if status != "OK" or not data:
                return []
            listing = data[0] if isinstance(data[0], bytes) else b""
            # An x:* search returns the newest UID even when it is < x, so the
            # cursor comparison below is what actually excludes seen mail.
            uids = sorted(int(uid) for uid in listing.split() if int(uid) > self._uid_cursor)
            raws: list[bytes] = []
            for uid in uids:
                status, fetched = client.uid("fetch", str(uid), "(RFC822)")
                if status != "OK":
                    continue
                for part in fetched:
                    if not isinstance(part, tuple) or len(part) < 2:
                        continue
                    payload = part[1]
                    if isinstance(payload, bytes):
                        raws.append(payload)
                self._uid_cursor = max(self._uid_cursor, uid)
            return raws
        finally:
            client.logout()

    async def poll_once(self, bindings: Sequence[RoomBinding]) -> None:
        """One IMAP sweep: fetch mail above the UID cursor and route each
        message to the binding whose mailbox address it was delivered to."""
        raws = await asyncio.to_thread(self._imap_fetch)
        for raw in raws:
            parsed = email.message_from_bytes(raw, policy=email.policy.default)
            recipients = ", ".join(
                str(parsed.get(header, "")) for header in ("To", "Cc", "Delivered-To")
            )
            binding = next((b for b in bindings if b.address in recipients), None)
            if binding is None:
                continue
            await self._deliver(parse_inbound(raw, binding))

    async def run(self, bindings: Sequence[RoomBinding], *, interval: float = 30.0) -> None:
        while True:
            await self.poll_once(bindings)
            await asyncio.sleep(interval)
