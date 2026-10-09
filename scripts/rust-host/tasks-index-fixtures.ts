// Rust Host tasks-index 的对拍 fixture（docs/specs/rust-host-r2.md §7）。
// 用真实 TS 实现（迁移定义、prepareTasksIndexStorage、TaskIndexRepo）在固定时钟下执行
// tasks-index-scenarios.ts 的输入，记录账本 checksum、sqlite_master、表内容与每一步返回值；
// Rust 侧 include_str! 嵌入并逐项复现。用法：pnpm exec tsx scripts/rust-host/tasks-index-fixtures.ts [--check]
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { format } from "oxfmt";
import { TASKS_DATABASE_MIGRATION_DEFINITIONS } from "../../packages/services/src/session/tasksDatabase/migrations.js";
import { runTasksDatabaseMigrations } from "../../packages/services/src/session/tasksDatabase/migrations.js";
import { prepareTasksIndexStorage } from "../../packages/services/src/session/tasksDatabase/startup.js";
import { TaskIndexRepo } from "../../packages/services/src/session/taskIndexRepo.js";
import {
  BASE_NOW,
  OPERATION_DUMP_TABLES,
  OPERATION_SCENARIOS,
  STARTUP_SCENARIOS,
} from "./tasks-index-scenarios.js";

const { DatabaseSync } = createRequire(import.meta.url)(
  "node:sqlite",
) as typeof import("node:sqlite");
type Database = InstanceType<typeof DatabaseSync>;
type Op = Record<string, any>;

const root = resolve(import.meta.dirname, "../..");
const fixtureDir = resolve(root, "apps/zcode-host/crates/sessions/src/tasks_index/fixtures");
const PREPARE_NOW = BASE_NOW + 1;

// 时钟固定后，账本 time_applied、分组顺序时间戳与 unread marker 都可复现。
let clock = BASE_NOW;
Date.now = () => clock;

const DUMP_SQL: Record<string, string> = {
  sqlite_master: "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY rowid",
  tasks_schema_migration: "SELECT * FROM tasks_schema_migration ORDER BY id",
  tasks: "SELECT * FROM tasks ORDER BY workspace_key, task_id",
  task_groups: "SELECT * FROM task_groups ORDER BY group_id",
  task_group_members: "SELECT * FROM task_group_members ORDER BY workspace_key, task_id",
  task_group_view_node_orders:
    "SELECT * FROM task_group_view_node_orders ORDER BY node_type, node_key",
  automations: "SELECT * FROM automations ORDER BY automation_id",
  off_peak_tasks: "SELECT * FROM off_peak_tasks ORDER BY off_peak_task_id",
};

function dump(path: string, tables: string[]) {
  const db = new DatabaseSync(path);
  try {
    return tables.map((table) => ({
      table,
      sql: DUMP_SQL[table],
      rows: db.prepare(DUMP_SQL[table]!).all(),
    }));
  } finally {
    db.close();
  }
}

function sha256(text: string): string {
  return createHash("sha256").update(text).digest("hex");
}

// JSON 里没有 undefined：除可为 null 的 target 外，顶层 null 按 TS 调用方会传的 undefined 处理。
function undefinedForNull(value: Record<string, unknown>): Record<string, unknown> {
  const result: Record<string, unknown> = {};
  for (const [key, entry] of Object.entries(value))
    result[key] = entry === null && key !== "target" ? undefined : entry;
  return result;
}

async function orNull<T>(run: () => Promise<T>): Promise<T | null> {
  try {
    return await run();
  } catch {
    return null;
  }
}

async function apply(repo: TaskIndexRepo, db: Database, op: Op): Promise<unknown> {
  if (op.now !== undefined) clock = op.now;
  const target = op.target;
  switch (op.op) {
    case "insertRow": {
      const columns = Object.keys(op.row);
      db.prepare(
        `INSERT INTO tasks (${columns.join(", ")}) VALUES (${columns.map(() => "?").join(", ")})`,
      ).run(...columns.map((column) => op.row[column]));
      return null;
    }
    case "seed":
      return repo.seedTaskMetaIfMissing(undefinedForNull(op.meta) as never);
    case "syncTop":
      return repo.syncTaskMetaAtGroupedTop({
        meta: undefinedForNull(op.meta) as never,
        searchableText: op.searchableText,
      });
    case "sync":
      return repo.syncTaskMeta({
        meta: undefinedForNull(op.meta) as never,
        searchableText: op.searchableText,
      });
    case "agentPatch":
      return repo.applyAgentPatch({ ...target, patch: undefinedForNull(op.patch) });
    case "unread":
      return orNull(async () => {
        if (!op.unread && typeof op.expectedUnreadAt === "number") {
          const result = await repo.clearTaskUnreadIfMatches({
            ...target,
            expectedUnreadAt: op.expectedUnreadAt,
          });
          return { meta: result.meta, changed: result.cleared };
        }
        const patch = { unreadAt: op.unread ? Date.now() : undefined };
        return { meta: await repo.updateTaskState({ ...target, patch }), changed: true };
      });
    case "pinned":
      return orNull(() => repo.updateTaskState({ ...target, patch: { pinned: op.value } }));
    case "archived":
      return orNull(() => repo.updateTaskState({ ...target, patch: { archived: op.value } }));
    case "delete":
      return orNull(() => repo.updateTaskState({ ...target, patch: { deleted: true } }));
    case "rename":
      return orNull(() =>
        repo.updateTaskState({
          ...target,
          patch: { title: op.title, updatedAt: Date.now(), titleOverridden: true },
        }),
      );
    case "get":
      return repo.getTaskMeta(target);
    case "list":
      return repo.listTaskMetas(op.filter);
    case "listDeleted":
      return repo.listDeletedTaskIds({
        workspacePath: target.workspacePath,
        workspaceIdentity: target.workspaceIdentity,
        provider: op.provider,
      });
    default:
      throw new Error(`unknown op ${op.op}`);
  }
}

async function runOperationScenario(dir: string, scenario: { name: string; ops: Op[] }) {
  const path = join(dir, scenario.name, "tasks-index.sqlite");
  clock = BASE_NOW;
  await prepareTasksIndexStorage(path, () => {});
  const repo = new TaskIndexRepo(path);
  await repo.ensureReady();
  const db = new DatabaseSync(path);
  const results: unknown[] = [];
  try {
    for (const op of scenario.ops) {
      const result = await apply(repo, db, op);
      // 经 JSON 往返：undefined 字段与 Rust 一样不出现，键序保留。
      results.push(JSON.parse(JSON.stringify(result ?? null)));
    }
  } finally {
    db.close();
    repo.close();
  }
  return { ...scenario, results, dumps: dump(path, OPERATION_DUMP_TABLES) };
}

async function runStartupScenario(dir: string, scenario: (typeof STARTUP_SCENARIOS)[number]) {
  // 嵌套目录：同时验证启动会 mkdir -p。
  const path = join(dir, scenario.name, "nested", ".zcode", "v2", "tasks-index.sqlite");
  clock = BASE_NOW;
  if (scenario.base === "migrated" || scenario.setup.length > 0) {
    await mkdir(dirname(path), { recursive: true });
    const db = new DatabaseSync(path);
    if (scenario.base === "migrated") runTasksDatabaseMigrations(db);
    for (const statement of scenario.setup) db.exec(statement);
    for (const id of scenario.resetLedger)
      db.prepare("DELETE FROM tasks_schema_migration WHERE id = ?").run(id);
    db.close();
  }
  clock = PREPARE_NOW;
  const progress: unknown[] = [];
  await prepareTasksIndexStorage(path, (phase, migration) =>
    progress.push(migration ? { phase, migration } : { phase }),
  );
  return { ...scenario, progress, dumps: dump(path, scenario.dump) };
}

async function emit(name: string, data: unknown): Promise<boolean> {
  const path = join(fixtureDir, name);
  const formatted = await format(path, `${JSON.stringify(data, null, 2)}\n`);
  if (formatted.errors.length) throw new Error(`Cannot format ${name}`);
  if (process.argv.includes("--check")) {
    const current = await readFile(path, "utf8").catch(() => "");
    if (current === formatted.code) return true;
    console.error(
      `[tasks-index-fixtures] ${name} is stale; run pnpm exec tsx scripts/rust-host/tasks-index-fixtures.ts`,
    );
    return false;
  }
  await mkdir(fixtureDir, { recursive: true });
  await writeFile(path, formatted.code);
  return true;
}

const dir = await mkdtemp(join(tmpdir(), "zcode-tasks-index-fixtures-"));
const warn = console.warn;
// 非法 meta_json 场景会让 TS repo 打 warn；fixture 生成只关心结果。
console.warn = () => {};
try {
  const startup = [];
  for (const scenario of STARTUP_SCENARIOS) startup.push(await runStartupScenario(dir, scenario));
  const operations = [];
  for (const scenario of OPERATION_SCENARIOS)
    operations.push(await runOperationScenario(dir, scenario));

  const migrations = TASKS_DATABASE_MIGRATION_DEFINITIONS.map((definition) => {
    const checksumInputJson = JSON.stringify(definition.checksumInput);
    return {
      id: definition.id,
      checksum: sha256(checksumInputJson),
      checksumInput: definition.checksumInput,
    };
  });
  // 再与真实 runner 写入的账本核对，fixture 的 checksum 不是另算一套。
  const ledger = startup[0]!.dumps.find((entry) => entry.table === "tasks_schema_migration")!;
  const applied = ledger.rows.map((row) => `${row.id}:${row.checksum}`).join(",");
  if (applied !== migrations.map((entry) => `${entry.id}:${entry.checksum}`).join(","))
    throw new Error("migration definitions disagree with the ledger written by the TS runner");

  const source = "scripts/rust-host/tasks-index-fixtures.ts";
  const ok = [
    await emit("migrations.json", { source, migrations }),
    await emit("parity.json", {
      source,
      baseNow: BASE_NOW,
      prepareNow: PREPARE_NOW,
      startup,
      operations,
    }),
  ].every(Boolean);
  if (!ok) process.exitCode = 1;
  else
    console.log(
      `[tasks-index-fixtures] ${process.argv.includes("--check") ? "up to date" : "wrote"}: ` +
        `${migrations.length} migrations, ${startup.length} startup + ${operations.length} operation scenarios`,
    );
} finally {
  console.warn = warn;
  await rm(dir, { recursive: true, force: true });
}
