"""Define Hugr subagents in Python, running on the Rust runtime.

Config keys mirror ``hugr.toml`` sections 1:1 (``models``, ``limits``,
``context``, ``grants`` for ``[tools]``); tools are ordinary Python callables.
Tool callables are trusted host code: Hugr jails what the *model* can invoke,
not what your Python does once invoked.
"""

from __future__ import annotations

import asyncio
import json
from dataclasses import dataclass
from typing import (
    AsyncIterator,
    Awaitable,
    Callable,
    List,
    Optional,
    Sequence,
    Union,
    cast,
    overload,
)

from ._native import NativeAgent
from ._types import (
    STATUS_ERROR,
    STATUS_SUCCESS,
    AgentCard,
    AgentCardDict,
    AgentEvent,
    AgentEventDict,
    AgentGrant,
    AgentGrants,
    AgentLimits,
    AgentStats,
    AgentStatsDict,
    AskStartedEvent,
    Answer,
    AnswerDict,
    AnswerMeta,
    AnswerReadyEvent,
    BlobRef,
    BlobRefInput,
    BlobHandle,
    BlobInput,
    BytesBlobRefInput,
    BytesBlobRef,
    ChildAgentStats,
    ContextConfig,
    ContextForgetConfig,
    DoneEvent,
    DoneReason,
    DurationStats,
    EmptyGrant,
    Feedback,
    FeedbackDict,
    GrantConfig,
    GrantsConfig,
    JsonObject,
    JsonScalar,
    JsonValue,
    LimitsConfig,
    McpGrant,
    McpGrants,
    MemoryGrant,
    ModelEndedEvent,
    ModelStartedEvent,
    ModelStats,
    ModelTierCard,
    ModelsConfig,
    NoticeEvent,
    PathBlobRef,
    PathBlobRefInput,
    RootGrant,
    Sha256BlobRef,
    Sha256BlobRefInput,
    StatsTotals,
    TextDeltaEvent,
    TierConfig,
    TierPrice,
    ToolCard,
    ToolEndedEvent,
    ToolSchema,
    ToolStartedEvent,
    ToolStats,
    TraceHead,
    TraceHeadDict,
    TraceStats,
    Usage,
    WebFetchGrant,
    agent_event_from_dict,
)

__all__ = [
    "Agent",
    "AgentCard",
    "AgentEvent",
    "AgentGrant",
    "AgentGrants",
    "AgentLimits",
    "AgentStats",
    "AskStartedEvent",
    "Answer",
    "AnswerMeta",
    "AnswerReadyEvent",
    "BlobHandle",
    "BlobInput",
    "BlobRef",
    "BlobRefInput",
    "BytesBlobRef",
    "BytesBlobRefInput",
    "ChildAgentStats",
    "ContextConfig",
    "ContextForgetConfig",
    "DoneEvent",
    "DoneReason",
    "DurationStats",
    "EmptyGrant",
    "Feedback",
    "GrantConfig",
    "GrantsConfig",
    "JsonObject",
    "JsonScalar",
    "JsonValue",
    "LimitsConfig",
    "McpGrant",
    "McpGrants",
    "MemoryGrant",
    "ModelEndedEvent",
    "ModelStartedEvent",
    "ModelStats",
    "ModelTierCard",
    "ModelsConfig",
    "NoticeEvent",
    "PathBlobRef",
    "PathBlobRefInput",
    "RootGrant",
    "Sha256BlobRef",
    "Sha256BlobRefInput",
    "StatsTotals",
    "TextDeltaEvent",
    "TierConfig",
    "TierPrice",
    "Tool",
    "ToolCard",
    "ToolEndedEvent",
    "ToolSchema",
    "ToolStartedEvent",
    "ToolStats",
    "TraceHead",
    "TraceStats",
    "Usage",
    "WebFetchGrant",
    "tool",
    "STATUS_SUCCESS",
    "STATUS_ERROR",
]

ToolFn = Callable[[JsonObject], Union[JsonValue, Awaitable[JsonValue]]]


@dataclass
class Tool:
    """One model-invocable tool: an explicit name/description/JSON-schema plus a sync or async callable."""

    fn: ToolFn
    name: str
    description: str
    schema: JsonObject
    requires_permission: bool = False
    background: bool = False

    def __call__(self, args: JsonObject) -> Union[JsonValue, Awaitable[JsonValue]]:
        return self.fn(args)


@overload
def tool(
    fn: ToolFn,
    *,
    name: Optional[str] = None,
    description: str = "",
    schema: Optional[JsonObject] = None,
    requires_permission: bool = False,
    background: bool = False,
) -> Tool:
    ...


@overload
def tool(
    fn: None = None,
    *,
    name: Optional[str] = None,
    description: str = "",
    schema: Optional[JsonObject] = None,
    requires_permission: bool = False,
    background: bool = False,
) -> Callable[[ToolFn], Tool]:
    ...


def tool(
    fn: Optional[ToolFn] = None,
    *,
    name: Optional[str] = None,
    description: str = "",
    schema: Optional[JsonObject] = None,
    requires_permission: bool = False,
    background: bool = False,
) -> Union[Tool, Callable[[ToolFn], Tool]]:
    """Wrap a callable as a :class:`Tool`. Usable as ``tool(fn, ...)`` or ``@tool(...)``.

    Schemas are explicit by design — the advertised tool surface stays auditable.
    """

    def wrap(fn: ToolFn) -> Tool:
        return Tool(
            fn=fn,
            name=name or fn.__name__,
            description=description or (fn.__doc__ or "").strip(),
            schema=schema or {"type": "object"},
            requires_permission=requires_permission,
            background=background,
        )

    return wrap(fn) if fn is not None else wrap


class Agent:
    """A Hugr subagent defined from Python data, assembled onto the Rust runtime.

    ``agent.ask(...)`` blocks and returns an :class:`Answer`; ``async for event
    in agent.run(...)`` streams the typed event vocabulary and ends with an
    ``answer_ready`` event. Traces persist under ``~/.hugr/<name>/`` exactly
    like every other surface.
    """

    def __init__(
        self,
        *,
        name: str,
        system: Optional[str] = None,
        models: Optional[ModelsConfig] = None,
        tools: Sequence[Tool] = (),
        grants: Optional[GrantsConfig] = None,
        limits: Optional[LimitsConfig] = None,
        context: Optional[ContextConfig] = None,
        response_schema: Optional[JsonObject] = None,
        version: str = "0.0.0",
        description: str = "",
        traces: Optional[str] = None,
        scratchpad: Optional[str] = None,
    ) -> None:
        config = {
            "name": name,
            "version": version,
            "description": description,
            "system": system,
            "models": models or {},
            "grants": grants or {},
            "limits": limits or {},
            "context": context or {},
            "response_schema": response_schema,
            "traces": traces,
            "scratchpad": scratchpad,
        }
        specs = [
            (
                t.name,
                t.description,
                json.dumps(t.schema),
                t.requires_permission,
                t.background,
                t.fn,
            )
            for t in tools
        ]
        self._native = NativeAgent(json.dumps(config), specs)

    @property
    def warnings(self) -> List[str]:
        return self._native.warnings()

    def describe(self) -> AgentCard:
        return AgentCard.from_dict(
            cast(AgentCardDict, json.loads(self._native.describe()))
        )

    def ask(
        self,
        question: str,
        *,
        trace_id: Optional[str] = None,
        blobs: Sequence[Union[BlobHandle, BlobInput]] = (),
        extra: JsonValue = None,
    ) -> Answer:
        raw = self._native.ask(_ask_json(question, trace_id, blobs, extra))
        return Answer.from_dict(cast(AnswerDict, json.loads(raw)))

    async def run(
        self,
        question: str,
        *,
        trace_id: Optional[str] = None,
        blobs: Sequence[Union[BlobHandle, BlobInput]] = (),
        extra: JsonValue = None,
    ) -> AsyncIterator[AgentEvent]:
        """Stream Rust-validated events cast into their public dataclasses."""
        stream = self._native.ask_events(_ask_json(question, trace_id, blobs, extra))
        while True:
            raw = await asyncio.to_thread(stream.next_event)
            if raw is None:
                return
            yield agent_event_from_dict(cast(AgentEventDict, json.loads(raw)))

    def feedback(self, trace_id: str, payload: JsonValue) -> Feedback:
        raw = self._native.feedback(trace_id, json.dumps(payload))
        return Feedback.from_dict(cast(FeedbackDict, json.loads(raw)))

    def feedback_for(self, trace_id: str) -> List[Feedback]:
        raw = self._native.feedback_for(trace_id)
        return [
            Feedback.from_dict(item)
            for item in cast(List[FeedbackDict], json.loads(raw))
        ]

    def traces(self) -> List[TraceHead]:
        return [
            TraceHead.from_dict(item)
            for item in cast(List[TraceHeadDict], json.loads(self._native.traces()))
        ]

    def stats(
        self, *, since: Optional[str] = None, trace: Optional[str] = None
    ) -> AgentStats:
        options: dict[str, str] = {}
        if since is not None:
            options["since"] = since
        if trace is not None:
            options["trace"] = trace
        return AgentStats.from_dict(
            cast(AgentStatsDict, json.loads(self._native.stats(json.dumps(options))))
        )


def _ask_json(
    question: str,
    trace_id: Optional[str],
    blobs: Sequence[Union[BlobHandle, BlobInput]],
    extra: JsonValue,
) -> str:
    ask: JsonObject = {"question": question}
    if trace_id is not None:
        ask["trace_id"] = trace_id
    if blobs:
        ask["blobs"] = cast(
            JsonValue,
            [
                blob.to_dict() if isinstance(blob, BlobHandle) else blob
                for blob in blobs
            ],
        )
    if extra is not None:
        ask["extra"] = extra
    return json.dumps(ask)
