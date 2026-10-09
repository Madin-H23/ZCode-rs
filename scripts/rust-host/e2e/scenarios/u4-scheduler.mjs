// U4：调度在 App 级 Rust Host 内（docs/specs/rust-u4-app-host.md §2.4、§6）。
//
// 1. 在界面上创建一个“每 1 分钟”的 automation（自定义重复），不点“立即运行”：它在下一个整分钟
//    到期，由 Host 内的 Scheduler（20 s 一轮）认领、派发、结算；mock 模型收到 prompt，
//    automation_runs 里 trigger=schedule 的运行记 succeeded。
// 2. 销毁所有主窗口（macOS 上 App 继续运行）：Host 不退出；下一分钟的运行照样派发并结算，
//    派发时没有任何窗口（Renderer 连接）。
// 3. 像点 Dock 图标一样重新打开主窗口：接入同一个 Host，在 Automations 里看到这个任务；最后停用它，
//    避免退出时还有运行在途。
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";

import { composer, sseText } from "../helpers.mjs";
import { DESTROY_ALL_MAIN_WINDOWS, REOPEN_MAIN_WINDOW } from "../main-process.mjs";
import { hostsOf, waitFor, waitUsable } from "../resilience-helpers.mjs";

const PROMPT = "scheduled ping from e2e";
const REPLY = "Scheduled run finished";
const TITLE = "E2E every minute";

export const mockRules = [
  {
    name: "automation",
    match: ({ text }) => text.includes(PROMPT),
    respond: ({ response }) => response.end(sseText(REPLY)),
  },
  {
    name: "default",
    match: () => true,
    respond: ({ response }) => response.end(sseText("ok")),
  },
];

function query(home, sql, ...params) {
  const database = new DatabaseSync(join(home, ".zcode", "v2", "tasks-index.sqlite"), {
    readOnly: true,
  });
  try {
    return database.prepare(sql).all(...params);
  } finally {
    database.close();
  }
}

const scheduledRuns = (home) =>
  query(home, "SELECT * FROM automation_runs WHERE trigger = 'schedule' ORDER BY created_at");

async function onlyHost(mainPid) {
  const hosts = await hostsOf(mainPid);
  if (hosts.length !== 1) throw new Error(`expected one Rust Host, found ${hosts.length}`);
  return hosts[0].pid;
}

async function createEveryMinuteAutomation(page, shots) {
  await composer(page);
  await page.getByTestId("automations-open").click();
  await page.getByTestId("automation-create-manually").click();
  await page.getByTestId("automation-form-title").fill(TITLE);
  await page.getByTestId("automation-form-prompt").fill(PROMPT);
  await page.getByTestId("automation-schedule-add").click();
  await page.getByTestId("automation-frequency-option-custom").click();
  // 自定义重复：每 1 分钟。
  const interval = page.getByTestId("automation-custom-interval-select");
  await interval.fill("1");
  await page.getByTestId("automation-custom-unit-select").click();
  await page.getByTestId("automation-custom-unit-option-minute").click();
  await page.screenshot({ path: join(shots, "01-custom-repeat.png") });
  await page.getByTestId("automation-custom-confirm").click();
  const submit = page.getByTestId("automation-form-submit");
  await waitFor(() => submit.isEnabled(), { what: "create button enabled" });
  await submit.click();
  await page
    .getByTestId("automation-card")
    .filter({ hasText: TITLE })
    .first()
    .waitFor({ state: "visible", timeout: 30_000 });
  await page.screenshot({ path: join(shots, "02-created.png") });
}

/** 等一次 trigger=schedule 的运行成功，且是在 `after` 之后创建的；返回它。 */
async function nextSucceededScheduledRun(home, after, what) {
  // 下一个整分钟 + 一轮轮询（20 s）+ 派发与回复；负载高时留足余量。
  return waitFor(
    () => scheduledRuns(home).find((row) => row.created_at > after && row.outcome === "succeeded"),
    { timeout: 150_000, interval: 1_000, what },
  );
}

export async function run(context) {
  const { page, shots, home, model, mainPid } = context;
  await waitUsable(page);
  const host = await onlyHost(mainPid());
  await createEveryMinuteAutomation(page, shots);
  const [created] = query(home, "SELECT * FROM automations");
  if (!created || created.title !== TITLE) throw new Error(`unexpected automation: ${created}`);
  if (!created.next_run_at || created.next_run_at - Date.now() > 61_000)
    throw new Error(`the first run is not within a minute: ${created.next_run_at}`);
  console.info(
    `[e2e] every-minute automation created; next run at ${new Date(created.next_run_at).toISOString()}`,
  );

  // 1. 到点运行（没有点“立即运行”）。
  const createdAt = Date.now();
  const first = await nextSucceededScheduledRun(home, 0, "the first scheduled run to succeed");
  if (first.dispatch_status !== "dispatched" || !first.session_id)
    throw new Error(`unexpected scheduled run: ${JSON.stringify(first)}`);
  if (!model.requests.some((request) => request.body.includes(PROMPT)))
    throw new Error("the mock model never received the scheduled prompt");
  const firedAfterMs = first.created_at - created.next_run_at;
  if (first.scheduled_at !== created.next_run_at)
    throw new Error(
      `the run is not the scheduled occurrence: scheduled_at=${first.scheduled_at}, next_run_at=${created.next_run_at}`,
    );
  console.info(
    `[e2e] scheduled run ${first.run_id} claimed ${firedAfterMs}ms after it was due and succeeded (session ${first.session_id}); waited ${Date.now() - createdAt}ms`,
  );
  await page.screenshot({ path: join(shots, "03-first-run.png") });

  // 2. 没有任何主窗口：Host 继续运行，下一次运行照样派发并结算。
  const destroyed = await context.main().evaluate(DESTROY_ALL_MAIN_WINDOWS);
  if (destroyed < 1) throw new Error("no main window was destroyed");
  const windowsGoneAt = Date.now();
  await waitFor(async () => context.pages().length === 0, { what: "all main windows to close" });
  if ((await onlyHost(mainPid())) !== host)
    throw new Error("the Host changed after the windows closed");
  const requestsBefore = model.requests.length;
  const headless = await nextSucceededScheduledRun(
    home,
    windowsGoneAt,
    "a scheduled run with no window to succeed",
  );
  if (model.requests.length <= requestsBefore)
    throw new Error("the mock model got no request while no window existed");
  if ((await onlyHost(mainPid())) !== host) throw new Error("the Host restarted without windows");
  if (context.pages().length !== 0) throw new Error("a window appeared during the headless run");
  const [automation] = query(home, "SELECT * FROM automations");
  if (automation.scheduled_run_count < 2)
    throw new Error(`scheduled runs not settled on the automation: ${JSON.stringify(automation)}`);
  console.info(
    `[e2e] with no window the Host (pid=${host}) dispatched and settled run ${headless.run_id}; scheduled_run_count=${automation.scheduled_run_count}`,
  );

  // 3. 重新打开主窗口（与点 Dock 图标相同）：接入同一个 Host，任务列表里能看到这个 automation。
  await context.main().evaluate(REOPEN_MAIN_WINDOW);
  const [reopened] = await waitFor(
    async () => {
      const pages = context.pages();
      return pages.length > 0 ? pages : null;
    },
    { timeout: 60_000, what: "a reopened main window" },
  );
  await waitUsable(reopened);
  if ((await onlyHost(mainPid())) !== host)
    throw new Error("reopening a window restarted the Host");
  await reopened.getByTestId("automations-open").click();
  const card = reopened.getByTestId("automation-card").filter({ hasText: TITLE }).first();
  await card.waitFor({ state: "visible", timeout: 30_000 });
  await reopened.screenshot({ path: join(shots, "04-reopened.png") });
  // 停用（卡片菜单 → 暂停）：退出时不再有运行到期。
  await card.getByTestId("automation-card-menu").first().click();
  await reopened.getByTestId("automation-action-toggle").first().click();
  await waitFor(() => query(home, "SELECT enabled FROM automations")[0]?.enabled === 0, {
    what: "the automation to pause",
  });
  // 已认领的运行（若有）结束后再退出，避免退出确认框。
  await waitFor(
    () => scheduledRuns(home).every((row) => row.outcome !== null && row.outcome !== undefined),
    { timeout: 90_000, interval: 1_000, what: "in-flight scheduled runs to settle" },
  );
  console.info("[e2e] reopened window attached to the same Host and paused the automation");
}
