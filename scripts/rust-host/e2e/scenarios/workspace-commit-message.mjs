// 工作区场景：Git 面板“生成提交信息”经 Rust Host 回调 Agent（CLI workspace/generateText），
// 再用生成的信息提交（docs/specs/rust-host-workspace.md §2.1）。
import { spawnSync } from "node:child_process";
import { appendFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

import { sendMessage, sseText } from "../helpers.mjs";

const COMMIT_SUBJECT = "docs: describe the rust host e2e workspace";

export const mockRules = [
  {
    name: "commit-message",
    match: ({ text }) => text.includes("Write exactly one Git commit message"),
    // 带 ``` 围栏与引号，验证 Host 按 Node 规则去装饰后再交给 UI。
    respond: ({ response }) => response.end(sseText("```\n" + COMMIT_SUBJECT + "\n```")),
  },
  { name: "default", match: () => true, respond: ({ response }) => response.end(sseText("ok")) },
];

/** 提交需要 git 身份：只写进隔离 HOME 的 ~/.gitconfig；再制造一处未暂存改动。 */
export async function seed({ home, workspace }) {
  await writeFile(
    join(home, ".gitconfig"),
    "[user]\n\tname = e2e\n\temail = e2e@example.invalid\n[commit]\n\tgpgsign = false\n",
  );
  await appendFile(join(workspace, "README.md"), "\nThe Rust Host generates commit messages.\n");
}

/** 失败时列出可点的控件，帮助定位界面变化（不看截图也能排查）。 */
export async function dumpControls(page, label) {
  const controls = await page.evaluate(() =>
    [...document.querySelectorAll("button,[role=menuitem],[data-testid]")]
      .filter((element) => element.getBoundingClientRect().width > 0)
      .slice(0, 80)
      .map((element) =>
        [
          element.getAttribute("data-testid"),
          element.getAttribute("aria-label"),
          element.textContent?.trim().slice(0, 40),
        ]
          .filter(Boolean)
          .join(" | "),
      ),
  );
  console.info(`[e2e] controls at ${label}:\n  ${controls.join("\n  ")}`);
}

function lastCommitSubject(workspace, home) {
  const result = spawnSync("git", ["log", "-1", "--format=%s"], {
    cwd: workspace,
    env: { ...process.env, HOME: home, GIT_CONFIG_NOSYSTEM: "1" },
    encoding: "utf8",
  });
  return result.stdout.trim();
}

/** Git 入口在会话的状态面板里：先建会话，再展开面板与 environment 小节。 */
async function revealGitTrigger(page) {
  await sendMessage(page, "say hello");
  await page.getByText("ok", { exact: true }).first().waitFor({ timeout: 60_000 });
  const trigger = page.getByTestId("git-action-trigger").first();
  const expand = page.getByRole("button", { name: "Expand status" }).first();
  if (!(await trigger.isVisible().catch(() => false)) && (await expand.isVisible().catch(() => false))) {
    await expand.click();
  }
  const section = page.locator('[data-status-section-trigger="environment"][aria-expanded="false"]');
  if (!(await trigger.isVisible().catch(() => false)) && (await section.count()) > 0) {
    await section.first().click();
  }
  return trigger;
}

export async function run({ page, shots, model, workspace, home }) {
  const trigger = await revealGitTrigger(page);
  try {
    // trial 点击做命中测试：启动遮罩或禁用态都不算通过。
    await trigger.click({ trial: true, timeout: 60_000 });
    await trigger.click();
    await page.getByTestId("git-commit-dialog").waitFor({ state: "visible", timeout: 30_000 });
  } catch (error) {
    await dumpControls(page, "git trigger");
    throw error;
  } finally {
    await page.screenshot({ path: join(shots, "01-commit-dialog.png") });
  }

  const generate = page.getByTestId("git-commit-generate-button").first();
  await generate.click({ trial: true, timeout: 30_000 });
  await generate.click();
  const input = page.getByTestId("git-commit-message-input").first();
  try {
    await page.waitForFunction(
      (expected) =>
        document.querySelector('[data-testid="git-commit-message-input"]')?.value === expected,
      COMMIT_SUBJECT,
      { timeout: 90_000 },
    );
  } finally {
    await page.screenshot({ path: join(shots, "02-generated.png") });
  }
  const prompt = model.requests.find((request) =>
    request.body.includes("Write exactly one Git commit message"),
  );
  if (!prompt) throw new Error("the commit message prompt never reached the model");
  for (const expected of ["Current branch: main", "README.md"]) {
    if (!prompt.body.includes(expected)) throw new Error(`prompt is missing ${expected}`);
  }
  console.info(`[e2e] generated commit message: ${await input.inputValue()}`);

  // 用生成的信息提交：Commit 动作项提交全部改动，弹窗关闭后 HEAD 的标题就是它。
  const commit = page.getByTestId("git-commit-action-item-commit").first();
  try {
    await commit.click({ timeout: 30_000 });
    await page.getByTestId("git-commit-dialog").waitFor({ state: "hidden", timeout: 30_000 });
  } catch (error) {
    await dumpControls(page, "commit action");
    throw error;
  } finally {
    await page.screenshot({ path: join(shots, "03-committed.png") });
  }
  for (let attempt = 0; attempt < 20; attempt += 1) {
    if (lastCommitSubject(workspace, home) === COMMIT_SUBJECT) break;
    await page.waitForTimeout(500);
  }
  const subject = lastCommitSubject(workspace, home);
  if (subject !== COMMIT_SUBJECT) throw new Error(`unexpected HEAD subject: ${subject}`);
  console.info("[e2e] committed with the generated message");
}
