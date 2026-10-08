// e2e-desktop.mjs 的界面流程：发送一条消息，等 mock 模型的回复流式出现在对话里。
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";

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

/** 只读打开隔离 HOME 里的 tasks-index.sqlite（WAL 下读者不阻塞 Host 的写入）。 */
function readTaskRows(home) {
  const database = new DatabaseSync(join(home, ".zcode", "v2", "tasks-index.sqlite"), {
    readOnly: true,
  });
  try {
    return database.prepare("SELECT task_id, title, workspace_path, deleted FROM tasks").all();
  } finally {
    database.close();
  }
}

export async function runCoreFlow({
  page,
  shots,
  model,
  workspace,
  home,
  mockReply,
  writtenReply,
}) {
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

  // 侧栏：syncer 写入 tasks-index 行并广播 task_created，Project 列表随之出现这个任务
  // （对话气泡里一处、侧栏一处）。
  await page.waitForFunction(
    () =>
      [...document.querySelectorAll("body *")].filter(
        (element) => element.childElementCount === 0 && element.textContent?.trim() === "say hello",
      ).length >= 2,
    undefined,
    { timeout: 30_000 },
  );
  await page.screenshot({ path: join(shots, "02b-sidebar.png") });
  const rows = readTaskRows(home);
  if (!rows.some((row) => row.title === "say hello" && row.workspace_path === workspace)) {
    throw new Error(`tasks-index row missing: ${JSON.stringify(rows)}`);
  }
  console.info("[e2e] new task listed in the sidebar and persisted in tasks-index");

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

  // 终端：Rust Host 的 PTY 在 workspace 里执行命令。
  await page.getByRole("button", { name: "Toggle terminal" }).first().click();
  await page.waitForTimeout(2500);
  await page.keyboard.type("echo rust-host-terminal > term-ok.txt");
  await page.keyboard.press("Enter");
  const termFile = join(workspace, "term-ok.txt");
  for (let attempt = 0; attempt < 20; attempt += 1) {
    if ((await readFile(termFile, "utf8").catch(() => "")).trim() === "rust-host-terminal") break;
    await page.waitForTimeout(500);
  }
  await page.screenshot({ path: join(shots, "09-terminal.png") });
  if ((await readFile(termFile, "utf8")).trim() !== "rust-host-terminal") {
    throw new Error("terminal command did not run");
  }
  console.info("[e2e] terminal ran a command in the workspace");

  // Git：Review 面板经 git 服务列出工作区改动；term-ok.txt 只可能来自 git status。
  await page.getByRole("button", { name: "Add tab" }).first().click();
  await page.getByRole("menuitem", { name: "Review" }).first().click();
  try {
    await page.getByText("term-ok.txt").first().waitFor({ state: "visible", timeout: 30_000 });
  } finally {
    await page.screenshot({ path: join(shots, "10-review.png") });
  }
  console.info("[e2e] git review lists workspace changes");
}

async function sendFollowUp(page, text) {
  const input = await composer(page);
  await input.click();
  await page.keyboard.type(text);
  await page.keyboard.press("Enter");
}

/** 冷启动后：侧栏仍列出上一轮创建的任务（来自 tasks-index，而非内存）。 */
export async function verifyColdRestart({ page, shots }) {
  try {
    // trial click 会做命中测试：启动遮罩还盖着时不算通过（仅 visible 会被遮罩下的 DOM 骗过）。
    await page
      .getByText("say hello", { exact: true })
      .first()
      .click({ trial: true, timeout: 60_000 });
  } finally {
    await page.screenshot({ path: join(shots, "11-cold-restart.png") });
  }
  console.info("[e2e] task list survived a cold restart");
}
