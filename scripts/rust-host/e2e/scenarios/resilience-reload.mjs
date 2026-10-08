// Renderer reload → 重新 attach（rust-host-r0.md §5）：流式进行中 reload，reload 之后的输出继续显示。
import { join } from "node:path";

import { sendMessage, sseChunk, sseText } from "../helpers.mjs";
import { findRunProcesses, waitUsable } from "../resilience-helpers.mjs";

const TICKS = 80;

export const mockRules = [
  {
    name: "ticker",
    match: ({ hasTools, text }) => hasTools && text.includes("count slowly"),
    respond: async ({ response, sleep }) => {
      response.write(sseChunk({ role: "assistant", content: "Counting:" }, null));
      for (let index = 1; index <= TICKS && !response.destroyed; index += 1) {
        await sleep(250);
        response.write(sseChunk({ content: ` tick-${index}` }, null));
      }
      response.end(sseChunk({ content: " done-counting" }, null) + sseChunk({}, "stop") + "data: [DONE]\n\n");
    },
  },
  {
    name: "echo",
    match: () => true,
    respond: ({ response, text }) => response.end(sseText(`Reply to: ${text}`)),
  },
];

const bodyHas = (page, text, timeout) =>
  page.waitForFunction((needle) => document.body.innerText.includes(needle), text, { timeout });

export async function run({ page, shots, workspace }) {
  await waitUsable(page);
  await sendMessage(page, "please count slowly");
  await bodyHas(page, "tick-3 ", 60_000);
  const before = await findRunProcesses(workspace);
  await page.reload();
  await waitUsable(page);
  // reload 后由恢复的标签页重新打开会话；没有自动打开时从侧栏点开。
  try {
    await bodyHas(page, "Counting:", 10_000);
  } catch {
    await page.getByText("please count slowly").first().click();
  }
  await bodyHas(page, `tick-${TICKS - 20} `, 30_000);
  await page.screenshot({ path: join(shots, "01-streaming-after-reload.png") });
  await bodyHas(page, "done-counting", 30_000);
  const after = await findRunProcesses(workspace);
  if (after.host !== before.host || after.cli !== before.cli)
    throw new Error("reload must reattach to the same Host and CLI");
  console.info("[e2e] reload reattached to the same Host; streaming continued to the end");
}
