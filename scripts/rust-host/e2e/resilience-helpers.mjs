// 韧性场景（docs/specs/rust-host-resilience.md §6）共用的进程与界面辅助函数。
//
// 只操作本次隔离运行拉起的进程：从 harness 启动的 Electron Main（context.mainPid()）向下找，
// Rust Host 是 Main 的子进程。runtime 链路（context.runtime）决定 Host 下面有什么：
//   embedded  runtime 在 Host 进程内，Host 下没有任何 CLI 进程（docs/specs/rust-u3-single-process.md）；
//   child     每个 workspace 一个 Rust CLI 子进程，以 `--cwd <临时 workspace>` 启动，路径唯一。
import { execFile } from "node:child_process";
import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { promisify } from "node:util";

const run = promisify(execFile);
const HOST_COMMAND = /zcode-host(\.exe)?(\s|$)/;
/** runtime 子进程的特征：Rust CLI 二进制，或任何以 `app-server --stdio` 启动的 CLI（含 TS CLI）。 */
const RUNTIME_COMMAND = /zcode-cli-rust|app-server --stdio/;

/** 当前进程表：pid、ppid、进程组、完整命令行。 */
export async function processTable() {
  const { stdout } = await run("ps", ["-axww", "-o", "pid=,ppid=,pgid=,command="], {
    maxBuffer: 16 * 1024 * 1024,
  });
  return stdout
    .split("\n")
    .map((line) => line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(.*)$/))
    .filter(Boolean)
    .map(([, pid, ppid, pgid, command]) => ({
      pid: Number(pid),
      ppid: Number(ppid),
      pgid: Number(pgid),
      command,
    }));
}

/** `pid` 的所有后代进程（不含它自己）。 */
export function descendantsOf(table, pid) {
  const result = [];
  const queue = [pid];
  while (queue.length > 0) {
    const parent = queue.shift();
    for (const row of table) {
      if (row.ppid === parent) {
        result.push(row);
        queue.push(row.pid);
      }
    }
  }
  return result;
}

/** 本次运行里服务这个 workspace 的 Rust CLI 进程（child 模式）。 */
export async function cliProcesses(workspace) {
  return (await processTable()).filter(
    (row) => row.command.includes("zcode-cli-rust") && row.command.includes(`--cwd ${workspace}`),
  );
}

export async function waitFor(
  check,
  { timeout = 30_000, interval = 250, what = "condition" } = {},
) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, interval));
  }
}

/** Electron Main 下当前存活的 Rust Host（U4 起一个 App 一个，docs/specs/rust-u4-app-host.md）。 */
export async function hostsOf(electronPid) {
  return (await processTable()).filter(
    (row) => row.ppid === electronPid && HOST_COMMAND.test(row.command),
  );
}

/** Host 下的 runtime 子进程（CLI）；embedded 模式下必须为空。 */
export async function runtimeProcessesUnder(hostPid) {
  return descendantsOf(await processTable(), hostPid).filter((row) =>
    RUNTIME_COMMAND.test(row.command),
  );
}

/** embedded 模式的断言：Host 进程树里没有任何 CLI 进程（runtime 运行在 Host 进程内）。 */
export async function assertNoRuntimeProcesses(hostPid) {
  const found = await runtimeProcessesUnder(hostPid);
  if (found.length > 0) {
    throw new Error(
      `embedded runtime expected, but the Host ${hostPid} has CLI processes: ${found
        .map((row) => `${row.pid} ${row.command.slice(0, 160)}`)
        .join("; ")}`,
    );
  }
}

/**
 * 等到本次运行的进程就位，返回 `{ electron, host, cli }`：
 * Host 是 Electron Main 下唯一的 Rust Host；child 模式下 cli 是 Host 下服务这个 workspace 的 CLI，
 * embedded 模式下 cli 为 null，并断言 Host 下没有任何 CLI 进程。
 */
export async function findRunProcesses({ mainPid, runtime, workspace }, options = {}) {
  const electron = mainPid();
  const found = await waitFor(
    async () => {
      const hosts = await hostsOf(electron);
      if (hosts.length !== 1) return null;
      const host = hosts[0].pid;
      if (runtime !== "child") return { electron, host, cli: null };
      const cli = descendantsOf(await processTable(), host).find(
        (row) =>
          row.command.includes("zcode-cli-rust") &&
          row.command.includes(`--cwd ${workspace}`) &&
          !(options.excludeCli ?? []).includes(row.pid),
      );
      return cli ? { electron, host, cli: cli.pid } : null;
    },
    {
      timeout: options.timeout ?? 60_000,
      what: runtime === "child" ? `the CLI process for ${workspace}` : "the Rust Host",
    },
  );
  if (runtime !== "child") await assertNoRuntimeProcesses(found.host);
  return found;
}

/** Main 落盘的日志（含 Host 经控制通道送来的结构化日志），按行。 */
export async function hostLogLines(home) {
  const dir = join(home, ".zcode", "v2", "logs");
  const names = (await readdir(dir).catch(() => [])).filter((name) => name.endsWith(".log"));
  const texts = await Promise.all(names.map((name) => readFile(join(dir, name), "utf8")));
  return texts.join("\n").split("\n");
}

/**
 * 这个 workspace 的 chat runtime 实例的启动代次（Host 每启动一个写一行
 * `agent[<workspace>#<代次>] started pid=<pid>`；嵌入式实例的 pid 为空）。两种链路都适用：
 * embedded 模式下没有 pid 可比，“同一个实例”就是代次没变。
 */
export async function runtimeGenerations(home, workspace) {
  return (await hostLogLines(home))
    .map((line) => line.match(/agent\[(.*)#(\d+)\] started pid=/))
    .filter((match) => match && match[1] === workspace)
    .map((match) => Number(match[2]));
}

export function kill(pid, signal = "SIGKILL") {
  process.kill(pid, signal);
}

/** 业务界面可用：输入框真实可点击（hit-test），不是只看可见性。 */
export async function waitUsable(page, timeout = 90_000) {
  const input = page
    .locator('textarea[placeholder*="Ask ZCode"], [contenteditable="true"]')
    .first();
  await input.waitFor({ state: "visible", timeout });
  await input.click({ trial: true, timeout });
  return input;
}
