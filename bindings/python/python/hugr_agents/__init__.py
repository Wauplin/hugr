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
from typing import Any, AsyncIterator, Awaitable, Callable, Dict, List, Optional, Sequence, Union

from ._native import NativeAgent
from ._types import STATUS_ERROR, STATUS_SUCCESS, Answer, AnswerMeta, BlobHandle, Feedback

__all__ = [
    "Agent",
    "Answer",
    "AnswerMeta",
    "BlobHandle",
    "Feedback",
    "Tool",
    "tool",
    "STATUS_SUCCESS",
    "STATUS_ERROR",
]

ToolFn = Callable[[Dict[str, Any]], Union[Any, Awaitable[Any]]]


@dataclass
class Tool:
    """One model-invocable tool: an explicit name/description/JSON-schema plus a sync or async callable."""

    fn: ToolFn
    name: str
    description: str
    schema: Dict[str, Any]
    requires_permission: bool = False
    background: bool = False

    def __call__(self, args: Dict[str, Any]) -> Any:
        return self.fn(args)


def tool(
    fn: Optional[ToolFn] = None,
    *,
    name: Optional[str] = None,
    description: str = "",
    schema: Optional[Dict[str, Any]] = None,
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
        models: Optional[Dict[str, Any]] = None,
        tools: Sequence[Tool] = (),
        grants: Optional[Dict[str, Any]] = None,
        limits: Optional[Dict[str, Any]] = None,
        context: Optional[Dict[str, Any]] = None,
        response_schema: Optional[Dict[str, Any]] = None,
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
            (t.name, t.description, json.dumps(t.schema), t.requires_permission, t.background, t.fn)
            for t in tools
        ]
        self._native = NativeAgent(json.dumps(config), specs)

    @property
    def warnings(self) -> List[str]:
        return self._native.warnings()

    def describe(self) -> Dict[str, Any]:
        return json.loads(self._native.describe())

    def ask(
        self,
        question: str,
        *,
        trace_id: Optional[str] = None,
        blobs: Sequence[BlobHandle] = (),
        extra: Any = None,
    ) -> Answer:
        raw = self._native.ask(_ask_json(question, trace_id, blobs, extra))
        return Answer.from_dict(json.loads(raw))

    async def run(
        self,
        question: str,
        *,
        trace_id: Optional[str] = None,
        blobs: Sequence[BlobHandle] = (),
        extra: Any = None,
    ) -> AsyncIterator[Dict[str, Any]]:
        """Stream this ask's events (dicts tagged by ``type``); the final event is ``answer_ready``."""
        stream = self._native.ask_events(_ask_json(question, trace_id, blobs, extra))
        while True:
            raw = await asyncio.to_thread(stream.next_event)
            if raw is None:
                return
            yield json.loads(raw)

    def feedback(self, trace_id: str, payload: Any) -> Feedback:
        raw = self._native.feedback(trace_id, json.dumps(payload))
        return Feedback.from_dict(json.loads(raw))

    def feedback_for(self, trace_id: str) -> List[Feedback]:
        raw = self._native.feedback_for(trace_id)
        return [Feedback.from_dict(f) for f in json.loads(raw)]

    def traces(self) -> List[Dict[str, Any]]:
        return json.loads(self._native.traces())

    def stats(self, *, since: Optional[str] = None, trace: Optional[str] = None) -> Dict[str, Any]:
        options: Dict[str, Any] = {}
        if since is not None:
            options["since"] = since
        if trace is not None:
            options["trace"] = trace
        return json.loads(self._native.stats(json.dumps(options)))


def _ask_json(question: str, trace_id: Optional[str], blobs: Sequence[BlobHandle], extra: Any) -> str:
    ask: Dict[str, Any] = {"question": question}
    if trace_id is not None:
        ask["trace_id"] = trace_id
    if blobs:
        ask["blobs"] = [b.to_dict() for b in blobs]
    if extra is not None:
        ask["extra"] = extra
    return json.dumps(ask)
