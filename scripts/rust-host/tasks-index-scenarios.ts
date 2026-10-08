// tasks-index Rust 移植的对拍输入（由 tasks-index-fixtures.ts 执行 TS 实现后写入 fixture）。
// 只用虚构路径与 ID：fixture 会提交进仓库，不能出现真实用户数据。

export const BASE_NOW = 1_760_000_000_000;

const W = "/tmp/zcode-fixture/workspace";
const W2 = "/tmp/zcode-fixture/other";
const REMOTE = "remote:ssh:fixture-host:22:dev:/srv/app";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Target = { workspacePath: string; workspaceIdentity?: string; taskId: string };

function meta(taskId: string, extra: Record<string, Json> = {}): Record<string, Json> {
  return {
    taskId,
    traceId: `trace-${taskId}`,
    title: `Task ${taskId}`,
    workspacePath: W,
    createdAt: 1000,
    updatedAt: 1000,
    mode: "build",
    provider: "glm",
    ...extra,
  };
}

const t = (taskId: string, extra: Partial<Target> = {}): Target => ({
  workspacePath: W,
  taskId,
  ...extra,
});

function row(taskId: string, metaJson: string, extra: Record<string, Json> = {}) {
  return {
    op: "insertRow",
    row: {
      workspace_key: W,
      workspace_path: W,
      task_id: taskId,
      title: `Column ${taskId}`,
      task_status: "running",
      provider: "glm",
      mode: "plan",
      model: "column-model",
      created_at: 10,
      updated_at: 20,
      meta_json: metaJson,
      ...extra,
    },
  };
}

const goal = (extra: Record<string, Json> = {}) => ({
  sessionID: " s1 ",
  targetID: "g1",
  objective: " ship ",
  status: "active",
  tokenBudget: 100,
  tokensUsed: 3,
  timeUsedSeconds: 2,
  time: { created: 4, updated: 5 },
  ...extra,
});

const valid = (extra: Record<string, Json> = {}) =>
  JSON.stringify({ ...meta("meta-id"), workspacePath: "/elsewhere", ...extra });

/** rowToMeta：合法 meta_json 的剥离、排序、trim、列覆盖，以及各类非法输入回退到列。 */
const rowToMetaOps = [
  row(
    "r01",
    `{"zExtra":"dropped","target":${JSON.stringify(goal({ activeInputId: null, unknown: 1 }))},` +
      `"changeSummary":{"fileCount":1,"added":2,"removed":0,"files":[{"path":"a.ts","added":2,"removed":0,"writeCount":1,"lastTurnIndex":0,"extra":true}]},` +
      `"lastError":{"message":"boom","code":"E1","traceId":" tr ","attribution":{"source":"provider","statusCode":429,"retryable":true}},` +
      `"status":"error","unreadAt":7,"title":"Meta title","traceId":"  trace-r1 ","taskId":"ignored","workspacePath":"/elsewhere",` +
      `"workspaceIdentity":" /elsewhere ","workspacePurpose":"project","createdAt":1e3,"updatedAt":2000.0,"mode":"build","model":"m1",` +
      `"thoughtLevel":" high ","runtimeEpoch":2,"provider":"glm","migrationSource":"claudeCode","forkedFromTaskId":" parent ",` +
      `"cronAutomationId":" cron-1 ","titleOverridden":false}`,
    { off_peak_task_id: "op-col", cron_automation_id: "cron-col", title_overridden: 1 },
  ),
  row("r02", "{not json"),
  row("r03", "null", {
    provider: "other",
    task_status: null,
    model: null,
    migration_source: "claudeCode",
    forked_from_task_id: "fork-col",
    unread_at: 99,
    title_overridden: 1,
  }),
  row("r04", "[]", { cron_automation_id: "cron-col", off_peak_task_id: "op-col" }),
  row("r05", JSON.stringify({ ...meta("x"), traceId: undefined })),
  row("r06", valid({ createdAt: -1 })),
  row("r07", valid({ createdAt: 1.5 })),
  row("r08", valid({ lastError: { message: "m", attribution: { source: "tool", extra: 1 } } })),
  row("r09", valid({ status: "cancelled" })),
  row("r10", valid({ provider: "claude" })),
  row("r11", valid({ target: null, unreadAt: 3 }), { unread_at: 42 }),
  row("r12", valid({ target: goal({ tokenBudget: undefined as unknown as Json }) })),
  row("r13", valid({ workspaceIdentity: "   " })),
  row("r14", valid({}), { cron_automation_id: "cron-col", off_peak_task_id: "op-col" }),
  row("r15", valid({ cronAutomationId: " c1 ", offPeakTaskId: "o1" }), {
    cron_automation_id: "c2",
    off_peak_task_id: "o2",
  }),
  row("r16", valid({ title: "" })),
  row(
    "r17",
    `{"taskId":"a","traceId":"first","traceId":"second","title":"dup","workspacePath":"/x","createdAt":1,"updatedAt":2,"mode":"yolo"}`,
  ),
  row(
    "r18",
    valid({
      changeSummary: {
        fileCount: 0,
        added: 0,
        removed: 0,
        files: [{ path: "p", added: 0, removed: 0, writeCount: 0, lastTurnIndex: 0 }],
      },
    }),
  ),
  row(
    "r19",
    `{"taskId":"a","traceId":"t","title":"big","workspacePath":"/x","createdAt":9007199254740993,"updatedAt":2,"mode":"build"}`,
  ),
  row("r20", valid({ unreadAt: -1 })),
  row("r21", valid({ createdAt: "1000" })),
  row("r22", valid({ lastError: { message: "" } })),
  row("r23", valid({ lastError: { message: "m", attribution: { reason: "😀".repeat(161) } } })),
  row("r24", valid({ lastError: { message: "m", attribution: { reason: "😀".repeat(160) } } })),
  row("r25", valid({ titleOverridden: "yes" as Json })),
  row("r26", valid({ target: goal({ summaryTitle: "" }) })),
  row("r27", valid({ target: goal({ time: undefined as unknown as Json }) })),
  row("r28", valid({ mode: "unknown" })),
  row("r29", valid({ workspacePurpose: "conversation", runtimeEpoch: 3, summaryTitle: "x" })),
  row("r30", valid({ target: goal({ summaryTitle: "Ship it", activeRunStartedAtMs: 5 }) })),
  row("r31", valid({ model: 5 as Json })),
  // workspace identity 投影：key/path/identity 的各种组合。
  row("i01", valid(), { workspace_identity: W }),
  row("i02", valid(), { workspace_key: REMOTE, workspace_identity: ` ${REMOTE} ` }),
  row("i03", valid(), {
    workspace_key: REMOTE,
    workspace_identity: "remote:ssh:old:22:dev:/srv/app",
  }),
  row("i04", valid(), { workspace_key: "/a", workspace_path: "/a", workspace_identity: "/b" }),
  row("i05", valid(), { workspace_key: "custom-id", workspace_identity: null }),
  row("i06", "{}", { workspace_key: "custom-id", workspace_identity: "custom-id" }),
  row("i07", "{}", { workspace_key: "remote:ssh:bad", workspace_identity: null }),
  { op: "list", filter: { includeDeleted: true } },
];

const syncOps = [
  { op: "seed", meta: meta("t1", { status: "running", thoughtLevel: "high" }) },
  { op: "seed", meta: meta("t1", { title: "ignored" }) },
  {
    op: "syncTop",
    meta: meta("t1", { title: "Snapshot", updatedAt: 900, status: "running", unknownKey: "kept" }),
    searchableText: "hello searchable world",
  },
  { op: "syncTop", meta: meta("t1", { updatedAt: 1100 }) },
  { op: "agentPatch", target: t("t1"), patch: { status: "completed", updatedAt: 2000 } },
  {
    op: "syncTop",
    meta: meta("t1", { status: "running", updatedAt: 1500, lastError: { message: "stale" } }),
  },
  { op: "syncTop", meta: meta("t1", { status: "error", updatedAt: 1600, target: null }) },
  { op: "rename", target: t("t1"), title: "Manual", now: BASE_NOW + 10 },
  { op: "agentPatch", target: t("t1"), patch: { title: "Agent title", lastError: null } },
  { op: "syncTop", meta: meta("t1", { title: "Snap", migrationSource: "claudeCode" }) },
  { op: "syncTop", meta: meta("t1", { searchableTextProbe: true }), searchableText: "" },
  { op: "agentPatch", target: t("t1"), patch: { target: goal(), updatedAt: 3000 } },
  { op: "agentPatch", target: t("t1"), patch: { target: null, lastError: { message: "late" } } },
  { op: "agentPatch", target: t("missing"), patch: { title: "x" } },
  // syncTaskMeta：同样的合并规则，但不初始化 grouped 顶层顺序。
  { op: "sync", meta: meta("t2", { status: "running" }), searchableText: "plain sync" },
  { op: "sync", meta: meta("t2", { updatedAt: 500, unknownKey: 1 }) },
  { op: "sync", meta: meta("t1", { status: "running", updatedAt: 10 }) },
  { op: "get", target: t("t1") },
];

const unreadOps = [
  { op: "seed", meta: meta("u1") },
  { op: "unread", target: t("u1"), unread: true, now: BASE_NOW },
  { op: "unread", target: t("u1"), unread: true, now: BASE_NOW },
  { op: "unread", target: t("u1"), unread: false, expectedUnreadAt: BASE_NOW },
  { op: "unread", target: t("u1"), unread: false, expectedUnreadAt: BASE_NOW + 1 },
  { op: "unread", target: t("u1"), unread: true, now: BASE_NOW },
  { op: "unread", target: t("u1"), unread: false },
  { op: "syncTop", meta: meta("u1", { unreadAt: 5 }) },
  { op: "unread", target: t("u1"), unread: true, now: BASE_NOW + 100 },
  { op: "syncTop", meta: meta("u1", { updatedAt: 1200 }) },
  { op: "pinned", target: t("u1"), value: true },
  { op: "unread", target: t("missing"), unread: true },
  { op: "unread", target: t("missing"), unread: false, expectedUnreadAt: 1 },
  { op: "delete", target: t("u1") },
  { op: "unread", target: t("u1"), unread: true },
];

const menuOps = [
  { op: "seed", meta: meta("m1", { updatedAt: 3000 }) },
  { op: "seed", meta: meta("m2", { updatedAt: 2000, provider: "other" as Json }) },
  { op: "seed", meta: meta("m3", { updatedAt: 2000, createdAt: 1500, titleOverridden: true }) },
  { op: "seed", meta: meta("m4", { workspacePath: W2, updatedAt: 5000 }) },
  { op: "pinned", target: t("m1"), value: true },
  { op: "archived", target: t("m2"), value: true },
  { op: "list", filter: { workspacePath: W, pinned: true } },
  { op: "list", filter: { workspacePath: W, archived: true } },
  { op: "list", filter: { workspacePath: W, provider: "glm" } },
  { op: "list", filter: {} },
  { op: "syncTop", meta: meta("m3", { updatedAt: 2100 }) },
  { op: "delete", target: t("m3") },
  { op: "listDeleted", target: t("m3") },
  { op: "listDeleted", target: t("m3"), provider: "other" },
  { op: "get", target: t("m3") },
  { op: "list", filter: { workspacePath: W, includeDeleted: true } },
  { op: "syncTop", meta: meta("m3", { updatedAt: 9000 }) },
  { op: "seed", meta: meta("m3", { title: "reseed" }) },
  { op: "pinned", target: t("m3"), value: true },
  { op: "rename", target: t("m3"), title: "dead" },
  { op: "archived", target: t("m2"), value: false },
  { op: "pinned", target: t("m1"), value: false },
  { op: "syncTop", meta: meta("m1", { updatedAt: 3001 }) },
  { op: "syncTop", meta: meta("m5", { updatedAt: 1 }) },
  { op: "list", filter: { workspacePath: W } },
];

const groupOps = [
  { op: "syncTop", meta: meta("g1") },
  { op: "syncTop", meta: meta("g2", { cronAutomationId: "auto-1" }) },
  { op: "syncTop", meta: meta("g2", { title: "again" }) },
  { op: "syncTop", meta: meta("g3", { offPeakTaskId: "op-1" }) },
  { op: "syncTop", meta: meta("g4", { workspaceIdentity: REMOTE, offPeakTaskId: "op-2" }) },
  { op: "syncTop", meta: meta("g5", { workspaceIdentity: ` ${REMOTE} ` }) },
  { op: "seed", meta: meta("g6", { cronAutomationId: "auto-2" }) },
  { op: "syncTop", meta: meta("g6", { cronAutomationId: "auto-3" }) },
  { op: "syncTop", meta: meta("g7", { workspaceIdentity: W }) },
  { op: "sync", meta: meta("g8", { cronAutomationId: "auto-9" }) },
  { op: "sync", meta: meta("g9", { offPeakTaskId: "op-9" }) },
  { op: "sync", meta: meta("g10") },
  { op: "syncTop", meta: meta("g10") },
  { op: "get", target: t("g7", { workspaceIdentity: W }) },
  { op: "list", filter: { workspacePath: W2, workspaceIdentity: REMOTE } },
  { op: "delete", target: t("g2") },
  { op: "delete", target: t("g4", { workspaceIdentity: REMOTE }) },
];

export const OPERATION_SCENARIOS = [
  { name: "row_to_meta", ops: rowToMetaOps },
  { name: "sync_merge", ops: syncOps },
  { name: "unread", ops: unreadOps },
  { name: "context_menu", ops: menuOps },
  { name: "grouped_order", ops: groupOps },
];

// 迁移 0002 / 0003 与启动修复：先用当前迁移建库，执行 setup，再删掉 resetLedger 里的账本行，
// 最后走一次完整启动（TS prepareTasksIndexStorage / Rust TasksIndex::open）。
const automation = (
  id: string,
  model: string | null,
  provider: string | null,
  level: string | null,
  selection: string | null = null,
) =>
  `INSERT INTO automations (automation_id, cron_expr, prompt, workspace_key, workspace_path, model, provider, thought_level, model_selection, created_at, updated_at) VALUES (${[id, "0 * * * *", "p", W, W, model, provider, level, selection].map(sql).join(", ")}, 1, 2)`;

function sql(value: string | null): string {
  return value === null ? "NULL" : `'${value.replaceAll("'", "''")}'`;
}

const legacyAutomations = [
  automation("a01", "custom:builtin:bigmodel:glm-4.6", null, null),
  automation("a02", "custom:builtin:zai-coding-plan:glm-5.1", "glm", " high "),
  automation("a03", "custom:my%20prov:model%3Ax", null, ""),
  automation("a04", "custom:bad%E0%A4%A:model", null, null),
  automation("a05", "custom:noseparator", null, null),
  automation("a06", "openai/gpt-4o$high", "ignored", "low"),
  automation("a07", "openai/gpt-4o$", null, null),
  automation("a08", "openai/$high", null, null),
  automation("a09", "openai/gpt$  ", null, null),
  automation("a10", "glm-4.6", "glm", null),
  automation("a11", "glm-4.6", " openrouter ", "medium"),
  automation("a12", "glm-4.6", null, null),
  automation("a13", "  ", null, null),
  automation("a14", "\t", null, null),
  automation("a15", null, null, null),
  automation("a16", null, null, null, '{"providerId":"x","modelId":"y"}'),
  automation("a17", "custom:builtin:unknown:foo", null, null),
  automation("a18", "custom:builtin:zai", null, null),
  automation("a19", "a/b/c", null, null),
  automation("a20", "custom:%E4%B8%AD:模型", null, null),
  automation("a21", "custom::model", null, null),
  automation("a22", "%ZZ/x", null, null),
  automation("a23", "custom:%C0%AF:x", null, null),
  automation("a24", "custom:builtin:bigmodel-start-plan:GLM-4.5-AIR", null, null, "stale"),
  automation("a25", "zcode-model", "zcode", null),
];

const glmSelections = [
  automation(
    "b01",
    null,
    null,
    null,
    '{"providerId":"account:zai-start-plan","modelId":"glm-5.1"}',
  ),
  automation(
    "b02",
    null,
    null,
    null,
    '{"providerId":"account:bigmodel-team-coding-plan","modelId":"GLM-4.6v-FLASH","options":{"reasoningLevel":"x"}}',
  ),
  automation("b03", null, null, null, '{"providerId":"bigmodel-api","modelId":"glm-5.1"}'),
  automation("b04", null, null, null, '{"providerId":"account:zai-start-plan","modelId":"glm-9"}'),
  automation("b05", null, null, null, "{broken"),
  automation("b06", null, null, null, "null"),
];

// 0001 以前的旧库：缺少后加列，且有两条重复的活跃闲时绑定（不能建唯一索引）。
const legacySchema = [
  `CREATE TABLE tasks (workspace_key TEXT NOT NULL, workspace_path TEXT NOT NULL, workspace_identity TEXT, task_id TEXT NOT NULL, title TEXT NOT NULL DEFAULT '', task_status TEXT, provider TEXT, mode TEXT NOT NULL DEFAULT 'build', model TEXT, migration_source TEXT, forked_from_task_id TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, unread_at INTEGER, pinned INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0, deleted INTEGER NOT NULL DEFAULT 0, meta_json TEXT NOT NULL DEFAULT '{}', PRIMARY KEY (workspace_key, task_id))`,
  `CREATE TABLE automations (automation_id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', cron_expr TEXT NOT NULL, prompt TEXT NOT NULL, model TEXT, provider TEXT, workspace_key TEXT NOT NULL, workspace_path TEXT NOT NULL, workspace_identity TEXT, location_kind TEXT NOT NULL DEFAULT 'local', recurring INTEGER NOT NULL DEFAULT 1, max_runs INTEGER, run_count INTEGER NOT NULL DEFAULT 0, enabled INTEGER NOT NULL DEFAULT 1, lifecycle_status TEXT NOT NULL DEFAULT 'active', next_run_at INTEGER, last_run_at INTEGER, running INTEGER NOT NULL DEFAULT 0, claimed_at INTEGER, dispatch_status TEXT NOT NULL DEFAULT 'idle', dispatch_attempts INTEGER NOT NULL DEFAULT 0, retry_at INTEGER, last_error TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL)`,
  `CREATE TABLE off_peak_tasks (off_peak_task_id TEXT PRIMARY KEY, server_ticket_id TEXT, title TEXT NOT NULL DEFAULT '', conversation_id TEXT, session_id TEXT, prompt TEXT NOT NULL, permission_mode TEXT NOT NULL, model TEXT, workspace_key TEXT NOT NULL, workspace_path TEXT NOT NULL, workspace_identity TEXT, status TEXT NOT NULL, queued_at INTEGER NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL)`,
  `INSERT INTO tasks (workspace_key, workspace_path, task_id, created_at, updated_at, meta_json) VALUES ('${W}', '${W}', 'legacy-1', 1, 2, '{}')`,
  `INSERT INTO automations (automation_id, cron_expr, prompt, model, provider, workspace_key, workspace_path, run_count, created_at, updated_at) VALUES ('legacy-a', '0 * * * *', 'p', 'openai/gpt-4o', NULL, '${W}', '${W}', 5, 1, 2)`,
  `INSERT INTO off_peak_tasks (off_peak_task_id, session_id, prompt, permission_mode, workspace_key, workspace_path, status, queued_at, created_at, updated_at) VALUES ('dup-1', 's', 'p', 'm', '${W}', '${W}', 'running', 1, 1, 1), ('dup-2', 's', 'p', 'm', '${W}', '${W}', 'queued', 1, 1, 1)`,
];

// 启动修复：闲时标记回填、闲时系统分组、已删除 task 的分组引用清理、awaiting_approval 回收。
const repairSetup = [
  `INSERT INTO tasks (workspace_key, workspace_path, task_id, created_at, updated_at) VALUES ('${W}', '${W}', 'op-task', 1, 2), ('${REMOTE}', '/srv/app', 'op-remote', 1, 2), ('${W}', '${W}', 'dead', 1, 2)`,
  `UPDATE tasks SET deleted = 1 WHERE task_id = 'dead'`,
  `INSERT INTO off_peak_tasks (off_peak_task_id, session_id, prompt, permission_mode, workspace_key, workspace_path, status, queued_at, created_at, updated_at) VALUES ('op-a', 'op-task', 'p', 'm', '${W}', '${W}', 'awaiting_approval', 1, 1, 1), ('op-b', 'op-remote', 'p', 'm', '${REMOTE}', '/srv/app', 'completed', 1, 1, 1)`,
  `INSERT INTO task_groups (group_id, title, created_at, updated_at) VALUES ('user-group', 'Mine', 1, 1)`,
  `INSERT INTO task_group_members (group_id, workspace_key, workspace_path, task_id, added_at, created_at, updated_at) VALUES ('user-group', '${W}', '${W}', 'dead', 1, 1, 1)`,
  `INSERT INTO task_group_view_node_orders (node_type, node_key, sort_order, created_at, updated_at) VALUES ('task', '["${W}","dead"]', 500, 1, 1), ('task', '${W}', 600, 1, 1), ('group', 'user-group', 700, 1, 1)`,
];

const REPAIR_TABLES = [
  "tasks",
  "task_groups",
  "task_group_members",
  "task_group_view_node_orders",
  "off_peak_tasks",
];

export const STARTUP_SCENARIOS = [
  {
    name: "fresh",
    base: "empty",
    setup: [],
    resetLedger: [],
    dump: ["sqlite_master", "tasks_schema_migration"],
  },
  {
    name: "legacy_upgrade",
    base: "empty",
    setup: legacySchema,
    resetLedger: [],
    dump: ["sqlite_master", "tasks_schema_migration", "tasks", "automations", "off_peak_tasks"],
  },
  {
    name: "provider_selection",
    base: "migrated",
    setup: [...legacyAutomations, ...glmSelections],
    resetLedger: ["0002_provider_selection", "0003_official_glm_selection"],
    dump: ["automations", "tasks_schema_migration"],
  },
  {
    name: "official_glm_selection",
    base: "migrated",
    setup: glmSelections,
    resetLedger: ["0003_official_glm_selection"],
    dump: ["automations", "tasks_schema_migration"],
  },
  { name: "repairs", base: "migrated", setup: repairSetup, resetLedger: [], dump: REPAIR_TABLES },
];

export const OPERATION_DUMP_TABLES = [
  "tasks",
  "task_groups",
  "task_group_members",
  "task_group_view_node_orders",
];
