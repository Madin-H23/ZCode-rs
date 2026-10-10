// App 级性能测量的汇总（app-perf.mjs report）：只用有效的测量运行（label = measure、valid），
// 每个指标按配置给出中位数与最小–最大值；离散度大的（(max − min) / 中位数 > 50%）标 †。
import { ROLES } from "./processes.mjs";

const CONFIG_IDS = ["A", "B", "C"];
const MiB = (kib) => (kib === null || kib === undefined ? null : kib / 1024);
const total = (roles, field) =>
  roles ? ROLES.reduce((sum, role) => sum + (roles[role]?.[field] ?? 0), 0) : null;
const snapshotOf = (result, label) => result.snapshots?.find((entry) => entry.label === label);
const median3 = (values) => {
  const clean = values.filter((value) => typeof value === "number");
  if (clean.length === 0) return null;
  const sorted = [...clean].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
};

/** 指标定义：[suite, key, 说明, 单位, (result) => number | null]。 */
function metricDefinitions() {
  const defs = [];
  const add = (suites, key, title, unit, pick) => {
    for (const suite of suites) defs.push({ suite, key, title, unit, pick });
  };
  const both = ["startup", "stream"];
  add(both, "usableMs", "启动 → composer 可点", "ms", (r) => r.coldStart?.usableMs);
  add(both, "hostReadyMs", "启动 → Host 数据库就绪", "ms", (r) => r.coldStart?.hostReadyMs);
  add(["startup"], "sendReadyMs", "启动 → 发送键可用", "ms", (r) => r.coldStart?.sendReadyMs);
  add(
    ["startup"],
    "firstTokenMs",
    "首条消息：Enter → 首段可见",
    "ms",
    (r) => r.coldStart?.firstTokenMs,
  );
  add(
    ["startup"],
    "replyCompleteMs",
    "首条消息：Enter → 回复完整",
    "ms",
    (r) => r.coldStart?.replyCompleteMs,
  );
  add(["startup"], "laterFirstTokenMs", "第 2–8 个 workspace 的首段（中位）", "ms", (r) =>
    median3((r.workspaceTurns ?? []).map((turn) => turn.firstTokenMs)),
  );
  for (const label of ["N1", "N4", "N8"]) {
    add(["startup"], `fp${label}`, `footprint 合计，${label}`, "MiB", (r) =>
      MiB(total(snapshotOf(r, label)?.roles, "footprintKiB")),
    );
    add(["startup"], `rss${label}`, `RSS 合计，${label}`, "MiB", (r) =>
      MiB(total(snapshotOf(r, label)?.roles, "rssKiB")),
    );
  }
  add(
    ["startup"],
    "fpPerWorkspace",
    "每多一个 workspace 的 footprint（(N8 − N1) / 7）",
    "MiB",
    (r) => {
      const n1 = total(snapshotOf(r, "N1")?.roles, "footprintKiB");
      const n8 = total(snapshotOf(r, "N8")?.roles, "footprintKiB");
      return n1 === null || n8 === null ? null : MiB((n8 - n1) / 7);
    },
  );
  add(["stream"], "fpIdle", "footprint 合计，空闲（启动后 30 s）", "MiB", (r) =>
    MiB(total(snapshotOf(r, "idle")?.roles, "footprintKiB")),
  );
  add(["stream"], "rssIdle", "RSS 合计，空闲", "MiB", (r) =>
    MiB(total(snapshotOf(r, "idle")?.roles, "rssKiB")),
  );
  add(
    ["stream"],
    "idleCpuTotal",
    "空闲 60 s（一个 workspace，没有会话）：CPU 合计",
    "%",
    (r) => r.idle?.processes.cpuPercent.total,
  );
  add(["stream"], "idleHostCpu", "空闲 60 s：Host + scheduler + runtime + MCP 的 CPU", "%", (r) =>
    r.idle
      ? ["host", "scheduler", "runtime", "mcp", "tool"].reduce(
          (sum, role) => sum + (r.idle.processes.cpuPercent[role] ?? 0),
          0,
        )
      : null,
  );
  for (const [name, label] of [
    ["one", "1 路流式"],
    ["four", "4 路流式（4 个订阅）"],
    ["fourSteady", "4 路流式（1 个订阅）"],
  ]) {
    const window = (r) => r.streams?.[name];
    add(
      ["stream"],
      `${name}CpuTotal`,
      `${label}：CPU 合计`,
      "%",
      (r) => window(r)?.processes.cpuPercent.total,
    );
    add(["stream"], `${name}FpMean`, `${label}：footprint 合计均值`, "MiB", (r) =>
      MiB(window(r)?.processes.footprint.total.meanKiB),
    );
    add(["stream"], `${name}FpPeak`, `${label}：footprint 合计峰值`, "MiB", (r) =>
      MiB(window(r)?.processes.footprint.total.peakKiB),
    );
    add(
      ["stream"],
      `${name}LongTasks`,
      `${label}：Renderer 长任务个数`,
      "",
      (r) => window(r)?.longTasks.count,
    );
    add(
      ["stream"],
      `${name}LongTaskMax`,
      `${label}：最长的长任务`,
      "ms",
      (r) => window(r)?.longTasks.maxMs,
    );
    add(
      ["stream"],
      `${name}KeyP50`,
      `${label}：按键到字符上屏 p50`,
      "ms",
      (r) => window(r)?.keyLatencyMs?.p50,
    );
    add(
      ["stream"],
      `${name}KeyMax`,
      `${label}：按键到字符上屏 最大`,
      "ms",
      (r) => window(r)?.keyLatencyMs?.max,
    );
    add(
      ["stream"],
      `${name}DeltaP50`,
      `${label}：mock 发出 → DOM 可见 p50`,
      "ms",
      (r) => window(r)?.deltaLatencyMs?.p50,
    );
    add(
      ["stream"],
      `${name}DeltaP95`,
      `${label}：mock 发出 → DOM 可见 p95`,
      "ms",
      (r) => window(r)?.deltaLatencyMs?.p95,
    );
  }
  add(["stream"], "switchMs", "4 路流式：点会话 → 其内容可见（4 次中位）", "ms", (r) =>
    median3(r.streams?.four?.switchLatencyMs ?? []),
  );
  add(["stream"], "toolMs", "工具负载一轮：Enter → 回复完整", "ms", (r) => r.tool?.completeMs);
  // 只计 Host 与 runtime 一侧：采样窗口的长度随进程数变化（A 的 footprint 读数更慢），
  // Renderer 等在窗口尾部的空闲 CPU 会混进合计，不可比。
  add(["stream"], "toolCpuSec", "工具负载一轮：Host + runtime CPU 时间", "s", (r) =>
    r.tool
      ? ["host", "runtime", "mcp", "tool"].reduce(
          (sum, role) => sum + (r.tool.processes.cpuSec[role] ?? 0),
          0,
        )
      : null,
  );
  add(["stream"], "toolFpPeak", "工具负载一轮：footprint 合计峰值", "MiB", (r) =>
    MiB(r.tool?.processes.footprint.total.peakKiB),
  );
  add(
    ["history"],
    "historyRenderedMs",
    "长历史：点开 → 最后一轮可见",
    "ms",
    (r) => r.history?.renderedMs,
  );
  add(
    ["history"],
    "historyScrollMs",
    "长历史：滚到顶 → 第一轮可见",
    "ms",
    (r) => r.history?.scrollToTopMs,
  );
  add(["history"], "fpHistory", "长历史：打开后 footprint 合计", "MiB", (r) =>
    MiB(total(snapshotOf(r, "history")?.roles, "footprintKiB")),
  );
  add(
    ["startup", "stream", "history"],
    "quitAllGoneMs",
    "退出：SIGTERM → 进程树全部结束",
    "ms",
    (r) => r.quit?.allGoneMs,
  );
  add(
    ["startup", "stream", "history"],
    "quitProcesses",
    "退出时的进程数",
    "",
    (r) => r.quit?.processes,
  );
  return defs;
}

function stats(values) {
  const clean = values.filter((value) => typeof value === "number" && Number.isFinite(value));
  if (clean.length === 0) return null;
  const sorted = [...clean].sort((a, b) => a - b);
  const median = median3(sorted);
  const min = sorted[0];
  const max = sorted.at(-1);
  return {
    n: sorted.length,
    median,
    min,
    max,
    values: clean,
    wide: sorted.length > 1 && median !== 0 && (max - min) / Math.abs(median) > 0.5,
  };
}

function format(value, unit) {
  if (value === null || value === undefined) return "—";
  if (unit === "ms" || unit === "MiB") return value >= 100 ? value.toFixed(0) : value.toFixed(1);
  if (unit === "%") return value.toFixed(1);
  if (unit === "s") return value.toFixed(2);
  return value.toFixed(value % 1 === 0 ? 0 : 1);
}

function cell(entry, unit) {
  if (!entry) return "—";
  const range = entry.n > 1 ? ` (${format(entry.min, unit)}–${format(entry.max, unit)})` : "";
  return `${format(entry.median, unit)}${range}${entry.wide ? " †" : ""}`;
}

/** 某个快照里每个角色的 footprint / RSS 中位数（MiB）。 */
function roleTable(runsByConfig, label, suite) {
  const lines = [
    `| 角色 | ${CONFIG_IDS.map((id) => `${id} footprint`).join(" | ")} | ${CONFIG_IDS.map((id) => `${id} RSS`).join(" | ")} |`,
    `| --- | ${CONFIG_IDS.map(() => "---:").join(" | ")} | ${CONFIG_IDS.map(() => "---:").join(" | ")} |`,
  ];
  const summary = {};
  for (const role of [...ROLES, "total"]) {
    const values = {};
    for (const id of CONFIG_IDS) {
      const runs = runsByConfig[id]?.[suite] ?? [];
      const pick = (field) =>
        stats(
          runs.map((run) => {
            const roles = snapshotOf(run, label)?.roles;
            if (!roles) return null;
            return MiB(role === "total" ? total(roles, field) : (roles[role]?.[field] ?? 0));
          }),
        );
      const count = median3(
        runs.map((run) => {
          const roles = snapshotOf(run, label)?.roles;
          return roles
            ? role === "total"
              ? total(roles, "count")
              : (roles[role]?.count ?? 0)
            : null;
        }),
      );
      values[id] = { footprint: pick("footprintKiB"), rss: pick("rssKiB"), count };
    }
    summary[role] = values;
    if (CONFIG_IDS.every((id) => !values[id].count)) continue;
    const name = role === "total" ? "**合计**" : role;
    const fmt = (entry, count) =>
      count ? `${format(entry?.median, "MiB")}${count > 1 ? ` (${count})` : ""}` : "—";
    lines.push(
      `| ${name} | ${CONFIG_IDS.map((id) => fmt(values[id].footprint, values[id].count)).join(" | ")} | ${CONFIG_IDS.map((id) => fmt(values[id].rss, values[id].count)).join(" | ")} |`,
    );
  }
  return { markdown: lines.join("\n"), summary };
}

/**
 * 流式窗口每个角色的 CPU % 与 footprint 均值（中位数）；helper 角色另按种类（network service 等）拆行，
 * 只有 CPU。
 */
function streamRoleTable(runsByConfig, name, source = (run) => run.streams?.[name]) {
  const lines = [
    `| 角色 | ${CONFIG_IDS.map((id) => `${id} CPU %`).join(" | ")} | ${CONFIG_IDS.map((id) => `${id} footprint 均值`).join(" | ")} |`,
    `| --- | ${CONFIG_IDS.map(() => "---:").join(" | ")} | ${CONFIG_IDS.map(() => "---:").join(" | ")} |`,
  ];
  const summary = {};
  for (const role of [...ROLES, "total"]) {
    const values = {};
    for (const id of CONFIG_IDS) {
      const windows = (runsByConfig[id]?.stream ?? []).map(source).filter(Boolean);
      values[id] = {
        cpu: stats(windows.map((window) => window.processes.cpuPercent[role])),
        footprint: stats(
          windows.map((window) => MiB(window.processes.footprint[role]?.meanKiB ?? 0)),
        ),
      };
    }
    summary[role] = values;
    if (CONFIG_IDS.every((id) => !values[id].footprint?.median && !values[id].cpu?.median))
      continue;
    lines.push(
      `| ${role === "total" ? "**合计**" : role} | ${CONFIG_IDS.map((id) => format(values[id].cpu?.median, "%")).join(" | ")} | ${CONFIG_IDS.map((id) => format(values[id].footprint?.median, "MiB")).join(" | ")} |`,
    );
  }
  const windowsOf = (id) => (runsByConfig[id]?.stream ?? []).map(source).filter(Boolean);
  const kinds = new Set(
    CONFIG_IDS.flatMap((id) =>
      windowsOf(id).flatMap((window) => Object.keys(window.processes.helperCpuPercent ?? {})),
    ),
  );
  for (const kind of [...kinds].sort()) {
    const values = Object.fromEntries(
      CONFIG_IDS.map((id) => [
        id,
        stats(windowsOf(id).map((window) => window.processes.helperCpuPercent?.[kind] ?? 0)),
      ]),
    );
    summary[`helper:${kind}`] = values;
    lines.push(
      `| helper: ${kind} | ${CONFIG_IDS.map((id) => format(values[id]?.median, "%")).join(" | ")} | ${CONFIG_IDS.map(() => "").join(" | ")} |`,
    );
  }
  return { markdown: lines.join("\n"), summary };
}

export function renderReport(results) {
  const measured = results.filter((result) => result.label === "measure");
  const valid = measured.filter((result) => result.valid);
  const runsByConfig = {};
  for (const result of valid) {
    ((runsByConfig[result.config] ??= {})[result.suite] ??= []).push(result);
  }
  const sections = [];
  const summary = { runs: {}, metrics: {}, roles: {}, loads: {} };
  for (const result of measured) {
    summary.runs[result.name] = {
      valid: result.valid,
      discarded: result.discarded ?? null,
      error: result.error?.split("\n")[0] ?? null,
      failed: result.checks.filter((check) => !check.ok).map((check) => check.name),
      load1: result.load1,
    };
  }
  const loads = valid.flatMap((result) => [result.load1.start, result.load1.end]).filter(Boolean);
  summary.loads = {
    min: Math.min(...loads),
    max: Math.max(...loads),
    median: median3(loads),
  };
  sections.push(
    `有效运行 ${valid.length} / ${measured.length}；1 分钟 load average（每次运行开始与结束）中位 ${summary.loads.median?.toFixed(1)}，范围 ${summary.loads.min.toFixed(1)}–${summary.loads.max.toFixed(1)}。`,
  );
  const defs = metricDefinitions();
  for (const suite of ["startup", "stream", "history"]) {
    const suiteDefs = defs.filter((def) => def.suite === suite);
    const lines = [
      `### ${suite}`,
      "",
      `| 指标 | ${CONFIG_IDS.join(" | ")} |`,
      `| --- | ${CONFIG_IDS.map(() => "---:").join(" | ")} |`,
    ];
    summary.metrics[suite] = {};
    for (const def of suiteDefs) {
      const entries = Object.fromEntries(
        CONFIG_IDS.map((id) => [id, stats((runsByConfig[id]?.[suite] ?? []).map(def.pick))]),
      );
      summary.metrics[suite][def.key] = { title: def.title, unit: def.unit, ...entries };
      if (CONFIG_IDS.every((id) => !entries[id])) continue;
      lines.push(
        `| ${def.title}${def.unit ? `（${def.unit}）` : ""} | ${CONFIG_IDS.map((id) => cell(entries[id], def.unit)).join(" | ")} |`,
      );
    }
    const counts = CONFIG_IDS.map((id) => `${id} ${(runsByConfig[id]?.[suite] ?? []).length}`);
    sections.push(`${lines.join("\n")}\n\n有效次数：${counts.join("，")}。`);
  }
  for (const [suite, label] of [
    ["startup", "N1"],
    ["startup", "N4"],
    ["startup", "N8"],
    ["stream", "idle"],
    ["history", "history"],
  ]) {
    const table = roleTable(runsByConfig, label, suite);
    summary.roles[`${suite}:${label}`] = table.summary;
    sections.push(
      `### 分角色占用：${suite} / ${label}（MiB，中位数；括号内为进程数）\n\n${table.markdown}`,
    );
  }
  const idle = streamRoleTable(runsByConfig, "idle", (run) => run.idle);
  summary.roles["stream:idleWindow"] = idle.summary;
  sections.push(`### 分角色：空闲 60 s 窗口（CPU 单核 %，footprint MiB）\n\n${idle.markdown}`);
  for (const [name, title] of [
    ["one", "1 路"],
    ["four", "4 路（4 个订阅）"],
    ["fourSteady", "4 路（1 个订阅）"],
  ]) {
    const table = streamRoleTable(runsByConfig, name);
    summary.roles[`stream:${name}`] = table.summary;
    sections.push(`### 分角色：${title}流式窗口（CPU 单核 %，footprint MiB）\n\n${table.markdown}`);
  }
  const tool = streamRoleTable(runsByConfig, "tool", (run) => run.tool);
  summary.roles["stream:tool"] = tool.summary;
  sections.push(
    `### 分角色：工具负载一轮（CPU 单核 %，footprint MiB）\n\nCPU % 按各自的采样窗口计，窗口长度随进程数不同，只适合看同一配置内各角色的比例。\n\n${tool.markdown}`,
  );
  return { markdown: `${sections.join("\n\n")}\n`, summary };
}
