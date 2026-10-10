import assert from "node:assert/strict";
import test from "node:test";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { ServerResponse } from "node:http";
import { randomUUID } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";
import {
  localTtftFactsSchema,
  zcodeComputerUseOperationEventSchema,
  zcodeMcpTelemetryEventSchema,
} from "@zcode/shared";
import {
  commandsQueryResultSchema,
  conversationTelemetryFactSchema,
  conversationTopicFrameSchema,
} from "@zcode/shared/zcode-protocol-v4";
import { end, event, fixture, type Harness } from "./zcode-cli-rust-fixture.js";

type Message = Record<string, any>;

function bashCall(res: ServerResponse) {
  res.writeHead(200, { "content-type": "text/event-stream" });
  event(res, {
    tool_calls: [
      {
        index: 0,
        id: "call-bash",
        type: "function",
        function: {
          name: "Bash",
          arguments: JSON.stringify({ command: "echo hi", description: "Echo" }),
        },
      },
    ],
  });
  end(res, "tool_calls");
}

function answer(res: ServerResponse) {
  res.writeHead(200, { "content-type": "text/event-stream" });
  event(res, { content: "done" });
  end(res, "stop");
}

// spec rust-m9-usage-logs §5：实时事件的遥测通过 Host 的 strict schema，且关联到提交命令。
test("Rust live telemetry passes the shared strict schemas", async () => {
  const f = await fixture({
    respond(_req, res) {
      if (f.requests.length === 1) bashCall(res);
      else answer(res);
    },
  });
  try {
    await writeFile(join(f.cwd, "a.txt"), "hello\n");
    const h = f.start();
    const id = await h.create();
    await h.subscribe(`conversation/${id}`);
    const after = h.messages.length;
    const command = h.envelope("sendText", id, { text: "run it" });
    await h.command(command);
    await h.completed(id, after);
    const sent = (method: string) =>
      h.messages
        .slice(after)
        .filter((m: Message) => m.method === method)
        .map((m: Message) => m.params);
    const facts = sent("v4/telemetry/event").map((fact) =>
      conversationTelemetryFactSchema.parse(fact),
    );
    const operations = sent("computer-use/operation-event").map((operation) =>
      zcodeComputerUseOperationEventSchema.parse(operation),
    );
    const kinds = facts.map((fact) => ("phase" in fact ? `${fact.kind}:${fact.phase}` : fact.kind));
    for (const kind of [
      "turn.started",
      "model.request.status",
      "usage.delta",
      "tool.lifecycle:scheduled",
      "tool.lifecycle:started",
      "tool.lifecycle:completed",
      "turn.terminal",
    ])
      assert.ok(kinds.includes(kind), kinds.join(","));
    for (const fact of facts.filter((f) => f.kind !== "model.request.status"))
      assert.equal(fact.sourceCommandId, command.commandId, fact.kind);
    const done = facts.find((f) => f.kind === "tool.lifecycle" && f.phase === "completed");
    assert.equal(done?.kind === "tool.lifecycle" && done.performance?.commandName, "echo");
    assert.deepEqual(
      operations.map((o) => o.kind),
      ["turn-started", "tool-scheduled", "tool-started", "turn-completed"],
    );
    const seqs = facts.map((f) => f.eventSeq);
    assert.deepEqual(
      seqs,
      [...seqs].sort((a, b) => a - b),
    );
  } finally {
    await f.close();
  }
});

/** A stdio MCP server whose one tool, `crash`, exits the process with code 3. */
const MCP_SERVER = `import { createInterface } from "node:readline";
const send = (message) => process.stdout.write(JSON.stringify(message) + "\\n");
createInterface({ input: process.stdin }).on("line", (line) => {
  const m = JSON.parse(line);
  if (m.method === "initialize")
    send({ jsonrpc: "2.0", id: m.id, result: { protocolVersion: m.params.protocolVersion,
      capabilities: { tools: {} }, serverInfo: { name: "fixture", version: "1.0.0" } } });
  else if (m.method === "tools/list")
    send({ jsonrpc: "2.0", id: m.id, result: { tools: [{ name: "crash",
      description: "Exits the server", inputSchema: { type: "object", properties: {} } }] } });
  else if (m.method === "tools/call") process.exit(3);
  else if (m.id !== undefined) send({ jsonrpc: "2.0", id: m.id, result: {} });
});
`;

// spec rust-m9-usage-logs §6：stdio MCP server 的启动、会话首次快照与崩溃通过 Host 的 strict schema。
test("Rust MCP process telemetry reports a stdio server's start and crash", async () => {
  const f = await fixture({
    respond(_req, res) {
      res.writeHead(200, { "content-type": "text/event-stream" });
      if (f.requests.length === 1) {
        event(res, {
          tool_calls: [
            {
              index: 0,
              id: "call-crash",
              type: "function",
              function: { name: "mcp__fixture__crash", arguments: "{}" },
            },
          ],
        });
        end(res, "tool_calls");
      } else {
        event(res, { content: "done" });
        end(res, "stop");
      }
    },
  });
  try {
    const server = join(f.cwd, "..", "mcp-server.mjs");
    await writeFile(server, MCP_SERVER);
    const h = f.start();
    const ack = await h.command(
      h.envelope("createSession", null, {
        workspaceId: h.workspace,
        mcpServers: [{ name: "fixture", command: process.execPath, args: [server], env: [] }],
      }),
    );
    const id = (ack.result as Message).sessionId as string;
    await h.subscribe(`conversation/${id}`);
    const after = h.messages.length;
    await h.command(h.envelope("sendText", id, { text: "crash it" }));
    await h.completed(id, after);
    const events = await h.wait(
      (m) => m.method === "process/mcpTelemetry" && m.params?.kind === "process_crash",
      after,
    );
    assert.ok(events);
    const sent = h.messages
      .slice(after)
      .filter((m: Message) => m.method === "process/mcpTelemetry")
      .map((m: Message) => zcodeMcpTelemetryEventSchema.parse(m.params));
    assert.deepEqual(
      sent.map((e) => e.kind),
      ["process_start", "session_startup", "process_crash"],
    );
    const [start, startup, crash] = sent;
    assert.ok(start?.kind === "process_start" && /^custom:[a-f0-9]{12}$/.test(start.mcpId));
    assert.ok(startup?.kind === "session_startup" && startup.processCount === 1);
    assert.ok(crash?.kind === "process_crash" && crash.exitCode === 3);
    assert.equal(crash.affectedSessionCount, 1);
    assert.equal(crash.mcpInstanceId, start.mcpInstanceId);
    const processes = (await h.client.request("process/childProcesses", {})) as Message;
    assert.deepEqual(processes.processes, []);
  } finally {
    await f.close();
  }
});

// spec rust-m9-usage-logs §7：带 ttft 的输入记录本地首字耗时，checkpoint 与帧上的事实都通过 strict schema。
test("Rust local TTFT records a prompt to its first text", async () => {
  const f = await fixture({ respond: (_req, res) => answer(res) });
  try {
    const h = f.start();
    const id = await h.create();
    await h.subscribe(`conversation/${id}`);
    const probe = commandsQueryResultSchema.parse(
      await h.client.request("v4/commands/query", {
        commands: [{ sessionId: null, commandId: `ttft-clock-${randomUUID()}` }],
        clock: true,
      }),
    );
    assert.ok(probe.clock && probe.clock.sentAt >= probe.clock.receivedAt);
    const after = h.messages.length;
    const observationId = randomUUID();
    const command = {
      ...h.envelope("sendText", id, { text: "hello" }),
      ttft: { version: 1, observationId },
    };
    await h.command(command);
    await h.completed(id, after);
    const checkpoints = h.messages
      .slice(after)
      .filter((m: Message) => m.method === "v4/telemetry/local-ttft")
      .map((m: Message) => localTtftFactsSchema.parse(m.params));
    assert.ok(checkpoints.length >= 3, JSON.stringify(checkpoints));
    const revisions = checkpoints.map((c) => c.revision);
    assert.deepEqual(
      revisions,
      revisions.map((_, i) => i + 1),
    );
    const last = checkpoints.at(-1)!;
    assert.equal(last.commandId, command.commandId);
    assert.equal(last.instanceId, probe.clock!.instanceId);
    assert.equal(last.sendMode, "idle");
    for (const at of [last.admittedAt, last.executionAt, last.requestAt])
      assert.ok(at! >= last.receivedAt);
    const frames = h.messages
      .slice(after)
      .map((m: Message) => m.params?.frame)
      .filter((frame: Message) => frame?.ttft);
    assert.ok(frames.length > 0);
    const facts = conversationTopicFrameSchema.parse(frames.at(-1)).ttft!;
    assert.equal(facts.observationId, observationId);
    assert.equal(facts.outputKind, "text");
    assert.ok(facts.outputAt! >= facts.requestAt! && facts.productTurnId && facts.cliVersion);
  } finally {
    await f.close();
  }
});

/** Streams `count` text deltas `gapMs` apart; returns the text written before `stop()`. */
function streamDeltas(res: ServerResponse, count: number, gapMs: number) {
  let stopped = false;
  let written = "";
  const done = (async () => {
    res.writeHead(200, { "content-type": "text/event-stream" });
    for (let i = 0; i < count && !stopped && !res.destroyed; i++) {
      const delta = `w${i}é `;
      event(res, { content: delta });
      written += delta;
      await delay(gapMs);
    }
  })();
  return {
    done,
    written: () => written,
    stop: () => {
      stopped = true;
    },
  };
}

/** The facts of `commandId`: its `stream.chunk` facts (with their index) and its `turn.terminal`. */
function chunkFacts(h: Harness, after: number, commandId: string) {
  const facts = h.messages
    .slice(after)
    .filter((m: Message) => m.method === "v4/telemetry/event")
    .map((m: Message) => conversationTelemetryFactSchema.parse(m.params))
    .filter((fact) => fact.sourceCommandId === commandId);
  const chunks = facts.flatMap((fact, index) =>
    fact.kind === "stream.chunk" ? [{ ...fact, index }] : [],
  );
  const terminal = facts.findIndex((fact) => fact.kind === "turn.terminal");
  const status = facts.flatMap((fact) => (fact.kind === "turn.terminal" ? [fact.status] : []))[0];
  return { facts, chunks, terminal, status };
}

/**
 * spec rust-m9-usage-logs §5.7：首个分片立即单独发出，其余按 250 ms 窗口合并，合并的那条带
 * chunkCount 与 lastChunkAgeMs；终态之前先发完，之后不再有分片事实。
 */
function assertMerged(h: Harness, after: number, commandId: string, text: string, deltas: number) {
  const { facts, chunks, terminal } = chunkFacts(h, after, commandId);
  assert.ok(terminal >= 0, facts.map((f) => f.kind).join(","));
  assert.equal(
    chunks.reduce((sum, c) => sum + c.chunkLength, 0),
    text.length,
  );
  assert.ok(chunks.length >= 2 && chunks.length < deltas / 2, JSON.stringify(chunks));
  const counts = chunks.map((c) => c.chunkCount ?? 1);
  assert.ok(counts.reduce((a, b) => a + b, 0) <= deltas);
  assert.ok(
    counts.some((count) => count >= 2),
    JSON.stringify(counts),
  );
  const first = chunks[0]!;
  assert.deepEqual(
    [first.firstChunk, first.chunkCount, first.lastChunkAgeMs],
    [true, undefined, undefined],
  );
  for (const chunk of chunks.slice(1)) {
    assert.equal(chunk.firstChunk, false);
    if (chunk.chunkCount !== undefined) assert.ok(chunk.lastChunkAgeMs! < 1000);
  }
  assert.ok(chunks.at(-1)!.index < terminal);
  const seqs = facts.map((f) => f.eventSeq);
  assert.deepEqual(
    seqs,
    [...seqs].sort((a, b) => a - b),
  );
}

test("Rust merges a stream's chunk facts per window and sends them before the turn ends", async () => {
  let stream!: ReturnType<typeof streamDeltas>;
  const f = await fixture({
    async respond(_req, res) {
      stream = streamDeltas(res, 30, 25);
      await stream.done;
      end(res, "stop");
    },
  });
  try {
    const h = f.start();
    const id = await h.create();
    await h.subscribe(`conversation/${id}`);
    const after = h.messages.length;
    const command = h.envelope("sendText", id, { text: "stream it" });
    await h.command(command);
    await h.completed(id, after);
    await delay(400);
    assertMerged(h, after, command.commandId, stream.written(), 30);
  } finally {
    await f.close();
  }
});

test("Rust sends the held chunk facts before a stopped turn's terminal fact", async () => {
  let stream!: ReturnType<typeof streamDeltas>;
  const f = await fixture({
    async respond(_req, res) {
      stream = streamDeltas(res, 400, 25);
      await stream.done;
    },
  });
  try {
    const h = f.start();
    const id = await h.create();
    await h.subscribe(`conversation/${id}`);
    const after = h.messages.length;
    const command = h.envelope("sendText", id, { text: "stream then stop" });
    await h.command(command);
    await h.wait(
      (m) =>
        m.params?.topic === `conversation/${id}` &&
        JSON.stringify(m.params.frame?.payload ?? {}).includes("w20é"),
      after,
    );
    await h.command(h.envelope("stop", id));
    await h.wait(
      (m) =>
        m.method === "v4/telemetry/event" &&
        m.params.kind === "turn.terminal" &&
        m.params.sessionId === id,
      after,
    );
    stream.stop();
    await delay(400);
    const { chunks, status } = chunkFacts(h, after, command.commandId);
    assert.equal(status, "interrupted");
    // 停止前 runtime 收到的是写出正文的一个前缀，至少到 w20。
    const received = stream.written().slice(
      0,
      chunks.reduce((sum, c) => sum + c.chunkLength, 0),
    );
    assert.ok(received.includes("w20é"), received);
    assertMerged(h, after, command.commandId, received, 400);
  } finally {
    await f.close();
  }
});

test("Rust sends the held chunk facts before a failed turn's terminal fact", async () => {
  let stream!: ReturnType<typeof streamDeltas>;
  const f = await fixture({
    env: { ZCODE_MODEL_RETRY_MAX_RETRIES: "0" },
    config: { retry: { maxRetries: 0, baseDelayMs: 1, maxDelayMs: 1, jitter: false } },
    async respond(_req, res, attempt) {
      if (attempt > 1) {
        res.writeHead(400, { "content-type": "application/json" });
        res.end('{"error":{"message":"bad request"}}');
        return;
      }
      stream = streamDeltas(res, 30, 25);
      await stream.done;
      res.end('data: {"error":{"code":"invalid_request_error","message":"broken"}}\n\n');
    },
  });
  try {
    const h = f.start();
    const id = await h.create();
    await h.subscribe(`conversation/${id}`);
    const after = h.messages.length;
    const command = h.envelope("sendText", id, { text: "stream then fail" });
    await h.command(command);
    await h.wait(
      (m) =>
        m.method === "v4/telemetry/event" &&
        m.params.kind === "turn.terminal" &&
        m.params.sessionId === id,
      after,
    );
    await delay(400);
    assert.equal(chunkFacts(h, after, command.commandId).status, "failed");
    assertMerged(h, after, command.commandId, stream.written(), 30);
  } finally {
    await f.close();
  }
});
