"""A scripted OpenAI-compatible streaming mock: each /chat/completions request pops the next scripted output."""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Dict, List


class MockOpenAi:
    def __init__(self) -> None:
        self.outputs: List[Dict[str, Any]] = []
        self.requests: List[Dict[str, Any]] = []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args: Any) -> None:
                pass

            def do_POST(self) -> None:
                body = self.rfile.read(int(self.headers["Content-Length"]))
                outer.requests.append(json.loads(body))
                if not outer.outputs:
                    self.send_response(500)
                    self.end_headers()
                    self.wfile.write(b"mock ran out of scripted outputs")
                    return
                output = outer.outputs.pop(0)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                for chunk in sse_chunks(output):
                    self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
                self.wfile.write(b"data: [DONE]\n\n")

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_port}/v1"

    def script_text(self, text: str) -> None:
        self.outputs.append({"text": text})

    def script_tool_call(self, name: str, args: Dict[str, Any], call_id: str = "call_1") -> None:
        self.outputs.append({"tool": {"id": call_id, "name": name, "args": args}})

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


def sse_chunks(output: Dict[str, Any]) -> List[Dict[str, Any]]:
    if "tool" in output:
        tool = output["tool"]
        delta = {
            "tool_calls": [
                {
                    "index": 0,
                    "id": tool["id"],
                    "function": {"name": tool["name"], "arguments": json.dumps(tool["args"])},
                }
            ]
        }
        finish = "tool_calls"
        deltas = [delta]
    else:
        text = output["text"]
        mid = max(1, len(text) // 2)
        deltas = [{"content": text[:mid]}, {"content": text[mid:]}]
        finish = "stop"
    chunks = [{"choices": [{"delta": d}]} for d in deltas]
    chunks.append({"choices": [{"delta": {}, "finish_reason": finish}]})
    chunks.append({"choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 3}})
    return chunks
