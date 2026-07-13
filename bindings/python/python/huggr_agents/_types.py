"""Typed Python mirrors of the Rust-validated Huggr JSON boundary."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Dict, List, Literal, Optional, TypedDict, Union, cast

JsonScalar = Union[None, bool, int, float, str]
JsonValue = Union[JsonScalar, List["JsonValue"], Dict[str, "JsonValue"]]
JsonObject = Dict[str, JsonValue]

STATUS_SUCCESS = "success"
STATUS_ERROR = "error"


class _TierConfigOptional(TypedDict, total=False):
    input_usd_per_m_tokens: float
    output_usd_per_m_tokens: float


class TierConfig(_TierConfigOptional):
    provider: str
    model: str


ModelTier = Literal["fast", "balanced", "powerful", "max"]


class ModelsConfig(TypedDict, total=False):
    default: ModelTier
    fast: TierConfig
    balanced: TierConfig
    powerful: TierConfig
    max: TierConfig


class ProviderConfig(TypedDict):
    base_url: str
    api_key_env: str


ProvidersConfig = Dict[str, ProviderConfig]


class CatalogModelsConfig(TypedDict, total=False):
    fast: TierConfig
    balanced: TierConfig
    powerful: TierConfig
    max: TierConfig


class ModelCatalogConfig(TypedDict):
    providers: ProvidersConfig
    models: CatalogModelsConfig


class LimitsConfig(TypedDict, total=False):
    max_model_calls: int
    max_cost_micro_usd: int
    timeout_s: int


class ContextForgetConfig(TypedDict, total=False):
    tool_ttl: Dict[str, int]
    keep_last_per_tool: Dict[str, int]


class ContextConfig(TypedDict, total=False):
    budget_tokens: int
    compaction: Literal["none", "truncate", "summarize"]
    trigger_tokens: int
    keep_recent_tokens: int
    max_block_tokens: int
    summary_model: ModelTier
    forget: ContextForgetConfig


class RootGrant(TypedDict, total=False):
    root: str


class WebFetchGrant(TypedDict, total=False):
    allow_hosts: List[str]


class MemoryGrant(TypedDict, total=False):
    root: str
    readonly: bool


class _McpGrantOptional(TypedDict, total=False):
    args: List[str]


class McpGrant(_McpGrantOptional):
    command: str


class AgentGrant(TypedDict):
    artifact: str


McpGrants = Dict[str, McpGrant]
AgentGrants = Dict[str, AgentGrant]
GrantConfig = Union[
    RootGrant, WebFetchGrant, MemoryGrant, McpGrants, AgentGrants, JsonObject
]


class EmptyGrant(TypedDict, total=False):
    pass


class GrantsConfig(TypedDict, total=False):
    fs_read: RootGrant
    traces_read: RootGrant
    web_fetch: WebFetchGrant
    scratchpad: EmptyGrant
    memory: MemoryGrant
    mcp: McpGrants
    agent: AgentGrants


class BytesBlobRefInput(TypedDict):
    kind: Literal["bytes"]
    base64: str


class PathBlobRefInput(TypedDict):
    kind: Literal["path"]
    path: str


class Sha256BlobRefInput(TypedDict):
    kind: Literal["sha256"]
    sha256: str


BlobRefInput = Union[BytesBlobRefInput, PathBlobRefInput, Sha256BlobRefInput]


class _BlobInputOptional(TypedDict, total=False):
    name: str


class BlobInput(_BlobInputOptional):
    ref: BlobRefInput
    media_type: str


class AskDict(TypedDict, total=False):
    question: str
    trace_id: str
    blobs: List[BlobInput]
    skills: List[str]
    extra: JsonValue


class AnswerMetaDict(TypedDict):
    duration_ms: int
    cost_micro_usd: int
    tokens_in: int
    tokens_out: int
    model_calls: int
    tool_calls: int


class _AnswerDictOptional(TypedDict, total=False):
    blobs: List[BlobInput]
    extra: JsonValue


class AnswerDict(_AnswerDictOptional):
    status: str
    response: JsonObject
    trace_id: str
    metadata: AnswerMetaDict


class FeedbackDict(TypedDict):
    trace_id: str
    payload: JsonValue
    created_at_ms: int


class UsageDict(TypedDict):
    input_tokens: int
    output_tokens: int
    extra: JsonValue


@dataclass
class AnswerMeta:
    duration_ms: int = 0
    cost_micro_usd: int = 0
    tokens_in: int = 0
    tokens_out: int = 0
    model_calls: int = 0
    tool_calls: int = 0

    @classmethod
    def from_dict(cls, data: AnswerMetaDict) -> "AnswerMeta":
        return cls(
            duration_ms=data.get("duration_ms", 0),
            cost_micro_usd=data.get("cost_micro_usd", 0),
            tokens_in=data.get("tokens_in", 0),
            tokens_out=data.get("tokens_out", 0),
            model_calls=data.get("model_calls", 0),
            tool_calls=data.get("tool_calls", 0),
        )


@dataclass
class BytesBlobRef:
    base64: str
    kind: Literal["bytes"] = field(init=False, default="bytes")

    def to_dict(self) -> BytesBlobRefInput:
        return {"kind": self.kind, "base64": self.base64}


@dataclass
class PathBlobRef:
    path: str
    kind: Literal["path"] = field(init=False, default="path")

    def to_dict(self) -> PathBlobRefInput:
        return {"kind": self.kind, "path": self.path}


@dataclass
class Sha256BlobRef:
    sha256: str
    kind: Literal["sha256"] = field(init=False, default="sha256")

    def to_dict(self) -> Sha256BlobRefInput:
        return {"kind": self.kind, "sha256": self.sha256}


BlobRef = Union[BytesBlobRef, PathBlobRef, Sha256BlobRef]


def blob_ref_from_dict(data: BlobRefInput) -> BlobRef:
    kind = data["kind"]
    if kind == "bytes":
        bytes_value = cast(BytesBlobRefInput, data)
        return BytesBlobRef(base64=bytes_value["base64"])
    if kind == "path":
        path_value = cast(PathBlobRefInput, data)
        return PathBlobRef(path=path_value["path"])
    sha_value = cast(Sha256BlobRefInput, data)
    return Sha256BlobRef(sha256=sha_value["sha256"])


@dataclass
class BlobHandle:
    """A file handed into or out of an ask."""

    ref: BlobRef
    media_type: str
    name: Optional[str] = None

    @classmethod
    def from_dict(cls, data: BlobInput) -> "BlobHandle":
        return cls(
            ref=blob_ref_from_dict(data["ref"]),
            media_type=data["media_type"],
            name=data.get("name"),
        )

    def to_dict(self) -> BlobInput:
        out: BlobInput = {"ref": self.ref.to_dict(), "media_type": self.media_type}
        if self.name is not None:
            out["name"] = self.name
        return out

    @classmethod
    def from_bytes(
        cls,
        base64: str,
        media_type: str = "application/octet-stream",
        name: Optional[str] = None,
    ) -> "BlobHandle":
        return cls(ref=BytesBlobRef(base64), media_type=media_type, name=name)

    @classmethod
    def from_path(
        cls,
        path: str,
        media_type: str = "application/octet-stream",
        name: Optional[str] = None,
    ) -> "BlobHandle":
        return cls(ref=PathBlobRef(path), media_type=media_type, name=name)

    @classmethod
    def from_sha256(
        cls,
        sha256: str,
        media_type: str = "application/octet-stream",
        name: Optional[str] = None,
    ) -> "BlobHandle":
        return cls(ref=Sha256BlobRef(sha256), media_type=media_type, name=name)


@dataclass
class Answer:
    status: str
    response: JsonObject
    trace_id: str
    metadata: AnswerMeta
    blobs: List[BlobHandle] = field(default_factory=list)
    extra: JsonValue = None

    @property
    def ok(self) -> bool:
        return self.status == STATUS_SUCCESS

    @classmethod
    def from_dict(cls, data: AnswerDict) -> "Answer":
        return cls(
            status=data["status"],
            response=data["response"],
            trace_id=data["trace_id"],
            metadata=AnswerMeta.from_dict(
                data.get("metadata", cast(AnswerMetaDict, {}))
            ),
            blobs=[BlobHandle.from_dict(blob) for blob in data.get("blobs", [])],
            extra=data.get("extra"),
        )


@dataclass
class Feedback:
    trace_id: str
    payload: JsonValue
    created_at_ms: int = 0

    @classmethod
    def from_dict(cls, data: FeedbackDict) -> "Feedback":
        return cls(
            trace_id=data["trace_id"],
            payload=data.get("payload"),
            created_at_ms=data.get("created_at_ms", 0),
        )


@dataclass
class Usage:
    input_tokens: int
    output_tokens: int
    extra: JsonValue = None

    @classmethod
    def from_dict(cls, data: UsageDict) -> "Usage":
        return cls(
            input_tokens=data["input_tokens"],
            output_tokens=data["output_tokens"],
            extra=data.get("extra"),
        )


@dataclass
class DoneReason:
    """A normalized view of Rust's externally tagged ``DoneReason``."""

    kind: str
    message: Optional[str] = None

    @classmethod
    def from_json(cls, data: JsonValue) -> "DoneReason":
        if isinstance(data, str):
            return cls(
                kind={"EndTurn": "end_turn", "Cancelled": "cancelled"}.get(data, data)
            )
        error = cast(JsonObject, data).get("Error")
        return cls(kind="error", message=cast(Optional[str], error))


@dataclass
class AskStartedEvent:
    trace_parent: Optional[str]
    type: Literal["ask_started"] = field(init=False, default="ask_started")


@dataclass
class ModelStartedEvent:
    op: int
    tier: str
    type: Literal["model_started"] = field(init=False, default="model_started")


@dataclass
class TextDeltaEvent:
    op: int
    text: str
    type: Literal["text_delta"] = field(init=False, default="text_delta")


@dataclass
class ModelEndedEvent:
    op: int
    usage: Usage
    type: Literal["model_ended"] = field(init=False, default="model_ended")


@dataclass
class ToolStartedEvent:
    op: int
    name: str
    args: JsonValue
    type: Literal["tool_started"] = field(init=False, default="tool_started")


@dataclass
class ToolEndedEvent:
    op: int
    name: str
    is_error: bool
    result: JsonValue
    type: Literal["tool_ended"] = field(init=False, default="tool_ended")


@dataclass
class NoticeEvent:
    message: str
    type: Literal["notice"] = field(init=False, default="notice")


@dataclass
class DoneEvent:
    reason: DoneReason
    type: Literal["done"] = field(init=False, default="done")


@dataclass
class AnswerReadyEvent:
    answer: Answer
    type: Literal["answer_ready"] = field(init=False, default="answer_ready")


AgentEvent = Union[
    AskStartedEvent,
    ModelStartedEvent,
    TextDeltaEvent,
    ModelEndedEvent,
    ToolStartedEvent,
    ToolEndedEvent,
    NoticeEvent,
    DoneEvent,
    AnswerReadyEvent,
]


class AskStartedEventDict(TypedDict):
    type: Literal["ask_started"]
    trace_parent: Optional[str]


class ModelStartedEventDict(TypedDict):
    type: Literal["model_started"]
    op: int
    tier: str


class TextDeltaEventDict(TypedDict):
    type: Literal["text_delta"]
    op: int
    text: str


class ModelEndedEventDict(TypedDict):
    type: Literal["model_ended"]
    op: int
    usage: UsageDict


class ToolStartedEventDict(TypedDict):
    type: Literal["tool_started"]
    op: int
    name: str
    args: JsonValue


class ToolEndedEventDict(TypedDict):
    type: Literal["tool_ended"]
    op: int
    name: str
    is_error: bool
    result: JsonValue


class NoticeEventDict(TypedDict):
    type: Literal["notice"]
    message: str


class DoneEventDict(TypedDict):
    type: Literal["done"]
    reason: JsonValue


class AnswerReadyEventDict(TypedDict):
    type: Literal["answer_ready"]
    answer: AnswerDict


AgentEventDict = Union[
    AskStartedEventDict,
    ModelStartedEventDict,
    TextDeltaEventDict,
    ModelEndedEventDict,
    ToolStartedEventDict,
    ToolEndedEventDict,
    NoticeEventDict,
    DoneEventDict,
    AnswerReadyEventDict,
]


def agent_event_from_dict(data: AgentEventDict) -> AgentEvent:
    event_type = data["type"]
    if event_type == "ask_started":
        ask_started = cast(AskStartedEventDict, data)
        return AskStartedEvent(trace_parent=ask_started["trace_parent"])
    if event_type == "model_started":
        model_started = cast(ModelStartedEventDict, data)
        return ModelStartedEvent(op=model_started["op"], tier=model_started["tier"])
    if event_type == "text_delta":
        text_delta = cast(TextDeltaEventDict, data)
        return TextDeltaEvent(op=text_delta["op"], text=text_delta["text"])
    if event_type == "model_ended":
        model_ended = cast(ModelEndedEventDict, data)
        return ModelEndedEvent(
            op=model_ended["op"], usage=Usage.from_dict(model_ended["usage"])
        )
    if event_type == "tool_started":
        tool_started = cast(ToolStartedEventDict, data)
        return ToolStartedEvent(
            op=tool_started["op"],
            name=tool_started["name"],
            args=tool_started["args"],
        )
    if event_type == "tool_ended":
        tool_ended = cast(ToolEndedEventDict, data)
        return ToolEndedEvent(
            op=tool_ended["op"],
            name=tool_ended["name"],
            is_error=tool_ended["is_error"],
            result=tool_ended["result"],
        )
    if event_type == "notice":
        notice = cast(NoticeEventDict, data)
        return NoticeEvent(message=notice["message"])
    if event_type == "done":
        done = cast(DoneEventDict, data)
        return DoneEvent(reason=DoneReason.from_json(done["reason"]))
    answer_ready = cast(AnswerReadyEventDict, data)
    return AnswerReadyEvent(answer=Answer.from_dict(answer_ready["answer"]))


@dataclass
class AgentLimits:
    max_model_calls: Optional[int] = None
    max_cost_micro_usd: Optional[int] = None
    timeout_ms: Optional[int] = None


@dataclass
class ToolSchema:
    name: str
    description: str
    parameters: JsonValue


@dataclass
class ToolCard:
    name: str
    description: str
    privilege: str
    runs_in_background: bool
    schema: ToolSchema
    scope: JsonValue = None


@dataclass
class TierPrice:
    input_usd_per_m_tokens: float
    output_usd_per_m_tokens: float


@dataclass
class ModelTierCard:
    selector: str
    default: bool
    pricing: Optional[TierPrice] = None
    details: Optional["ModelDetails"] = None


@dataclass
class ModelDetails:
    provider: str
    model: str
    base_url: str
    api_key_env: str
    api_key_resolved: bool
    source: str
    resolved_from: str


class AgentLimitsDict(TypedDict, total=False):
    max_model_calls: int
    max_cost_micro_usd: int
    timeout_ms: int


class ToolSchemaDict(TypedDict):
    name: str
    description: str
    parameters: JsonValue


class _ToolCardDictOptional(TypedDict, total=False):
    scope: JsonValue


class ToolCardDict(_ToolCardDictOptional):
    name: str
    description: str
    privilege: str
    runs_in_background: bool
    schema: ToolSchemaDict


class TierPriceDict(TypedDict):
    input_usd_per_m_tokens: float
    output_usd_per_m_tokens: float


class _ModelTierCardDictOptional(TypedDict, total=False):
    pricing: TierPriceDict
    details: "ModelDetailsDict"


class ModelDetailsDict(TypedDict):
    provider: str
    model: str
    base_url: str
    api_key_env: str
    api_key_resolved: bool
    source: str
    resolved_from: str


class ModelTierCardDict(_ModelTierCardDictOptional):
    selector: str
    default: bool


class _AgentCardDictOptional(TypedDict, total=False):
    context: JsonValue


class AgentCardDict(_AgentCardDictOptional):
    name: str
    version: str
    description: str
    tools: List[ToolCardDict]
    model_tiers: List[ModelTierCardDict]
    limits: AgentLimitsDict


@dataclass
class AgentCard:
    name: str
    version: str
    description: str
    tools: List[ToolCard]
    model_tiers: List[ModelTierCard]
    context: JsonValue
    limits: AgentLimits

    @classmethod
    def from_dict(cls, data: AgentCardDict) -> "AgentCard":
        return cls(
            name=data["name"],
            version=data["version"],
            description=data["description"],
            tools=[
                ToolCard(
                    name=tool["name"],
                    description=tool["description"],
                    privilege=tool["privilege"],
                    runs_in_background=tool["runs_in_background"],
                    schema=ToolSchema(
                        name=tool["schema"]["name"],
                        description=tool["schema"]["description"],
                        parameters=tool["schema"]["parameters"],
                    ),
                    scope=tool.get("scope"),
                )
                for tool in data["tools"]
            ],
            model_tiers=[
                ModelTierCard(
                    selector=tier["selector"],
                    default=tier["default"],
                    pricing=TierPrice(**tier["pricing"])
                    if tier.get("pricing") is not None
                    else None,
                    details=ModelDetails(**tier["details"])
                    if tier.get("details") is not None
                    else None,
                )
                for tier in data["model_tiers"]
            ],
            context=data.get("context"),
            limits=AgentLimits(**data["limits"]),
        )


@dataclass
class TraceHead:
    trace_id: str
    depends_on: Optional[str]
    agent_name: str
    agent_version: str
    created_at: Optional[int]
    question: str
    status: str

    @classmethod
    def from_dict(cls, data: "TraceHeadDict") -> "TraceHead":
        return cls(
            trace_id=data["trace_id"],
            depends_on=data.get("depends_on"),
            agent_name=data["agent_name"],
            agent_version=data["agent_version"],
            created_at=data.get("created_at"),
            question=data["question"],
            status=data["status"],
        )


class _TraceHeadDictOptional(TypedDict, total=False):
    depends_on: Optional[str]
    created_at: Optional[int]


class TraceHeadDict(_TraceHeadDictOptional):
    trace_id: str
    agent_name: str
    agent_version: str
    question: str
    status: str


@dataclass
class StatsTotals:
    duration_ms: int
    cost_micro_usd: int
    cost_own_micro_usd: int
    cost_delegated_micro_usd: int
    tokens_in: int
    tokens_out: int
    model_calls: int
    tool_calls: int


@dataclass
class DurationStats:
    mean_ms: int
    median_ms: int
    p95_ms: int


@dataclass
class TraceStats:
    trace_id: str
    depends_on: Optional[str]
    question: str
    status: str
    created_at: Optional[int]
    feedback_count: int
    totals: StatsTotals


@dataclass
class ModelStats:
    selector: str
    calls: int
    tokens_in: int
    tokens_out: int
    cost_micro_usd: int


@dataclass
class ToolStats:
    name: str
    calls: int
    error_count: int
    total_latency_ms: int
    mean_latency_ms: int


@dataclass
class ChildAgentStats:
    name: str
    calls: int
    cost_delegated_micro_usd: int


class StatsTotalsDict(TypedDict):
    duration_ms: int
    cost_micro_usd: int
    cost_own_micro_usd: int
    cost_delegated_micro_usd: int
    tokens_in: int
    tokens_out: int
    model_calls: int
    tool_calls: int


class DurationStatsDict(TypedDict):
    mean_ms: int
    median_ms: int
    p95_ms: int


class _TraceStatsDictOptional(TypedDict, total=False):
    depends_on: Optional[str]
    created_at: Optional[int]


class TraceStatsDict(_TraceStatsDictOptional):
    trace_id: str
    question: str
    status: str
    feedback_count: int
    totals: StatsTotalsDict


class ModelStatsDict(TypedDict):
    selector: str
    calls: int
    tokens_in: int
    tokens_out: int
    cost_micro_usd: int


class ToolStatsDict(TypedDict):
    name: str
    calls: int
    error_count: int
    total_latency_ms: int
    mean_latency_ms: int


class ChildAgentStatsDict(TypedDict):
    name: str
    calls: int
    cost_delegated_micro_usd: int


class AgentStatsDict(TypedDict):
    ask_count: int
    feedback_count: int
    totals: StatsTotalsDict
    duration: DurationStatsDict
    traces: List[TraceStatsDict]
    models: List[ModelStatsDict]
    tools: List[ToolStatsDict]
    children: List[ChildAgentStatsDict]


@dataclass
class AgentStats:
    ask_count: int
    feedback_count: int
    totals: StatsTotals
    duration: DurationStats
    traces: List[TraceStats]
    models: List[ModelStats]
    tools: List[ToolStats]
    children: List[ChildAgentStats]

    @classmethod
    def from_dict(cls, data: AgentStatsDict) -> "AgentStats":
        return cls(
            ask_count=data["ask_count"],
            feedback_count=data["feedback_count"],
            totals=StatsTotals(**data["totals"]),
            duration=DurationStats(**data["duration"]),
            traces=[
                TraceStats(
                    trace_id=trace["trace_id"],
                    depends_on=trace.get("depends_on"),
                    question=trace["question"],
                    status=trace["status"],
                    created_at=trace.get("created_at"),
                    feedback_count=trace["feedback_count"],
                    totals=StatsTotals(**trace["totals"]),
                )
                for trace in data["traces"]
            ],
            models=[ModelStats(**model) for model in data["models"]],
            tools=[ToolStats(**tool) for tool in data["tools"]],
            children=[ChildAgentStats(**child) for child in data["children"]],
        )
