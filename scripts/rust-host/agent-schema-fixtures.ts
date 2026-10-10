// Rust Host agent 子系统的 schema 对拍样本（docs/specs/rust-host-agent.md §2.9）。
// 发给 Renderer 的 TTFT、telemetry fact、CUA 观察在 Node 用 strict zod 校验；Rust 侧逐字段移植。
// 这里用真实 zod schema 判定一组手写样本，Rust 单测 include_str! 后逐条比较判定结果。
// 用法：pnpm exec tsx scripts/rust-host/agent-schema-fixtures.ts [--check]
import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { format } from "oxfmt";
import { localTtftFactsSchema } from "../../packages/shared/src/localTtft.js";
import { conversationTelemetryFactSchema } from "../../packages/shared/src/zcode-protocol-v4/telemetry.js";
import { cuaPermissionObservationSchema } from "../../packages/shared/src/zcode-protocol-v4/cuaPermission.js";

const root = resolve(import.meta.dirname, "../..");
const output = resolve(
  root,
  "apps/zcode-host/crates/sessions/src/agent/fixtures/schema-cases.json",
);

type Json = Record<string, unknown>;
const schemas = {
  localTtftFacts: localTtftFactsSchema,
  conversationTelemetryFact: conversationTelemetryFactSchema,
  cuaPermissionObservation: cuaPermissionObservationSchema,
} as const;
type SchemaName = keyof typeof schemas;

const uuid = "123e4567-e89b-42d3-a456-426614174000";
const ttft: Json = {
  version: 1,
  observationId: uuid,
  instanceId: "cli-1",
  commandId: "cmd-1",
  receivedAt: 10,
  details: [{ id: "d1", stage: "context", start: 1, end: 2, source: "cli" }],
};
const cua: Json = {
  schemaVersion: 1,
  eventId: "e1",
  eventSeq: 0,
  occurredAt: 5,
  sessionId: "s1",
  toolCallId: "t1",
  permissionStatus: {
    schemaVersion: 1,
    platform: "darwin",
    grantOwner: "dev.zcode.app",
    accessibility: "granted",
    screenRecording: "unknown",
  },
};
const base: Json = { version: 1, eventId: "e1", eventSeq: 3, occurredAt: 1.5, sessionId: "s1" };
const facts: Record<string, Json> = {
  turnStarted: { ...base, kind: "turn.started", inputSource: "workflow_launch" },
  modelRequest: {
    ...base,
    kind: "model.request.status",
    requestId: "r1",
    status: "model_request_completed",
    providerId: "p",
    modelId: "m",
    providerHostname: "api.example.test",
    transport: "sse",
    attempt: 0,
    maxAttempts: 3,
    durationMs: 12.5,
  },
  streamChunk: { ...base, kind: "stream.chunk", channel: "text", chunkLength: 4, firstChunk: true },
  streamChunkMerged: {
    ...base,
    kind: "stream.chunk",
    channel: "thought",
    chunkLength: 40,
    firstChunk: false,
    chunkCount: 12,
    lastChunkAgeMs: 0,
  },
  toolLifecycle: {
    ...base,
    kind: "tool.lifecycle",
    phase: "completed",
    toolCallId: "tc",
    performance: { totalMs: 3, commandHash: "0123456789abcdef", workspaceKind: "local" },
  },
  permission: { ...base, kind: "permission.lifecycle", phase: "resolved", toolCallId: "tc" },
  usage: {
    ...base,
    kind: "usage.delta",
    inputTokens: 1,
    outputTokens: 2,
    totalTokens: 3,
    reasoningTokens: 0,
    cacheReadTokens: 0,
    cacheWriteTokens: 0,
  },
  subagent: {
    ...base,
    kind: "subagent.lifecycle",
    phase: "spawned",
    agentId: "a",
    childSessionId: "c",
    background: false,
  },
  workflow: {
    ...base,
    kind: "workflow.lifecycle",
    phase: "actor-spawned",
    runId: "run",
    agentId: "a",
    childSessionId: "c",
  },
  turnTerminal: { ...base, kind: "turn.terminal", status: "success", toolCallCount: 2 },
  compaction: {
    ...base,
    kind: "compaction.terminal",
    operationId: "op",
    status: "completed",
    trigger: "auto",
    attempt: 1,
  },
};

const cases: Array<{ name: string; schema: SchemaName; input: unknown }> = [
  { name: "ttft valid", schema: "localTtftFacts", input: ttft },
  { name: "ttft unknown key", schema: "localTtftFacts", input: { ...ttft, prompt: "x" } },
  { name: "ttft bad uuid", schema: "localTtftFacts", input: { ...ttft, observationId: "nope" } },
  { name: "ttft version 2", schema: "localTtftFacts", input: { ...ttft, version: 2 } },
  {
    name: "ttft detail end before start",
    schema: "localTtftFacts",
    input: { ...ttft, details: [{ id: "d", stage: "mcp", start: 5, end: 1, source: "cli" }] },
  },
  {
    name: "ttft detail unknown stage",
    schema: "localTtftFacts",
    input: { ...ttft, details: [{ id: "d", stage: "other", start: 1, source: "cli" }] },
  },
  {
    name: "ttft identifier too long",
    schema: "localTtftFacts",
    input: { ...ttft, commandId: "x".repeat(129) },
  },
  { name: "ttft negative time", schema: "localTtftFacts", input: { ...ttft, receivedAt: -1 } },
  { name: "ttft revision float", schema: "localTtftFacts", input: { ...ttft, revision: 1.5 } },
  {
    name: "ttft full optional",
    schema: "localTtftFacts",
    input: {
      ...ttft,
      sendMode: "queued",
      outputKind: "tool",
      terminal: "rejected",
      truncated: true,
    },
  },
  { name: "ttft not object", schema: "localTtftFacts", input: [ttft] },
  { name: "cua valid", schema: "cuaPermissionObservation", input: cua },
  {
    name: "cua nested unknown key",
    schema: "cuaPermissionObservation",
    input: { ...cua, permissionStatus: { ...(cua.permissionStatus as Json), extra: 1 } },
  },
  {
    name: "cua wrong platform",
    schema: "cuaPermissionObservation",
    input: { ...cua, permissionStatus: { ...(cua.permissionStatus as Json), platform: "win32" } },
  },
  { name: "cua empty turn", schema: "cuaPermissionObservation", input: { ...cua, turnId: "" } },
  { name: "cua negative seq", schema: "cuaPermissionObservation", input: { ...cua, eventSeq: -1 } },
  ...Object.entries(facts).map(([name, input]) => ({
    name: `fact ${name}`,
    schema: "conversationTelemetryFact" as const,
    input,
  })),
  {
    name: "fact unknown kind",
    schema: "conversationTelemetryFact",
    input: { ...base, kind: "other" },
  },
  {
    name: "fact unknown key",
    schema: "conversationTelemetryFact",
    input: { ...facts.streamChunk, text: "secret" },
  },
  {
    name: "fact chunk count zero",
    schema: "conversationTelemetryFact",
    input: { ...facts.streamChunkMerged, chunkCount: 0 },
  },
  {
    name: "fact chunk age negative",
    schema: "conversationTelemetryFact",
    input: { ...facts.streamChunkMerged, lastChunkAgeMs: -1 },
  },
  {
    name: "fact chunk age fraction",
    schema: "conversationTelemetryFact",
    input: { ...facts.streamChunkMerged, lastChunkAgeMs: 1.5 },
  },
  {
    name: "fact hostname with port",
    schema: "conversationTelemetryFact",
    input: { ...facts.modelRequest, providerHostname: "api.example.test:443" },
  },
  {
    name: "fact hostname ipv6",
    schema: "conversationTelemetryFact",
    input: { ...facts.modelRequest, providerHostname: "[::1]" },
  },
  {
    name: "fact hostname with space",
    schema: "conversationTelemetryFact",
    input: { ...facts.modelRequest, providerHostname: " a" },
  },
  {
    name: "fact automation and offpeak",
    schema: "conversationTelemetryFact",
    input: { ...facts.turnStarted, automationId: "a", offPeakTaskId: "o" },
  },
  {
    name: "fact offpeak run type without task",
    schema: "conversationTelemetryFact",
    input: { ...facts.turnStarted, offPeakRunType: "init" },
  },
  {
    name: "fact offpeak run type with task",
    schema: "conversationTelemetryFact",
    input: { ...facts.turnStarted, offPeakRunType: "resume", offPeakTaskId: "o" },
  },
  {
    name: "fact actor spawned without child",
    schema: "conversationTelemetryFact",
    input: { ...facts.workflow, childSessionId: undefined },
  },
  {
    name: "fact run settled without status",
    schema: "conversationTelemetryFact",
    input: { ...facts.workflow, phase: "run-settled" },
  },
  {
    name: "fact run settled with status",
    schema: "conversationTelemetryFact",
    input: { ...facts.workflow, phase: "run-settled", status: "stopped", stopReason: "user" },
  },
  {
    name: "fact bad command hash",
    schema: "conversationTelemetryFact",
    input: {
      ...facts.toolLifecycle,
      performance: { commandHash: "0123456789ABCDEF" },
    },
  },
  {
    name: "fact nested performance unknown",
    schema: "conversationTelemetryFact",
    input: { ...facts.toolLifecycle, performance: { argv: ["rm"] } },
  },
  {
    name: "fact compaction attempt zero",
    schema: "conversationTelemetryFact",
    input: { ...facts.compaction, attempt: 0 },
  },
  {
    name: "fact memory flag",
    schema: "conversationTelemetryFact",
    input: { ...facts.usage, memoryEnabled: true, turnId: "t" },
  },
  {
    name: "fact empty session",
    schema: "conversationTelemetryFact",
    input: { ...facts.usage, sessionId: "" },
  },
];

// JSON 不能表达 undefined：先序列化一次，与 CLI 实际送来的形状一致。
const normalized = cases.map((item) => ({
  ...item,
  input: JSON.parse(JSON.stringify(item.input)) as unknown,
}));
const fixture = {
  cases: normalized.map((item) => ({
    name: item.name,
    schema: item.schema,
    input: item.input,
    valid: schemas[item.schema].safeParse(item.input).success,
  })),
};
const formatted = await format(output, `${JSON.stringify(fixture)}\n`);
const text = formatted.code;
if (process.argv.includes("--check")) {
  const current = await readFile(output, "utf8").catch(() => "");
  if (current !== text) {
    console.error(`agent schema fixture is stale: ${output}`);
    process.exit(1);
  }
} else {
  await writeFile(output, text);
}
