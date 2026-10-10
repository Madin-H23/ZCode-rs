// 流式 CPU 剖析（stream-cpu.mjs）的汇总：<out>/runs/*.json → Markdown 表（中位数与逐次读数）。只用于测量。
import { readdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

// ---------------------------------------------------------------------------------------------
// 汇总

const median = (values) => {
  const sorted = values.filter((value) => typeof value === "number").sort((a, b) => a - b);
  if (sorted.length === 0) return null;
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
};
const fmt = (value, digits = 1) =>
  value === null || value === undefined || Number.isNaN(value) ? "—" : value.toFixed(digits);

/** 一组窗口（同一条件的两次 H 或两次 C）的合并：CPU 按墙钟加权平均，消息数相加。 */
function merge(windows) {
  const list = (Array.isArray(windows) ? windows : [windows]).filter(Boolean);
  if (list.length === 0) return null;
  const seconds = list.reduce((sum, window) => sum + (window.to - window.from) / 1000, 0);
  const roles = {};
  const network = { percent: 0 };
  let total = 0;
  for (const window of list) {
    const weight = (window.to - window.from) / 1000 / seconds;
    for (const [role, value] of Object.entries(window.cpu?.roles ?? {}))
      roles[role] = (roles[role] ?? 0) + value * weight;
    network.percent +=
      (window.cpu?.processes ?? [])
        .filter((row) => /NetworkService/.test(row.kind))
        .reduce((sum, row) => sum + row.percent, 0) * weight;
    total += (window.cpu?.total ?? 0) * weight;
  }
  const tap = {};
  for (const window of list)
    for (const [key, value] of Object.entries(window.tap ?? {})) {
      tap[key] ??= { messages: 0, bytes: 0 };
      tap[key].messages += value.messages;
      tap[key].bytes += value.bytes;
    }
  return { seconds, roles, network: network.percent, total, tap };
}

function rolesOf(merged) {
  const roles = merged?.roles ?? {};
  return {
    Main: roles.main ?? 0,
    Renderer: roles.renderer ?? 0,
    GPU: roles.gpu ?? 0,
    "network service": merged?.network ?? 0,
    其他辅助进程: (roles.helper ?? 0) - (merged?.network ?? 0),
    Host: roles.host ?? 0,
    "runtime 子进程":
      (roles.runtime ?? 0) + (roles.mcp ?? 0) + (roles.tool ?? 0) + (roles.scheduler ?? 0),
    合计: merged?.total ?? 0,
  };
}

function tapRates(merged) {
  const entries = Object.entries(merged?.tap ?? {});
  const sum = (suffix, field) =>
    entries
      .filter(([key]) => key.endsWith(suffix))
      .reduce((total, [, value]) => total + value[field], 0);
  const seconds = merged?.seconds ?? Number.NaN;
  return {
    "收 条/s": sum(".in", "messages") / seconds,
    "收 KiB/s": sum(".in", "bytes") / 1024 / seconds,
    "发 条/s": sum(".out", "messages") / seconds,
    "React 提交/s": (merged?.tap?.["react.commit"]?.messages ?? Number.NaN) / seconds,
  };
}

/** trace 里某个角色的进程（或其中名为 `thread` 的线程）的 CPU ms/s。 */
function traceCpu(trace, role, thread) {
  const process = (trace?.processes ?? []).find((candidate) => candidate.role === role);
  if (!process) return null;
  if (!thread) return process.cpuMsPerSec;
  return process.threads
    .filter((candidate) => candidate.name === thread)
    .reduce((sum, candidate) => sum + candidate.cpuMsPerSec, 0);
}

export async function report(outDir) {
  const runsDir = join(outDir, "runs");
  const runs = [];
  for (const file of (await readdir(runsDir)).filter((name) => name.endsWith(".json")))
    runs.push(JSON.parse(await readFile(join(runsDir, file), "utf8")));
  const valid = runs
    .filter((run) => run.valid && run.steady)
    .sort((a, b) => a.startedAt.localeCompare(b.startedAt));
  const configs = [...new Set(valid.map((run) => run.config))].sort();
  const lines = [];
  const row = (label, pick, digits = 1) => {
    const cells = configs.map((config) => {
      const values = valid.filter((run) => run.config === config).map(pick);
      return `${fmt(median(values), digits)} (${values.map((value) => fmt(value, digits)).join(" / ")})`;
    });
    lines.push(`| ${label} | ${cells.join(" | ")} |`);
  };
  const header = (title) => {
    lines.push("");
    lines.push(`| ${title} | ${configs.join(" | ")} |`);
    lines.push(`| --- | ${configs.map(() => "---:").join(" | ")} |`);
  };
  lines.push(
    `有效运行（按时间）：${valid
      .map(
        (run) =>
          `${run.name}（${run.startedAt.slice(11, 16)} UTC，load ${fmt(run.load1.start)} / ${fmt(run.load1.end)}）`,
      )
      .join("，")}`,
  );
  lines.push(
    "表中是中位数，括号里是逐次读数。H = Playwright 连着、探针开着；C = 断开 Playwright、暂停探针。",
  );
  const windows = [
    ["report", "report（与 App 级报告相同：H + 打字与切换会话）", (run) => merge(run.report)],
    ["warm H", "warm H（4 个会话都订阅着）", (run) => merge(run.warm?.harness)],
    ["warm C", "warm C", (run) => merge(run.warm?.clean)],
    ["steady H", "steady H（保温结束，只订阅可见会话）", (run) => merge(run.steady?.harness)],
    ["steady C", "steady C", (run) => merge(run.steady?.clean)],
  ];
  for (const [, title, pick] of windows) {
    header(`${title}，单核 %`);
    for (const role of Object.keys(rolesOf(null))) row(role, (run) => rolesOf(pick(run))[role]);
  }
  header("Renderer 收发的 RPC 消息（同一个计数补丁）");
  for (const [label, , pick] of windows)
    for (const field of ["收 条/s", "收 KiB/s", "发 条/s", "React 提交/s"])
      row(`${label} ${field}`, (run) => tapRates(pick(run))[field]);
  header("抓包分类（steady，H 条件 3 s）：条/s 与平均字节");
  const classes = new Set(valid.flatMap((run) => Object.keys(run.capture?.classes ?? {})));
  for (const key of classes) {
    row(`${key.slice(0, 110)} 条/s`, (run) => run.capture?.classes?.[key]?.perSec ?? 0);
    row(`${key.slice(0, 110)} 字节`, (run) => run.capture?.classes?.[key]?.avgBytes ?? null, 0);
  }
  row(
    "CDP Network.webSocketFrameReceived 条/s",
    (run) => run.capture?.cdpWebSocketFrames?.perSec ?? null,
  );
  header("trace（steady，contentTracing 5 s）：线程 CPU ms/s");
  for (const [label, key] of [
    ["H", "harnessTrace"],
    ["C", "cleanTrace"],
  ]) {
    const trace = (run) => run.steady?.[key];
    row(`${label} Main 进程`, (run) => traceCpu(trace(run), "main"));
    row(`${label} Main DevTools 线程`, (run) =>
      traceCpu(trace(run), "main", "Chrome_DevToolsHandlerThread"),
    );
    row(`${label} Main CrBrowserMain`, (run) => traceCpu(trace(run), "main", "CrBrowserMain"));
    row(`${label} Renderer 进程`, (run) => traceCpu(trace(run), "renderer"));
    row(`${label} Renderer 主线程`, (run) => traceCpu(trace(run), "renderer", "CrRendererMain"));
    row(`${label} Renderer Compositor`, (run) => traceCpu(trace(run), "renderer", "Compositor"));
    row(`${label} GPU 进程`, (run) => traceCpu(trace(run), "gpu"));
  }
  header("CPU profile（warm C，8 s）：Renderer 主线程 ms/s");
  row("Renderer 非 idle", (run) => run.warm?.profile?.renderer?.busyMsPerSec);
  row("Renderer (program)", (run) => run.warm?.profile?.renderer?.special?.["(program)"]);
  row("Renderer GC", (run) => run.warm?.profile?.renderer?.special?.["(garbage collector)"]);
  header("CPU profile（steady C，直连 CDP / inspector，10 s）：主线程 ms/s");
  row("Renderer 非 idle", (run) => run.profile?.renderer?.busyMsPerSec);
  row("Renderer (program)", (run) => run.profile?.renderer?.special?.["(program)"]);
  row("Renderer GC", (run) => run.profile?.renderer?.special?.["(garbage collector)"]);
  row("Main 非 idle", (run) => run.profile?.main?.busyMsPerSec);
  header("Main 日志行 / s（steady）");
  row("H", (run) => median(run.steady.harness.map((window) => window.mainLog?.linesPerSec)));
  row("C", (run) => median(run.steady.clean.map((window) => window.mainLog?.linesPerSec)));
  const markdown = `${lines.join("\n")}\n`;
  await writeFile(join(outDir, "summary.md"), markdown);
  console.info(markdown);
}
