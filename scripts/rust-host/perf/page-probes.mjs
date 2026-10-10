// App 级性能测量的页面探针：装进主窗口 Renderer 的主世界（page.evaluate），记录
//   - 长任务（PerformanceObserver longtask，≥ 50 ms 的主线程任务）
//   - 标记词首次出现在 DOM 里的时刻（mock.mjs 的 qa…s / qm…x… / qa…e）：每帧最多检查一次
//     document.body.textContent，按顺序只查下一个尚未出现的标记
//   - 输入延迟：composer 里每次按键，从 keydown 事件的时间戳到字符出现在 DOM 后的下一帧
//   - Enter 的 keydown 时刻（发送一条消息的起点）
//   - 页面可见性的变化（visibilitychange）：窗口被完全遮挡时 Chromium 会把页面置为 hidden 并停止出帧
// 所有时刻同时记 Date.now()（与 mock、harness 同一时钟）和 performance.now()。

/** 装探针（同一页面重复调用无副作用）。 */
export async function installProbes(page) {
  await page.evaluate(() => {
    if (window.__perf) return;
    const perf = {
      longTasks: [],
      watches: [],
      seen: {},
      keyLatencies: [],
      lastEnter: null,
      pendingKey: null,
    };
    window.__perf = perf;
    perf.visibility = [{ at: Date.now(), state: document.visibilityState }];
    document.addEventListener("visibilitychange", () =>
      perf.visibility.push({ at: Date.now(), state: document.visibilityState }),
    );
    try {
      new PerformanceObserver((list) => {
        for (const entry of list.getEntries())
          perf.longTasks.push({ start: entry.startTime, duration: entry.duration });
      }).observe({ type: "longtask", buffered: true });
    } catch {
      perf.longTaskUnsupported = true;
    }
    const composerElement = () =>
      document.querySelector('[contenteditable="true"]') ??
      document.querySelector('textarea[placeholder*="Ask ZCode"]');
    const composerLength = () => {
      const element = composerElement();
      return (element?.value ?? element?.textContent ?? "").length;
    };
    document.addEventListener(
      "keydown",
      (event) => {
        if (event.key === "Enter") perf.lastEnter = { at: Date.now(), now: performance.now() };
        // 只量可打印字符；带修饰键的（清空 composer 用的 Cmd+A）与其他键清掉未完成的记录。
        else if (event.key.length === 1 && !event.metaKey && !event.ctrlKey && !event.altKey)
          perf.pendingKey = { timeStamp: event.timeStamp, length: composerLength() };
        else perf.pendingKey = null;
      },
      true,
    );
    let checkScheduled = false;
    const nextPaint = (callback) =>
      requestAnimationFrame(() => {
        const channel = new MessageChannel();
        channel.port1.onmessage = callback;
        channel.port2.postMessage(null);
      });
    const check = () => {
      checkScheduled = false;
      if (perf.pendingKey && composerLength() > perf.pendingKey.length) {
        const key = perf.pendingKey;
        perf.pendingKey = null;
        nextPaint(() => perf.keyLatencies.push(performance.now() - key.timeStamp));
      }
      if (perf.watches.length === 0) return;
      const text = document.body.textContent ?? "";
      const at = Date.now();
      const now = performance.now();
      for (const watch of perf.watches) {
        while (watch.next < watch.markers.length && text.includes(watch.markers[watch.next])) {
          perf.seen[watch.markers[watch.next]] = { at, now };
          watch.next += 1;
        }
      }
      perf.watches = perf.watches.filter((watch) => watch.next < watch.markers.length);
    };
    perf.observer = new MutationObserver(() => {
      if (checkScheduled) return;
      checkScheduled = true;
      requestAnimationFrame(check);
    });
    perf.observe = () =>
      perf.observer.observe(document.body, { subtree: true, childList: true, characterData: true });
    perf.observe();
  });
}

/**
 * 暂停 / 恢复探针的 DOM 观察（MutationObserver 与每帧一次的文本检查）。只供量“探针本身的成本”与
 * 不带探针的 App 底数（stream-cpu.mjs）；暂停期间出现的标记不会被记录。
 */
export async function setProbesActive(page, active) {
  await page.evaluate((on) => {
    const perf = window.__perf;
    if (!perf?.observer) return;
    if (on) perf.observe();
    else perf.observer.disconnect();
  }, active);
}

/** 开始等这组标记（按顺序出现）。 */
export async function watchMarkers(page, markers) {
  await page.evaluate((list) => {
    window.__perf.watches.push({ markers: list, next: 0 });
    // 立即检查一次：标记可能已经在 DOM 里（例如冷加载的长历史）。
    document.body.appendChild(document.createComment("perf"));
  }, markers);
}

/** 已出现的标记：marker → { at, now }。 */
export async function seenMarkers(page) {
  return page.evaluate(() => window.__perf.seen);
}

/** 等某个标记出现，返回 { at, now }。 */
export async function waitMarker(page, marker, timeout = 120_000) {
  const handle = await page.waitForFunction((name) => window.__perf.seen[name] ?? null, marker, {
    timeout,
    polling: 100,
  });
  return handle.jsonValue();
}

export async function lastEnter(page) {
  return page.evaluate(() => window.__perf.lastEnter);
}

export async function pageNow(page) {
  return page.evaluate(() => performance.now());
}

/** [from, to]（performance.now()）内开始的长任务：个数、总时长、最长一次。 */
export async function longTasksBetween(page, from, to) {
  return page.evaluate(
    ({ from: start, to: end }) => {
      const tasks = window.__perf.longTasks.filter(
        (task) => task.start >= start && task.start <= end,
      );
      return {
        unsupported: Boolean(window.__perf.longTaskUnsupported),
        count: tasks.length,
        totalMs: tasks.reduce((sum, task) => sum + task.duration, 0),
        maxMs: tasks.reduce((max, task) => Math.max(max, task.duration), 0),
      };
    },
    { from, to },
  );
}

/** 取出并清空已记录的按键延迟（ms）。 */
export async function takeKeyLatencies(page) {
  return page.evaluate(() => window.__perf.keyLatencies.splice(0));
}

/**
 * 切换会话的交互延迟：从点击（click 事件时间戳）到 `marker` 出现在可见文本（innerText，不含隐藏的
 * DOM）后的下一帧。点击前 marker 已经可见时返回 null（测量无效）。
 */
export async function measureClickUntilVisible(page, locator, marker, timeout = 30_000) {
  const armed = await page.evaluate((name) => {
    const perf = window.__perf;
    perf.switchResult = null;
    if (document.body.innerText.includes(name)) return false;
    const onClick = (event) => {
      document.removeEventListener("click", onClick, true);
      const clickStamp = event.timeStamp;
      const poll = () => {
        if (!document.body.innerText.includes(name)) {
          requestAnimationFrame(poll);
          return;
        }
        requestAnimationFrame(() => {
          const channel = new MessageChannel();
          channel.port1.onmessage = () => {
            perf.switchResult = performance.now() - clickStamp;
          };
          channel.port2.postMessage(null);
        });
      };
      requestAnimationFrame(poll);
    };
    document.addEventListener("click", onClick, true);
    return true;
  }, marker);
  await locator.click();
  if (!armed) return null;
  const handle = await page.waitForFunction(() => window.__perf.switchResult, undefined, {
    timeout,
    polling: 100,
  });
  return handle.jsonValue();
}

/** [from, to]（Date.now()）内页面是否一直可见（没有记录到 hidden）。 */
export async function visibleThroughout(page, from, to) {
  return page.evaluate(
    ({ from: start, to: end }) => {
      const changes = window.__perf.visibility ?? [];
      const before = changes.filter((change) => change.at <= start).at(-1);
      return (
        before?.state !== "hidden" &&
        !changes.some(
          (change) => change.at > start && change.at <= end && change.state === "hidden",
        )
      );
    },
    { from, to },
  );
}
