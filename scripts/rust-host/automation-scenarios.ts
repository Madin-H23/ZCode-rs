// Rust Host automation 的对拍输入（docs/specs/rust-host-automation.md §10），由
// automation-fixtures.ts 用真实 TS 实现执行。时间戳都是固定值；时区由生成脚本切换。

/** America/New_York：2026-03-08 02:00 EST 起夏令时，2026-11-01 02:00 EDT 结束。 */
export const DST_START = 1772953200000;
export const DST_END = 1793512800000;

export const ZONES = ["America/New_York", "Asia/Shanghai"] as const;

export const CRON_FROMS = [
  DST_START - 2 * 3600_000,
  DST_END - 3 * 3600_000,
  1791300000000, // 2026-10-06（周二）
  1835395200000, // 2028-02-28（闰年）
  1798761570000, // 2026-12-31 跨年前
  1772900000000,
];

export const CRON_EXPRESSIONS = [
  "* * * * *",
  "0 9 * * *",
  "*/15 * * * *",
  "*/7 * * * *",
  "0 0 L * *",
  "0 0 LW * *",
  "0 0 15W * *",
  "0 0 1W * *",
  "0 0 * * 5L",
  "0 0 * * 1#2",
  "0 0 * * 1#5",
  "0 0 1,15 * 1",
  "0 0 31 2 *",
  "30 2 * * *",
  "30 1 * * *",
  "0 0 29 2 *",
  "5 4 * * sun",
  "0 22 * * 1-5",
  "0 0 1 jan *",
  "0 9 * jan-mar mon-fri",
  "@daily",
  "@weekly",
  "@monthly",
  "@yearly",
  "0 0 0 1 1 * 2027",
  "0 0 0 1 1 * 2027,2030",
  "*/10 * * * * *",
  "30 0 9 * * *",
  "0 12 * * ?",
  "0 0 ? * 1",
  "0 0 1-31/2 * *",
  "0 0 * * +1#1",
  "0 0 13 * +5",
  "0 0 13 * 5",
  "0 0 * * 7",
  "0 0 * * 5-7",
  "0 9 8 3 *",
  "0 2 8 3 *",
  "5* * * * *",
  "0 0 1,L * *",
  "2026-10-10T10:00:00",
  "2026-10-10T10:00:00Z",
  "2026-12-31T23:59:59+08:00",
  "",
  "* * * *",
  "60 * * * *",
  "* 24 * * *",
  "0 0 32 * *",
  "0 0 * 13 *",
  "0 0 * * 8",
  "a b c d e",
  "5/10 * * * *",
  "1-2-3 * * * *",
  "* * * * * * * *",
  "@reboot",
  "0 0 L-5 * *",
  "0 0 * * 1#6",
  "0 0 * * 1#0",
  "12:30 * * * *",
  "0 0 * * L",
  "*/0 * * * *",
  "*/61 * * * *",
  "0 0 W * *",
  "0 0 1-5W * *",
  "0 0 0 1 1 * */5",
];

const ANCHOR = 1791300000000;

export const SCHEDULE_RULES: Record<string, unknown>[] = [
  { unit: "minute", interval: 10, hour: 0, minute: 0, anchorAt: ANCHOR },
  { unit: "hourly", interval: 5, hour: 0, minute: 30, anchorAt: ANCHOR },
  { unit: "daily", interval: 3, hour: 2, minute: 30, anchorAt: DST_START - 5 * 86400_000 },
  { unit: "daily", interval: 1, hour: 9, minute: 0, anchorAt: ANCHOR },
  { unit: "weekly", interval: 2, hour: 8, minute: 15, weekdays: [5, 1, 3], anchorAt: ANCHOR },
  { unit: "weekly", interval: 1, hour: 23, minute: 59, anchorAt: ANCHOR },
  {
    unit: "monthly",
    interval: 1,
    hour: 10,
    minute: 0,
    monthDays: [31, 15],
    monthlyMode: "date",
    anchorAt: ANCHOR,
  },
  {
    unit: "monthly",
    interval: 2,
    hour: 10,
    minute: 0,
    weekdays: [2],
    monthlyMode: "weekday",
    anchorAt: ANCHOR,
  },
  {
    unit: "yearly",
    interval: 1,
    hour: 0,
    minute: 0,
    months: [2],
    monthDays: [29],
    anchorAt: ANCHOR,
  },
  { unit: "yearly", interval: 2, hour: 6, minute: 0, anchorAt: ANCHOR },
];

export const SCHEDULE_FROMS = [ANCHOR - 1, ANCHOR, ANCHOR + 7 * 86400_000 + 1, DST_END + 3600_000];

export const INTERVAL_CARRIERS: [string, number, string][] = [
  ["minute", 45, "*/45 * * * *"],
  ["hourly", 50, "15 * * * *"],
  ["daily", 40, "30 9 * * *"],
  ["weekly", 3, "0 8 * * 1,3"],
  ["monthly", 13, "0 7 5 * *"],
  ["yearly", 2, "0 0 1 6 *"],
  ["daily", 2, "* * * * *"],
  ["weekly", 2, "0 0x10 * * 1,"],
];

/** 一次性固定月日 cron：刚错过 30 s（补执行）、刚错过 10 min（拒绝）、未来、40 min 前（下一年）。 */
export const INITIAL_CASES: { cronExpr: string; recurring: boolean; from: number }[] = [
  { cronExpr: "0 10 6 10 *", recurring: false, from: 1791301230000 },
  { cronExpr: "0 10 6 10 *", recurring: false, from: 1791301800000 },
  { cronExpr: "0 10 6 10 *", recurring: true, from: 1791301800000 },
  { cronExpr: "30 12 6 10 *", recurring: false, from: 1791301800000 },
  { cronExpr: "0 9 6 10 *", recurring: false, from: 1791301800000 },
];

export const SERVICE_NOW = 1791300000000;
export const WS = { workspacePath: "/tmp/ws-a" };
export const WS_B = { workspacePath: "/tmp/ws-b", workspaceIdentity: " id-b " };

type Op = Record<string, unknown>;

/** service 级场景：每步 `op` 对应 AutomationService / AutomationRepo 的一个方法。 */
export const SERVICE_SCENARIOS: { name: string; ops: Op[] }[] = [
  {
    name: "create-validate-update",
    ops: [
      {
        op: "create",
        params: { ...WS, title: "daily", cronExpr: "0 9 * * *", prompt: "p", recurring: true },
      },
      {
        op: "create",
        params: {
          ...WS,
          title: "sel",
          cronExpr: "*/10 * * * *",
          prompt: "p",
          recurring: true,
          mode: "edit",
          modelSelection: {
            providerId: " zai ",
            modelId: "glm-5",
            options: { reasoningLevel: "high" },
          },
        },
      },
      {
        op: "create",
        params: { ...WS, title: "delay", cronExpr: "* * * * *", prompt: "p", relativeDelayMinutes: 90 },
      },
      {
        op: "create",
        params: {
          ...WS,
          title: "carrier",
          cronExpr: "0 9 * * *",
          prompt: "p",
          recurring: true,
          intervalUnit: "daily",
          interval: 40,
        },
      },
      {
        op: "create",
        params: {
          ...WS_B,
          title: "rule",
          cronExpr: "0 9 * * 1",
          prompt: "p",
          recurring: true,
          scheduleRule: { minute: 0, unit: "weekly", interval: 2, hour: 9, weekdays: [1] },
          endAt: SERVICE_NOW + 3 * 86400_000,
        },
      },
      {
        op: "create",
        params: { ...WS, title: "once", cronExpr: "0 10 6 10 *", prompt: "p", recurring: false },
        now: 1791301800000,
      },
      { op: "create", params: { ...WS, title: "bad", cronExpr: "61 * * * *", prompt: "p" } },
      {
        op: "create",
        params: { ...WS, title: "bad-mode", cronExpr: "0 9 * * *", prompt: "p", mode: "turbo" },
      },
      {
        op: "create",
        params: {
          ...WS,
          title: "bad-delay",
          cronExpr: "* * * * *",
          prompt: "p",
          relativeDelayMinutes: 5,
          recurring: true,
        },
      },
      {
        op: "create",
        params: {
          ...WS,
          title: "bad-rule",
          cronExpr: "0 9 * * *",
          prompt: "p",
          scheduleRule: { unit: "monthly", interval: 1, hour: 9, minute: 0, anchorAt: 1 },
        },
      },
      {
        op: "create",
        params: {
          ...WS,
          title: "bad-carrier",
          cronExpr: "0 9 * * *",
          prompt: "p",
          intervalUnit: "daily",
          interval: 201,
        },
      },
      { op: "list" },
      { op: "list", scope: WS },
      { op: "list", scope: WS_B },
      { op: "update", id: 0, patch: { title: "renamed", cronExpr: "0 10 * * *" }, now: SERVICE_NOW + 60_000 },
      { op: "update", id: 0, patch: { maxRuns: null } },
      { op: "update", id: 0, patch: { recurring: true, maxRuns: 3 } },
      { op: "update", id: 0, patch: { recurring: false, maxRuns: 2 }, now: SERVICE_NOW + 120_000 },
      { op: "update", id: 0, patch: { cronExpr: "*/5 * * * *" }, now: SERVICE_NOW + 180_000 },
      { op: "update", id: 1, patch: { modelSelection: null, mode: null }, now: SERVICE_NOW + 240_000 },
      {
        op: "update",
        id: 4,
        scope: WS_B,
        patch: { scheduleRule: { unit: "weekly", interval: 2, hour: 9, minute: 0, weekdays: [1] } },
        now: SERVICE_NOW + 300_000,
      },
      {
        op: "update",
        id: 4,
        scope: WS_B,
        patch: { endAt: null, scheduleEditedByUser: true },
        now: SERVICE_NOW + 360_000,
      },
      { op: "update", id: 4, scope: WS, patch: { title: "cross" } },
      { op: "update", id: 0, patch: { intervalUnit: "hourly", interval: 30 }, now: SERVICE_NOW + 420_000 },
      { op: "update", id: 0, patch: { scheduleRule: null }, now: SERVICE_NOW + 480_000 },
      { op: "update", id: 2, patch: { endAt: SERVICE_NOW }, now: SERVICE_NOW + 540_000 },
      { op: "setEnabled", id: 1, enabled: false },
      { op: "setEnabled", id: 1, enabled: true, now: SERVICE_NOW + 600_000 },
      { op: "restart", id: 1 },
      { op: "sql", sql: "UPDATE automations SET lifecycle_status = 'failed', run_count = 4" },
      { op: "restart", id: 1, now: SERVICE_NOW + 660_000 },
      { op: "hasTaskBinding", scope: WS, targetTaskId: "task-1" },
      { op: "list", scope: WS },
      { op: "delete", id: 3, scope: WS_B },
      { op: "delete", id: 3, scope: WS },
      { op: "list" },
    ],
  },
  {
    name: "runs",
    ops: [
      {
        op: "create",
        params: {
          ...WS,
          title: "r",
          cronExpr: "0 9 * * *",
          prompt: "p",
          recurring: true,
          targetTaskId: "task-1",
          botDeliveryTarget: {
            provider: "feishu",
            botId: " bot ",
            providerUserId: "u1",
            chatType: "private",
          },
        },
      },
      { op: "hasTaskBinding", scope: WS, targetTaskId: "task-1" },
      { op: "getBotDeliveryTarget", id: 0 },
      { op: "getModelSelectionForDispatch", id: 0, scope: WS },
      { op: "runNow", id: 0, scope: WS, now: SERVICE_NOW + 1000 },
      { op: "runNow", id: 0, scope: WS, now: SERVICE_NOW + 2000 },
      {
        op: "fixRunModelSelection",
        run: 0,
        selection: { providerId: "zai", modelId: "glm-5", options: { reasoningLevel: "max" } },
      },
      {
        op: "fixRunModelSelection",
        run: 0,
        selection: { providerId: "other", modelId: "x", options: {} },
      },
      { op: "markRunOutcome", run: 0, outcome: "running" },
      { op: "markManualRunDispatched", run: 0, sessionId: "task-1", dispatchedAt: SERVICE_NOW + 3000 },
      { op: "markManualRunDispatched", run: 0, sessionId: "task-1", dispatchedAt: SERVICE_NOW + 4000 },
      { op: "touchManualClaim", id: 0, now: SERVICE_NOW + 5000 },
      { op: "markRunOutcome", run: 0, outcome: "succeeded" },
      { op: "markRunOutcome", run: 0, outcome: "running" },
      { op: "releaseManualClaim", id: 0 },
      {
        op: "ensureRunClaimed",
        runId: "#0:1791400000000",
        id: 0,
        scheduledAt: 1791400000000,
        trigger: "schedule",
      },
      { op: "markRunOutcome", runId: "#0:1791400000000", outcome: "failed", error: "boom" },
      { op: "markRunDispatch", runId: "#0:1791400000000", status: "failed_to_dispatch", error: "x" },
      { op: "runNow", id: 0, scope: WS_B, now: SERVICE_NOW + 6000 },
      { op: "sql", sql: "UPDATE automations SET running = 1, claimed_at = 1" },
      { op: "runNow", id: 0, scope: WS, now: SERVICE_NOW + 7000 },
      { op: "listRuns", id: 0, scope: WS },
      { op: "deleteRun", run: 0, scope: WS_B },
      { op: "deleteRun", run: 0, scope: WS },
      { op: "listRuns", id: 0 },
      { op: "getRun", runId: "#0:1791400000000" },
      {
        op: "sql",
        sql: "UPDATE automations SET model_selection = NULL, mode = '', schedule_rule = NULL",
      },
      { op: "getModelSelectionForDispatch", id: 0, scope: WS },
      { op: "list" },
    ],
  },
];
