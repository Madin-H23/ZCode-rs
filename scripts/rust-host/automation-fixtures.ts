// Rust Host automation 的对拍 fixture（docs/specs/rust-host-automation.md §10）。
// 用真实 TS 实现（croner、automationCron.ts、AutomationService、AutomationRepo）在固定时钟与固定时区下
// 执行 automation-scenarios.ts 的输入，记录下次 / 上次触发时间、调度规则、每步返回值与表内容；
// Rust 侧 include_str! 嵌入并逐项复现。用法：pnpm exec tsx scripts/rust-host/automation-fixtures.ts [--check]
//
// 错误文案里有 toLocaleString()，依赖 ICU 默认区域；不是 en-US 时以 LANG=en_US.UTF-8 重新执行自身。
import { spawnSync } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { format } from "oxfmt";

if (Intl.DateTimeFormat().resolvedOptions().locale !== "en-US") {
  const result = spawnSync(process.execPath, process.execArgv.concat(process.argv.slice(1)), {
    stdio: "inherit",
    env: { ...process.env, LANG: "en_US.UTF-8", LC_ALL: "en_US.UTF-8" },
  });
  process.exit(result.status ?? 1);
}

const { Cron } = await import("croner");
const cronModule = await import("../../packages/services/src/session/automationCron.js");
const { AutomationService } = await import("../../packages/services/src/session/automationService.js");
const { AutomationRepo } = await import("../../packages/services/src/session/automationRepo.js");
const { prepareTasksIndexStorage } = await import(
  "../../packages/services/src/session/tasksDatabase/startup.js"
);
const scenarios = await import("./automation-scenarios.js");

const { DatabaseSync } = createRequire(import.meta.url)(
  "node:sqlite",
) as typeof import("node:sqlite");
type Op = Record<string, any>;

const root = resolve(import.meta.dirname, "../..");
const fixtureDir = resolve(root, "apps/zcode-host/crates/services/src/automation/fixtures");

let clock = scenarios.SERVICE_NOW;
const realNow = Date.now;
Date.now = () => clock;

function setZone(zone: string) {
  // Node 在 TZ 变化时重置时区缓存。
  process.env.TZ = zone;
}

function attempt<T>(run: () => T): { value?: T; error?: string } {
  try {
    return { value: run() };
  } catch (error) {
    return { error: error instanceof Error ? error.message : String(error) };
  }
}

function cronCases() {
  const out = [];
  for (const zone of scenarios.ZONES) {
    setZone(zone);
    for (const expression of scenarios.CRON_EXPRESSIONS) {
      const valid = cronModule.isValidCronExpr(expression);
      const runs = valid
        ? scenarios.CRON_FROMS.map((from) => {
            const cron = new Cron(expression);
            // croner 在个别模式下会在回溯里越界抛 TypeError；Rust 端按同样的文案报错。
            const next = attempt(() => cron.nextRun(new Date(from))?.getTime() ?? null);
            const previous = attempt(
              () => cron.previousRuns(1, new Date(from))[0]?.getTime() ?? null,
            );
            return { from, next, previous };
          })
        : [];
      out.push({ zone, expression, valid, runs });
    }
  }
  return out;
}

function scheduleCases() {
  setZone("America/New_York");
  const rules = scenarios.SCHEDULE_RULES.map((rule) => ({
    rule,
    definition: cronModule.scheduleRuleDefinition(rule as any),
    next: scenarios.SCHEDULE_FROMS.map((from) => ({
      from,
      next: cronModule.computeScheduleRuleNextRunAt(rule as any, from),
    })),
  }));
  const carriers = scenarios.INTERVAL_CARRIERS.map(([unit, interval, cronExpr]) => ({
    unit,
    interval,
    cronExpr,
    rule: cronModule.buildIntervalScheduleRule(unit as any, interval, cronExpr, 1791300000000),
  }));
  const relative = [1, 90, 1440].map((delay) => ({
    delay,
    result: cronModule.buildRelativeDelaySchedule(delay, 1791300000000),
  }));
  const inferred = ["*/10 * * * *", " */5  * * * * ", "*/0 * * * *", "*/10 * * * 1"].map(
    (cronExpr) => ({
      cronExpr,
      rule: cronModule.inferMinuteIntervalScheduleRule(cronExpr, 1791300000000) ?? null,
    }),
  );
  const initial = scenarios.INITIAL_CASES.map((input) => ({
    ...input,
    ...attempt(() =>
      cronModule.computeInitialAutomationNextRunAt(
        { cronExpr: input.cronExpr, recurring: input.recurring },
        input.from,
      ),
    ),
  }));
  return { rules, carriers, relative, inferred, initial };
}

/** 生成的 id 换成稳定记号，Rust 侧做同样的替换后比较。 */
function normalize(value: unknown, ids: Map<string, string>): unknown {
  if (typeof value === "string") {
    let text = value;
    for (const [id, token] of [...ids.entries()].sort((a, b) => b[0].length - a[0].length))
      text = text.split(id).join(token);
    return text;
  }
  if (Array.isArray(value)) return value.map((item) => normalize(item, ids));
  if (value && typeof value === "object")
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [key, normalize(item, ids)]),
    );
  return value;
}

async function runServiceScenario(dir: string, scenario: { name: string; ops: Op[] }) {
  setZone("America/New_York");
  const path = join(dir, scenario.name, "tasks-index.sqlite");
  clock = scenarios.SERVICE_NOW;
  await prepareTasksIndexStorage(path, () => {});
  const repo = new AutomationRepo(path);
  const service = new AutomationService(repo);
  await repo.ensureReady();
  const db = new DatabaseSync(path);
  const automations: string[] = [];
  const runs: string[] = [];
  const ids = new Map<string, string>();
  const results: unknown[] = [];
  const key = (scope?: { workspacePath: string; workspaceIdentity?: string }) =>
    scope ? scope.workspaceIdentity?.trim() || scope.workspacePath : undefined;
  const runId = (op: Op) =>
    op.run !== undefined ? runs[op.run]! : String(op.runId).replace("#0", automations[0]!);
  try {
    for (const op of scenario.ops) {
      if (op.now !== undefined) clock = op.now;
      const id = op.id === undefined ? "" : automations[op.id]!;
      let result: { value?: unknown; error?: string };
      try {
        let value: unknown;
        switch (op.op) {
          case "create":
            value = await service.create(op.params);
            automations.push((value as any).automationId);
            ids.set((value as any).automationId, `<automation:${automations.length - 1}>`);
            break;
          case "list":
            value = await service.list(op.scope);
            break;
          case "update":
            value = (await service.update(id, op.patch, op.scope ?? scenarios.WS)) ?? null;
            break;
          case "delete":
            value = await service.delete(id, op.scope);
            break;
          case "setEnabled":
            value = await service.setEnabled(id, op.enabled, op.scope ?? scenarios.WS);
            break;
          case "restart":
            value = await service.restart(id, op.scope ?? scenarios.WS);
            break;
          case "hasTaskBinding":
            value = await service.hasTaskBinding({ ...op.scope, targetTaskId: op.targetTaskId });
            break;
          case "runNow": {
            const claimed = await service.runNow(id, op.scope);
            if (claimed) {
              runs.push(claimed.run.runId);
              ids.set(claimed.run.runId, `<run:${runs.length - 1}>`);
            }
            value = claimed;
            break;
          }
          case "getBotDeliveryTarget":
            value = (await repo.getBotDeliveryTarget(id)) ?? null;
            break;
          case "getModelSelectionForDispatch":
            value = (await repo.getModelSelectionForDispatch(id, key(op.scope)!)) ?? null;
            break;
          case "fixRunModelSelection":
            value = await repo.fixRunModelSelection(runId(op), op.selection);
            break;
          case "markRunOutcome":
            value = await repo.markRunOutcome(runId(op), op.outcome, op.error);
            break;
          case "markRunDispatch":
            value = await repo.markRunDispatch({
              runId: runId(op),
              dispatchStatus: op.status,
              error: op.error,
            });
            break;
          case "markManualRunDispatched":
            value = await repo.markManualRunDispatched({
              runId: runId(op),
              sessionId: op.sessionId,
              dispatchedAt: op.dispatchedAt,
            });
            break;
          case "touchManualClaim":
            value = await repo.touchManualClaim(id, scenarios.WS.workspacePath);
            break;
          case "releaseManualClaim":
            value = await repo.releaseManualClaim(id, scenarios.WS.workspacePath);
            break;
          case "ensureRunClaimed":
            value = await repo.ensureRunClaimed({
              runId: runId(op),
              automationId: id,
              workspaceKey: scenarios.WS.workspacePath,
              scheduledAt: op.scheduledAt,
              trigger: op.trigger,
            });
            break;
          case "listRuns":
            value = await service.listRuns(id, op.scope);
            break;
          case "deleteRun":
            value = await service.deleteRun(runId(op), op.scope);
            break;
          case "getRun":
            value = (await repo.getRun(runId(op))) ?? null;
            break;
          case "sql":
            db.exec(op.sql);
            break;
          default:
            throw new Error(`unknown op ${op.op}`);
        }
        // 经 JSON 往返：undefined 字段与 Rust 一样不出现，键序保留。
        result = { value: JSON.parse(JSON.stringify(value ?? null)) };
      } catch (error) {
        result = { error: error instanceof Error ? error.message : String(error) };
      }
      results.push(normalize(result, ids));
    }
    const dumps = {
      automations: db.prepare("SELECT * FROM automations ORDER BY created_at, title").all(),
      automation_runs: db.prepare("SELECT * FROM automation_runs ORDER BY created_at, run_id").all(),
    };
    return { ...scenario, results, dumps: normalize(dumps, ids) };
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
      `[automation-fixtures] ${name} is stale; run pnpm exec tsx scripts/rust-host/automation-fixtures.ts`,
    );
    return false;
  }
  await mkdir(fixtureDir, { recursive: true });
  await writeFile(path, formatted.code);
  return true;
}

const dir = await mkdtemp(join(tmpdir(), "zcode-automation-fixtures-"));
try {
  const cron = cronCases();
  const schedule = scheduleCases();
  const service = [];
  for (const scenario of scenarios.SERVICE_SCENARIOS)
    service.push(await runServiceScenario(dir, scenario));
  const source = "scripts/rust-host/automation-fixtures.ts";
  const ok = [
    await emit("cron.json", { source, cases: cron }),
    await emit("schedule.json", { source, ...schedule }),
    await emit("service.json", { source, serviceNow: scenarios.SERVICE_NOW, scenarios: service }),
  ].every(Boolean);
  if (!ok) process.exitCode = 1;
  else
    console.log(
      `[automation-fixtures] ${process.argv.includes("--check") ? "up to date" : "wrote"}: ` +
        `${cron.length} cron cases, ${schedule.rules.length} rules, ${service.length} service scenarios`,
    );
} finally {
  Date.now = realNow;
  await rm(dir, { recursive: true, force: true });
}
