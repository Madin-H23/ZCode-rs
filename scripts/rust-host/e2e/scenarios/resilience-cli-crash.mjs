// runtime 崩溃 → 懒重启（rust-host-r2.md §2.1 规则 6，rust-u3-single-process.md §2.5）：崩溃只结束本
// workspace 的 runtime 实例，Rust Host 不重启；同一会话里再发一条消息，新一代 runtime 接手，对话继续。
//   child     只杀本次隔离运行的 Rust CLI 进程（SIGKILL）。
//   embedded  runtime 在 Host 进程内，没有可杀的进程：用调试构建的故障注入（ZCODE_RUNTIME_FAULT_MARKER），
//             发一条含标记的消息，让这个 workspace 的 Engine 循环 panic。
import { join } from "node:path";

import { composer, sendMessage, sseText } from "../helpers.mjs";
import {
  cliProcesses,
  findRunProcesses,
  hostLogLines,
  kill,
  runtimeGenerations,
  waitFor,
  waitUsable,
} from "../resilience-helpers.mjs";

const FAULT = "zcode-e2e-fault-5d2c91";

export const mockRules = [
  {
    name: "echo",
    match: () => true,
    respond: ({ response, text }) => response.end(sseText(`Reply to: ${text}`)),
  },
];

export function env({ runtime }) {
  return runtime === "child" ? {} : { ZCODE_RUNTIME_FAULT_MARKER: FAULT };
}

async function roundTrip(page, text) {
  await waitUsable(page);
  await sendMessage(page, text);
  await page.getByText(`Reply to: ${text}`).first().waitFor({ timeout: 60_000 });
}

async function crashChild(context, before) {
  const { workspace } = context;
  kill(before.cli);
  await waitFor(
    async () => !(await cliProcesses(workspace)).some((row) => row.pid === before.cli),
    { what: "the killed CLI to exit" },
  );
  console.info(`[e2e] killed CLI pid=${before.cli}`);
}

/** Host 日志里本 workspace 的 runtime 实例：最新的代次与 Engine panic 的次数。 */
async function runtimeFacts(home, workspace) {
  const panics = (await hostLogLines(home)).filter(
    (line) => line.includes("panic: panicked at") && line.includes("engine_fault.rs"),
  ).length;
  return { generation: Math.max(0, ...(await runtimeGenerations(home, workspace))), panics };
}

async function crashEmbedded(context) {
  const { page, shots, home, workspace } = context;
  const before = await runtimeFacts(home, workspace);
  await waitUsable(page);
  await sendMessage(page, `please crash ${FAULT}`);
  // 界面：这次发送失败并给出可读的错误，输入原样退回 composer，用户可以改了再发。
  await page.getByText("Failed to send").first().waitFor({ timeout: 30_000 });
  const input = await composer(page);
  if (!(await input.innerText()).includes(FAULT))
    throw new Error("the failed message should be restored into the composer");
  await page.screenshot({ path: join(shots, "01-send-failed.png") });
  // 详情对话框给出传输层的原因：实例结束时挂着的请求以“transport closed”失败。
  await page.getByRole("button", { name: "Show details" }).first().click();
  const dialog = page.getByRole("dialog").filter({ hasText: "Failed to send" });
  await dialog.getByText("ZCode agent transport closed").waitFor({ timeout: 5_000 });
  await page.screenshot({ path: join(shots, "02-error-details.png") });
  console.info(`[e2e] error details: ${(await dialog.innerText()).replace(/\s+/g, " ")}`);
  await page.keyboard.press("Escape");
  await dialog.waitFor({ state: "hidden", timeout: 5_000 });
  // Host：标记经 Host 环境到达了 runtime 的环境（故障注入只认 runtime 环境里的标记），Engine 循环
  // panic，这个实例结束；之后的请求懒启动新一代。
  const crashed = await waitFor(
    async () => {
      const facts = await runtimeFacts(home, workspace);
      return facts.panics === before.panics + 1 ? facts : null;
    },
    { what: "the runtime panic in the Host log" },
  );
  // 实例结束的日志记下 runtime 自己给出的原因，而不是“输出管道关闭”（rust-u3-single-process.md §2.2）。
  const ended = (await hostLogLines(home)).find((line) =>
    line.includes(`agent[${workspace}#${before.generation}] closed:`),
  );
  if (!ended?.includes("runtime_exit:") || !ended.includes("fault injection"))
    throw new Error(`the Host log should record why the runtime ended: ${ended}`);
  console.info(
    `[e2e] runtime generation ${before.generation} panicked: ${ended.split("closed: ")[1]}`,
  );
  // 退回的草稿含标记：清空后再发正常消息，否则会再次触发故障注入。
  await input.click();
  await page.keyboard.press(process.platform === "darwin" ? "Meta+A" : "Control+A");
  await page.keyboard.press("Backspace");
  return crashed;
}

export async function run(context) {
  const { page, shots, runtime } = context;
  await roundTrip(page, "before the runtime crash");
  const before = await findRunProcesses(context);
  console.info(`[e2e] runtime=${runtime} host pid=${before.host} cli pid=${before.cli}`);
  if (runtime === "child") await crashChild(context, before);
  else await crashEmbedded(context);
  await page.screenshot({ path: join(shots, "03-after-crash.png") });

  // 同一会话里继续发送：subscribe / command 懒启动新一代 runtime。
  await roundTrip(page, "after the runtime crash");
  if (runtime !== "child") {
    const facts = await runtimeFacts(context.home, context.workspace);
    if (facts.panics !== 1)
      throw new Error(`expected exactly one runtime panic, saw ${facts.panics}`);
    if (facts.generation < 2) throw new Error("the runtime was not restarted");
    console.info(`[e2e] the workspace runtime is now generation ${facts.generation}`);
  }
  const after = await findRunProcesses(context, { excludeCli: before.cli ? [before.cli] : [] });
  if (after.host !== before.host)
    throw new Error("the Rust Host should not restart for a runtime crash");
  await page.getByText("Reply to: before the runtime crash").first().waitFor({ timeout: 5_000 });
  await page.screenshot({ path: join(shots, "04-recovered.png") });
  console.info(
    `[e2e] the runtime restarted under the same host (cli pid=${after.cli}); the conversation continued`,
  );
}
