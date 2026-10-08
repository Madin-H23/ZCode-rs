// e2e-desktop.mjs 的界面流程：发送一条消息，等 mock 模型的回复流式出现在对话里。
import { readFile } from "node:fs/promises";
import { join } from "node:path";

async function composer(page) {
  const candidates = [
    page.locator('textarea[placeholder*="Ask ZCode"]'),
    page.locator('[contenteditable="true"]'),
  ];
  for (let attempt = 0; attempt < 120; attempt += 1) {
    for (const candidate of candidates) {
      const first = candidate.first();
      if (await first.isVisible().catch(() => false)) return first;
    }
    await page.waitForTimeout(500);
  }
  throw new Error("composer not found");
}

export async function runCoreFlow({ page, shots, model, workspace, mockReply, writtenReply }) {
  const input = await composer(page);
  await page.screenshot({ path: join(shots, "01-ready.png") });
  await input.click();
  await page.keyboard.type("say hello");
  await page.keyboard.press("Enter");
  try {
    await page.getByText(mockReply).first().waitFor({ state: "visible", timeout: 60_000 });
  } finally {
    await page.screenshot({ path: join(shots, "02-after-send.png") });
  }
  if (model.requests.length === 0) throw new Error("mock model never received a request");
  console.info(`[e2e] reply streamed; model requests=${model.requests.length}`);

  // 权限：Write 需要批准；默认选中 Allow，确认后工具执行，模型收到工具结果再回复。
  await sendFollowUp(page, "please write file hello.txt");
  await page.getByText("Permission required").first().waitFor({ timeout: 60_000 });
  await page.screenshot({ path: join(shots, "03-permission.png") });
  await page.getByRole("button", { name: "Confirm" }).click();
  try {
    await page.getByText(writtenReply).first().waitFor({ state: "visible", timeout: 60_000 });
  } finally {
    await page.screenshot({ path: join(shots, "04-after-permission.png") });
  }
  const written = await readFile(join(workspace, "hello.txt"), "utf8");
  if (written !== "hi from e2e\n") throw new Error(`unexpected file content: ${written}`);
  console.info("[e2e] permission approved and file written");

  // 文件改动：展开回合末尾的改动卡片，Renderer 经 conversationFileChangesV4 读取改动。
  await page.getByText("1 file changed").first().click();
  await page.getByRole("button", { name: "Review" }).first().click();
  await page.waitForTimeout(2000);
  await page.screenshot({ path: join(shots, "05-file-changes.png") });
  await page.keyboard.press("Escape");

  // Stop：慢速流进行中点停止，发送按钮恢复。
  await sendFollowUp(page, "run a long task");
  const stop = page.locator('button[aria-label="Stop"]').first();
  await stop.waitFor({ state: "visible", timeout: 30_000 });
  await page.waitForTimeout(1500);
  await stop.click();
  try {
    await stop.waitFor({ state: "hidden", timeout: 30_000 });
  } finally {
    await page.screenshot({ path: join(shots, "06-after-stop.png") });
  }
  console.info("[e2e] stop returned the composer to idle");

  // 切换模型：选择 mock-model-2，下一条消息的模型请求必须用它。
  await page.getByText("Mock/mock-model").first().click();
  await page.waitForTimeout(800);
  await page.screenshot({ path: join(shots, "07-model-menu.png") });
  // 菜单按 provider 分组：先展开 Mock 子菜单再选模型。
  await page.getByText("Mock", { exact: true }).last().hover();
  await page.getByText("Mock", { exact: true }).last().click();
  await page.waitForTimeout(500);
  await page.screenshot({ path: join(shots, "07b-model-submenu.png") });
  await page.getByText("mock-model-2", { exact: true }).last().click();
  await page.keyboard.press("Escape");
  const before = model.requests.length;
  await sendFollowUp(page, "say hello again");
  await page.getByText(mockReply).nth(1).waitFor({ state: "visible", timeout: 60_000 });
  const used = model.requests
    .slice(before)
    .map((request) => {
      try {
        return JSON.parse(request.body).model;
      } catch {
        return undefined;
      }
    })
    .filter(Boolean);
  await page.screenshot({ path: join(shots, "08-switched-model.png") });
  if (!used.includes("mock-model-2")) throw new Error(`model switch not applied: ${used}`);
  console.info("[e2e] model switch applied");
}

async function sendFollowUp(page, text) {
  const input = await composer(page);
  await input.click();
  await page.keyboard.type(text);
  await page.keyboard.press("Enter");
}
