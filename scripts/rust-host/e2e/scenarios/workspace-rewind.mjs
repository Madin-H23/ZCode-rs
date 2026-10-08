// 工作区场景：回退（rewind）。Agent 用 Write 写文件后，编辑这条用户消息并选“回退工作区”，
// 文件改动必须撤销。git-checkpoint channel 在本地 desktop 没有 UI 调用方（只给远端集合），
// 它由 services 的集成测试覆盖；界面上的回退走 CLI 的文件检查点，经 Rust Host 转发。
import { realpathSync } from "node:fs";
import { access, readFile } from "node:fs/promises";
import { join } from "node:path";

import { sendMessage, sseText, sseToolCall } from "../helpers.mjs";
import { dumpControls } from "./workspace-commit-message.mjs";

const WRITTEN = "rewind-me\n";

export const mockRules = [
  {
    name: "tool-result",
    match: ({ hasTools, messages }) => hasTools && messages.at(-1)?.role === "tool",
    respond: ({ response }) => response.end(sseText("File written for rewind")),
  },
  {
    name: "write-file",
    match: ({ hasTools, text }) => hasTools && text.includes("write the rewind file"),
    respond: ({ response, workspace }) =>
      // CLI 的文件检查点拒绝经过符号链接的路径（macOS 临时目录 /var → /private/var），用规范路径。
      response.end(
        sseToolCall("Write", {
          file_path: join(realpathSync(workspace), "rewind.txt"),
          content: WRITTEN,
        }),
      ),
  },
  {
    name: "default",
    match: () => true,
    respond: ({ response }) => response.end(sseText("Rewound and answered")),
  },
];

async function exists(path) {
  return access(path).then(
    () => true,
    () => false,
  );
}

export async function run({ page, shots, workspace }) {
  const target = join(workspace, "rewind.txt");
  await sendMessage(page, "please write the rewind file");
  await page.getByText("Permission required").first().waitFor({ timeout: 60_000 });
  await page.getByRole("button", { name: "Confirm" }).click();
  await page.getByText("File written for rewind").first().waitFor({ timeout: 60_000 });
  if ((await readFile(target, "utf8")) !== WRITTEN) throw new Error("tool did not write the file");
  await page.screenshot({ path: join(shots, "01-written.png") });

  // 编辑第一条用户消息：悬停后出现 Edit；改写内容后点“回退工作区并发送”。
  try {
    const bubble = page.getByText("please write the rewind file", { exact: true }).first();
    await bubble.hover();
    const editId = await page.evaluate(
      () =>
        [...document.querySelectorAll("[data-testid]")]
          .map((element) => element.getAttribute("data-testid"))
          .find((id) => /^v4-edit-\d+$/.test(id ?? "")) ?? null,
    );
    if (!editId) throw new Error("edit button not found");
    await page.getByTestId(editId).click();
    const rowId = editId.replace("v4-edit-", "");
    const input = page.getByTestId(`v4-edit-input-${rowId}`).first();
    await input.waitFor({ state: "visible", timeout: 10_000 });
    await input.click();
    await page.keyboard.press(process.platform === "darwin" ? "Meta+A" : "Control+A");
    await page.keyboard.type("say hello after rewind");
    const rewind = page.getByTestId(`v4-edit-rewind-workspace-${rowId}`).first();
    await page.waitForFunction(
      (id) => {
        const button = document.querySelector(`[data-testid="${id}"]`);
        return button instanceof HTMLButtonElement && !button.disabled;
      },
      `v4-edit-rewind-workspace-${rowId}`,
      { timeout: 30_000 },
    );
    await rewind.click();
    // 有可回退的文件时会先弹出“Undo file changes”预览，确认后才执行。
    const confirm = page.getByRole("button", { name: "Undo files" }).first();
    if (await confirm.isVisible({ timeout: 5_000 }).catch(() => false)) await confirm.click();
  } catch (error) {
    await dumpControls(page, "rewind");
    throw error;
  } finally {
    await page.screenshot({ path: join(shots, "02-rewind.png") });
  }

  for (let attempt = 0; attempt < 60 && (await exists(target)); attempt += 1) {
    await page.waitForTimeout(500);
  }
  await page.screenshot({ path: join(shots, "03-after-rewind.png") });
  if (await exists(target)) {
    // 例如 CLI 判定文件不可安全撤销时会弹出冲突说明，把它带进失败信息。
    const dialogs = await page.evaluate(() =>
      [...document.querySelectorAll('[role="dialog"],[role="alertdialog"]')]
        .map((element) => element.textContent?.trim().slice(0, 400))
        .join(" | "),
    );
    throw new Error(`rewind did not undo the written file; dialogs: ${dialogs || "(none)"}`);
  }
  await page.getByText("Rewound and answered").first().waitFor({ timeout: 60_000 });
  console.info("[e2e] rewind undid the agent's file change and resent the edited message");
}
