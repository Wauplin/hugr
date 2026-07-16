"""Define huglets in Python, running on the Rust runtime.

Config keys mirror ``huggr.toml`` sections 1:1 (``models``, ``limits``,
``context``, ``grants`` for ``[tools]``); tools are ordinary Python callables
whose parameter schema is inferred from type annotations (or passed explicitly
via ``schema=``). Tool callables are trusted host code: Huggr jails what the
*model* can invoke, not what your Python does once invoked.
"""

from __future__ import annotations

import asyncio
import inspect
import json
import types
from dataclasses import dataclass
from typing import (
    Any,
    AsyncIterator,
    Awaitable,
    Callable,
    List,
    Optional,
    Sequence,
    Union,
    cast,
    get_args,
    get_origin,
    get_type_hints,
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
    CatalogModelsConfig,
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
    ModelCatalogConfig,
    ModelDetails,
    ModelStartedEvent,
    ModelStats,
    ModelTierCard,
    ModelsConfig,
    NoticeEvent,
    PathBlobRef,
    PathBlobRefInput,
    RootGrant,
    ProvidersConfig,
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
    "CatalogModelsConfig",
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
    "ModelCatalogConfig",
    "ModelDetails",
    "ModelStartedEvent",
    "ModelStats",
    "ModelTierCard",
    "ModelsConfig",
    "NoticeEvent",
    "PathBlobRef",
    "PathBlobRefInput",
    "RootGrant",
    "ProvidersConfig",
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

ToolFn = Callable[..., Union[JsonValue, Awaitable[JsonValue]]]
DictToolFn = Callable[[JsonObject], Union[JsonValue, Awaitable[JsonValue]]]


@dataclass
class Tool:
    """One model-invocable tool: a name/description/JSON-schema plus a sync or async callable taking the arguments dict."""

    fn: DictToolFn
    name: str
    description: str
    schema: JsonObject
    requires_permission: bool = False
    background: bool = False

    def __call__(self, args: JsonObject) -> Union[JsonValue, Awaitable[JsonValue]]:
        return self.fn(args)


_SCALAR_SCHEMAS: dict[Any, JsonObject] = {
    str: {"type": "string"},
    int: {"type": "integer"},
    float: {"type": "number"},
    bool: {"type": "boolean"},
}


def _annotation_schema(annotation: Any) -> JsonObject:
    """JSON schema for one parameter annotation; raises for shapes we cannot map."""
    if annotation in _SCALAR_SCHEMAS:
        return dict(_SCALAR_SCHEMAS[annotation])
    if annotation is list:
        return {"type": "array"}
    if annotation is dict:
        return {"type": "object"}
    if annotation is Any:
        return {}
    origin = get_origin(annotation)
    if origin is list:
        args = get_args(annotation)
        return {"type": "array", "items": _annotation_schema(args[0])} if args else {"type": "array"}
    if origin is dict:
        return {"type": "object"}
    if origin is Union or origin is getattr(types, "UnionType", None):
        variants = [a for a in get_args(annotation) if a is not type(None)]
        if len(variants) == 1:
            return _annotation_schema(variants[0])
    raise TypeError(
        f"cannot infer a JSON schema for annotation {annotation!r}; "
        "use str/int/float/bool/list[...]/dict/Optional[...]/X | None or pass schema= explicitly"
    )


def _schema_from_signature(fn: ToolFn) -> JsonObject:
    """Infer the tool's parameters schema from `fn`'s type annotations, FastAPI-style."""
    hints = get_type_hints(fn)
    properties: JsonObject = {}
    required: List[JsonValue] = []
    for param in inspect.signature(fn).parameters.values():
        if param.kind in (inspect.Parameter.VAR_POSITIONAL, inspect.Parameter.VAR_KEYWORD):
            raise TypeError(f"tool `{fn.__name__}` cannot use *args/**kwargs; declare named parameters")
        if param.kind is inspect.Parameter.POSITIONAL_ONLY:
            raise TypeError(
                f"tool `{fn.__name__}` parameter `{param.name}` is positional-only; "
                "tool arguments are passed by keyword, so drop the `/` marker"
            )
        if param.name not in hints:
            raise TypeError(
                f"tool `{fn.__name__}` parameter `{param.name}` has no type annotation; "
                "annotate it or pass schema= explicitly"
            )
        prop = _annotation_schema(hints[param.name])
        if param.default is not inspect.Parameter.empty:
            prop["default"] = param.default
        else:
            required.append(param.name)
        properties[param.name] = prop
    schema: JsonObject = {"type": "object", "properties": properties, "additionalProperties": False}
    if required:
        schema["required"] = required
    return schema


def _splat(fn: ToolFn) -> DictToolFn:
    """Adapt a named-parameters callable to the runtime's arguments-dict calling convention."""

    def call(args: JsonObject) -> Union[JsonValue, Awaitable[JsonValue]]:
        return fn(**args)

    return call


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
    """Wrap a callable as a :class:`Tool`. Usable as ``@tool``, ``@tool(...)``, or ``tool(fn, ...)``.

    By default the parameters schema is inferred from the function's type
    annotations, FastAPI-style: each named parameter becomes a schema property
    (`str`/`int`/`float`/`bool`/`list[...]`/`dict`/`Optional[...]`), parameters
    without defaults are required, and the model's arguments are passed as
    keyword arguments. The name defaults to the function name and the
    description to its docstring.

    Pass ``schema=`` to advertise a hand-written JSON schema instead; the
    callable then receives the raw arguments dict as its single parameter.
    """

    def wrap(fn: ToolFn) -> Tool:
        return Tool(
            fn=cast(DictToolFn, fn) if schema is not None else _splat(fn),
            name=name or fn.__name__,
            description=description or (fn.__doc__ or "").strip(),
            schema=schema if schema is not None else _schema_from_signature(fn),
            requires_permission=requires_permission,
            background=background,
        )

    return wrap(fn) if fn is not None else wrap


class Agent:
    """A huglet defined from Python data, assembled onto the Rust runtime.

    ``agent.ask(...)`` blocks and returns an :class:`Answer`; ``async for event
    in agent.run(...)`` streams the typed event vocabulary and ends with an
    ``answer_ready`` event. Traces persist under ``~/.huggr/<name>/`` exactly
    like every other surface.
    """

    def __init__(
        self,
        *,
        name: str,
        system: Optional[str] = None,
        models: Optional[ModelsConfig] = None,
        providers: Optional[ProvidersConfig] = None,
        model_overrides: Optional[ModelCatalogConfig] = None,
        api_token: Optional[str] = None,
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
            "models": models or {"default": "balanced"},
            "providers": providers or {},
            "model_overrides": model_overrides,
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
        self._native = NativeAgent(json.dumps(config), specs, api_token)

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
        skills: Sequence[str] = (),
        extra: JsonValue = None,
    ) -> Answer:
        raw = self._native.ask(_ask_json(question, trace_id, blobs, skills, extra))
        return Answer.from_dict(cast(AnswerDict, json.loads(raw)))

    async def run(
        self,
        question: str,
        *,
        trace_id: Optional[str] = None,
        blobs: Sequence[Union[BlobHandle, BlobInput]] = (),
        skills: Sequence[str] = (),
        extra: JsonValue = None,
    ) -> AsyncIterator[AgentEvent]:
        """Stream Rust-validated events cast into their public dataclasses."""
        stream = self._native.ask_events(_ask_json(question, trace_id, blobs, skills, extra))
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
    skills: Sequence[str],
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
    if skills:
        ask["skills"] = cast(JsonValue, list(skills))
    if extra is not None:
        ask["extra"] = extra
    return json.dumps(ask)
