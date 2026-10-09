// 多窗口场景（docs/specs/rust-u4-app-host.md §2.9）共用的辅助函数：ZCODE_DEV_EXTRA_MAIN_WINDOW=1 让 Main
// 在第一个主窗口就绪后再建一个主窗口；两个窗口接入同一个 App 级 Rust Host。
import { sendMessage } from "./helpers.mjs";
import { markPage } from "./main-process.mjs";
import { hostsOf, waitFor, waitUsable } from "./resilience-helpers.mjs";

/** 场景 env()：打开第二个主窗口的开发开关。 */
export const EXTRA_MAIN_WINDOW_ENV = { ZCODE_DEV_EXTRA_MAIN_WINDOW: "1" };

/** Electron Main 下唯一的 Rust Host 的 pid（没有或多于一个时失败）。 */
export async function onlyHost(mainPid, timeout = 60_000) {
  const hosts = await waitFor(
    async () => {
      const alive = await hostsOf(mainPid);
      return alive.length > 0 ? alive : null;
    },
    { timeout, what: "the Rust Host" },
  );
  if (hosts.length !== 1) throw new Error(`expected one Rust Host, found ${hosts.length}`);
  return hosts[0].pid;
}

/** 等到 `count` 个主窗口页面出现。 */
export async function mainWindowPages(context, count) {
  const pages = await waitFor(
    async () => {
      const found = context.pages();
      return found.length >= count ? found : null;
    },
    { timeout: 60_000, what: `${count} main windows` },
  );
  if (pages.length !== count)
    throw new Error(`expected ${count} main windows, found ${pages.length}`);
  return pages;
}

/**
 * 两个主窗口都出现并可用；返回 [第一个, 第二个]（第一个是 harness 启动时找到的页面），
 * 并在页面里打上 "first" / "second" 标记，供 Main 侧按窗口操作。
 */
export async function twoUsableWindows(context) {
  const pages = await mainWindowPages(context, 2);
  await Promise.all(pages.map((page) => waitUsable(page)));
  const first = pages.find((page) => page === context.page) ?? pages[0];
  const second = pages.find((page) => page !== first);
  await markPage(first, "first");
  await markPage(second, "second");
  return [first, second];
}

/** 发一条消息并等 echo 规则的回复（`Reply to: <text>`）。 */
export async function roundTrip(page, text) {
  await waitUsable(page);
  await sendMessage(page, text);
  await page.getByText(`Reply to: ${text}`).first().waitFor({ state: "visible", timeout: 60_000 });
}
