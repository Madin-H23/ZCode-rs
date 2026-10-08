// 启动失败页（docs/specs/rust-host-resilience.md §2.2）：tasks-index 打不开时显示失败页；
// Exit 退出 App；再次启动后等 30 s 以上（capability 若在拉起时签发就已过期），排除故障并 Retry，进入主界面。
import { mkdir, rm } from "node:fs/promises";
import { join } from "node:path";

import { sendMessage, sseText } from "../helpers.mjs";
import { waitUsable } from "../resilience-helpers.mjs";

export const mockRules = [
  {
    name: "echo",
    match: () => true,
    respond: ({ response, text }) => response.end(sseText(`Reply to: ${text}`)),
  },
];

const blocker = (home) => join(home, ".zcode", "v2", "tasks-index.sqlite");

// tasks-index.sqlite 是目录：SQLite 打不开，属于可重试的失败（open_failed）。
export async function seed({ home }) {
  await mkdir(blocker(home), { recursive: true });
}

async function failurePage(page) {
  const status = page.locator('[data-testid="database-startup-status"][data-phase="failed"]');
  await status.waitFor({ state: "visible", timeout: 60_000 });
  const retry = page.getByTestId("database-startup-retry");
  await retry.click({ trial: true });
  return { status, retry };
}

export async function run({ page, shots, home, relaunch }) {
  const first = await failurePage(page);
  console.info(`[e2e] startup failed with ${await first.status.getAttribute("data-error-code")}`);
  await page.screenshot({ path: join(shots, "01-failed.png") });

  const closed = page.waitForEvent("close", { timeout: 30_000 });
  await page.getByRole("button", { name: "Exit" }).click();
  await closed;
  console.info("[e2e] Exit quit the app");

  const next = await relaunch();
  const { retry } = await failurePage(next);
  // capability 只有 30 s 有效期：等过这个时间，确认 endpoint 是在 ready 时才签发的。
  await new Promise((resolve) => setTimeout(resolve, 31_000));
  await rm(blocker(home), { recursive: true });
  await retry.click();
  await waitUsable(next);
  await next.screenshot({ path: join(shots, "02-after-retry.png") });
  await sendMessage(next, "after the retry");
  await next.getByText("Reply to: after the retry").first().waitFor({ timeout: 60_000 });
  console.info("[e2e] Retry prepared storage again and the app is usable");
}
