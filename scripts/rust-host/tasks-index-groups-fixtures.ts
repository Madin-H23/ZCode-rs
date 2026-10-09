// Rust Host tasks-index 分组、grouped 视图、搜索与归档的对拍 fixture（docs/specs/rust-host-tasks.md §2.1–2.2）。
// 用真实 TS TaskIndexRepo 在固定时钟下执行下面的场景，记录每一步返回值（或错误文案）与最终表内容；
// Rust 侧 tasks_index/groups_tests.rs include_str! 嵌入并逐项复现。
// 用法：pnpm exec tsx scripts/rust-host/tasks-index-groups-fixtures.ts [--check]
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { format } from "oxfmt";
import { prepareTasksIndexStorage } from "../../packages/services/src/session/tasksDatabase/startup.js";
import { TaskIndexRepo } from "../../packages/services/src/session/taskIndexRepo.js";

const { DatabaseSync } = createRequire(import.meta.url)(
  "node:sqlite",
) as typeof import("node:sqlite");
type Database = InstanceType<typeof DatabaseSync>;
type Op = Record<string, any>;

const root = resolve(import.meta.dirname, "../..");
const fixturePath = resolve(
  root,
  "apps/zcode-host/crates/sessions/src/tasks_index/fixtures/parity-groups.json",
);
const BASE_NOW = 1_760_000_000_000;
const DAY = 24 * 60 * 60 * 1000;
let clock = BASE_NOW;
Date.now = () => clock;

const W = "/work/alpha";
const OTHER = "/work/beta";
const REMOTE = "ssh:dev@host:/srv/repo";

function row(taskId: string, extra: Record<string, unknown> = {}): Op {
  const workspace = (extra.workspace_path as string | undefined) ?? W;
  return {
    op: "insertRow",
    row: {
      workspace_key: workspace,
      workspace_path: workspace,
      task_id: taskId,
      title: `Title ${taskId}`,
      task_status: "completed",
      provider: "glm",
      created_at: 100,
      updated_at: 200,
      searchable_text: "",
      mode: "build",
      meta_json: JSON.stringify({
        taskId,
        traceId: `trace-${taskId}`,
        title: `Title ${taskId}`,
        mode: "build",
        workspacePath: workspace,
        createdAt: 100,
        updatedAt: 200,
        provider: "glm",
      }),
      ...extra,
    },
  };
}

const scope = (workspacePath: string, extra: Record<string, unknown> = {}) => ({
  workspacePath,
  ...extra,
});
const ref = (taskId: string, workspacePath = W) => ({ workspacePath, taskId });

const GROUP_DUMPS = [
  "task_groups",
  "task_group_members",
  "task_group_view_node_orders",
  "task_group_workspace_bootstraps",
];

export const SCENARIOS: Array<{ name: string; dump: string[]; ops: Op[] }> = [
  {
    name: "groups_crud",
    dump: GROUP_DUMPS,
    ops: [
      { op: "createGroup", params: { title: "  Alpha ", color: "red" }, now: BASE_NOW + 1 },
      { op: "createGroup", params: {}, now: BASE_NOW + 2 },
      { op: "createGroup", params: { title: "Odd", color: "pink" }, now: BASE_NOW + 3 },
      { op: "renameGroup", group: 0, title: "   ", now: BASE_NOW + 4 },
      { op: "renameGroup", groupId: "missing", title: "x" },
      { op: "updateColor", group: 1, color: "blue", now: BASE_NOW + 5 },
      { op: "updateColor", group: 1, color: "pink" },
      { op: "updateColor", groupId: "missing", color: "red" },
      { op: "structure", scopes: [scope(W)] },
      { op: "deleteGroup", group: 0 },
      { op: "deleteGroup", groupId: "missing" },
      { op: "structure", scopes: [] },
    ],
  },
  {
    name: "grouped_order",
    dump: GROUP_DUMPS,
    ops: [
      row("t1", { created_at: 101 }),
      row("t2", { created_at: 102 }),
      row("t3", { created_at: 103 }),
      row("t4", { created_at: 104 }),
      row("t5", { created_at: 105 }),
      row("t6", { created_at: 106 }),
      row("pinned", { pinned: 1 }),
      row("archived", { archived: 1 }),
      row("claude", { provider: "claude" }),
      row("o1", { workspace_path: OTHER }),
      { op: "createGroup", params: { title: "A" }, now: BASE_NOW + 10 },
      { op: "createGroup", params: { title: "B" }, now: BASE_NOW + 11 },
      {
        op: "exec",
        sql: `INSERT INTO task_groups (group_id, title, color, created_at, updated_at)
          VALUES ('zcode-default-group-cron', 'Cron', 'gray', 50, 50)`,
      },
      {
        op: "exec",
        sql: `INSERT INTO task_group_members (group_id, workspace_key, workspace_path, workspace_identity,
            task_id, sort_order, added_at, created_at, updated_at)
          VALUES ('zcode-default-group-cron', '${W}', '${W}', NULL, 't6', NULL, 60, 60, 60)`,
      },
      {
        op: "apply",
        now: BASE_NOW + 20,
        input: {
          workspaceScopes: [scope(W)],
          topLevelNodes: [
            { type: "group", group: 0 },
            { type: "task", task: ref("t3") },
            { type: "group", group: 1 },
            { type: "task", task: ref("claude") },
          ],
          groups: [
            { group: 0, taskRefs: [ref("t2"), ref("t1"), ref("claude")] },
            { group: 1, taskRefs: [ref("t4")] },
          ],
        },
      },
      { op: "structure", scopes: [scope(W), scope(W)] },
      {
        op: "exec",
        sql: `INSERT INTO task_group_members (group_id, workspace_key, workspace_path, workspace_identity,
            task_id, sort_order, added_at, created_at, updated_at)
          SELECT group_id, '${W}', '${W}', NULL, 't5', NULL, 70, 70, 70 FROM task_groups WHERE title = 'B'`,
      },
      {
        op: "apply",
        now: BASE_NOW + 30,
        input: {
          workspaceScopes: [scope(W)],
          topLevelNodes: [
            { type: "task", task: ref("t3") },
            { type: "group", group: 0 },
          ],
          groups: [{ group: 0, taskRefs: [ref("t1")] }],
        },
      },
      {
        op: "apply",
        input: {
          workspaceScopes: [scope(W)],
          topLevelNodes: [{ type: "task", task: ref("o1", OTHER) }],
          groups: [],
        },
      },
      {
        op: "apply",
        input: {
          workspaceScopes: [scope(W)],
          topLevelNodes: [{ type: "task", task: ref("pinned") }],
          groups: [],
        },
      },
      {
        op: "apply",
        input: {
          workspaceScopes: [scope(W)],
          topLevelNodes: [{ type: "group", groupId: "nope" }],
          groups: [],
        },
      },
      {
        op: "apply",
        input: {
          workspaceScopes: [scope(W)],
          topLevelNodes: [],
          groups: [
            { group: 0, taskRefs: [ref("t1")] },
            { group: 1, taskRefs: [ref("t1")] },
          ],
        },
      },
      { op: "structure", scopes: [scope(OTHER)] },
    ],
  },
  {
    name: "task_list",
    dump: ["tasks"],
    ops: [
      row("a", { updated_at: 300, created_at: 10, searchable_text: "the quick brown fox jumps" }),
      row("b", {
        updated_at: 200,
        created_at: 30,
        title: "Needle in title",
        searchable_text: "nothing here",
      }),
      row("c", {
        updated_at: 100,
        created_at: 20,
        searchable_text: `${"x".repeat(40)} NEEDLE ${"y".repeat(90)} needle tail`,
      }),
      row("p", { pinned: 1, searchable_text: "needle pinned" }),
      row("ar", { archived: 1, searchable_text: "needle archived" }),
      row("d", { deleted: 1, searchable_text: "needle deleted" }),
      row("g", { provider: "claude", searchable_text: "needle claude" }),
      row("r1", {
        workspace_key: REMOTE,
        workspace_path: "/srv/repo",
        workspace_identity: REMOTE,
        searchable_text: "remote needle",
      }),
      { op: "list", query: { kind: "timeline", workspaceScopes: [scope(W)] }, provider: "glm" },
      {
        op: "list",
        query: { kind: "timeline", workspaceScopes: [scope(W)], sortBy: "created", limit: 2 },
        provider: "glm",
      },
      { op: "list", query: { kind: "timeline", workspaceScopes: [scope(W)], limit: 0 } },
      { op: "list", query: { kind: "pinned", workspaceScopes: [scope(W)] }, provider: "glm" },
      { op: "list", query: { kind: "archived", workspaceScopes: [scope(W)] }, provider: "glm" },
      {
        op: "list",
        query: { kind: "timeline", workspaceScopes: [scope(W)], search: "  NeEdle " },
        provider: "glm",
      },
      { op: "list", query: { kind: "timeline", workspaceScopes: [scope(W)], search: "zzz" } },
      {
        op: "list",
        query: {
          kind: "active",
          workspaceScopes: [
            scope(W, { workspacePurpose: "conversation" }),
            scope("/srv/repo", { workspaceIdentity: REMOTE }),
            scope(W),
          ],
          search: "needle",
        },
        provider: "glm",
      },
      { op: "list", query: { kind: "timeline", workspaceScopes: [] } },
      { op: "list", query: { kind: "timeline", workspaceScopes: [scope("  ")] } },
    ],
  },
  {
    name: "archive_delete",
    dump: ["tasks", "task_group_members", "task_group_view_node_orders"],
    ops: [
      row("old", { updated_at: BASE_NOW - 10 * DAY }),
      row("recent", { updated_at: BASE_NOW - 2 * DAY }),
      row("running", { updated_at: BASE_NOW - 10 * DAY, task_status: "running" }),
      row("pinnedOld", { updated_at: BASE_NOW - 10 * DAY, pinned: 1 }),
      row("unreadOld", { updated_at: BASE_NOW - 10 * DAY, unread_at: 5 }),
      row("claudeOld", { updated_at: BASE_NOW - 9 * DAY, provider: "claude" }),
      row("gone", { updated_at: BASE_NOW - 10 * DAY, deleted: 1, archived: 1 }),
      { op: "archiveStale", target: { workspacePath: W }, olderThanDays: 7.9, provider: "glm" },
      { op: "archiveStale", target: { workspacePath: W }, olderThanDays: 0 },
      { op: "archiveStale", target: { workspacePath: W }, olderThanDays: 7 },
      {
        op: "exec",
        sql: `INSERT INTO task_group_view_node_orders (node_type, node_key, sort_order, created_at, updated_at)
          VALUES ('task', '${JSON.stringify([W, "old"])}', 10, 1, 1), ('task', '${W}', 20, 1, 1)`,
      },
      { op: "deleteArchived", target: { workspacePath: W, taskId: "old" }, now: BASE_NOW + 40 },
      { op: "deleteArchived", target: { workspacePath: W, taskId: "old" } },
      { op: "deleteArchived", target: { workspacePath: W, taskId: "running" } },
      { op: "deleteArchived", target: { workspacePath: W, taskId: "missing" } },
    ],
  },
];

const DUMP_SQL: Record<string, string> = {
  tasks: "SELECT * FROM tasks ORDER BY workspace_key, task_id",
  task_groups: "SELECT * FROM task_groups ORDER BY created_at, title",
  task_group_members: "SELECT * FROM task_group_members ORDER BY workspace_key, task_id",
  task_group_view_node_orders:
    "SELECT * FROM task_group_view_node_orders ORDER BY node_type, sort_order, node_key",
  task_group_workspace_bootstraps:
    "SELECT * FROM task_group_workspace_bootstraps ORDER BY workspace_key",
};

// 分组 id 是随机 uuid：结果里记录真实 id，后续 op 用 `group: n` 引用第 n 个创建的分组；
// dump 时把它们换成占位符，Rust 侧按同样规则替换后比较。
function resolveGroups(value: unknown, ids: string[]): unknown {
  if (Array.isArray(value)) return value.map((entry) => resolveGroups(entry, ids));
  if (value && typeof value === "object") {
    const result: Record<string, unknown> = {};
    for (const [key, entry] of Object.entries(value)) {
      if (key === "group" && typeof entry === "number") result.groupId = ids[entry];
      else result[key] = resolveGroups(entry, ids);
    }
    return result;
  }
  return value;
}

function maskGroups(value: unknown, ids: string[]): unknown {
  const text = JSON.stringify(value ?? null);
  return JSON.parse(
    ids.reduce((masked, id, index) => masked.split(id).join(`<group-${index}>`), text),
  );
}

async function apply(repo: TaskIndexRepo, db: Database, op: Op, ids: string[]): Promise<unknown> {
  if (op.now !== undefined) clock = op.now;
  const resolved = resolveGroups(op, ids) as Op;
  switch (op.op) {
    case "insertRow": {
      const columns = Object.keys(op.row);
      db.prepare(
        `INSERT INTO tasks (${columns.join(", ")}) VALUES (${columns.map(() => "?").join(", ")})`,
      ).run(...columns.map((column) => op.row[column]));
      return null;
    }
    case "exec":
      db.exec(op.sql);
      return null;
    case "createGroup": {
      const group = await repo.createTaskGroup(op.params);
      ids.push(group.id);
      return group;
    }
    case "renameGroup":
      return repo.renameTaskGroup({ groupId: resolved.groupId, title: op.title });
    case "updateColor":
      return repo.updateTaskGroupColor({ groupId: resolved.groupId, color: op.color });
    case "deleteGroup":
      return repo.deleteTaskGroup({ groupId: resolved.groupId });
    case "structure":
      return repo.queryGroupedTaskViewStructure({ workspaceScopes: op.scopes });
    case "apply":
      return repo.applyGroupedTaskViewOrder({ ...resolved.input, provider: "glm" });
    case "list":
      return repo.queryTaskList({ ...op.query, provider: op.provider });
    case "archiveStale":
      return repo.archiveStaleTasks({
        ...op.target,
        olderThanDays: op.olderThanDays,
        provider: op.provider,
      });
    case "deleteArchived":
      return repo.deleteArchivedTask(op.target);
    default:
      throw new Error(`unknown op ${op.op}`);
  }
}

async function run(dir: string, scenario: (typeof SCENARIOS)[number]) {
  const path = join(dir, scenario.name, "tasks-index.sqlite");
  clock = BASE_NOW;
  await prepareTasksIndexStorage(path, () => {});
  const repo = new TaskIndexRepo(path);
  await repo.ensureReady();
  const db = new DatabaseSync(path);
  const ids: string[] = [];
  const results: unknown[] = [];
  try {
    for (const op of scenario.ops) {
      try {
        results.push(maskGroups(await apply(repo, db, op, ids), ids));
      } catch (error) {
        results.push({ error: error instanceof Error ? error.message : String(error) });
      }
    }
    const dumps = scenario.dump.map((table) => ({
      table,
      sql: DUMP_SQL[table],
      rows: maskGroups(db.prepare(DUMP_SQL[table]!).all(), ids),
    }));
    return { ...scenario, results, dumps };
  } finally {
    db.close();
    repo.close();
  }
}

const dir = await mkdtemp(join(tmpdir(), "zcode-tasks-index-groups-"));
try {
  const scenarios = [];
  for (const scenario of SCENARIOS) scenarios.push(await run(dir, scenario));
  const data = {
    source: "scripts/rust-host/tasks-index-groups-fixtures.ts",
    baseNow: BASE_NOW,
    scenarios,
  };
  const formatted = await format(fixturePath, `${JSON.stringify(data, null, 2)}\n`);
  if (formatted.errors.length) throw new Error("Cannot format parity-groups.json");
  if (process.argv.includes("--check")) {
    const current = await readFile(fixturePath, "utf8").catch(() => "");
    if (current !== formatted.code) {
      console.error(
        "[tasks-index-groups-fixtures] parity-groups.json is stale; run pnpm exec tsx scripts/rust-host/tasks-index-groups-fixtures.ts",
      );
      process.exitCode = 1;
    } else console.log("[tasks-index-groups-fixtures] up to date");
  } else {
    await writeFile(fixturePath, formatted.code);
    console.log(`[tasks-index-groups-fixtures] wrote ${scenarios.length} scenarios`);
  }
} finally {
  await rm(dir, { recursive: true, force: true });
}
