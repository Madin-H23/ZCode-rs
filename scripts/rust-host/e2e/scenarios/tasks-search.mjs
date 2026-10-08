// tasks 场景（docs/specs/rust-host-tasks.md §6）：命令中心的任务搜索走 window-controller →
// zcode-task listTaskList 全文检索。只在回复正文里出现的词也能搜到，并带回正文摘要；
// R2 的“标题包含”退化实现搜不到它。
import { join } from "node:path";

import { sendMessage, sseText } from "../helpers.mjs";

const TITLE = "remember the zebra";
const REPLY = "The quokka answer lives in the body";

export const mockRules = [
  {
    name: "default",
    match: () => true,
    respond: ({ response }) => response.end(sseText(REPLY)),
  },
];

async function dialogText(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('[role="dialog"]')].map((el) => el.textContent ?? "").join("\n"),
  );
}

export async function run({ page, shots }) {
  await sendMessage(page, TITLE);
  await page.getByText(REPLY).first().waitFor({ state: "visible", timeout: 60_000 });
  // 等侧栏出现这条任务（对话气泡一处、侧栏一处），说明 syncer 已写入 tasks-index。
  await page.waitForFunction(
    (title) =>
      [...document.querySelectorAll("body *")].filter(
        (element) => element.childElementCount === 0 && element.textContent?.trim() === title,
      ).length >= 2,
    TITLE,
    { timeout: 60_000 },
  );
  // 终态回源把回复正文写进 searchable_text；稍等 syncer 完成后再搜索。
  await page.waitForTimeout(1500);

  await page.keyboard.press(process.platform === "darwin" ? "Meta+k" : "Control+k");
  const search = page.locator('[placeholder="Search actions, tasks, or files"]').first();
  await search.waitFor({ state: "visible", timeout: 15_000 });
  await search.click();
  await page.keyboard.type("quokka");
  try {
    await page.waitForFunction(
      (title) => {
        const text = [...document.querySelectorAll('[role="dialog"]')]
          .map((el) => el.textContent ?? "")
          .join("\n");
        return text.includes(title) && text.includes("answer lives in the body");
      },
      TITLE,
      { timeout: 30_000 },
    );
  } catch (error) {
    await page.screenshot({ path: join(shots, "tasks-search-failed.png") });
    console.info(`[e2e] command center text: ${(await dialogText(page)).slice(0, 600)}`);
    throw error;
  }
  // 结果行可点击（hit-test），而不仅仅是可见。
  const row = page.locator('[role="dialog"]').getByText(TITLE, { exact: true }).first();
  await row.click({ trial: true });
  await page.screenshot({ path: join(shots, "tasks-search.png") });
  console.info("[e2e] full-text task search returned the body snippet");
  await page.keyboard.press("Escape");
}
