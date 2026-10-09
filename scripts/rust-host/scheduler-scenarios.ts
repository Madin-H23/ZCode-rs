/* eslint-disable max-lines -- 对拍的操作序列集中在一处，拆开后 fixture 与 Rust 测试更难逐步对照。 */
// Rust Host 调度状态机的对拍输入（docs/specs/rust-u4-app-host.md §2.4、§6），由 scheduler-fixtures.ts
// 用真实 TS 实现执行。时间戳都是固定值。
type Op = Record<string, any>;

export const BASE = 1791300000000;
const STALE = 10 * 60_000;

const SELECTION = '{"providerId":"p","modelId":"m","options":{"reasoningLevel":"high"}}';

/** automations 的一行；未给的列用表默认值。 */
function automation(id: string, columns: Record<string, string | number | null>): Op {
  const row: Record<string, string | number | null> = {
    automation_id: id,
    title: id,
    cron_expr: "*/5 * * * *",
    prompt: `run ${id}`,
    model_selection: "null",
    workspace_key: "/tmp/ws-a",
    workspace_path: "/tmp/ws-a",
    created_at: 1,
    updated_at: 1,
    ...columns,
  };
  const names = Object.keys(row);
  const values = names.map((name) => {
    const value = row[name];
    return value === null ? "NULL" : typeof value === "number" ? String(value) : `'${value}'`;
  });
  return { op: "sql", sql: `INSERT INTO automations (${names}) VALUES (${values})` };
}

function manualRun(runId: string, automationId: string, attempts: number, updatedAt: number): Op {
  return {
    op: "sql",
    sql: `INSERT INTO automation_runs (run_id, automation_id, workspace_key, scheduled_at, trigger, dispatch_status, attempts, created_at, updated_at) VALUES ('${runId}', '${automationId}', '/tmp/ws-a', ${updatedAt}, 'manual', 'claimed', ${attempts}, ${updatedAt}, ${updatedAt})`,
  };
}

function offPeak(id: string, columns: Record<string, string | number | null>): Op {
  const row: Record<string, string | number | null> = {
    off_peak_task_id: id,
    server_ticket_id: `ticket-${id}`,
    title: id,
    prompt: `do ${id}`,
    permission_mode: "yolo",
    model_selection: SELECTION,
    workspace_key: "/tmp/ws-a",
    workspace_path: "/tmp/ws-a",
    status: "queued",
    queued_at: 100,
    schedulable: 1,
    created_at: 100,
    updated_at: 100,
    ...columns,
  };
  const names = Object.keys(row);
  const values = names.map((name) => {
    const value = row[name];
    return value === null ? "NULL" : typeof value === "number" ? String(value) : `'${value}'`;
  });
  return { op: "sql", sql: `INSERT INTO off_peak_tasks (${names}) VALUES (${values})` };
}

const run = (automationId: string, scheduledAt: number) => ({
  runId: `${automationId}:${scheduledAt}`,
  automationId,
  workspaceKey: "/tmp/ws-a",
  scheduledAt,
  trigger: "schedule",
});

export const OPS: Op[] = [
  // ── automation 认领 ──
  automation("a-due", { next_run_at: BASE - 1000, created_at: 1 }),
  automation("a-future", { next_run_at: BASE + 60_000, created_at: 2 }),
  automation("a-retry-wait", {
    next_run_at: BASE - 120_000,
    retry_at: BASE + 10_000,
    dispatch_attempts: 2,
    dispatch_status: "failed_to_dispatch",
    created_at: 3,
  }),
  automation("a-retry-due", {
    next_run_at: BASE - 600_000,
    retry_at: BASE - 1,
    dispatch_attempts: 1,
    dispatch_status: "failed_to_dispatch",
    created_at: 4,
  }),
  automation("a-disabled", {
    next_run_at: BASE - 1000,
    enabled: 0,
    lifecycle_status: "paused",
    created_at: 5,
  }),
  automation("a-foreign", {
    next_run_at: BASE - 1000,
    running: 1,
    claimed_at: BASE - 60_000,
    dispatch_status: "claimed",
    created_at: 6,
  }),
  automation("a-stale", {
    next_run_at: BASE - 2000,
    running: 1,
    claimed_at: BASE - STALE - 1,
    dispatch_status: "claimed",
    created_at: 7,
  }),
  automation("a-expired", { next_run_at: BASE - 1000, end_at: BASE - 1, created_at: 8 }),
  automation("a-once", {
    next_run_at: BASE - 500,
    recurring: 0,
    schedule_rule: '{"unit":"minute","interval":5,"hour":0,"minute":0,"anchorAt":1791299700000}',
    target_task_id: "task-1",
    mode: "plan",
    workspace_identity: " id-a ",
    created_at: 9,
  }),
  { op: "claimDue", now: BASE },
  // 第二个调度器（另一条连接或另一个 App 实例）随后认领：同一批任务不会再被认领。
  { op: "claimDue", now: BASE },
  { op: "upsertRunClaimed", run: run("a-due", BASE - 1000), now: BASE + 1 },
  // 同一 runId 再次认领（重试）：attempts + 1，model_selection 只在为空时写入。
  {
    op: "upsertRunClaimed",
    run: run("a-due", BASE - 1000),
    modelSelection: { providerId: " p2 ", modelId: "m2", options: {} },
    now: BASE + 2,
  },
  {
    op: "upsertRunClaimed",
    run: run("a-due", BASE - 1000),
    modelSelection: { providerId: "p3", modelId: "m3" },
    now: BASE + 3,
  },
  { op: "getRun", runId: `a-due:${BASE - 1000}` },
  { op: "markRunDispatch", runId: `a-due:${BASE - 1000}`, status: "dispatched", sessionId: "s-1" },
  { op: "markDispatched", id: "a-due", dispatchedAt: BASE + 5, nextRunAt: BASE + 300_000 },
  { op: "upsertRunClaimed", run: run("a-retry-due", BASE - 600_000), now: BASE + 6 },
  {
    op: "markRunDispatch",
    runId: `a-retry-due:${BASE - 600_000}`,
    status: "failed_to_dispatch",
    error: "no model",
    now: BASE + 7,
  },
  {
    op: "markDispatchFailed",
    id: "a-retry-due",
    failedAt: BASE + 8,
    error: "no model",
    kind: "transient",
    nextRunAt: BASE + 300_000,
  },
  {
    op: "markDispatchFailed",
    id: "a-stale",
    failedAt: BASE + 9,
    error: "fatal",
    kind: "permanent",
    nextRunAt: null,
  },
  // 一次性任务派发成功即 completed（scheduled_run_count 达到默认上限 1）。
  { op: "markDispatched", id: "a-once", dispatchedAt: BASE + 10, nextRunAt: BASE + 600_000 },
  { op: "markDispatched", id: "missing", dispatchedAt: BASE + 10, nextRunAt: null },
  { op: "markDispatchFailed", id: "missing", failedAt: BASE + 10, error: "x", kind: "transient" },
  { op: "releaseClaim", id: "a-foreign", now: BASE + 11 },
  { op: "releaseClaim", id: "a-future", now: BASE + 12 },
  {
    op: "skipAndReschedule",
    params: {
      automationId: "a-future",
      runId: `a-future:${BASE - 400_000}`,
      workspaceKey: "/tmp/ws-a",
      scheduledAt: BASE - 400_000,
      reason: "computer_asleep_or_app_not_running",
      nextRunAt: BASE + 120_000,
    },
    now: BASE + 13,
  },
  // 同一 runId 再记一次：ON CONFLICT 更新原行。
  {
    op: "skipAndReschedule",
    params: {
      automationId: "a-future",
      runId: `a-future:${BASE - 400_000}`,
      workspaceKey: "/tmp/ws-a",
      scheduledAt: BASE - 400_000,
      reason: "again",
      nextRunAt: BASE + 180_000,
    },
    now: BASE + 14,
  },
  automation("a-missed-once", {
    next_run_at: BASE - 900_000,
    recurring: 0,
    max_runs: 1,
    created_at: 10,
  }),
  {
    op: "skipAndReschedule",
    params: {
      automationId: "a-missed-once",
      runId: `a-missed-once:${BASE - 900_000}`,
      workspaceKey: "/tmp/ws-a",
      scheduledAt: BASE - 900_000,
      reason: "computer_asleep_or_app_not_running",
      nextRunAt: null,
      finalize: true,
    },
    now: BASE + 15,
  },
  // ── 退避上限 ──
  automation("a-limit-recurring", {
    next_run_at: BASE - 1000,
    dispatch_attempts: 4,
    retry_at: BASE - 1,
    running: 1,
    claimed_at: BASE,
    created_at: 11,
  }),
  automation("a-limit-once", {
    next_run_at: BASE - 1000,
    recurring: 0,
    max_runs: 3,
    dispatch_attempts: 4,
    retry_at: BASE - 1,
    running: 1,
    claimed_at: BASE,
    created_at: 12,
  }),
  automation("a-attempt-4", {
    next_run_at: BASE - 1000,
    dispatch_attempts: 3,
    created_at: 13,
  }),
  {
    op: "markDispatchFailed",
    id: "a-limit-recurring",
    failedAt: BASE + 16,
    error: "e5",
    kind: "transient",
    nextRunAt: BASE + 240_000,
  },
  {
    op: "markDispatchFailed",
    id: "a-limit-once",
    failedAt: BASE + 17,
    error: "e5",
    kind: "transient",
    nextRunAt: BASE + 240_000,
  },
  {
    op: "markDispatchFailed",
    id: "a-attempt-4",
    failedAt: BASE + 18,
    error: "e4",
    kind: "transient",
  },
  // ── end_at：下一次越过截止即 completed；有限次任务未达上限仍 active ──
  automation("a-end", { next_run_at: BASE - 1000, end_at: BASE + 100_000, created_at: 14 }),
  automation("a-max3", {
    next_run_at: BASE - 1000,
    recurring: 0,
    max_runs: 3,
    scheduled_run_count: 1,
    run_count: 4,
    created_at: 15,
  }),
  { op: "markDispatched", id: "a-end", dispatchedAt: BASE + 19, nextRunAt: BASE + 200_000 },
  { op: "markDispatched", id: "a-max3", dispatchedAt: BASE + 20, nextRunAt: BASE + 50_000 },
  // ── manual run ──
  automation("a-manual", { next_run_at: BASE + 900_000, created_at: 16 }),
  automation("a-manual-stale", { next_run_at: BASE + 900_000, created_at: 17 }),
  automation("a-busy", {
    next_run_at: BASE + 900_000,
    running: 1,
    claimed_at: BASE - 1000,
    created_at: 18,
  }),
  automation("a-manual-twice", {
    next_run_at: BASE + 900_000,
    model_selection: SELECTION,
    created_at: 19,
  }),
  manualRun("a-manual:manual:1", "a-manual", 0, BASE - 50),
  manualRun("a-manual:manual:held", "a-manual", 1, BASE - 40),
  manualRun("a-manual-stale:manual:1", "a-manual-stale", 1, BASE - STALE - 5),
  manualRun("a-busy:manual:1", "a-busy", 0, BASE - 30),
  manualRun("a-manual-twice:manual:1", "a-manual-twice", 0, BASE - 20),
  manualRun("a-manual-twice:manual:2", "a-manual-twice", 0, BASE - 10),
  { op: "claimManualRuns", now: BASE + 100 },
  { op: "claimManualRuns", now: BASE + 100 },
  {
    op: "markManualRunDispatched",
    runId: "a-manual:manual:1",
    sessionId: "s-m1",
    dispatchedAt: BASE + 101,
  },
  { op: "settleManualClaim", id: "a-manual", runId: "a-manual:manual:1", ok: true },
  {
    op: "markRunDispatch",
    runId: "a-manual-stale:manual:1",
    status: "failed_to_dispatch",
    error: "boom",
    now: BASE + 102,
  },
  // 在途上下文丢失（迟到回报）时用 run 台账兜回 workspaceKey 释放锁。
  {
    op: "settleManualClaim",
    id: "a-manual-stale",
    runId: "a-manual-stale:manual:1",
    ok: false,
    now: BASE + 103,
  },
  {
    op: "settleManualClaim",
    id: "a-manual-twice",
    runId: "missing:manual:x",
    ok: false,
    now: BASE + 104,
  },
  {
    op: "settleManualClaim",
    id: "missing",
    runId: "missing:manual:y",
    ok: false,
    now: BASE + 105,
  },
  // 锁释放后，同一 automation 排在后面的 manual run 可以被认领。
  { op: "claimManualRuns", now: BASE + 106 },
  // ── 闲时任务 ──
  offPeak("o-run", { status: "running", started_at: 50, conversation_id: "c-run", queued_at: 1 }),
  offPeak("o-q1", { queued_at: 10, created_at: 10 }),
  offPeak("o-q2", { queued_at: 5, created_at: 20 }),
  offPeak("o-q2b", { queued_at: 5, created_at: 15, workspace_identity: "id-x" }),
  offPeak("o-hidden", { schedulable: 0 }),
  offPeak("o-bad", { model_selection: null, queued_at: 2 }),
  offPeak("o-foreign", { claim_running: 1, claimed_at: BASE - 1000, queued_at: 3 }),
  offPeak("o-stale", { claim_running: 1, claimed_at: BASE - STALE - 1, queued_at: 4 }),
  offPeak("o-paused", { status: "paused" }),
  { op: "countActive" },
  { op: "recoverInterrupted", now: BASE },
  { op: "recoverInterrupted", now: BASE + 1 },
  { op: "countActive" },
  { op: "offPeakClaimDue", now: BASE + 2 },
  { op: "offPeakClaimDue", now: BASE + 2 },
  {
    op: "markRunning",
    id: "o-q2",
    update: { startedAt: BASE + 3, conversationId: "c-2", sessionId: "c-2" },
  },
  { op: "markRunning", id: "o-q2", update: { startedAt: BASE + 4 } },
  {
    op: "markRunning",
    id: "o-run",
    update: { startedAt: BASE + 5, serverTicketId: "ticket-new" },
  },
  { op: "releaseOffPeakClaim", id: "o-q1", error: "boom", now: BASE + 6 },
  { op: "releaseOffPeakClaim", id: "o-q1", error: "again", now: BASE + 7 },
  { op: "releaseOffPeakClaim", id: "o-q2b", now: BASE + 8 },
  { op: "releaseOffPeakClaim", id: "o-stale", error: "", now: BASE + 9 },
  { op: "countActive" },
  // 结算（offPeakDispatchSettlement.ts）：先把目标重新置为在途认领。
  {
    op: "sql",
    sql: `UPDATE off_peak_tasks SET claim_running = 1, claimed_at = ${BASE + 10} WHERE off_peak_task_id IN ('o-q1', 'o-q2b', 'o-stale', 'o-hidden')`,
  },
  {
    op: "settleOffPeak",
    id: "o-q1",
    ok: false,
    error: "gateway busy",
    failureKind: "transient",
    now: BASE + 11,
  },
  { op: "sql", sql: "UPDATE off_peak_tasks SET claim_running = 1 WHERE off_peak_task_id = 'o-q1'" },
  {
    op: "settleOffPeak",
    id: "o-q1",
    ok: false,
    error: "gateway busy",
    failureKind: "transient",
    now: BASE + 12,
  },
  { op: "sql", sql: "UPDATE off_peak_tasks SET claim_running = 1 WHERE off_peak_task_id = 'o-q1'" },
  { op: "settleOffPeak", id: "o-q1", ok: true, sessionId: "s-q1", now: BASE + 13 },
  {
    op: "settleOffPeak",
    id: "o-q2b",
    ok: false,
    error: "no credentials",
    failureKind: "permanent",
    now: BASE + 14,
  },
  {
    op: "settleOffPeak",
    id: "o-q2b",
    ok: false,
    error: "late",
    failureKind: "permanent",
    now: BASE + 15,
  },
  { op: "settleOffPeak", id: "o-q2b", ok: true, sessionId: "s-late", now: BASE + 16 },
  { op: "settleOffPeak", id: "o-stale", ok: true, sessionId: "", now: BASE + 17 },
  {
    op: "settleOffPeak",
    id: "o-hidden",
    ok: false,
    error: "e",
    failureKind: "transient",
    now: BASE + 18,
  },
  { op: "countActive" },
];
