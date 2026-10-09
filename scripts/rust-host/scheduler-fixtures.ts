// Rust Host 调度状态机的对拍 fixture（docs/specs/rust-u4-app-host.md §2.4、§6）。
// 用真实 TS 实现（prepareTasksIndexStorage、AutomationRepo、OffPeakTaskRepo、scheduler 的
// manualClaimRelease.ts 与 offPeakDispatchSettlement.ts）在固定时钟下执行 scheduler-scenarios.ts 的操作序列，记录每步返回值
// 与最终表内容；Rust 侧 include_str! 嵌入并逐步复现。用法：pnpm exec tsx scripts/rust-host/scheduler-fixtures.ts [--check]
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { format } from "oxfmt";
import { settleManualClaimForDispatchResult } from "../../packages/desktop/src/scheduler/manualClaimRelease.js";
import { settleOffPeakDispatchResult } from "../../packages/desktop/src/scheduler/offPeakDispatchSettlement.js";
import { AutomationRepo } from "../../packages/services/src/session/automationRepo.js";
import { OffPeakTaskRepo } from "../../packages/services/src/session/offPeakTaskRepo.js";
import { prepareTasksIndexStorage } from "../../packages/services/src/session/tasksDatabase/startup.js";
import { BASE, OPS } from "./scheduler-scenarios.js";

const { DatabaseSync } = createRequire(import.meta.url)(
  "node:sqlite",
) as typeof import("node:sqlite");

const root = resolve(import.meta.dirname, "../..");
const fixtureDir = resolve(root, "apps/zcode-host/crates/automation/src/scheduler/fixtures");
let clock = BASE;
const realNow = Date.now;
Date.now = () => clock;

async function runScenario(dir: string) {
  const path = join(dir, "tasks-index.sqlite");
  clock = BASE;
  await prepareTasksIndexStorage(path, () => {});
  const repo = new AutomationRepo(path);
  const offPeakRepo = new OffPeakTaskRepo(path);
  await repo.ensureReady();
  await offPeakRepo.ensureReady();
  const db = new DatabaseSync(path);
  const retryAt = new Map<string, number>();
  const retryAttempts = new Map<string, number>();
  const results: unknown[] = [];
  try {
    for (const op of OPS) {
      if (op.now !== undefined) clock = op.now;
      let result: { value?: unknown; error?: string };
      try {
        let value: unknown;
        switch (op.op) {
          case "sql":
            db.exec(op.sql);
            break;
          case "claimDue":
            value = await repo.claimDue(op.now);
            break;
          case "claimManualRuns":
            value = await repo.claimManualRuns(op.now);
            break;
          case "upsertRunClaimed":
            value = await repo.upsertRunClaimed({ ...op.run, modelSelection: op.modelSelection });
            break;
          case "getRun":
            value = await repo.getRun(op.runId);
            break;
          case "markRunDispatch":
            value = await repo.markRunDispatch({
              runId: op.runId,
              dispatchStatus: op.status,
              sessionId: op.sessionId,
              error: op.error,
            });
            break;
          case "markDispatched":
            value = await repo.markDispatched(op.id, {
              dispatchedAt: op.dispatchedAt,
              nextRunAt: op.nextRunAt,
            });
            break;
          case "markDispatchFailed":
            value = await repo.markDispatchFailed(op.id, {
              failedAt: op.failedAt,
              error: op.error,
              kind: op.kind,
              nextRunAt: op.nextRunAt,
            });
            break;
          case "releaseClaim":
            value = await repo.releaseClaim(op.id);
            break;
          case "skipAndReschedule":
            value = await repo.skipAndReschedule(op.params);
            break;
          case "markManualRunDispatched":
            value = await repo.markManualRunDispatched({
              runId: op.runId,
              sessionId: op.sessionId,
              dispatchedAt: op.dispatchedAt,
            });
            break;
          case "settleManualClaim": {
            const errors: string[] = [];
            await settleManualClaimForDispatchResult({
              repo,
              automationId: op.id,
              runId: op.runId,
              ok: op.ok,
              logError: (message) => errors.push(message),
            });
            value = { errors };
            break;
          }
          case "countActive":
            value = await offPeakRepo.countActive();
            break;
          case "recoverInterrupted":
            value = await offPeakRepo.recoverInterrupted(op.now);
            break;
          case "offPeakClaimDue":
            value = await offPeakRepo.claimDue(op.now);
            break;
          case "markRunning":
            value = await offPeakRepo.markRunning(op.id, op.update);
            break;
          case "releaseOffPeakClaim":
            value = await offPeakRepo.releaseClaim(op.id, { error: op.error, now: op.now });
            break;
          case "settleOffPeak": {
            const logs: string[] = [];
            await settleOffPeakDispatchResult(
              {
                repo: offPeakRepo,
                retryAt,
                retryAttempts,
                now: () => clock,
                log: (level, message) => logs.push(`${level}: ${message}`),
              },
              {
                type: "offpeak-dispatch-result",
                offPeakTaskId: op.id,
                ok: op.ok,
                // Host 的 off-peak-run-result 成功时 conversationId 与 sessionId 都是会话 id。
                ...(op.ok ? { conversationId: op.sessionId, sessionId: op.sessionId } : {}),
                ...(op.ok ? {} : { error: op.error, failureKind: op.failureKind }),
              },
            );
            value = {
              retryAt: Object.fromEntries([...retryAt].sort()),
              attempts: Object.fromEntries([...retryAttempts].sort()),
            };
            break;
          }
          default:
            throw new Error(`unknown op ${op.op}`);
        }
        // 经 JSON 往返：undefined 字段与 Rust 一样不出现，键序保留。
        result = { value: JSON.parse(JSON.stringify(value ?? null)) };
      } catch (error) {
        result = { error: error instanceof Error ? error.message : String(error) };
      }
      results.push(result);
    }
    const dumps = {
      automations: db.prepare("SELECT * FROM automations ORDER BY automation_id").all(),
      automation_runs: db.prepare("SELECT * FROM automation_runs ORDER BY run_id").all(),
      off_peak_tasks: db.prepare("SELECT * FROM off_peak_tasks ORDER BY off_peak_task_id").all(),
    };
    return { baseNow: BASE, ops: OPS, results, dumps };
  } finally {
    db.close();
    repo.close();
    offPeakRepo.close();
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
      `[scheduler-fixtures] ${name} is stale; run pnpm exec tsx scripts/rust-host/scheduler-fixtures.ts`,
    );
    return false;
  }
  await mkdir(fixtureDir, { recursive: true });
  await writeFile(path, formatted.code);
  return true;
}

const dir = await mkdtemp(join(tmpdir(), "zcode-scheduler-fixtures-"));
try {
  const scenario = await runScenario(dir);
  const ok = await emit("repo.json", {
    source: "scripts/rust-host/scheduler-fixtures.ts",
    ...scenario,
  });
  if (!ok) process.exitCode = 1;
  else
    console.log(
      `[scheduler-fixtures] ${process.argv.includes("--check") ? "up to date" : "wrote"}: ${OPS.length} ops`,
    );
} finally {
  Date.now = realNow;
  await rm(dir, { recursive: true, force: true });
}
