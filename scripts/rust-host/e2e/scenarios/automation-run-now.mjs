// automation：在设置页手动创建定时任务，打开它点“立即运行”；Rust Host 直接派发 manual run，
// mock 模型收到 prompt，运行结束后 automation_runs 记 succeeded，承载会话带 cron 标记。
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";

import { composer, sseText } from "../helpers.mjs";

const PROMPT = "automation ping from e2e";
const REPLY = "Automation run finished";

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

function query(home, sql) {
  const database = new DatabaseSync(join(home, ".zcode", "v2", "tasks-index.sqlite"), {
    readOnly: true,
  });
  try {
    return database.prepare(sql).all();
  } finally {
    database.close();
  }
}

async function waitFor(description, check, timeout = 60_000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${description}`);
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
}

export async function run({ page, shots, model, home }) {
  await composer(page);
  await page.getByTestId("automations-open").click();
  await page.getByTestId("automation-create-manually").click();
  await page.screenshot({ path: join(shots, "automation-01-form.png") });

  await page.getByTestId("automation-form-title").fill("E2E automation");
  await page.getByTestId("automation-form-prompt").fill(PROMPT);
  // 新建表单没有默认计划：选“每天”。
  await page.getByTestId("automation-schedule-add").click();
  await page.getByTestId("automation-frequency-option-daily").click();
  const submit = page.getByTestId("automation-form-submit");
  await waitFor("create button enabled", () => submit.isEnabled());
  await submit.click();

  const card = page.getByTestId("automation-card").filter({ hasText: "E2E automation" });
  await card.first().waitFor({ state: "visible", timeout: 30_000 });
  await page.screenshot({ path: join(shots, "automation-02-list.png") });
  const [created] = query(home, "SELECT * FROM automations");
  if (!created || created.title !== "E2E automation" || created.lifecycle_status !== "active")
    throw new Error(`unexpected automation row: ${JSON.stringify(created)}`);
  if (!created.next_run_at) throw new Error("automation has no next_run_at");
  console.info(`[e2e] automation created; next run at ${new Date(created.next_run_at).toISOString()}`);

  await card.first().click();
  const runNow = page.getByTestId("automation-run-now");
  await runNow.click({ trial: true });
  await waitFor("run-now enabled", () => runNow.isEnabled());
  await runNow.click();

  await waitFor("mock model to receive the automation prompt", () =>
    model.requests.some((request) => request.body.includes(PROMPT)),
  );
  const run = await waitFor("manual run to succeed", () => {
    const runs = query(home, "SELECT * FROM automation_runs WHERE trigger = 'manual'");
    return runs.find((row) => row.outcome === "succeeded");
  });
  await page.screenshot({ path: join(shots, "automation-03-after-run.png") });
  if (run.dispatch_status !== "dispatched" || !run.session_id || !run.model_selection)
    throw new Error(`unexpected run row: ${JSON.stringify(run)}`);
  const [automation] = query(home, "SELECT * FROM automations");
  if (automation.run_count !== 1 || automation.running !== 0)
    throw new Error(`manual run not settled on the automation: ${JSON.stringify(automation)}`);
  const [task] = query(
    home,
    `SELECT task_id, cron_automation_id, unread_at FROM tasks WHERE task_id = '${run.session_id}'`,
  );
  if (!task || task.cron_automation_id !== automation.automation_id)
    throw new Error(`run session not stamped with the automation: ${JSON.stringify(task)}`);
  if (!task.unread_at) throw new Error("background run did not mark the session unread");
  console.info(`[e2e] manual run succeeded in session ${run.session_id}`);
}
