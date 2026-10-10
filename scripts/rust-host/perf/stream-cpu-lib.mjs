// 流式 CPU 剖析（stream-cpu.mjs）的窗口工具：分进程 CPU（ps 的累计 CPU 时间差）、Main 的 EventEmitter
// 事件计数与日志行数、重新连接 Playwright、B 的 CDP WebSocket 帧计数、4 路流的覆盖检查。只用于测量。
import { createRequire } from "node:module";
import { join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

import { logTime } from "../e2e/app.mjs";
import { hostLogLines } from "../e2e/resilience-helpers.mjs";
import { classifyTree, processUsageTable } from "./processes.mjs";

const ROOT = resolve(import.meta.dirname, "../../..");
const requireDesktop = createRequire(join(ROOT, "packages/desktop/package.json"));

// ---------------------------------------------------------------------------------------------
// 进程 CPU

/** 一个辅助进程的种类：utility 的 sub-type（network.mojom.NetworkService 等）或 --type。 */
export function helperKind(command) {
  return (
    command.match(/--utility-sub-type=([^\s]+)/)?.[1] ??
    command.match(/--type=([^\s]+)/)?.[1] ??
    command.split(/\s+/)[0].split("/").pop()
  );
}

export async function cpuTable(mainPid, rendererPids) {
  return classifyTree(await processUsageTable(), mainPid, rendererPids);
}

/** 两次进程表之间每个角色、每个辅助进程的单核 CPU %。 */
export function cpuBetween(before, after, wallSec) {
  const first = new Map(before.map((row) => [row.pid, row]));
  const roles = {};
  const processes = [];
  for (const row of after) {
    const base = first.get(row.pid)?.cpuSec ?? 0;
    const percent = ((row.cpuSec - base) / wallSec) * 100;
    roles[row.role] = (roles[row.role] ?? 0) + percent;
    processes.push({
      pid: row.pid,
      role: row.role,
      kind: row.role === "helper" || row.role === "gpu" ? helperKind(row.command) : row.role,
      percent: Math.round(percent * 100) / 100,
    });
  }
  const total = Object.values(roles).reduce((sum, value) => sum + value, 0);
  return {
    wallSec,
    total: Math.round(total * 10) / 10,
    roles: Object.fromEntries(
      Object.entries(roles).map(([role, value]) => [role, Math.round(value * 100) / 100]),
    ),
    processes: processes.filter((row) => row.percent > 0.05).sort((a, b) => b.percent - a.percent),
  };
}

// ---------------------------------------------------------------------------------------------
// Main：EventEmitter 事件计数与日志行数

export const EMIT_TAP_INSTALL = `(() => {
  const proto = require("events").EventEmitter.prototype;
  if (globalThis.__perfEmitTap) return "already";
  const original = proto.emit;
  const tap = { original, counts: new Map() };
  globalThis.__perfEmitTap = tap;
  proto.emit = function (event, ...args) {
    let key = typeof event === "string" ? event : String(event);
    if (event === "message" || event === "line") {
      const first = args[0];
      let type;
      if (typeof first === "string") type = first.match(/"type":"([^"]+)"/)?.[1] ?? "text";
      else if (first && typeof first === "object")
        type = first.type ?? first.data?.type ?? (first.ports ? "ports" : "object");
      else type = typeof first;
      key = key + " " + (this?.constructor?.name ?? "?") + " " + type;
    }
    tap.counts.set(key, (tap.counts.get(key) ?? 0) + 1);
    return original.call(this, event, ...args);
  };
  return "installed";
})()`;

export const EMIT_TAP_TAKE = `(() => {
  const tap = globalThis.__perfEmitTap;
  if (!tap) return [];
  require("events").EventEmitter.prototype.emit = tap.original;
  delete globalThis.__perfEmitTap;
  return [...tap.counts.entries()].sort((a, b) => b[1] - a[1]).slice(0, 80);
})()`;

/** Main 落盘日志里 [from, to] 内的行数，按类别（去掉时间戳、数字归一）。 */
export async function mainLogCounts(home, from, to) {
  const lines = (await hostLogLines(home)).filter((line) => {
    const at = logTime(line);
    return at >= from && at <= to;
  });
  const categories = new Map();
  for (const line of lines) {
    const body = line.replace(/^\[[^\]]+\]\s*/, "").replace(/^\[(info|warn|error|debug)\]\s*/i, "");
    const key = body
      .slice(0, 90)
      .replace(/[0-9a-f]{8}-[0-9a-f-]{27,}/gi, "<id>")
      .replace(/\d+/g, "#");
    categories.set(key, (categories.get(key) ?? 0) + 1);
  }
  const seconds = (to - from) / 1000;
  return {
    linesPerSec: Math.round((lines.length / seconds) * 10) / 10,
    top: [...categories.entries()]
      .sort((a, b) => b[1] - a[1])
      .slice(0, 15)
      .map(([key, count]) => [key, Math.round((count / seconds) * 10) / 10]),
  };
}

// ---------------------------------------------------------------------------------------------
// 一次运行

export async function cpuWindow(mainPid, rendererPids, ms) {
  const before = await cpuTable(mainPid, rendererPids);
  const from = Date.now();
  await sleep(ms);
  const after = await cpuTable(mainPid, rendererPids);
  const to = Date.now();
  return { from, to, cpu: cpuBetween(before, after, (to - from) / 1000) };
}

export function streamsCover(log, tags, from, to) {
  return tags.every((tag) => {
    const sent = log.sent[tag] ?? [];
    return sent.some((entry) => entry.sentAt <= from) && sent.some((entry) => entry.sentAt >= to);
  });
}

export async function connectPage(cdpPort) {
  const { chromium } = requireDesktop("playwright-core");
  const browser = await chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`);
  const page = browser
    .contexts()
    .flatMap((context) => context.pages())
    .find((candidate) => candidate.url().includes("index.html"));
  if (!page) throw new Error("renderer page not found after reconnecting");
  return { browser, page };
}

/** B 的 CDP 口径：在一个 CDP 会话里开 Network 域，数 Renderer 收到的 WebSocket 帧。 */
export async function countCdpFrames(page, ms) {
  const session = await page.context().newCDPSession(page);
  const frames = { received: 0, payloadChars: 0, sent: 0 };
  session.on("Network.webSocketFrameReceived", (event) => {
    frames.received += 1;
    frames.payloadChars += event.response.payloadData?.length ?? 0;
  });
  session.on("Network.webSocketFrameSent", () => {
    frames.sent += 1;
  });
  await session.send("Network.enable");
  await sleep(ms);
  await session.send("Network.disable").catch(() => undefined);
  await session.detach().catch(() => undefined);
  return { ...frames, perSec: Math.round((frames.received / (ms / 1000)) * 10) / 10 };
}
