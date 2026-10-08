// tasks 场景（docs/specs/rust-host-tasks.md §6）：右键归档 → 归档列表 → “删除全部已归档任务”。
// 删除走 zcode-task deleteArchivedTasks（逐项事务、整批一次广播），tasks-index 行写入删除墓碑，
// 归档列表随 task_deleted 事件刷新。
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";

import { sendMessage, sseText } from "../helpers.mjs";

const TITLE = "archive me please";
const REPLY = "Archive scenario reply";

export const mockRules = [
  // 标题生成请求（不带 tools）回固定标题，侧栏行与 tasks-index 的标题都等于 TITLE。
  {
    name: "title",
    match: ({ hasTools }) => !hasTools,
    respond: ({ response }) => response.end(sseText(TITLE)),
  },
  {
    name: "default",
    match: () => true,
    respond: ({ response }) => response.end(sseText(REPLY)),
  },
];

function taskRow(home) {
  const database = new DatabaseSync(join(home, ".zcode", "v2", "tasks-index.sqlite"), {
    readOnly: true,
  });
  try {
    return database
      .prepare("SELECT task_id, archived, deleted FROM tasks WHERE title = ?")
      .get(TITLE);
  } finally {
    database.close();
  }
}

async function waitForRow(home, predicate, what) {
  for (let attempt = 0; attempt < 60; attempt += 1) {
    const row = taskRow(home);
    if (row && predicate(row)) return row;
    await new Promise((done) => setTimeout(done, 500));
  }
  throw new Error(`tasks-index row never became ${what}: ${JSON.stringify(taskRow(home))}`);
}

export async function run({ page, shots, home }) {
  await sendMessage(page, TITLE);
  await page.getByText(REPLY).first().waitFor({ state: "visible", timeout: 60_000 });
  await waitForRow(home, (row) => row.archived === 0, "present");

  // 侧栏行在 DOM 里先于对话气泡出现。
  await page.getByText(TITLE, { exact: true }).first().click({ button: "right" });
  await page.getByRole("menuitem", { name: "Archive task" }).click();
  await waitForRow(home, (row) => row.archived === 1, "archived");

  await page.getByRole("button", { name: "Archived" }).first().click();
  const archivedRow = page.getByText(TITLE, { exact: true }).first();
  await archivedRow.click({ trial: true });
  await page.getByRole("button", { name: "Archive actions" }).first().click();
  await page.getByRole("menuitem").filter({ hasText: "Delete all archived tasks" }).first().click();
  const confirm = page.getByRole("button").filter({ hasText: "Delete all archived tasks" }).last();
  await confirm.click();
  const row = await waitForRow(home, (r) => r.deleted === 1, "deleted");
  console.info(`[e2e] archived task ${row.task_id} deleted through deleteArchivedTasks`);

  // 归档列表随 task_deleted 刷新：标题不再出现在侧栏。
  await page
    .waitForFunction(
      (title) =>
        ![...document.querySelectorAll("nav *, aside *")].some(
          (element) => element.childElementCount === 0 && element.textContent?.trim() === title,
        ),
      TITLE,
      { timeout: 30_000 },
    )
    .catch(async (error) => {
      await page.screenshot({ path: join(shots, "tasks-archive-failed.png") });
      throw error;
    });
  await page.screenshot({ path: join(shots, "tasks-archive.png") });
  console.info("[e2e] archived list refreshed after deletion");
}
