// D2：Rust Host 崩溃 → Main 自动重启 → App 再次可用；60 s 内第 4 次崩溃放弃自动重启，
// 页面给出“重新加载 / 退出”，重新加载后恢复（docs/specs/rust-host-resilience.md §2.1）。
import { join } from "node:path";

import { sendMessage, sseText } from "../helpers.mjs";
import { findRunProcesses, hostsOf, kill, waitFor, waitUsable } from "../resilience-helpers.mjs";

export const mockRules = [
  {
    name: "echo",
    match: () => true,
    respond: ({ response, text }) => response.end(sseText(`Reply to: ${text}`)),
  },
];

async function newHost(electron, previous) {
  const hosts = await waitFor(
    async () => {
      const alive = await hostsOf(electron);
      return alive.length === 1 && alive[0].pid !== previous ? alive : null;
    },
    { timeout: 30_000, what: "a new Rust Host" },
  );
  return hosts[0].pid;
}

async function roundTrip(page, text) {
  await waitUsable(page);
  await sendMessage(page, text);
  await page.getByText(`Reply to: ${text}`).first().waitFor({ state: "visible", timeout: 60_000 });
}

export async function run({ page, shots, workspace }) {
  await roundTrip(page, "before the crash");
  const first = await findRunProcesses(workspace);
  console.info(`[e2e] host pid=${first.host}, electron pid=${first.electron}`);

  // 第 1 次崩溃：立即离开旧界面，显示“正在重启”，随后自动 reload 并恢复。
  kill(first.host);
  const lost = page.locator('[data-testid="host-connection-lost"]');
  await lost.waitFor({ state: "visible", timeout: 5_000 });
  await page.screenshot({ path: join(shots, "01-reconnecting.png") });
  if ((await lost.getAttribute("data-state")) !== "reconnecting")
    throw new Error("expected the reconnecting state");
  let host = await newHost(first.electron, first.host);
  await roundTrip(page, "after the restart");
  await page.screenshot({ path: join(shots, "02-recovered.png") });
  console.info(`[e2e] host restarted automatically (pid=${host}) and the app is usable`);

  // 再连续崩溃 3 次：第 2、3 次仍自动重启，第 4 次（60 s 内）放弃。
  for (let index = 0; index < 2; index += 1) {
    kill(host);
    host = await newHost(first.electron, host);
  }
  kill(host);
  const failed = page.locator('[data-testid="host-connection-lost"][data-state="failed"]');
  await failed.waitFor({ state: "visible", timeout: 15_000 });
  // 状态页必须真实可点（不被 HTML 启动壳遮住），且很快可点。
  const reload = page.getByTestId("host-connection-reload");
  await reload.click({ trial: true, timeout: 2_000 });
  await page.screenshot({ path: join(shots, "03-gave-up.png") });
  await new Promise((resolve) => setTimeout(resolve, 6_000));
  if ((await hostsOf(first.electron)).length !== 0)
    throw new Error("Main restarted the host after giving up");
  console.info("[e2e] gave up after the 4th crash within 60 s; no host is running");

  // 用户手动重新加载：dom-ready 拉起新 Host，App 恢复。
  await reload.click();
  host = await newHost(first.electron, host);
  await roundTrip(page, "after the manual reload");
  await page.screenshot({ path: join(shots, "04-manual-reload.png") });
  console.info(`[e2e] manual reload started a new host (pid=${host})`);
}
