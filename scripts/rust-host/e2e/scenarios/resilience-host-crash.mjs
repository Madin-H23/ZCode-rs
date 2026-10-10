// D2 + U4：App 级 Rust Host 崩溃 → Main 自动重启 → 所有主窗口 reload 后可用；60 s 内第 4 次崩溃放弃
// 自动重启，所有主窗口显示失败页（“重新加载 / 退出”），任意一个窗口的重新加载触发一次重启，其余窗口
// 在新 Host 就绪后被 reload（docs/specs/rust-host-resilience.md §2.1、docs/specs/rust-u4-app-host.md §2.8）。
// 两个主窗口（ZCODE_DEV_EXTRA_MAIN_WINDOW=1）共用一个 Host，全程 Electron Main 下最多一个 Host。
//
// 工具进程回收（docs/specs/rust-tool-process-reaping.md）：第一次崩溃前 mock 模型让 runtime 运行一条安静的
// Bash 命令（bash 等待 `sleep 300`），账本里出现它的记录、所有者是 runtime 所在的进程；Host 被 SIGKILL、
// 自动重启、下一个 runtime 实例启动之后，这棵进程树（按记下的 pid 与启动时刻核对）已结束，账本为空。
// 嵌入式链路靠新实例的清扫；子进程链路里 CLI 读到 stdin EOF 自己收口。
import { join } from "node:path";

import { sendMessage, sseText, sseToolCall } from "../helpers.mjs";
import { ledgerEntries, ledgerDir, processIdentities, stillAlive } from "../ledger-helpers.mjs";
import {
  descendantsOf,
  findRunProcesses,
  hostsOf,
  kill,
  processTable,
  waitFor,
  waitUsable,
} from "../resilience-helpers.mjs";
import {
  EXTRA_MAIN_WINDOW_ENV,
  onlyHost,
  roundTrip,
  twoUsableWindows,
} from "../window-helpers.mjs";

const QUIET_PROMPT = "please run a quiet command";
/** 复合命令：bash 不能 exec 成 sleep，必须 fork 并等待它，进程树里有两个进程。 */
const QUIET_COMMAND = "sleep 300; echo quiet-done";
let quietRequested = false;

export const mockRules = [
  {
    // 只应答一次：重启后同一会话的历史里仍有这条用户消息。
    name: "quiet-command",
    match: ({ hasTools, messages, text }) =>
      !quietRequested &&
      hasTools &&
      messages.at(-1)?.role === "user" &&
      text.includes(QUIET_PROMPT),
    respond: ({ response }) => {
      quietRequested = true;
      response.end(
        sseToolCall("Bash", { command: QUIET_COMMAND, description: "Quiet e2e command" }),
      );
    },
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

/**
 * 让 runtime 运行 QUIET_COMMAND，等到账本里出现它的记录、bash 已经 fork 出 sleep；返回记录与这棵树
 * 每个进程的身份（pid、启动时刻、命令行）。权限卡片出现时确认（只读命令通常不需要）。
 */
async function startQuietCommand(page, { home }, owner) {
  const before = new Set((await ledgerEntries(home)).map((entry) => entry.name));
  await sendMessage(page, QUIET_PROMPT);
  const confirm = page.getByRole("button", { name: "Confirm" }).first();
  const found = await waitFor(
    async () => {
      const record = (await ledgerEntries(home)).find(
        (entry) => entry.purpose === "bash" && !before.has(entry.name),
      );
      if (!record) {
        if (await confirm.isVisible().catch(() => false))
          await confirm.click({ timeout: 2_000 }).catch(() => undefined);
        return null;
      }
      const tree = descendantsOf(await processTable(), record.rootPid);
      return tree.some((row) => /(^|\/)sleep 300$/.test(row.command)) ? { record, tree } : null;
    },
    { timeout: 60_000, what: "the quiet Bash command's ledger record and its sleep process" },
  );
  const { record, tree } = found;
  if (record.ownerPid !== owner)
    throw new Error(
      `the ledger record ${record.name} names owner ${record.ownerPid}, expected the runtime's process ${owner}`,
    );
  const identities = await processIdentities([record.rootPid, ...tree.map((row) => row.pid)]);
  if (!identities.has(record.rootPid))
    throw new Error(`the quiet command's root process ${record.rootPid} is gone before the crash`);
  console.info(
    `[e2e] quiet Bash command running: ledger record ${record.name}; tree ${[
      ...identities.entries(),
    ]
      .map(([pid, identity]) => `${pid} "${identity.command}"`)
      .join(", ")}`,
  );
  return { record, identities };
}

/** Host 重启、下一个 runtime 实例启动之后：那棵进程树已结束（同一 pid 与启动时刻的进程不存在），账本为空。 */
async function assertQuietCommandReaped({ home }, quiet) {
  await waitFor(
    async () =>
      (await stillAlive(quiet.identities)).length === 0 && (await ledgerEntries(home)).length === 0,
    { timeout: 30_000, what: "the quiet command's process tree to end and the ledger to empty" },
  ).catch(async (error) => {
    const alive = await stillAlive(quiet.identities);
    const entries = await ledgerEntries(home);
    throw new Error(
      `${error.message}: still alive ${JSON.stringify(alive)}; ledger ${ledgerDir(home)}: ${entries
        .map((entry) => entry.name)
        .join(", ")}`,
    );
  });
  console.info(
    `[e2e] after the restart the quiet command's tree (${[...quiet.identities.keys()].join(", ")}) is gone and the ledger is empty`,
  );
}

/** 失败时的收尾：只结束仍是记下身份的进程（同一 pid、启动时刻与命令行），不按命令名匹配。 */
async function reapLeftovers(quiet) {
  if (!quiet) return;
  for (const [pid] of await stillAlive(quiet.identities)) {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      // 已经退出
    }
  }
}

export async function run(context) {
  let quiet;
  try {
    await crashAndRecover(context, (value) => (quiet = value));
  } catch (error) {
    await reapLeftovers(quiet);
    throw error;
  }
}

async function crashAndRecover(context, remember) {
  const { shots } = context;
  const [first, second] = await twoUsableWindows(context);
  await roundTrip(first, "before the crash");
  const initial = await findRunProcesses(context);
  console.info(`[e2e] host pid=${initial.host}, electron pid=${initial.electron}, 2 windows`);
  // 账本记录的所有者：嵌入式是 Host 进程，子进程链路是这个 workspace 的 CLI。
  const quiet = await startQuietCommand(
    first,
    context,
    context.runtime === "child" ? initial.cli : initial.host,
  );
  remember(quiet);

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
  await assertQuietCommandReaped(context, quiet);
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
