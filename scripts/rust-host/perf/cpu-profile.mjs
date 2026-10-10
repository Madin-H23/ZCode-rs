// 流式 CPU 剖析（stream-cpu.mjs）的 CPU profile 工具：直连 CDP / Node inspector（不经 Playwright，
// 避免 Playwright 打开的 Network 等域本身产生负载），以及把 V8 CPU profile 汇总成“每秒多少 ms CPU”
// 的表：按函数的 self time、按源文件（Renderer 生产包有 hidden sourcemap，映射回 TS 源文件）、
// 按任务入口（每个样本最外层的 JS 帧）。只用于测量。
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(import.meta.dirname, "../../..");
const require = createRequire(join(root, "packages/desktop/package.json"));

/** 一个原始 CDP / inspector WebSocket 连接：call(method, params)、on(event, handler)、close()。 */
export async function connectCdp(url) {
  const socket = new WebSocket(url);
  await new Promise((done, fail) => {
    socket.addEventListener("open", done, { once: true });
    socket.addEventListener("error", () => fail(new Error(`CDP connect failed: ${url}`)), {
      once: true,
    });
  });
  let nextId = 0;
  const pending = new Map();
  const listeners = new Map();
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(String(event.data));
    if (message.id !== undefined) {
      const waiter = pending.get(message.id);
      pending.delete(message.id);
      waiter?.(message);
      return;
    }
    for (const handler of listeners.get(message.method) ?? []) handler(message.params);
  });
  return {
    call(method, params = {}) {
      return new Promise((done, fail) => {
        const id = ++nextId;
        pending.set(id, (message) =>
          message.error
            ? fail(new Error(`${method}: ${message.error.message}`))
            : done(message.result),
        );
        socket.send(JSON.stringify({ id, method, params }));
      });
    },
    on(method, handler) {
      if (!listeners.has(method)) listeners.set(method, []);
      listeners.get(method).push(handler);
    },
    close() {
      socket.close();
    },
  };
}

/** 主窗口 Renderer（加载 /renderer/index.html 的 page target）的 CDP WebSocket 地址。 */
export async function rendererTargetUrl(cdpPort) {
  const response = await fetch(`http://127.0.0.1:${cdpPort}/json/list`);
  const targets = await response.json();
  const target = targets.find(
    (candidate) => candidate.type === "page" && candidate.url.includes("/renderer/index.html"),
  );
  if (!target) throw new Error("renderer page target not found");
  return target.webSocketDebuggerUrl;
}

/** 在一个 CDP / inspector 连接上采一段 CPU profile。 */
export async function profileFor(connection, ms, intervalUs = 250) {
  await connection.call("Profiler.enable");
  await connection.call("Profiler.setSamplingInterval", { interval: intervalUs });
  await connection.call("Profiler.start");
  await new Promise((done) => setTimeout(done, ms));
  const { profile } = await connection.call("Profiler.stop");
  await connection.call("Profiler.disable");
  return profile;
}

// ---------------------------------------------------------------------------------------------
// sourcemap

const consumers = new Map();

async function consumerFor(url) {
  if (!url.startsWith("file://")) return null;
  if (consumers.has(url)) return consumers.get(url);
  const file = fileURLToPath(url);
  let consumer = null;
  if (existsSync(`${file}.map`)) {
    const { SourceMapConsumer } = require("source-map-js");
    consumer = new SourceMapConsumer(JSON.parse(await readFile(`${file}.map`, "utf8")));
  }
  consumers.set(url, consumer);
  return consumer;
}

/** 源文件路径缩短成仓库内的相对路径（node_modules 只留包名）。 */
function shortSource(source) {
  if (!source) return "?";
  const clean = source.replace(/^(\.\.\/)+/, "").replace(/^webpack:\/\//, "");
  const modules = clean.lastIndexOf("node_modules/");
  if (modules >= 0) {
    const rest = clean
      .slice(modules + "node_modules/".length)
      .replace(/^\.pnpm\/[^/]+\/node_modules\//, "");
    const parts = rest.split("/");
    return `npm:${parts[0].startsWith("@") ? `${parts[0]}/${parts[1]}` : parts[0]}`;
  }
  return clean;
}

async function frameInfo(callFrame, cache) {
  const key = `${callFrame.functionName}|${callFrame.url}|${callFrame.lineNumber}|${callFrame.columnNumber}`;
  if (cache.has(key)) return cache.get(key);
  let info;
  if (!callFrame.url) {
    info = {
      fn: callFrame.functionName || "(anonymous)",
      file: callFrame.functionName.startsWith("(") ? callFrame.functionName : "(native)",
    };
  } else {
    const consumer = await consumerFor(callFrame.url);
    const original = consumer?.originalPositionFor({
      line: callFrame.lineNumber + 1,
      column: callFrame.columnNumber,
    });
    if (original?.source) {
      const file = shortSource(original.source);
      info = {
        fn: `${original.name ?? callFrame.functionName ?? "?"}@${file}:${original.line}`,
        file,
      };
    } else {
      const base = callFrame.url.split("/").slice(-2).join("/");
      info = {
        fn: `${callFrame.functionName || "(anonymous)"}@${base}:${callFrame.lineNumber + 1}`,
        file: base,
      };
    }
  }
  cache.set(key, info);
  return info;
}

/**
 * 汇总一份 V8 CPU profile：每个样本的时长记到它的叶子帧（self）与所有祖先帧（total，去重递归）。
 * 返回值单位都是“每秒墙钟的 CPU ms”（profile 时长归一化），只列前 `top` 项。
 */
export async function summarizeProfile(profile, { top = 25 } = {}) {
  const nodes = new Map(profile.nodes.map((node) => [node.id, node]));
  const parent = new Map();
  for (const node of profile.nodes)
    for (const child of node.children ?? []) parent.set(child, node.id);
  const cache = new Map();
  const infoOf = new Map();
  for (const node of profile.nodes) infoOf.set(node.id, await frameInfo(node.callFrame, cache));
  const wallMs = (profile.endTime - profile.startTime) / 1000;
  const self = new Map();
  const total = new Map();
  const files = new Map();
  const entries = new Map();
  const special = new Map();
  let busyMs = 0;
  const add = (map, key, ms) => map.set(key, (map.get(key) ?? 0) + ms);
  const samples = profile.samples ?? [];
  const deltas = profile.timeDeltas ?? [];
  for (let index = 0; index < samples.length; index += 1) {
    // DevTools 的约定：样本 i 的时长是到下一个样本的间隔。
    const ms = (deltas[index + 1] ?? deltas[index] ?? 0) / 1000;
    const leaf = samples[index];
    const leafInfo = infoOf.get(leaf);
    const name = nodes.get(leaf).callFrame.functionName;
    if (
      name === "(idle)" ||
      name === "(program)" ||
      name === "(garbage collector)" ||
      name === "(root)"
    ) {
      add(special, name, ms);
      if (name !== "(idle)") busyMs += ms;
      if (name !== "(garbage collector)") continue;
    } else {
      busyMs += ms;
    }
    add(self, leafInfo.fn, ms);
    add(files, leafInfo.file, ms);
    const seen = new Set();
    let outermost = null;
    for (let id = leaf; id !== undefined; id = parent.get(id)) {
      const node = nodes.get(id);
      if (node.callFrame.functionName === "(root)") break;
      const info = infoOf.get(id);
      if (!seen.has(info.fn)) {
        seen.add(info.fn);
        add(total, info.fn, ms);
      }
      outermost = info.fn;
    }
    if (outermost) add(entries, outermost, ms);
  }
  const perSec = (map) =>
    [...map.entries()]
      .sort((a, b) => b[1] - a[1])
      .slice(0, top)
      .map(([key, ms]) => [key, Math.round((ms / wallMs) * 1000 * 100) / 100]);
  return {
    wallMs: Math.round(wallMs),
    // 采样到的非 idle 时间（JS + GC + “(program)”，即主线程上非 JS 的引擎与 Blink 工作），ms / s。
    busyMsPerSec: Math.round((busyMs / wallMs) * 1000 * 10) / 10,
    special: Object.fromEntries(perSec(special)),
    selfTop: perSec(self),
    totalTop: perSec(total),
    filesTop: perSec(files),
    entriesTop: perSec(entries),
  };
}
