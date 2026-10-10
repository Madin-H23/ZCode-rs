// 工具进程账本（docs/specs/rust-tool-process-reaping.md §2.1）的读取与断言。
//
// runtime（嵌入 Host 或 CLI 子进程，两种链路相同）把它启动的每一棵 Bash / hook / MCP stdio 进程树记在
// `<数据目录>/tool-results/processes/` 下，一棵树一个空文件，确认结束后删除。harness 把数据目录固定为
// 隔离 HOME 里的 `.zcode/rust`（ZCODE_CLI_RUST_DATA_DIR，见 e2e/app.mjs），这里只读这个目录，
// 不读写开发者的 ~/.zcode。
import { execFile } from "node:child_process";
import { readdir } from "node:fs/promises";
import { join } from "node:path";
import { promisify } from "node:util";

const run = promisify(execFile);

/** harness 给 Rust runtime 的数据目录（launchApp 以 ZCODE_CLI_RUST_DATA_DIR 传入）。 */
export function rustDataDir(home) {
  return join(home, ".zcode", "rust");
}

export function ledgerDir(home) {
  return join(rustDataDir(home), "tool-results", "processes");
}

/** 记录文件名：`<用途>-<根 pid>-<根启动时间>-<所有者 pid>-<所有者启动时间>-<实例>`。 */
const RECORD = /^(bash|hook|mcp)-(\d+)-([^-]+)-(\d+)-([^-]+)-(\d+)$/;

/** 账本目录里的全部条目；符合记录格式的解析出用途、根 pid 与所有者 pid。目录不存在时为空。 */
export async function ledgerEntries(home) {
  const names = await readdir(ledgerDir(home)).catch((error) =>
    error.code === "ENOENT" ? [] : Promise.reject(error),
  );
  return names.map((name) => {
    const match = name.match(RECORD);
    return match
      ? { name, purpose: match[1], rootPid: Number(match[2]), ownerPid: Number(match[4]) }
      : { name, purpose: null, rootPid: null, ownerPid: null };
  });
}

/**
 * App 正常退出之后账本为空：runtime 收口时确认结束了它启动的每一棵进程树并删除记录；
 * 被强杀的 runtime 留下的记录由下一个实例启动时的清扫处理。只适用于 Rust runtime（TS CLI 不写账本）。
 */
export async function assertLedgerEmpty(home) {
  const entries = await ledgerEntries(home);
  if (entries.length > 0)
    throw new Error(
      `tool process ledger not empty after a clean quit (${ledgerDir(home)}): ${entries
        .map((entry) => entry.name)
        .join(", ")}`,
    );
  console.info("[e2e] tool process ledger empty after the clean quit");
}

/**
 * 进程的身份快照：pid → { started, command }（`ps -o lstart`，秒级启动时刻）。不存在的 pid 不在结果里。
 * 用来确认“那个进程”已经结束，而不是 pid 被别的进程复用后误判为仍存活。
 */
export async function processIdentities(pids) {
  if (pids.length === 0) return new Map();
  const { stdout } = await run("ps", ["-ww", "-o", "pid=,lstart=,command=", "-p", pids.join(",")], {
    maxBuffer: 4 * 1024 * 1024,
  }).catch((error) => (error.code === 1 ? { stdout: "" } : Promise.reject(error)));
  const result = new Map();
  for (const line of stdout.split("\n")) {
    // lstart 固定 5 段：`Fri Oct 10 08:59:23 2026`。
    const match = line.trim().match(/^(\d+)\s+(\S+\s+\S+\s+\d+\s+[\d:]+\s+\d+)\s+(.*)$/);
    if (match) result.set(Number(match[1]), { started: match[2], command: match[3] });
  }
  return result;
}

/** `before`（processIdentities 的结果）里仍以同一身份存活的进程。 */
export async function stillAlive(before) {
  const now = await processIdentities([...before.keys()]);
  return [...before.entries()].filter(([pid, identity]) => {
    const current = now.get(pid);
    return current && current.started === identity.started && current.command === identity.command;
  });
}
