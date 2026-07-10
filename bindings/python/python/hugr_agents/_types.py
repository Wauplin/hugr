"""Typed mirrors of the Hugr JSON contract. Field names are identical to the wire form."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional

STATUS_SUCCESS = "success"
STATUS_ERROR = "error"


@dataclass
class AnswerMeta:
    duration_ms: int = 0
    cost_micro_usd: int = 0
    tokens_in: int = 0
    tokens_out: int = 0
    model_calls: int = 0
    tool_calls: int = 0

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "AnswerMeta":
        return cls(**{k: data.get(k, 0) for k in cls.__dataclass_fields__})


@dataclass
class BlobHandle:
    """A file handed into or out of an ask. `ref` is the wire-form blob reference object."""

    ref: Dict[str, Any]
    media_type: str
    name: Optional[str] = None

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "BlobHandle":
        return cls(ref=data["ref"], media_type=data["media_type"], name=data.get("name"))

    def to_dict(self) -> Dict[str, Any]:
        out: Dict[str, Any] = {"ref": self.ref, "media_type": self.media_type}
        if self.name is not None:
            out["name"] = self.name
        return out

    @classmethod
    def from_path(cls, path: str, media_type: str = "application/octet-stream", name: Optional[str] = None) -> "BlobHandle":
        return cls(ref={"kind": "path", "path": path}, media_type=media_type, name=name)

    @classmethod
    def from_sha256(cls, sha256: str, media_type: str = "application/octet-stream", name: Optional[str] = None) -> "BlobHandle":
        return cls(ref={"kind": "sha256", "sha256": sha256}, media_type=media_type, name=name)


@dataclass
class Answer:
    status: str
    response: Dict[str, Any]
    trace_id: str
    metadata: AnswerMeta
    blobs: List[BlobHandle] = field(default_factory=list)
    extra: Any = None

    @property
    def ok(self) -> bool:
        return self.status == STATUS_SUCCESS

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "Answer":
        return cls(
            status=data["status"],
            response=data["response"],
            trace_id=data["trace_id"],
            metadata=AnswerMeta.from_dict(data.get("metadata", {})),
            blobs=[BlobHandle.from_dict(b) for b in data.get("blobs", [])],
            extra=data.get("extra"),
        )


@dataclass
class Feedback:
    trace_id: str
    payload: Any
    created_at_ms: int = 0

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "Feedback":
        return cls(
            trace_id=data["trace_id"],
            payload=data.get("payload"),
            created_at_ms=data.get("created_at_ms", 0),
        )
