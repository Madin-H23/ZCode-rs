// Gateway 的连接收口（docs/specs/rust-u2-gateway.md §2.4）：流式输出进行中销毁一个窗口。
//
// 两个主窗口接入同一个 App 级 Rust Host（ZCODE_DEV_EXTRA_MAIN_WINDOW=1）。第二个窗口发起一段慢速流，
// 流式中途真正销毁这个窗口（macOS 上点关闭只是隐藏）。验证：
//   1. Host 上这个窗口的连接被收口（Renderer 连接数减一），Host 不重启；
//   2. runtime 里的 run 不受窗口关闭影响：第一个窗口从侧栏打开这个会话，接着看到流式输出直到结束；
//   3. 第一个窗口的对话照常往返。
// 慢速流由场景控制何时结束（release），保证销毁时它一定还在进行。
import { join } from "node:path";

import { sendMessage, sseChunk, sseText } from "../helpers.mjs";
import { destroyMarkedWindowExpression } from "../main-process.mjs";
import { hostRpcConnections, waitFor } from "../resilience-helpers.mjs";
import {
  EXTRA_MAIN_WINDOW_ENV,
  onlyHost,
  roundTrip,
  twoUsableWindows,
} from "../window-helpers.mjs";

let release;
const released = new Promise((resolve) => (release = resolve));
let streamFinished = false;

export const mockRules = [
  {
    name: "ticker",
    match: ({ hasTools, text }) => hasTools && text.includes("count slowly"),
    respond: async ({ response, sleep }) => {
      response.write(sseChunk({ role: "assistant", content: "Counting:" }, null));
      let done = false;
      released.then(() => (done = true));
      for (let index = 1; !done && !response.destroyed; index += 1) {
        await sleep(250);
        response.write(sseChunk({ content: ` tick-${index}` }, null));
      }
      response.end(
        sseChunk({ content: " done-counting" }, null) + sseChunk({}, "stop") + "data: [DONE]\n\n",
      );
      streamFinished = !response.destroyed;
    },
  },
  {
    // 会话标题请求（不带 tools）回用户原文：侧栏标题就是第一条消息。
    name: "title",
    match: ({ hasTools }) => !hasTools,
    respond: ({ response, text }) => response.end(sseText(text)),
  },
  {
    name: "echo",
    match: () => true,
    respond: ({ response, text }) => response.end(sseText(`Reply to: ${text}`)),
  },
];

export function env() {
  return EXTRA_MAIN_WINDOW_ENV;
}

const bodyHas = (page, text, timeout = 60_000) =>
  page.waitForFunction((needle) => document.body.innerText.includes(needle), text, { timeout });

export async function run(context) {
  const { shots, mainPid, home } = context;
  const [first, second] = await twoUsableWindows(context);
  const host = await onlyHost(mainPid());
  const connections = await waitFor(
    async () => {
      const count = await hostRpcConnections(home, host);
      return count >= 2 ? count : null;
    },
    { what: "both windows to connect to the Host" },
  );

  // 第二个窗口发起慢速流，流式中途销毁它。
  await second.bringToFront();
  await sendMessage(second, "please count slowly");
  await bodyHas(second, "tick-3 ");
  await second.screenshot({ path: join(shots, "01-second-streaming.png") });
  const destroyed = await context.main().evaluate(destroyMarkedWindowExpression("second"));
  if (!destroyed) throw new Error("the second window was not found in Main");
  await waitFor(async () => context.pages().length === 1, { what: "the second window to close" });

  // 1. Gateway 收口了这个窗口的连接；Host 没有重启。
  const remaining = await waitFor(
    async () => {
      const count = await hostRpcConnections(home, host);
      return count === connections - 1 ? count : null;
    },
    { timeout: 30_000, what: `the Host to drop to ${connections - 1} Renderer connections` },
  );
  if ((await onlyHost(mainPid())) !== host) throw new Error("closing a window restarted the Host");
  console.info(
    `[e2e] the destroyed window's connection was closed mid-stream (${connections} -> ${remaining} Renderer connections, Host pid ${host})`,
  );

  // 2. 第一个窗口打开这个会话：run 仍在进行，订阅接着收到输出直到结束。
  await first.bringToFront();
  await first.getByText("please count slowly", { exact: true }).first().click();
  await bodyHas(first, "Counting:");
  release();
  await waitFor(async () => streamFinished, { what: "the stream to finish" });
  await bodyHas(first, "done-counting");
  await first.screenshot({ path: join(shots, "02-first-sees-the-end.png") });
  console.info("[e2e] the run outlived its window; the first window followed it to the end");

  // 3. 第一个窗口照常往返。
  await roundTrip(first, "still here after the window was destroyed");
  if ((await onlyHost(mainPid())) !== host) throw new Error("the Host pid changed");
}
