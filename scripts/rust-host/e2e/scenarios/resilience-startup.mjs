// 启动失败页（docs/specs/rust-host-resilience.md §2.2、docs/specs/rust-u4-app-host.md §2.8）：tasks-index
// 打不开时所有主窗口（ZCODE_DEV_EXTRA_MAIN_WINDOW=1，两个）都显示失败页；任一窗口的 Exit 退出 App；
// 再次启动后等 30 s 以上（capability 若在拉起时签发就已过期），排除故障，在第二个窗口点 Retry：
// 两个窗口都进入主界面（App 级 Host 的启动状态向所有主窗口扇出，任意窗口的 retry 都被接受）。
import { mkdir, rm } from "node:fs/promises";
import { join } from "node:path";

import { sseText } from "../helpers.mjs";
import { waitUsable } from "../resilience-helpers.mjs";
import { EXTRA_MAIN_WINDOW_ENV, mainWindowPages, roundTrip } from "../window-helpers.mjs";

export const mockRules = [
  {
    name: "echo",
    match: () => true,
    respond: ({ response, text }) => response.end(sseText(`Reply to: ${text}`)),
  },
];

export function env() {
  return EXTRA_MAIN_WINDOW_ENV;
}

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

/** 两个主窗口都停在失败页；返回 [harness 找到的第一个页面, 另一个]。 */
async function bothOnFailurePage(context, page) {
  const pages = await mainWindowPages(context, 2);
  const other = pages.find((candidate) => candidate !== page);
  const first = await failurePage(page);
  const second = await failurePage(other);
  return { pages: [page, other], first, second };
}

export async function run(context) {
  const { page, shots, home, relaunch } = context;
  const initial = await bothOnFailurePage(context, page);
  console.info(
    `[e2e] startup failed with ${await initial.first.status.getAttribute("data-error-code")} in both windows`,
  );
  await page.screenshot({ path: join(shots, "01-failed.png") });

  const closed = page.waitForEvent("close", { timeout: 30_000 });
  await initial.pages[1].getByRole("button", { name: "Exit" }).click();
  await closed;
  console.info("[e2e] Exit from the second window quit the app");

  const next = await relaunch();
  const again = await bothOnFailurePage(context, next);
  // capability 只有 30 s 有效期：等过这个时间，确认 endpoint 是在 ready 时才签发的。
  await new Promise((resolve) => setTimeout(resolve, 31_000));
  await rm(blocker(home), { recursive: true });
  await again.second.retry.click();
  for (const window of again.pages) await waitUsable(window);
  await next.screenshot({ path: join(shots, "02-after-retry.png") });
  await roundTrip(next, "after the retry");
  await roundTrip(again.pages[1], "after the retry in the second window");
  console.info(
    "[e2e] Retry from the second window prepared storage again; both windows are usable",
  );
}
