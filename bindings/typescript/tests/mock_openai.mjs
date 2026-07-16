// A scripted OpenAI-compatible streaming mock: each /chat/completions request
// pops the next scripted output.

import http from "node:http";

export class MockOpenAi {
  constructor() {
    this.outputs = [];
    this.requests = [];
    this.authorizations = [];
    this.server = http.createServer((req, res) => {
      let body = "";
      req.on("data", (chunk) => (body += chunk));
      req.on("end", async () => {
        this.requests.push(JSON.parse(body));
        this.authorizations.push(req.headers.authorization);
        const output = this.outputs.shift();
        if (!output) {
          res.writeHead(500);
          res.end("mock ran out of scripted outputs");
          return;
        }
        if (output.transportError) {
          req.socket.destroy();
          return;
        }
        res.writeHead(200, { "content-type": "text/event-stream" });
        const chunks = sseChunks(output);
        res.write(`data: ${JSON.stringify(chunks.shift())}\n\n`);
        output.firstChunk?.();
        if (output.release) await output.release;
        for (const chunk of chunks) {
          res.write(`data: ${JSON.stringify(chunk)}\n\n`);
        }
        res.write("data: [DONE]\n\n");
        res.end();
      });
    });
  }

  listen() {
    return new Promise((resolve) => {
      this.server.listen(0, "127.0.0.1", () => resolve(this));
    });
  }

  get baseUrl() {
    return `http://127.0.0.1:${this.server.address().port}/v1`;
  }

  scriptText(text, usage) {
    this.outputs.push({ text, usage });
  }

  scriptTransportFailure() {
    this.outputs.push({ transportError: true });
  }

  scriptPausedText(text) {
    let release;
    let firstChunk;
    const output = {
      text,
      release: new Promise((resolve) => { release = resolve; }),
      firstChunk: () => firstChunk(),
    };
    const started = new Promise((resolve) => { firstChunk = resolve; });
    this.outputs.push(output);
    return { started, release };
  }

  scriptToolCall(name, args, callId = "call_1", usage) {
    this.outputs.push({ tool: { id: callId, name, args }, usage });
  }

  close() {
    this.server.close();
  }
}

function sseChunks(output) {
  let deltas;
  let finish;
  if (output.tool) {
    deltas = [
      {
        tool_calls: [
          {
            index: 0,
            id: output.tool.id,
            function: { name: output.tool.name, arguments: JSON.stringify(output.tool.args) },
          },
        ],
      },
    ];
    finish = "tool_calls";
  } else {
    const mid = Math.max(1, Math.floor(output.text.length / 2));
    deltas = [{ content: output.text.slice(0, mid) }, { content: output.text.slice(mid) }];
    finish = "stop";
  }
  const chunks = deltas.map((delta) => ({ choices: [{ delta }] }));
  chunks.push({ choices: [{ delta: {}, finish_reason: finish }] });
  chunks.push({ choices: [], usage: output.usage ?? { prompt_tokens: 7, completion_tokens: 3 } });
  return chunks;
}
