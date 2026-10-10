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
  seenMarkers,
  setProbesActive,
  visibleThroughout,
  waitMarker,
  watchMarkers,
} from "./page-probes.mjs";
import { mainWindowRendererPids, snapshotTree, startWindowSampler } from "./processes.mjs";
import { connectPage, streamsCover } from "./stream-cpu-lib.mjs";

export const WORKSPACE_COUNT = 8;
/** Renderer 离开会话后保温的时长（v4 session data lease 的 keepWarmMs）。 */
const KEEP_WARM_MS = 30_000;
/** 稳态快照之前的静置：保温结束、订阅释放之后再读。 */
const SETTLE_MS = KEEP_WARM_MS + 5_000;
const IDLE_SETTLE_MS = 30_000;
/** 空闲 CPU 窗口：启动静置之后，只有活动 workspace 预热了 runtime、没有会话。 */
const IDLE_CPU_MS = 60_000;
const STREAM_WINDOW_MS = 20_000;
/** 1 路流的段数（20 ms 一段，约 40 s）：覆盖断开、20 s 窗口与重新连接。 */
const ONE_STREAM_CHUNKS = 2000;
/** 4 路流的段数（约 100 s）：覆盖 4 个订阅的窗口、等保温结束、1 个订阅的窗口。 */
const FOUR_STREAM_CHUNKS = 5000;

/** 在首页 composer 选定 workspace（侧栏 New task → 选择项目菜单）。 */
export async function newTaskIn(page, workspaceName) {
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

/**
 * 断开 Playwright、暂停页面探针，执行 `fn`，再连回、恢复探针。所有 CPU 窗口与稳态内存快照都在这种条件下读，
 * 三种配置相同：Playwright 经 CDP 连着 Renderer 时总会打开 Network 域，Rust Host 的每个 WebSocket 帧都会
 * 变成一条经 Main 转发给 Playwright 的 CDP 事件（MessagePort 没有这份开销），抬高 B 的 Main 与 Renderer 读数；
 * 探针每帧读一次 document.body.textContent，也会抬高 Renderer（报告“流式 CPU 为什么持平”）。
 * Main 的 inspector 仍连着（只用来枚举 webContents），断开期间不连 Renderer 的 CDP 客户端。
 * connectOverCDP 的 close 只断开连接，不关 App；连回后更新 context.page 与 app.page。
 */
async function detached(context, fn) {
  await setProbesActive(context.page, false);
  await context.app.browser.close();
  // Playwright 断开时的收尾（关闭各个域）不要落进窗口。
  await sleep(1000);
  try {
    return await fn();
  } finally {
    const { browser, page } = await connectPage(context.app.cdpPort);
    context.app.browser = browser;
    context.app.page = page;
    context.page = page;
    await setProbesActive(page, true);
    // 重新连接的 CDP 初始化（Playwright 打开各个域）不要落进下一个窗口。
    await sleep(1000);
  }
}

/** 断开状态下的一个采样窗口（由 detached 包住）：分角色 CPU、footprint。 */
async function sampleWindow(context, ms, samplerOptions) {
  const sampler = await windowSampler(context, samplerOptions);
  const windowStart = Date.now();
  await sleep(ms);
  const windowEnd = Date.now();
  return { windowStart, windowEnd, processes: await sampler.stop() };
}

/** 连回之后补上窗口内的 Renderer 长任务（PerformanceObserver 在断开期间照常记录）。 */
async function addLongTasks(page, window) {
  const origin = await page.evaluate(() => performance.timeOrigin);
  window.longTasks = await longTasksBetween(
    page,
    window.windowStart - origin,
    window.windowEnd - origin,
  );
  return window;
}

// ---------------------------------------------------------------------------------------------
// suites

/** 冷启动 + 首条消息 + N = 1 / 4 / 8 个 workspace 各完成一轮之后的稳态占用。 */
async function startupSuite(context) {
  const { result } = context;
  // 计时（启动、首段）要靠 Playwright 与探针；静置与快照时断开（context.page 在连回后换成新对象）。
  const first = await replyTurn(context.page, "f1");
  result.coldStart.sendReadyMs = first.sendReadyAt - context.spawnAt;
  result.coldStart.firstTokenMs = first.firstTokenMs;
  result.coldStart.replyCompleteMs = first.completeMs;
  result.checks.push({ name: "first reply complete", ok: first.complete });
  await detached(context, async () => {
    await sleep(SETTLE_MS);
    result.snapshots.push(await snapshot(context, "N1"));
  });
  result.workspaceTurns = [];
  for (let index = 1; index < WORKSPACE_COUNT; index += 1) {
    await newTaskIn(context.page, `ws${index + 1}`);
    const turn = await replyTurn(context.page, `w${index + 1}`);
    result.workspaceTurns.push({ workspace: index + 1, ...turn });
    result.checks.push({ name: `ws${index + 1} reply complete`, ok: turn.complete });
    if (index + 1 === 4 || index + 1 === WORKSPACE_COUNT)
      await detached(context, async () => {
        await sleep(SETTLE_MS);
        result.snapshots.push(await snapshot(context, `N${index + 1}`));
      });
  }
}

/**
 * 空闲 → 1 路流式 → 4 路流式（不同 workspace）。每个 CPU 窗口都断开 Playwright、暂停探针（detached），
 * 窗口里没有任何交互；切换会话、正确性检查在窗口外、连着 Playwright 做。
 */
async function streamSuite(context) {
  const { result, log } = context;
  /**
   * 流式窗口的有效性：流覆盖整个窗口；窗口确实在出帧。窗口被完全遮挡（页面 hidden）或显示器休眠时，
   * Renderer 照样处理消息与渲染，但 GPU 进程不再合成，CPU 读数会低一截（2026-10-11 的一次运行 GPU 为 0）。
   * 正常出帧时 GPU 进程在 8–13%，这里以 2% 为界。
   */
  const covers = async (name, tags, window) => {
    result.checks.push({
      name: `${name}: streams cover the window`,
      ok: streamsCover(log, tags, window.windowStart, window.windowEnd),
    });
    const gpu = window.processes.cpuPercent.gpu;
    result.checks.push({ name: `${name}: window painted (GPU ${gpu.toFixed(1)}%)`, ok: gpu >= 2 });
    result.checks.push({
      name: `${name}: page visible`,
      ok: await visibleThroughout(context.page, window.windowStart, window.windowEnd),
    });
  };

  // 空闲：启动后静置 30 s 取快照，再量 60 s 整个进程树的 CPU（只有活动 workspace 预热了 runtime，没有会话）。
  result.idle = await detached(context, async () => {
    await sleep(IDLE_SETTLE_MS);
    result.snapshots.push(await snapshot(context, "idle"));
    return sampleWindow(context, IDLE_CPU_MS, { intervalMs: 2000, footprintEveryMs: 20_000 });
  });

  // 1 路：首页 composer（ws1）。
  const single = expectedMarkers("s1", ONE_STREAM_CHUNKS);
  await watchMarkers(context.page, single);
  await send(context.page, `perf:stream:s1:${ONE_STREAM_CHUNKS}`);
  await waitMarker(context.page, startMarker("s1"));
  await sleep(2000);
  const one = await detached(context, () =>
    sampleWindow(context, STREAM_WINDOW_MS, { intervalMs: 500, footprintEveryMs: 2000 }),
  );
  await addLongTasks(context.page, one);
  await covers("stream s1", ["s1"], one);
  await waitMarker(context.page, endMarker("s1"), 90_000);
  const seenOne = await seenMarkers(context.page);
  result.checks.push({ name: "stream s1 complete", ok: single.every((marker) => seenOne[marker]) });
  result.streams = { one };

  // 4 路：ws2–ws4 先开始，ws1 最后开始并保持可见。
  const tags = ["s4w2", "s4w3", "s4w4", "s4w1"];
  const names = ["ws2", "ws3", "ws4", "ws1"];
  const title = (tag) => `perf:stream:${tag}:${FOUR_STREAM_CHUNKS}`;
  for (let index = 0; index < tags.length; index += 1) {
    await newTaskIn(context.page, names[index]);
    await watchMarkers(context.page, [startMarker(tags[index])]);
    await send(context.page, title(tags[index]));
    await waitMarker(context.page, startMarker(tags[index]));
  }
  await sleep(2000);
  // 依次点侧栏切到另外三个会话再切回 ws1：4 个会话的 conversation 订阅都在保温期内（与首次报告的 4 路窗口
  // 相同的订阅状态）。离开 ws2 的时刻（第二次点击）起 30 s 内要量完。
  const switchLatencies = [];
  const clicks = [];
  for (const tag of tags) {
    clicks.push(Date.now());
    const locator = context.page.getByText(title(tag), { exact: true }).first();
    switchLatencies.push(await measureClickUntilVisible(context.page, locator, startMarker(tag)));
    await sleep(1000);
  }
  const windows = await detached(context, async () => {
    const warm = await sampleWindow(context, STREAM_WINDOW_MS, {
      intervalMs: 500,
      footprintEveryMs: 2000,
    });
    // 1 个订阅：等最后离开的会话（ws4，最后一次点击）保温结束，只剩可见会话的订阅，另外 3 路只有遥测与
    // 列表事实。
    const settleUntil = clicks.at(-1) + KEEP_WARM_MS + 5000;
    if (Date.now() < settleUntil) await sleep(settleUntil - Date.now());
    const steady = await sampleWindow(context, STREAM_WINDOW_MS, {
      intervalMs: 500,
      footprintEveryMs: 2000,
    });
    return { warm, steady };
  });
  const four = await addLongTasks(context.page, windows.warm);
  four.switchLatencyMs = switchLatencies;
  four.withinKeepWarm = four.windowEnd < clicks[1] + KEEP_WARM_MS;
  const fourSteady = await addLongTasks(context.page, windows.steady);
  result.streams.four = four;
  result.streams.fourSteady = fourSteady;
  result.checks.push({ name: "4 subscriptions warm through the window", ok: four.withinKeepWarm });
  await covers("4 streams, 4 subscriptions", tags, four);
  await covers("4 streams, 1 subscription", tags, fourSteady);
  // 正确性：等 4 路都结束，再逐个切过去确认末段在界面上。
  for (let index = 0; index < tags.length; index += 1) {
    const deadline = Date.now() + 150_000;
    while (
      !(log.sent[tags[index]] ?? []).some((entry) => entry.marker === endMarker(tags[index]))
    ) {
      if (Date.now() > deadline) break;
      await sleep(500);
    }
    await context.page.getByText(title(tags[index]), { exact: true }).first().click();
    const visible = await context.page
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
