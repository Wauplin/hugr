import asyncio
import json
import sys
from dataclasses import is_dataclass

import pytest

import huggr_agents as huggr
from mock_server import MockOpenAi


@pytest.fixture()
def server():
    server = MockOpenAi()
    yield server
    server.close()


@pytest.fixture(autouse=True)
def huggr_home(tmp_path, monkeypatch):
    monkeypatch.setenv("HUGGR_HOME", str(tmp_path / "huggr-home"))
    monkeypatch.delenv("HUGGR_AGENT_HOME", raising=False)
    return tmp_path


def make_agent(server, tools=(), **kwargs):
    return huggr.Agent(
        name="py-test-agent",
        system="Answer as JSON.",
        providers={"test": {"base_url": server.base_url, "api_key_env": "TEST_KEY"}},
        models={
            "default": "balanced",
            "balanced": {
                "provider": "test",
                "model": "mock-model",
                "input_usd_per_m_tokens": 1.0,
                "output_usd_per_m_tokens": 2.0,
            },
        },
        tools=tools,
        **kwargs,
    )


def lookup_tool(calls):
    @huggr.tool(
        name="lookup",
        description="Look a word up.",
        schema={
            "type": "object",
            "properties": {"word": {"type": "string"}},
            "required": ["word"],
        },
    )
    def lookup(args):
        calls.append(args)
        return {"definition": f"meaning of {args['word']}"}

    return lookup


def test_sync_tool_round_trip(server, huggr_home):
    calls = []
    agent = make_agent(server, tools=[lookup_tool(calls)])
    server.script_tool_call("lookup", {"word": "huggr"})
    server.script_text(json.dumps({"answer": "huggr is a toolkit"}))

    answer = agent.ask("What is huggr?")

    assert answer.ok
    assert answer.response == {"answer": "huggr is a toolkit"}
    assert calls == [{"word": "huggr"}]
    assert answer.metadata.model_calls == 2
    assert answer.metadata.tool_calls == 1
    assert answer.metadata.cost_micro_usd > 0
    # The tool result was sent back to the model on the second request.
    second = server.requests[1]
    assert any(m.get("role") == "tool" for m in second["messages"])
    # Traces land under HUGGR_HOME/<agent>/traces (idea 17 layout).
    traces_dir = huggr_home / "huggr-home" / "py-test-agent" / "traces"
    assert any(traces_dir.glob("*.json"))


def test_runtime_model_catalog_overrides_author_mapping(server):
    agent = make_agent(
        server,
        model_overrides={
            "providers": {
                "runtime": {
                    "base_url": server.base_url,
                    "api_key_env": "RUNTIME_KEY",
                }
            },
            "models": {
                "powerful": {
                    "provider": "runtime",
                    "model": "runtime-model",
                    "input_usd_per_m_tokens": 0.5,
                    "output_usd_per_m_tokens": 1.0,
                }
            },
        },
    )
    tiers = {tier.selector: tier for tier in agent.describe().model_tiers}
    assert tiers["balanced"].details is not None
    assert tiers["balanced"].details.model == "runtime-model"
    assert tiers["balanced"].details.source == "runtime"
    assert tiers["balanced"].details.resolved_from == "powerful"


def test_async_tool(server):
    calls = []

    @huggr.tool(name="lookup", description="d", schema={"type": "object"})
    async def lookup(args):
        await asyncio.sleep(0)
        calls.append(args)
        return {"definition": "async ok"}

    agent = make_agent(server, tools=[lookup])
    server.script_tool_call("lookup", {"word": "x"})
    server.script_text('{"answer": "done"}')
    answer = agent.ask("q")
    assert answer.ok
    assert calls == [{"word": "x"}]


def test_tool_exception_is_semantic_error(server):
    @huggr.tool(name="boom", description="d", schema={"type": "object"})
    def boom(args):
        raise RuntimeError("kaput")

    agent = make_agent(server, tools=[boom])
    server.script_tool_call("boom", {})
    server.script_text('{"answer": "recovered"}')
    answer = agent.ask("q")
    assert answer.ok
    tool_msg = next(
        m for m in server.requests[1]["messages"] if m.get("role") == "tool"
    )
    assert "kaput" in tool_msg["content"]


def test_errors_are_answers(server):
    agent = make_agent(server)
    # No scripted output → the mock returns HTTP 500 → error answer, not an exception.
    answer = agent.ask("q")
    assert answer.status == huggr.STATUS_ERROR
    assert "error" in answer.response
    assert answer.trace_id


def test_resume_and_fork(server):
    agent = make_agent(server)
    server.script_text('{"answer": "first"}')
    first = agent.ask("first question")
    assert first.ok

    server.script_text('{"answer": "second"}')
    second = agent.ask("follow-up", trace_id=first.trace_id)
    assert second.ok
    assert second.trace_id != first.trace_id
    # The resumed turn re-fed the parent conversation to the model.
    resumed_messages = server.requests[-1]["messages"]
    assert any("first question" in json.dumps(m) for m in resumed_messages)
    heads = agent.traces()
    assert all(is_dataclass(head) for head in heads)
    by_id = {head.trace_id: head for head in heads}
    assert by_id[second.trace_id].depends_on == first.trace_id


def test_event_stream_ordering(server):
    calls = []
    agent = make_agent(server, tools=[lookup_tool(calls)])
    server.script_tool_call("lookup", {"word": "huggr"})
    server.script_text('{"answer": "ok"}')

    async def collect():
        return [event async for event in agent.run("q")]

    events = asyncio.run(collect())
    assert all(is_dataclass(event) for event in events)
    types = [event.type for event in events]
    assert types[0] == "ask_started"
    assert types[-1] == "answer_ready"
    assert "tool_started" in types and "tool_ended" in types
    assert types.index("tool_started") < types.index("tool_ended")
    assert "model_started" in types and "text_delta" in types
    model_ended = next(
        event for event in events if isinstance(event, huggr.ModelEndedEvent)
    )
    assert is_dataclass(model_ended.usage)
    done = next(event for event in events if isinstance(event, huggr.DoneEvent))
    assert is_dataclass(done.reason)
    assert done.reason.kind == "end_turn"
    ready = events[-1]
    assert isinstance(ready, huggr.AnswerReadyEvent)
    assert is_dataclass(ready.answer)
    assert ready.answer.ok


def test_feedback_round_trip(server):
    agent = make_agent(server)
    server.script_text('{"answer": "x"}')
    answer = agent.ask("q")
    fb = agent.feedback(answer.trace_id, {"score": 5, "note": "helped"})
    assert fb.trace_id == answer.trace_id
    stored = agent.feedback_for(answer.trace_id)
    assert [f.payload for f in stored] == [{"score": 5, "note": "helped"}]
    stats = agent.stats()
    assert is_dataclass(stats)
    assert is_dataclass(stats.totals)
    assert all(
        is_dataclass(trace) and is_dataclass(trace.totals) for trace in stats.traces
    )
    with pytest.raises(RuntimeError):
        agent.feedback("no-such-trace", {"score": 0})


def test_response_contract_casts_final_json(server):
    agent = make_agent(
        server,
        response_schema={
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
            "additionalProperties": False,
        },
    )
    server.script_text('{"answer": "typed"}')
    answer = agent.ask("q")
    assert answer.ok
    # The schema rode the provider request as response_format.
    assert server.requests[0]["response_format"]["json_schema"]["schema"][
        "required"
    ] == ["answer"]


def test_blob_input_uses_typed_ref_dataclass():
    blob = huggr.BlobHandle.from_path("./report.pdf", media_type="application/pdf")

    assert is_dataclass(blob)
    assert isinstance(blob.ref, huggr.PathBlobRef)
    assert blob.ref.path == "./report.pdf"
    assert blob.to_dict() == {
        "ref": {"kind": "path", "path": "./report.pdf"},
        "media_type": "application/pdf",
    }


def test_inferred_schema_from_annotations():
    @huggr.tool
    def lookup(word: str, limit: int = 3) -> dict:
        """Look a word up."""
        return {"word": word, "limit": limit}

    assert lookup.name == "lookup"
    assert lookup.description == "Look a word up."
    assert lookup.schema == {
        "type": "object",
        "properties": {
            "word": {"type": "string"},
            "limit": {"type": "integer", "default": 3},
        },
        "additionalProperties": False,
        "required": ["word"],
    }
    # The runtime's arguments dict is splatted into the named parameters.
    assert lookup({"word": "huggr"}) == {"word": "huggr", "limit": 3}


def test_inferred_schema_optional_list_and_dict():
    from typing import Optional

    @huggr.tool
    def report(tags: list[str], meta: dict, note: Optional[str] = None):
        return {"tags": tags, "meta": meta, "note": note}

    props = report.schema["properties"]
    assert props["tags"] == {"type": "array", "items": {"type": "string"}}
    assert props["meta"] == {"type": "object"}
    assert props["note"] == {"type": "string", "default": None}
    assert report.schema["required"] == ["tags", "meta"]


@pytest.mark.skipif(sys.version_info < (3, 10), reason="PEP 604 unions need Python 3.10")
def test_inferred_schema_pep604_union():
    @huggr.tool
    def report(note: str | None = None):
        return {"note": note}

    assert report.schema["properties"]["note"] == {"type": "string", "default": None}
    assert "required" not in report.schema


def test_inferred_schema_rejects_unannotated_params():
    with pytest.raises(TypeError, match="no type annotation"):

        @huggr.tool
        def bad(word):
            return word


def test_inferred_schema_rejects_positional_only_params():
    with pytest.raises(TypeError, match="`word` is positional-only"):

        @huggr.tool
        def bad(word: str, /):
            return word


def test_inferred_tool_round_trip(server):
    calls = []

    @huggr.tool
    def lookup(word: str):
        """Look a word up."""
        calls.append(word)
        return {"definition": f"meaning of {word}"}

    agent = make_agent(server, tools=[lookup])
    server.script_tool_call("lookup", {"word": "huggr"})
    server.script_text('{"answer": "ok"}')
    answer = agent.ask("What is huggr?")
    assert answer.ok
    assert calls == ["huggr"]


def test_inferred_async_tool_round_trip(server):
    calls = []

    @huggr.tool
    async def lookup(word: str):
        """Look a word up."""
        await asyncio.sleep(0)
        calls.append(word)
        return {"definition": "async ok"}

    agent = make_agent(server, tools=[lookup])
    server.script_tool_call("lookup", {"word": "x"})
    server.script_text('{"answer": "done"}')
    answer = agent.ask("q")
    assert answer.ok
    assert calls == ["x"]


def test_describe_lists_tools_and_tiers(server):
    agent = make_agent(server, tools=[lookup_tool([])])
    card = agent.describe()
    assert is_dataclass(card)
    names = [tool.name for tool in card.tools]
    assert "lookup" in names and "scratch_write" in names
    assert all(is_dataclass(tool) and is_dataclass(tool.schema) for tool in card.tools)
    assert [tier.selector for tier in card.model_tiers] == [
        "fast",
        "balanced",
        "powerful",
        "max",
    ]
    balanced = card.model_tiers[1]
    assert balanced.details is not None
    assert balanced.details.model == "mock-model"
    assert balanced.default is True
