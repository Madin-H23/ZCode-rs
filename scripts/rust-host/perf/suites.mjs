// App 级性能测量的 suite（runner.mjs 的 runSuite 调用）：每个 suite 拿到一个已经可用的 App
// （context = { app, page, result, log, workspaces, home, spawnAt }），把测量结果写进 context.result。
import { loadavg } from "node:os";
import { setTimeout as sleep } from "node:timers/promises";

import { composer, submitComposer } from "../e2e/helpers.mjs";
import { endMarker, expectedMarkers, HISTORY_TURNS, historyMarker, startMarker } from "./mock.mjs";
import {
  lastEnter,
  longTasksBetween,
  measureClickUntilVisible,
  pageNow,
  seenMarkers,
  takeKeyLatencies,
  waitMarker,
  watchMarkers,
} from "./page-probes.mjs";
import { mainWindowRendererPids, snapshotTree, startWindowSampler } from "./processes.mjs";

export const WORKSPACE_COUNT = 8;
/** 稳态快照之前的静置：Renderer 离开会话后保温 30 s 才释放（v4 session data lease 的 keepWarmMs）。 */
const SETTLE_MS = 35_000;
const IDLE_SETTLE_MS = 30_000;
const STREAM_WINDOW_MS = 20_000;

function percentile(values, p) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1))];
}

function distribution(values) {
  return {
    n: values.length,
    p50: percentile(values, 50),
    p95: percentile(values, 95),
    max: values.length ? Math.max(...values) : null,
  };
}

/** 在首页 composer 选定 workspace（侧栏 New task → 选择项目菜单）。 */
async function newTaskIn(page, workspaceName) {
  await page.getByText("New task", { exact: true }).first().click();
  const picker = page.locator('[aria-label="Choose workspace"]').first();
  await picker.waitFor({ state: "visible", timeout: 30_000 });
  if ((await picker.innerText()).trim() !== workspaceName) {
    // 选择项目的弹层带搜索框，列表只显示前几个：先按名字过滤再点；选中后弹层不自动关闭，按 Escape 关掉，
    // 否则 Enter 落在弹层里，消息发不出去。
    await picker.click();
    const search = page.locator('input[placeholder="Search workspaces"]').first();
    await search.waitFor({ state: "visible", timeout: 30_000 });
    await search.fill(workspaceName);
    await page.getByText(workspaceName, { exact: true }).last().click();
    await page.waitForFunction(
      (name) =>
        document.querySelector('[aria-label="Choose workspace"]')?.textContent?.trim() === name,
      workspaceName,
      { timeout: 30_000 },
    );
    if (await search.isVisible().catch(() => false)) await page.keyboard.press("Escape");
    await search.waitFor({ state: "hidden", timeout: 10_000 });
  }
}

/**
 * 输入并发送一条消息；返回 { sendReadyAt, enter }：发送键可用的时刻（harness 时钟）与页面记录的
 * Enter keydown 时刻。
 */
export async function send(page, text) {
  const input = await composer(page);
  await input.click();
  await page.keyboard.insertText(text);
  await submitComposer(page);
  const sendReadyAt = Date.now();
  const enter = await lastEnter(page);
  return { sendReadyAt, enter };
}

/** 发一条短回复并等它完整出现：首段、末段相对 Enter 的时间，以及所有标记都按序出现。 */
async function replyTurn(page, tag) {
  const markers = expectedMarkers(tag, 40);
  await watchMarkers(page, markers);
  const sent = await send(page, `perf:reply:${tag}`);
  const first = await waitMarker(page, startMarker(tag));
  const done = await waitMarker(page, endMarker(tag));
  const seen = await seenMarkers(page);
  return {
    sendReadyAt: sent.sendReadyAt,
    firstTokenMs: first.at - sent.enter.at,
    completeMs: done.at - sent.enter.at,
    complete: markers.every((marker) => seen[marker]),
  };
}

/** 一次稳态快照（附 1 分钟 load average）。 */
async function snapshot(context, label) {
  const rendererPids = await mainWindowRendererPids(context.app.main);
  const result = await snapshotTree(context.app.pid, rendererPids);
  return { label, load1: loadavg()[0], ...result };
}

async function windowSampler(context, options) {
  return startWindowSampler(
    context.app.pid,
    await mainWindowRendererPids(context.app.main),
    options,
  );
}

/** mock 发出的标记段 → 页面看到它的时刻之差（ms），只取 [from, to]（harness 时钟）内发出的。 */
function deltaLatencies(log, seen, tag, from, to) {
  return (log.sent[tag] ?? [])
    .filter((entry) => entry.sentAt >= from && entry.sentAt <= to && seen[entry.marker])
    .map((entry) => seen[entry.marker].at - entry.sentAt);
}

/** 流式窗口里每秒在 composer 里敲一个字符；结束后清空 composer（不发送）。 */
async function typeDuring(page, count) {
  const input = await composer(page);
  await input.click();
  for (let index = 0; index < count; index += 1) {
    await page.keyboard.press("x");
    await sleep(1000);
  }
  await page.keyboard.press("Meta+A");
  await page.keyboard.press("Backspace");
}

// ---------------------------------------------------------------------------------------------
// suites

/** 冷启动 + 首条消息 + N = 1 / 4 / 8 个 workspace 各完成一轮之后的稳态占用。 */
async function startupSuite(context) {
  const { page, result } = context;
  const first = await replyTurn(page, "f1");
  result.coldStart.sendReadyMs = first.sendReadyAt - context.spawnAt;
  result.coldStart.firstTokenMs = first.firstTokenMs;
  result.coldStart.replyCompleteMs = first.completeMs;
  result.checks.push({ name: "first reply complete", ok: first.complete });
  await sleep(SETTLE_MS);
  result.snapshots.push(await snapshot(context, "N1"));
  result.workspaceTurns = [];
  for (let index = 1; index < WORKSPACE_COUNT; index += 1) {
    await newTaskIn(page, `ws${index + 1}`);
    const turn = await replyTurn(page, `w${index + 1}`);
    result.workspaceTurns.push({ workspace: index + 1, ...turn });
    result.checks.push({ name: `ws${index + 1} reply complete`, ok: turn.complete });
    if (index + 1 === 4 || index + 1 === WORKSPACE_COUNT) {
      await sleep(SETTLE_MS);
      result.snapshots.push(await snapshot(context, `N${index + 1}`));
    }
  }
}

/** 一个流式窗口：采样进程、长任务、按键延迟；可选在窗口后段切换会话。 */
async function streamWindow(context, { keys, switches = [] }) {
  const { page } = context;
  const sampler = await windowSampler(context, { intervalMs: 500, footprintEveryMs: 2000 });
  const from = await pageNow(page);
  const windowStart = Date.now();
  await typeDuring(page, keys);
  const switchLatencies = [];
  let firstSwitchAt = null;
  for (const { title, marker } of switches) {
    firstSwitchAt ??= Date.now();
    const locator = page.getByText(title, { exact: true }).first();
    switchLatencies.push(await measureClickUntilVisible(page, locator, marker));
    await sleep(1500);
  }
  const to = await pageNow(page);
  const windowEnd = Date.now();
  const processes = await sampler.stop();
  return {
    windowStart,
    windowEnd,
    firstSwitchAt,
    processes,
    longTasks: await longTasksBetween(page, from, to),
    keyLatencyMs: distribution(await takeKeyLatencies(page)),
    switchLatencyMs: switchLatencies,
  };
}

/** 空闲占用 → 1 路流式 → 4 路流式（不同 workspace）→ 工具负载。 */
async function streamSuite(context) {
  const { page, result, log, workspaces } = context;
  await sleep(IDLE_SETTLE_MS);
  result.snapshots.push(await snapshot(context, "idle"));

  // 1 路：首页 composer（ws1）。
  const single = expectedMarkers("s1", 1500);
  await watchMarkers(page, single);
  await send(page, "perf:stream:s1:1500");
  await waitMarker(page, startMarker("s1"));
  await sleep(2000);
  const one = await streamWindow(context, { keys: Math.round(STREAM_WINDOW_MS / 1000) });
  await waitMarker(page, endMarker("s1"), 60_000);
  const seenOne = await seenMarkers(page);
  result.checks.push({ name: "stream s1 complete", ok: single.every((marker) => seenOne[marker]) });
  one.deltaLatencyMs = distribution(
    deltaLatencies(log, seenOne, "s1", one.windowStart, one.windowEnd),
  );
  result.streams = { one };

  // 4 路：ws2–ws4 先开始，ws1 最后开始并保持可见。
  const tags = ["s4w2", "s4w3", "s4w4", "s4w1"];
  const names = ["ws2", "ws3", "ws4", "ws1"];
  for (let index = 0; index < tags.length; index += 1) {
    await newTaskIn(page, names[index]);
    await watchMarkers(page, [startMarker(tags[index])]);
    await send(page, `perf:stream:${tags[index]}:2000`);
    await waitMarker(page, startMarker(tags[index]));
  }
  await watchMarkers(page, expectedMarkers("s4w1", 2000).slice(1));
  await sleep(2000);
  const four = await streamWindow(context, {
    keys: 12,
    switches: [
      { title: "perf:stream:s4w2:2000", marker: startMarker("s4w2") },
      { title: "perf:stream:s4w3:2000", marker: startMarker("s4w3") },
      { title: "perf:stream:s4w4:2000", marker: startMarker("s4w4") },
      { title: "perf:stream:s4w1:2000", marker: startMarker("s4w1") },
    ],
  });
  const seenFour = await seenMarkers(page);
  four.deltaLatencyMs = distribution(
    deltaLatencies(log, seenFour, "s4w1", four.windowStart, four.firstSwitchAt ?? four.windowEnd),
  );
  result.streams.four = four;
  // 正确性：等 4 路都结束，再逐个切过去确认末段在界面上。
  for (let index = 0; index < tags.length; index += 1) {
    const deadline = Date.now() + 90_000;
    while (
      !(log.sent[tags[index]] ?? []).some((entry) => entry.marker === endMarker(tags[index]))
    ) {
      if (Date.now() > deadline) break;
      await sleep(500);
    }
    await page.getByText(`perf:stream:${tags[index]}:2000`, { exact: true }).first().click();
    const visible = await page
      .waitForFunction(
        (marker) => document.body.innerText.includes(marker),
        endMarker(tags[index]),
        {
          timeout: 60_000,
          polling: 200,
        },
      )
      .then(() => true)
      .catch(() => false);
    result.checks.push({ name: `stream ${tags[index]} complete`, ok: visible });
  }

  // 工具负载：Bash 大输出 + Read 大文件，ws1 的新会话。
  await newTaskIn(page, "ws1");
  await watchMarkers(page, expectedMarkers("t1", 40));
  const sampler = await windowSampler(context, { intervalMs: 250, footprintEveryMs: 1000 });
  const sent = await send(page, "perf:tool:t1");
  const done = await waitMarker(page, endMarker("t1"), 180_000);
  const processes = await sampler.stop();
  const toolResults = log.toolResults.t1 ?? [];
  result.tool = { completeMs: done.at - sent.enter.at, toolResultChars: toolResults, processes };
  result.checks.push({
    name: "tool turn complete with both tool results",
    ok: toolResults.length === 2 && toolResults.every((length) => length > 1000),
  });
  void workspaces;
}

/** 冷加载长历史会话：点开 → 最后一轮可见 → 滚到顶、第一轮可见；之后的稳态占用。 */
async function historySuite(context) {
  const { page, result } = context;
  const last = historyMarker(HISTORY_TURNS - 1);
  const firstTurn = historyMarker(0);
  await watchMarkers(page, [last]);
  const item = page.getByText("perf:hist:0", { exact: true }).first();
  await item.waitFor({ state: "visible", timeout: 60_000 });
  const clickAt = Date.now();
  await item.click();
  const rendered = await waitMarker(page, last, 120_000);
  result.history = { renderedMs: rendered.at - clickAt };
  // 可滚动：找出最高的可滚动容器，滚到顶，直到第一轮的标记在可见文本里。
  const scrolled = await page.evaluate(async (marker) => {
    const start = performance.now();
    const scrollers = [...document.querySelectorAll("*")].filter((element) => {
      const style = getComputedStyle(element);
      return (
        /(auto|scroll)/.test(style.overflowY) && element.scrollHeight > element.clientHeight + 10
      );
    });
    const scroller = scrollers.sort((a, b) => b.scrollHeight - a.scrollHeight)[0];
    if (!scroller) return { ok: false };
    const height = scroller.scrollHeight;
    for (let attempt = 0; attempt < 600; attempt += 1) {
      scroller.scrollTop = 0;
      await new Promise((resolve) => requestAnimationFrame(resolve));
      if (document.body.innerText.includes(marker))
        return { ok: true, ms: performance.now() - start, scrollHeight: height };
    }
    return { ok: false, scrollHeight: height };
  }, firstTurn);
  result.history.scrollToTopMs = scrolled.ms ?? null;
  result.history.scrollHeight = scrolled.scrollHeight ?? null;
  result.checks.push({
    name: "history rendered and scrollable to the first turn",
    ok: scrolled.ok,
  });
  await sleep(10_000);
  result.snapshots.push(await snapshot(context, "history"));
}

export const SUITE_RUNNERS = {
  startup: startupSuite,
  stream: streamSuite,
  history: historySuite,
};
