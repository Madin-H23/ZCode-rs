// CLI 崩溃 → 懒重启（rust-host-r2.md §2.1 规则 6）：只杀本次隔离运行的 CLI，再发一条消息仍成功。
import { join } from "node:path";

import { sendMessage, sseText } from "../helpers.mjs";
import { cliProcesses, findRunProcesses, kill, waitFor, waitUsable } from "../resilience-helpers.mjs";

export const mockRules = [
  {
    name: "echo",
    match: () => true,
    respond: ({ response, text }) => response.end(sseText(`Reply to: ${text}`)),
  },
];

export async function run({ page, shots, workspace }) {
  await waitUsable(page);
  await sendMessage(page, "before the CLI crash");
  await page.getByText("Reply to: before the CLI crash").first().waitFor({ timeout: 60_000 });
  const before = await findRunProcesses(workspace);
  kill(before.cli);
  await waitFor(async () => !(await cliProcesses(workspace)).some((row) => row.pid === before.cli), {
    what: "the killed CLI to exit",
  });
  console.info(`[e2e] killed CLI pid=${before.cli}`);
  await page.screenshot({ path: join(shots, "01-after-kill.png") });

  // 同一会话里继续发送：subscribe / command 懒启动新一代 CLI。
  await waitUsable(page);
  await sendMessage(page, "after the CLI crash");
  await page.getByText("Reply to: after the CLI crash").first().waitFor({ timeout: 60_000 });
  const after = await findRunProcesses(workspace, { excludeCli: [before.cli] });
  if (after.host !== before.host) throw new Error("the Rust Host should not restart for a CLI crash");
  await page.screenshot({ path: join(shots, "02-recovered.png") });
  console.info(`[e2e] new CLI pid=${after.cli} under the same host; the conversation continued`);
}
