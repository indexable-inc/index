"""Shared test fixtures: one canonical-message factory for the adapter tests.

Importable as a sibling module because pytest puts this directory on
``sys.path`` (no ``__init__.py`` here, by design).
"""

from __future__ import annotations

from switchboard import Identity, Message, Provenance, ThreadRef


def make_message(body: str = "hi", thread: ThreadRef | None = None) -> Message:
    return Message(
        id="sb-eng-1",
        room_id="eng",
        sender=Identity(id="alice", display_name="Alice"),
        body=body,
        thread=thread,
        provenance=Provenance(origin_platform="memory", origin_message_id="m1"),
    )
