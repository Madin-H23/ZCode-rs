// U4：两个主窗口接入同一个 App 级 Rust Host（docs/specs/rust-u4-app-host.md §2.2、§2.3、§2.9、§6）。
//
// ZCODE_DEV_EXTRA_MAIN_WINDOW=1 让 Main 在第一个主窗口就绪后再建一个主窗口。验证：
//   1. 两个窗口都可用（各自发消息得到回复），Electron Main 下始终只有一个 Rust Host；
//   2. 一个窗口里的广播驱动的修改在另一个窗口跟随：主题（只经 broadcast 同步，没有 storage 事件）
//      与界面语言；
//   3. 真正销毁第二个窗口（macOS 上点关闭只是隐藏）后，第一个窗口继续正常对话；
//   全程 Host 的 pid 不变。
import { join } from "node:path";

import { sseText } from "../helpers.mjs";
import { destroyMarkedWindowExpression } from "../main-process.mjs";
import { hostLogLines, waitFor } from "../resilience-helpers.mjs";
import {
  EXTRA_MAIN_WINDOW_ENV,
  onlyHost,
  roundTrip,
  twoUsableWindows,
} from "../window-helpers.mjs";

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

const isDark = (page) => page.evaluate(() => document.documentElement.classList.contains("dark"));

async function chooseFromAccountMenu(page, submenu, item) {
  await page.getByTestId("login-trigger").first().click();
  await page.getByText(submenu, { exact: true }).last().hover();
  await page.getByText(submenu, { exact: true }).last().click();
  await page.getByText(item, { exact: true }).last().click();
  await page.keyboard.press("Escape");
}

export async function run(context) {
  const { shots, mainPid } = context;
  const [first, second] = await twoUsableWindows(context);
  const host = await onlyHost(mainPid());
  const attachments = (await hostLogLines(context.home)).filter((line) =>
    /attachment \S+ connected/.test(line),
  );
  if (attachments.length < 2)
    throw new Error(`expected both windows to attach, Host log has ${attachments.length}`);
  console.info(`[e2e] two main windows attached to one Rust Host (pid=${host})`);

  // 1. 两个窗口都可用。
  await roundTrip(first, "hello from the first window");
  await roundTrip(second, "hello from the second window");
  await first.screenshot({ path: join(shots, "01-first.png") });
  await second.screenshot({ path: join(shots, "02-second.png") });
  console.info("[e2e] both windows chat through the shared Host");

  // 2. 广播：第一个窗口切换主题（Cmd/Ctrl+Shift+L），第二个窗口跟随；再切回来，两边一致。
  const before = await isDark(first);
  if ((await isDark(second)) !== before) throw new Error("windows start with different themes");
  await first.bringToFront();
  await first.keyboard.press(process.platform === "darwin" ? "Meta+Shift+L" : "Control+Shift+L");
  await first.waitForFunction(
    (dark) => document.documentElement.classList.contains("dark") !== dark,
    before,
  );
  await second.waitForFunction(
    (dark) => document.documentElement.classList.contains("dark") !== dark,
    before,
    { timeout: 15_000 },
  );
  await second.screenshot({ path: join(shots, "03-second-follows-theme.png") });
  console.info(`[e2e] theme switched in the first window followed in the second (dark=${!before})`);

  // 语言：第二个窗口在账户菜单里切到中文，第一个窗口跟随。
  await second.bringToFront();
  await chooseFromAccountMenu(second, "Language", "中文简体");
  await second.getByText("新建任务").first().waitFor({ state: "visible", timeout: 15_000 });
  await first.getByText("新建任务").first().waitFor({ state: "visible", timeout: 15_000 });
  await first.screenshot({ path: join(shots, "04-first-follows-locale.png") });
  console.info("[e2e] language switched in the second window followed in the first");

  // 3. 真正销毁第二个窗口：Host 不受影响，第一个窗口继续对话。
  const destroyed = await context.main().evaluate(destroyMarkedWindowExpression("second"));
  if (!destroyed) throw new Error("the second window was not found in Main");
  await waitFor(async () => context.pages().length === 1, { what: "the second window to close" });
  await new Promise((resolve) => setTimeout(resolve, 1_000));
  if ((await onlyHost(mainPid())) !== host) throw new Error("closing a window restarted the Host");
  await roundTrip(first, "still here after closing the second window");
  await first.screenshot({ path: join(shots, "05-first-after-close.png") });
  if ((await onlyHost(mainPid())) !== host) throw new Error("the Host pid changed");
  console.info(
    `[e2e] second window destroyed; the first keeps chatting on the same Host (pid=${host})`,
  );
}
