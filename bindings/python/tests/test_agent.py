import asyncio
import json

import pytest

import hugr_agents as hugr
from mock_server import MockOpenAi


@pytest.fixture()
def server():
    server = MockOpenAi()
    yield server
    server.close()


@pytest.fixture(autouse=True)
def hugr_home(tmp_path, monkeypatch):
    monkeypatch.setenv("HUGR_HOME", str(tmp_path / "hugr-home"))
    monkeypatch.delenv("HUGR_AGENT_HOME", raising=False)
    return tmp_path


def make_agent(server, tools=(), **kwargs):
    return hugr.Agent(
        name="py-test-agent",
        system="Answer as JSON.",
        models={
            "base_url": server.base_url,
            "default": "medium",
            "medium": {"model": "mock-model", "input_usd_per_m_tokens": 1.0, "output_usd_per_m_tokens": 2.0},
        },
        tools=tools,
        **kwargs,
    )


def lookup_tool(calls):
    @hugr.tool(
        name="lookup",
        description="Look a word up.",
        schema={"type": "object", "properties": {"word": {"type": "string"}}, "required": ["word"]},
    )
    def lookup(args):
        calls.append(args)
        return {"definition": f"meaning of {args['word']}"}

    return lookup


def test_sync_tool_round_trip(server, hugr_home):
    calls = []
    agent = make_agent(server, tools=[lookup_tool(calls)])
    server.script_tool_call("lookup", {"word": "hugr"})
    server.script_text(json.dumps({"answer": "hugr is a toolkit"}))

    answer = agent.ask("What is hugr?")

    assert answer.ok
    assert answer.response == {"answer": "hugr is a toolkit"}
    assert calls == [{"word": "hugr"}]
    assert answer.metadata.model_calls == 2
    assert answer.metadata.tool_calls == 1
    assert answer.metadata.cost_micro_usd > 0
    # The tool result was sent back to the model on the second request.
    second = server.requests[1]
    assert any(m.get("role") == "tool" for m in second["messages"])
    # Traces land under HUGR_HOME/<agent>/traces (idea 17 layout).
    traces_dir = hugr_home / "hugr-home" / "py-test-agent" / "traces"
    assert any(traces_dir.glob("*.json"))


def test_async_tool(server):
    calls = []

    @hugr.tool(name="lookup", description="d", schema={"type": "object"})
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
    @hugr.tool(name="boom", description="d", schema={"type": "object"})
    def boom(args):
        raise RuntimeError("kaput")

    agent = make_agent(server, tools=[boom])
    server.script_tool_call("boom", {})
    server.script_text('{"answer": "recovered"}')
    answer = agent.ask("q")
    assert answer.ok
    tool_msg = next(m for m in server.requests[1]["messages"] if m.get("role") == "tool")
    assert "kaput" in tool_msg["content"]


def test_errors_are_answers(server):
    agent = make_agent(server)
    # No scripted output → the mock returns HTTP 500 → error answer, not an exception.
    answer = agent.ask("q")
    assert answer.status == hugr.STATUS_ERROR
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
    by_id = {h["trace_id"]: h for h in heads}
    assert by_id[second.trace_id]["depends_on"] == first.trace_id


def test_event_stream_ordering(server):
    calls = []
    agent = make_agent(server, tools=[lookup_tool(calls)])
    server.script_tool_call("lookup", {"word": "hugr"})
    server.script_text('{"answer": "ok"}')

    async def collect():
        return [event async for event in agent.run("q")]

    events = asyncio.run(collect())
    types = [e["type"] for e in events]
    assert types[0] == "ask_started"
    assert types[-1] == "answer_ready"
    assert "tool_started" in types and "tool_ended" in types
    assert types.index("tool_started") < types.index("tool_ended")
    assert "model_started" in types and "text_delta" in types
    answer = hugr.Answer.from_dict(events[-1]["answer"])
    assert answer.ok


def test_feedback_round_trip(server):
    agent = make_agent(server)
    server.script_text('{"answer": "x"}')
    answer = agent.ask("q")
    fb = agent.feedback(answer.trace_id, {"score": 5, "note": "helped"})
    assert fb.trace_id == answer.trace_id
    stored = agent.feedback_for(answer.trace_id)
    assert [f.payload for f in stored] == [{"score": 5, "note": "helped"}]
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
    assert server.requests[0]["response_format"]["json_schema"]["schema"]["required"] == ["answer"]


def test_describe_lists_tools_and_tiers(server):
    agent = make_agent(server, tools=[lookup_tool([])])
    card = agent.describe()
    names = [t["name"] for t in card["tools"]]
    assert "lookup" in names and "scratch_write" in names
    assert card["model_tiers"][0]["selector"] == "medium"
    assert card["model_tiers"][0]["default"] is True
