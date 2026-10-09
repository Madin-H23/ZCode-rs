// D2 + U4：App 级 Rust Host 崩溃 → Main 自动重启 → 所有主窗口 reload 后可用；60 s 内第 4 次崩溃放弃
// 自动重启，所有主窗口显示失败页（“重新加载 / 退出”），任意一个窗口的重新加载触发一次重启，其余窗口
// 在新 Host 就绪后被 reload（docs/specs/rust-host-resilience.md §2.1、docs/specs/rust-u4-app-host.md §2.8）。
// 两个主窗口（ZCODE_DEV_EXTRA_MAIN_WINDOW=1）共用一个 Host，全程 Electron Main 下最多一个 Host。
import { join } from "node:path";

import { sseText } from "../helpers.mjs";
import { findRunProcesses, hostsOf, kill, waitFor, waitUsable } from "../resilience-helpers.mjs";
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

async function newHost(electron, previous) {
  const hosts = await waitFor(
    async () => {
      const alive = await hostsOf(electron);
      if (alive.length > 1)
        throw new Error(`more than one Rust Host: ${alive.map((row) => row.pid)}`);
      return alive.length === 1 && alive[0].pid !== previous ? alive : null;
    },
    { timeout: 30_000, what: "a new Rust Host" },
  );
  return hosts[0].pid;
}

const lostScreen = (page, state) =>
  page.locator(`[data-testid="host-connection-lost"]${state ? `[data-state="${state}"]` : ""}`);

export async function run(context) {
  const { shots } = context;
  const [first, second] = await twoUsableWindows(context);
  await roundTrip(first, "before the crash");
  const initial = await findRunProcesses(context);
  console.info(`[e2e] host pid=${initial.host}, electron pid=${initial.electron}, 2 windows`);

  // 第 1 次崩溃：两个窗口都立即离开旧界面，显示“正在重启”；新 Host 就绪后都被 reload 并恢复。
  kill(initial.host);
  for (const page of [first, second]) {
    const lost = lostScreen(page);
    await lost.waitFor({ state: "visible", timeout: 5_000 });
    if ((await lost.getAttribute("data-state")) !== "reconnecting")
      throw new Error("expected the reconnecting state in every window");
  }
  await first.screenshot({ path: join(shots, "01-reconnecting.png") });
  let host = await newHost(initial.electron, initial.host);
  await roundTrip(first, "after the restart");
  await roundTrip(second, "after the restart in the second window");
  if ((await findRunProcesses(context)).host !== host)
    throw new Error("the restarted Host is not the one serving the workspace");
  await second.screenshot({ path: join(shots, "02-recovered-second.png") });
  console.info(`[e2e] host restarted automatically (pid=${host}); both windows reloaded and work`);

  // 再连续崩溃 3 次：第 2、3 次仍自动重启，第 4 次（60 s 内）放弃。
  for (let index = 0; index < 2; index += 1) {
    kill(host);
    host = await newHost(initial.electron, host);
  }
  kill(host);
  for (const page of [first, second]) {
    await lostScreen(page, "failed").waitFor({ state: "visible", timeout: 15_000 });
    // 状态页必须真实可点（不被 HTML 启动壳遮住），且很快可点。
    await page.getByTestId("host-connection-reload").click({ trial: true, timeout: 2_000 });
  }
  await first.screenshot({ path: join(shots, "03-gave-up-first.png") });
  await second.screenshot({ path: join(shots, "03-gave-up-second.png") });
  await new Promise((resolve) => setTimeout(resolve, 6_000));
  if ((await hostsOf(initial.electron)).length !== 0)
    throw new Error("Main restarted the host after giving up");
  console.info("[e2e] gave up after the 4th crash within 60 s; both windows show the failure page");

  // 第二个窗口点“重新加载”：拉起新 Host；第一个窗口仍停在失败页，新 Host 就绪后被 reload。
  await second.getByTestId("host-connection-reload").click();
  host = await newHost(initial.electron, host);
  await roundTrip(second, "after the retry");
  await waitUsable(first);
  await roundTrip(first, "after the retry in the first window");
  if ((await onlyHost(initial.electron)) !== host) throw new Error("the Host changed again");
  await first.screenshot({ path: join(shots, "04-retry-first.png") });
  console.info(
    `[e2e] Retry from one window started a new host (pid=${host}); both windows recovered`,
  );
}
