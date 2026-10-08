// 韧性场景（docs/specs/rust-host-resilience.md §6）共用的进程与界面辅助函数。
//
// 只操作本次隔离运行拉起的进程：CLI 以 `--cwd <临时 workspace>` 启动，路径唯一；
// Rust Host 是这个 CLI 的父进程，Electron Main 又是 Rust Host 的父进程。
import { execFile } from "node:child_process";
import { promisify } from "node:util";

const run = promisify(execFile);

/** 当前进程表：pid、ppid、完整命令行。 */
export async function processTable() {
  const { stdout } = await run("ps", ["-axww", "-o", "pid=,ppid=,command="], {
    maxBuffer: 16 * 1024 * 1024,
  });
  return stdout
    .split("\n")
    .map((line) => line.trim().match(/^(\d+)\s+(\d+)\s+(.*)$/))
    .filter(Boolean)
    .map(([, pid, ppid, command]) => ({ pid: Number(pid), ppid: Number(ppid), command }));
}

/** 本次运行里服务这个 workspace 的 Rust CLI 进程。 */
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

/** 等到 workspace 有一个 CLI；返回它和它的 Rust Host（父进程）以及 Electron Main（祖父进程）。 */
export async function findRunProcesses(workspace, options = {}) {
  return waitFor(
    async () => {
      const table = await processTable();
      const cli = table.find(
        (row) =>
          row.command.includes("zcode-cli-rust") &&
          row.command.includes(`--cwd ${workspace}`) &&
          !(options.excludeCli ?? []).includes(row.pid),
      );
      const host = cli && table.find((row) => row.pid === cli.ppid);
      if (!host || !/zcode-host(\.exe)?(\s|$)/.test(host.command)) return null;
      return { cli: cli.pid, host: host.pid, electron: host.ppid };
    },
    { timeout: options.timeout ?? 60_000, what: `CLI process for ${workspace}` },
  );
}

/** Electron Main 下当前存活的 Rust Host（每个窗口一个）。 */
export async function hostsOf(electronPid) {
  return (await processTable()).filter(
    (row) => row.ppid === electronPid && /zcode-host(\.exe)?(\s|$)/.test(row.command),
  );
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
