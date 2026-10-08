// Rust Host 闲时任务仓库的对拍 fixture（docs/specs/rust-host-automation.md 第 6 节）。
// 用真实 TS 实现（prepareTasksIndexStorage、OffPeakTaskRepo）在固定时钟下执行下面的操作序列，记录每步返回值与
// 最终表内容；Rust 侧 include_str! 嵌入并逐项复现。用法：pnpm exec tsx scripts/rust-host/off-peak-fixtures.ts [--check]
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { format } from "oxfmt";
import { OffPeakTaskRepo } from "../../packages/services/src/session/offPeakTaskRepo.js";
import { prepareTasksIndexStorage } from "../../packages/services/src/session/tasksDatabase/startup.js";

const { DatabaseSync } = createRequire(import.meta.url)(
  "node:sqlite",
) as typeof import("node:sqlite");
type Op = Record<string, any>;

const root = resolve(import.meta.dirname, "../..");
const fixtureDir = resolve(root, "apps/zcode-host/crates/services/src/off_peak/fixtures");
const BASE_NOW = 1791300000000;
let clock = BASE_NOW;
const realNow = Date.now;
Date.now = () => clock;

const WS = { workspacePath: "/tmp/ws-a" };
const SELECTION = {
  providerId: " account:bigmodel-offpeak-idle-plan ",
  modelId: "GLM-5.3",
  options: { reasoningLevel: "high" },
};
const create = (id: string, extra: Record<string, unknown> = {}, options: Op = {}): Op => ({
  op: "create",
  params: {
    ...WS,
    title: `task ${id}`,
    prompt: "do it",
    permissionMode: "yolo",
    modelSelection: SELECTION,
    ...extra,
  },
  options: { offPeakTaskId: id, ...options },
});

export const OPS: Op[] = [
  create("op-1", {}, { serverTicketId: "t-1", queuePosition: 3, registeredAt: BASE_NOW }),
  create("op-2", { workspaceIdentity: " id-b ", boundSessionId: "sess-1" }, { schedulable: true }),
  create("op-3", { boundSessionId: "sess-1", workspaceIdentity: " id-b " }),
  create("op-4", { modelSelection: { providerId: "p", modelId: "m", options: {} } }, { now: BASE_NOW + 5 }),
  {
    op: "sql",
    sql: "INSERT INTO tasks (task_id, workspace_key, workspace_path, provider, title, created_at, updated_at) VALUES ('sess-1', 'id-b', '/tmp/ws-b', 'glm', 'Bound session', 1, 1)",
  },
  { op: "list" },
  { op: "list", scope: { workspacePath: "/tmp/ws-b", workspaceIdentity: "id-b" } },
  { op: "get", id: "op-1" },
  { op: "get", id: "missing" },
  { op: "hasActiveBoundTask", key: "id-b", session: "sess-1" },
  { op: "setPaused", id: "op-1", paused: true, now: BASE_NOW + 10 },
  { op: "setPaused", id: "op-1", paused: true, now: BASE_NOW + 11 },
  { op: "updateEditableFields", id: "op-1", params: { title: "renamed", prompt: undefined } },
  {
    op: "updateEditableFields",
    id: "op-1",
    params: { modelSelection: { providerId: "x", modelId: "y" }, permissionMode: "plan" },
    now: BASE_NOW + 12,
  },
  { op: "updateEditableFields", id: "op-1", params: { modelSelection: null } },
  { op: "setPaused", id: "op-1", paused: false, now: BASE_NOW + 13 },
  {
    op: "updateSchedulingSnapshot",
    id: "op-1",
    patch: { schedulable: true, queuePosition: null, nextPollAt: BASE_NOW + 5000, now: BASE_NOW + 14 },
  },
  {
    op: "updateSchedulingSnapshot",
    id: "op-4",
    patch: { serverTicketId: "t-4", registeredAt: BASE_NOW + 15 },
    now: BASE_NOW + 15,
  },
  { op: "listNonTerminal" },
  { op: "sql", sql: "UPDATE off_peak_tasks SET status = 'running', started_at = 7, conversation_id = 'c-1', claim_running = 1 WHERE off_peak_task_id = 'op-1'" },
  { op: "setPaused", id: "op-1", paused: true },
  { op: "markHistoryDeleted", id: "op-2", now: BASE_NOW + 16 },
  { op: "markHistoryDeleted", id: "op-1", now: BASE_NOW + 17 },
  { op: "markHistoryDeleted", id: "op-1", now: BASE_NOW + 18 },
  { op: "requeueForContinuation", id: "op-1", now: BASE_NOW + 19 },
  { op: "requeueForContinuation", id: "op-1", now: BASE_NOW + 20 },
  {
    op: "markTerminal",
    id: "op-1",
    update: { status: "completed", endedAt: BASE_NOW + 21, filesChanged: 2 },
  },
  {
    op: "markTerminal",
    id: "op-1",
    update: { status: "failed", endedAt: BASE_NOW + 22, failureReason: "late" },
  },
  {
    op: "markTerminal",
    id: "op-2",
    update: { status: "failed", endedAt: BASE_NOW + 23, failureReason: "boom", dispatchError: "x" },
  },
  { op: "updateEditableFields", id: "op-1", params: { title: "after terminal" } },
  { op: "hasActiveBoundTask", key: "id-b", session: "sess-1" },
  { op: "listUnsettledTerminal" },
  { op: "markSettled", id: "op-1", at: BASE_NOW + 24 },
  { op: "markSettled", id: "op-3", at: BASE_NOW + 25 },
  { op: "listUnsettledTerminal" },
  { op: "sql", sql: "UPDATE off_peak_tasks SET model_selection = 'not json' WHERE off_peak_task_id = 'op-2'" },
  { op: "get", id: "op-2" },
  { op: "delete", id: "op-4" },
  { op: "list" },
];

async function runScenario(dir: string) {
  const path = join(dir, "tasks-index.sqlite");
  clock = BASE_NOW;
  await prepareTasksIndexStorage(path, () => {});
  const repo = new OffPeakTaskRepo(path);
  await repo.ensureReady();
  const db = new DatabaseSync(path);
  const results: unknown[] = [];
  try {
    for (const op of OPS) {
      if (op.now !== undefined) clock = op.now;
      let result: { value?: unknown; error?: string };
      try {
        let value: unknown;
        switch (op.op) {
          case "create":
            value = await repo.create(op.params, op.options);
            break;
          case "list":
            value = await repo.list(op.scope);
            break;
          case "get":
            value = await repo.get(op.id);
            break;
          case "hasActiveBoundTask":
            value = await repo.hasActiveBoundTask(op.key, op.session);
            break;
          case "setPaused":
            value = await repo.setPaused(op.id, op.paused, op.now ? { now: op.now } : undefined);
            break;
          case "updateEditableFields":
            value = await repo.updateEditableFields(op.id, op.params);
            break;
          case "updateSchedulingSnapshot":
            value = await repo.updateSchedulingSnapshot(op.id, op.patch);
            break;
          case "listNonTerminal":
            value = await repo.listNonTerminal();
            break;
          case "listUnsettledTerminal":
            value = await repo.listUnsettledTerminal();
            break;
          case "markHistoryDeleted":
            value = await repo.markHistoryDeleted(op.id, { now: op.now });
            break;
          case "requeueForContinuation":
            value = await repo.requeueForContinuation(op.id, { now: op.now });
            break;
          case "markTerminal":
            value = await repo.markTerminal(op.id, op.update);
            break;
          case "markSettled":
            value = await repo.markSettled(op.id, op.at);
            break;
          case "delete":
            value = await repo.delete(op.id);
            break;
          case "sql":
            db.exec(op.sql);
            break;
          default:
            throw new Error(`unknown op ${op.op}`);
        }
        result = { value: JSON.parse(JSON.stringify(value ?? null)) };
      } catch (error) {
        result = { error: error instanceof Error ? error.message : String(error) };
      }
      results.push(result);
    }
    const rows = db.prepare("SELECT * FROM off_peak_tasks ORDER BY off_peak_task_id").all();
    return { baseNow: BASE_NOW, ops: OPS, results, rows };
  } finally {
    db.close();
    repo.close();
  }
}

async function emit(name: string, data: unknown): Promise<boolean> {
  const path = join(fixtureDir, name);
  const formatted = await format(path, `${JSON.stringify(data, null, 2)}\n`);
  if (formatted.errors.length) throw new Error(`Cannot format ${name}`);
  if (process.argv.includes("--check")) {
    const current = await readFile(path, "utf8").catch(() => "");
    if (current === formatted.code) return true;
    console.error(
      `[off-peak-fixtures] ${name} is stale; run pnpm exec tsx scripts/rust-host/off-peak-fixtures.ts`,
    );
    return false;
  }
  await mkdir(fixtureDir, { recursive: true });
  await writeFile(path, formatted.code);
  return true;
}

const dir = await mkdtemp(join(tmpdir(), "zcode-off-peak-fixtures-"));
try {
  const scenario = await runScenario(dir);
  const ok = await emit("repo.json", {
    source: "scripts/rust-host/off-peak-fixtures.ts",
    ...scenario,
  });
  if (!ok) process.exitCode = 1;
  else
    console.log(
      `[off-peak-fixtures] ${process.argv.includes("--check") ? "up to date" : "wrote"}: ${OPS.length} ops`,
    );
} finally {
  Date.now = realNow;
  await rm(dir, { recursive: true, force: true });
}
