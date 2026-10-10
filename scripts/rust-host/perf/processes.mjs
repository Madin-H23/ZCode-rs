// App 级性能测量（scripts/rust-host/perf/app-perf.mjs）的进程层：从 harness 拉起的 Electron Main
// 向下遍历进程树，把每个进程归到一个角色，并读取 RSS（ps）、macOS physical footprint（footprint）
// 与累计 CPU 时间（ps time）。
//
// 角色（ROLES）：
//   main       harness 启动的 Electron Main
//   renderer   主窗口的 Renderer（Main 的 webContents 里加载 index.html 的那个）
//   renderer-other  其他 Renderer（隐藏窗口等）
//   gpu        GPU 进程
//   helper     其他 Electron 辅助进程（network / storage 等 utility、crashpad）
//   host       Node Host（utility 进程，标题 zcode-host-local-<窗口>）或 Rust Host（zcode-host）
//   scheduler  Node Host 以外的 Node utility 进程（Node scheduler，serviceName zcode-cron-scheduler）
//   runtime    CLI 子进程（TS CLI：Electron-as-Node 运行 zcode.cjs，进程标题改成 zcode-cli；Rust CLI：zcode-cli-rust）
//   mcp        Host 或 runtime 下的 MCP 服务进程（TS CLI 为官方插件起的 node_repl 宿主，标题 zcode-node-repl-mcp）
//   tool       Host 或 runtime 下的其他子进程：Bash / git / PTY shell / 存储准备等
//   other      不属于以上的 Main 后代
import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";

import { descendantsOf } from "../e2e/resilience-helpers.mjs";

const exec = promisify(execFile);

export const ROLES = [
  "main",
  "renderer",
  "renderer-other",
  "gpu",
  "helper",
  "host",
  "scheduler",
  "runtime",
  "mcp",
  "tool",
  "other",
];

const NODE_HOST_TITLE = /^zcode-host-local-\d+\s*$/;
const RUST_HOST = /(^|\/)zcode-host(\s|$)/;
const NODE_UTILITY = /--utility-sub-type=node\.mojom\.NodeService/;
const RUNTIME = /^zcode-cli(\s|$)|zcode-cli-rust|app-server --stdio/;
const MCP = /mcp/i;

/** `ps` 的累计 CPU 时间（`M:SS.ss` 或 `H:MM:SS.ss`）→ 秒。 */
function cpuSeconds(text) {
  return text
    .split(":")
    .map(Number)
    .reduce((total, part) => total * 60 + part, 0);
}

/** 进程表：pid、ppid、RSS（KiB）、累计 CPU（秒）、完整命令行。 */
export async function processUsageTable() {
  const { stdout } = await exec("ps", ["-axww", "-o", "pid=,ppid=,rss=,time=,command="], {
    maxBuffer: 32 * 1024 * 1024,
  });
  return stdout
    .split("\n")
    .map((line) => line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+([\d:.]+)\s+(.*)$/))
    .filter(Boolean)
    .map(([, pid, ppid, rss, time, command]) => ({
      pid: Number(pid),
      ppid: Number(ppid),
      rssKiB: Number(rss),
      cpuSec: cpuSeconds(time),
      command,
    }));
}

/**
 * Main 进程里所有 webContents 的 OS pid 与 URL（经 Main inspector）。主窗口的 Renderer 加载
 * `/renderer/index.html`；其他 webContents 归 renderer-other。
 */
export const WEB_CONTENTS_EXPRESSION = `require("electron").webContents.getAllWebContents()
  .filter((wc) => !wc.isDestroyed())
  .map((wc) => ({ pid: wc.getOSProcessId(), url: wc.getURL(), type: wc.getType() }))`;

export async function mainWindowRendererPids(main) {
  const contents = await main.evaluate(WEB_CONTENTS_EXPRESSION);
  return new Set(
    contents.filter((wc) => wc.url.includes("/renderer/index.html")).map((wc) => wc.pid),
  );
}

/** Main 及其全部后代，每个带上角色。 */
export function classifyTree(table, mainPid, rendererPids) {
  const main = table.find((row) => row.pid === mainPid);
  if (!main) return [];
  const rows = [main, ...descendantsOf(table, mainPid)];
  const byPid = new Map(rows.map((row) => [row.pid, row]));
  const roleOf = new Map();
  const direct = (row) => {
    if (row.pid === mainPid) return "main";
    if (/--type=renderer/.test(row.command))
      return rendererPids.has(row.pid) ? "renderer" : "renderer-other";
    if (/--type=gpu-process/.test(row.command)) return "gpu";
    if (NODE_HOST_TITLE.test(row.command.trim())) return "host";
    if (row.ppid === mainPid && RUST_HOST.test(row.command)) return "host";
    if (NODE_UTILITY.test(row.command)) return "scheduler";
    if (RUNTIME.test(row.command)) return "runtime";
    if (/--type=/.test(row.command)) return "helper";
    return null;
  };
  const resolve = (row) => {
    if (roleOf.has(row.pid)) return roleOf.get(row.pid);
    let role = direct(row);
    if (!role) {
      // 祖先里有 Host 或 runtime 的，是它们启动的 MCP 服务、工具与其他子进程。
      let parent = byPid.get(row.ppid);
      while (parent && !role) {
        const parentRole = direct(parent);
        if (parentRole === "host" || parentRole === "runtime")
          role = MCP.test(row.command) ? "mcp" : "tool";
        parent = byPid.get(parent.ppid);
      }
      role ??= "other";
    }
    roleOf.set(row.pid, role);
    return role;
  };
  return rows.map((row) => ({ ...row, role: resolve(row) }));
}

/** macOS physical footprint（活动监视器的“内存”）：pid → { footprintKiB, peakKiB }。不存在的 pid 不在结果里。 */
export async function footprints(pids) {
  const result = new Map();
  if (pids.length === 0 || process.platform !== "darwin") return result;
  const dir = await mkdtemp(join(tmpdir(), "zcode-app-perf-fp-"));
  const file = join(dir, "footprint.json");
  try {
    await exec(
      "footprint",
      ["--noCategories", "-j", file, ...pids.flatMap((pid) => ["-p", String(pid)])],
      { maxBuffer: 16 * 1024 * 1024 },
    ).catch(() => undefined);
    const json = JSON.parse(await readFile(file, "utf8").catch(() => "{}"));
    for (const entry of json.processes ?? []) {
      result.set(entry.pid, {
        footprintKiB: Math.round((entry.auxiliary?.phys_footprint ?? entry.footprint) / 1024),
        peakKiB: Math.round((entry.auxiliary?.phys_footprint_peak ?? 0) / 1024),
      });
    }
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
  return result;
}

function emptyRoleTotals() {
  return Object.fromEntries(
    ROLES.map((role) => [
      role,
      { count: 0, rssKiB: 0, footprintKiB: 0, peakFootprintKiB: 0, cpuSec: 0 },
    ]),
  );
}

/** 一次快照：每个角色的进程数、RSS、footprint（含进程生命期峰值）与累计 CPU；另附进程明细。 */
export async function snapshotTree(mainPid, rendererPids) {
  const rows = classifyTree(await processUsageTable(), mainPid, rendererPids);
  const prints = await footprints(rows.map((row) => row.pid));
  const roles = emptyRoleTotals();
  for (const row of rows) {
    const totals = roles[row.role];
    const print = prints.get(row.pid);
    totals.count += 1;
    totals.rssKiB += row.rssKiB;
    totals.cpuSec += row.cpuSec;
    totals.footprintKiB += print?.footprintKiB ?? 0;
    totals.peakFootprintKiB += print?.peakKiB ?? 0;
  }
  return {
    at: Date.now(),
    roles,
    processes: rows.map((row) => ({
      pid: row.pid,
      ppid: row.ppid,
      role: row.role,
      rssKiB: row.rssKiB,
      cpuSec: row.cpuSec,
      footprintKiB: prints.get(row.pid)?.footprintKiB ?? null,
      command: row.command.slice(0, 160),
    })),
  };
}

/**
 * 窗口采样：每 `intervalMs` 读一次进程表（RSS、CPU），每 `footprintEveryMs` 读一次 footprint。
 * stop() 返回窗口内每个角色的 CPU 百分比（单核 %，= CPU 时间差 / 墙钟）、RSS 与 footprint 的均值和峰值。
 * 窗口内结束的进程只计到最后一次采样（短命的工具进程 CPU 偏低，报告里说明）。
 */
export function startWindowSampler(
  mainPid,
  rendererPids,
  { intervalMs = 500, footprintEveryMs = 2000 } = {},
) {
  const firstCpu = new Map();
  const lastCpu = new Map();
  const roleOfPid = new Map();
  const rssSeries = [];
  const footprintSeries = [];
  let stopped = false;
  let lastFootprintAt = 0;
  const startedAt = Date.now();
  const loop = (async () => {
    let first = true;
    while (!stopped) {
      const tickAt = Date.now();
      const rows = classifyTree(await processUsageTable(), mainPid, rendererPids);
      const rss = Object.fromEntries(ROLES.map((role) => [role, 0]));
      for (const row of rows) {
        roleOfPid.set(row.pid, row.role);
        // 窗口开始前已存在的进程以首个样本为基准；窗口内新出现的进程从 0 起算。
        if (!firstCpu.has(row.pid)) firstCpu.set(row.pid, first ? row.cpuSec : 0);
        lastCpu.set(row.pid, row.cpuSec);
        rss[row.role] += row.rssKiB;
      }
      rssSeries.push(rss);
      if (tickAt - lastFootprintAt >= footprintEveryMs) {
        lastFootprintAt = tickAt;
        const prints = await footprints(rows.map((row) => row.pid));
        const totals = Object.fromEntries(ROLES.map((role) => [role, 0]));
        for (const row of rows) totals[row.role] += prints.get(row.pid)?.footprintKiB ?? 0;
        footprintSeries.push(totals);
      }
      first = false;
      const wait = intervalMs - (Date.now() - tickAt);
      if (wait > 0) await new Promise((resolve) => setTimeout(resolve, wait));
    }
  })();
  return {
    async stop() {
      stopped = true;
      await loop;
      const wallSec = (Date.now() - startedAt) / 1000;
      const cpu = Object.fromEntries(ROLES.map((role) => [role, 0]));
      for (const [pid, last] of lastCpu) cpu[roleOfPid.get(pid)] += last - firstCpu.get(pid);
      const stats = (series) =>
        Object.fromEntries(
          [...ROLES, "total"].map((role) => {
            const values = series.map((sample) =>
              role === "total" ? ROLES.reduce((sum, name) => sum + sample[name], 0) : sample[role],
            );
            return [
              role,
              {
                meanKiB: values.length
                  ? Math.round(values.reduce((a, b) => a + b, 0) / values.length)
                  : 0,
                peakKiB: values.length ? Math.max(...values) : 0,
              },
            ];
          }),
        );
      const cpuPercent = Object.fromEntries(
        ROLES.map((role) => [role, (cpu[role] / wallSec) * 100]),
      );
      cpuPercent.total = ROLES.reduce((sum, role) => sum + cpuPercent[role], 0);
      return {
        wallSec,
        samples: rssSeries.length,
        footprintSamples: footprintSeries.length,
        cpuSec: cpu,
        cpuPercent,
        rss: stats(rssSeries),
        footprint: stats(footprintSeries),
      };
    },
  };
}

/** 退出前 Main 的全部后代（含 Main）的身份：pid + 命令行。 */
export async function treeIdentities(mainPid) {
  const table = await processUsageTable();
  const main = table.find((row) => row.pid === mainPid);
  return [...(main ? [main] : []), ...descendantsOf(table, mainPid)].map((row) => ({
    pid: row.pid,
    command: row.command,
  }));
}

/** `identities` 里仍以同一命令行存活的进程。 */
export async function stillRunning(identities) {
  const table = await processUsageTable();
  const alive = new Map(table.map((row) => [row.pid, row.command]));
  return identities.filter((identity) => alive.get(identity.pid) === identity.command);
}
