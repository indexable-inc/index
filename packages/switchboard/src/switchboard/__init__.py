"""Switchboard: cross-platform chatrooms over one canonical IR.

Platform frontends (Slack, email, in-memory) lower messages into a shared IR;
the router applies room policy and fans out to every other platform binding
and to agent members. See the package README for the architecture sketch.
"""

from __future__ import annotations

from .adapter import Adapter, AdapterSendError, ConfigError, InboundMessage, render_for_relay
from .agent import AgentParticipant, Responder, echo_responder, llm_responder
from .email import EmailAdapter, ImapConfig, SmtpConfig, build_email, parse_inbound
from .ir import (
    INTERNAL_MARKER,
    Attachment,
    Direction,
    Directory,
    ForwardingRules,
    Hop,
    Identity,
    IdentityKind,
    Member,
    Message,
    Provenance,
    Reaction,
    Role,
    Room,
    RoomBinding,
    ThreadRef,
)
from .memory import InMemoryAdapter
from .router import AGENT_ORIGIN_PLATFORM, Router
from .slack import SlackAdapter

__version__ = "0.1.0"

__all__ = [
    "AGENT_ORIGIN_PLATFORM",
    "INTERNAL_MARKER",
    "Adapter",
    "AdapterSendError",
    "AgentParticipant",
    "Attachment",
    "ConfigError",
    "Direction",
    "Directory",
    "EmailAdapter",
    "ForwardingRules",
    "Hop",
    "Identity",
    "IdentityKind",
    "ImapConfig",
    "InMemoryAdapter",
    "InboundMessage",
    "Member",
    "Message",
    "Provenance",
    "Reaction",
    "Responder",
    "Role",
    "Room",
    "RoomBinding",
    "Router",
    "SlackAdapter",
    "SmtpConfig",
    "ThreadRef",
    "build_email",
    "echo_responder",
    "llm_responder",
    "parse_inbound",
    "render_for_relay",
]
