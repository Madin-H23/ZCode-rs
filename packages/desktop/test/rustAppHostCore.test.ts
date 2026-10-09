// App 级 Rust Host 状态机（docs/specs/rust-u4-app-host.md §2.1、§2.8）：拉起 → ready →
// N 个窗口接入 → 窗口关闭不影响 Host → 意外退出 → 退避重启 → 放弃 → 任意窗口 Retry。
import assert from "node:assert/strict";
import test from "node:test";
import type { HostConnectionStatePayload } from "@zcode/shared";
import { RustAppHostCore } from "../src/main/rustAppHostCore.js";

type Effect =
  | { kind: "spawn"; generation: number }
  | { kind: "issue"; windowId: number; generation: number; pageToken: number }
  | { kind: "reload"; windowId: number }
  | { kind: "notify"; payload: HostConnectionStatePayload; windowId?: number };

function setup() {
  const effects: Effect[] = [];
  const timers: Array<{ at: number; fire: () => void; cancelled: boolean }> = [];
  let now = 0;
  const core = new RustAppHostCore({
    spawn: (generation) => effects.push({ kind: "spawn", generation }),
    issueEndpoint: (windowId, generation, pageToken) =>
      effects.push({ kind: "issue", windowId, generation, pageToken }),
    reloadWindow: (windowId) => effects.push({ kind: "reload", windowId }),
    notifyConnectionState: (payload, windowId) =>
      effects.push(
        windowId === undefined
          ? { kind: "notify", payload }
          : { kind: "notify", payload, windowId },
      ),
    setTimer: (delayMs, fire) => {
      const timer = { at: now + delayMs, fire, cancelled: false };
      timers.push(timer);
      return () => {
        timer.cancelled = true;
      };
    },
    now: () => now,
    logger: { info: () => {}, warn: () => {} },
  });
  return {
    core,
    /** 取出并清空已发生的 effects。 */
    take: () => effects.splice(0),
    /** 推进时钟并触发到期的定时器。 */
    advance: (ms: number) => {
      now += ms;
      for (const timer of timers.splice(0)) {
        if (timer.cancelled) continue;
        if (timer.at <= now) timer.fire();
        else timers.push(timer);
      }
    },
    /** 让第 generation 代进程启动并就绪。 */
    bootReady: (generation: number, attemptId = `attempt-${generation}`) => {
      core.hostSpawned(generation);
      core.hostStartupState(generation, { phase: "ready", attemptId });
    },
  };
}

test("one host for the app: windows attach once it is ready and closing a window keeps it", () => {
  const { core, take } = setup();
  core.start();
  core.start();
  assert.deepEqual(take(), [{ kind: "spawn", generation: 1 }]);

  // 窗口 1 在 Host 就绪前 dom-ready：等就绪后签发。
  core.windowDomReady(1);
  assert.deepEqual(take(), []);
  core.hostSpawned(1);
  core.hostStartupState(1, { phase: "starting", attemptId: "a" });
  assert.deepEqual(take(), []);
  core.hostStartupState(1, { phase: "ready", attemptId: "a" });
  const [first] = take();
  assert.ok(first?.kind === "issue");
  assert.deepEqual([first.windowId, first.generation], [1, 1]);

  // 同一尝试重复 ready 不再签发；窗口 2 在就绪后 dom-ready 立即签发。
  core.hostStartupState(1, { phase: "ready", attemptId: "a" });
  core.windowDomReady(2);
  const issued = take();
  assert.equal(issued.length, 1);
  assert.equal(issued[0]?.kind === "issue" && issued[0].windowId, 2);

  // 窗口 1 reload：新页面换新 token，旧 token 的结果被丢弃。
  const oldToken = first.pageToken;
  core.windowDomReady(1);
  const [reissued] = take();
  assert.equal(reissued?.kind, "issue");
  assert.equal(core.isCurrentPage(1, oldToken), false);
  assert.equal(reissued?.kind === "issue" && core.isCurrentPage(1, reissued.pageToken), true);

  // 关闭窗口 2：Host 仍在运行，没有 dispose、重启或通知。
  core.windowClosed(2);
  assert.equal(core.hasWindow(2), false);
  assert.equal(core.hasWindow(1), true);
  assert.deepEqual(take(), []);
  assert.deepEqual(core.snapshot(), {
    phase: "running",
    generation: 1,
    ready: true,
    processAlive: true,
  });
});

test("an unexpected exit restarts with backoff and reloads every main window after the new host is ready", () => {
  const { core, take, advance, bootReady } = setup();
  core.start();
  bootReady(1);
  core.windowDomReady(1);
  core.windowDomReady(2);
  take();

  core.hostExited(1, 9);
  assert.deepEqual(take(), [
    {
      kind: "notify",
      payload: { state: "restarting", attempt: 1, maxAttempts: 3, delayMs: 500 },
    },
  ]);
  assert.equal(core.snapshot().phase, "restart-scheduled");

  // 退避结束前不拉起；结束后拉起第 2 代。
  advance(499);
  assert.deepEqual(take(), []);
  advance(1);
  assert.deepEqual(take(), [{ kind: "spawn", generation: 2 }]);

  // 旧代的迟到事件被忽略。
  core.hostStartupState(1, { phase: "ready", attemptId: "late" });
  core.hostExited(1, 0);
  assert.deepEqual(take(), []);

  // 新代就绪：先 reload 所有旧页面，不直接给旧页面签发。
  bootReady(2);
  assert.deepEqual(take(), [
    { kind: "reload", windowId: 1 },
    { kind: "reload", windowId: 2 },
  ]);
  // reload 后的 dom-ready 在 Host 就绪时立即签发。
  core.windowDomReady(1);
  core.windowDomReady(2);
  assert.deepEqual(
    take().map((effect) => (effect.kind === "issue" ? [effect.windowId, effect.generation] : [])),
    [
      [1, 2],
      [2, 2],
    ],
  );
});

test("a window reloaded during the backoff attaches to the new host without a second reload", () => {
  const { core, take, advance, bootReady } = setup();
  core.start();
  bootReady(1);
  core.windowDomReady(1);
  core.windowDomReady(2);
  core.hostExited(1, 1);
  take();

  // 用户在退避期间手动 reload 窗口 1：不提前拉起，也不在就绪后再 reload 它。
  core.windowDomReady(1);
  assert.deepEqual(take(), []);
  advance(500);
  assert.deepEqual(take(), [{ kind: "spawn", generation: 2 }]);
  bootReady(2);
  assert.deepEqual(
    take().map((effect) => [effect.kind, "windowId" in effect ? effect.windowId : null]),
    [
      ["reload", 2],
      ["issue", 1],
    ],
  );
});

test("gives up after three exits within 60 s and one Retry from any window restarts once", () => {
  const { core, take, advance, bootReady } = setup();
  core.start();
  bootReady(1);
  core.windowDomReady(1);
  core.windowDomReady(2);
  take();

  const delays = [500, 2_000, 5_000];
  for (const [index, delayMs] of delays.entries()) {
    const generation = index + 1;
    core.hostExited(generation, 1);
    const [notify] = take();
    assert.deepEqual(notify, {
      kind: "notify",
      payload: { state: "restarting", attempt: index + 1, maxAttempts: 3, delayMs },
    });
    advance(delayMs);
    assert.deepEqual(take(), [{ kind: "spawn", generation: generation + 1 }]);
    core.hostSpawned(generation + 1);
  }

  // 第 4 次（60 s 内）放弃：所有主窗口显示失败页，不再自动拉起。
  core.hostExited(4, 1);
  assert.deepEqual(take(), [{ kind: "notify", payload: { state: "failed", attempts: 3 } }]);
  advance(60_000);
  assert.deepEqual(take(), []);
  assert.equal(core.snapshot().phase, "gave-up");

  // 窗口 2 点 Retry（页面自身 reload → dom-ready）：立即拉起一次，不经过重启策略。
  core.windowDomReady(2);
  assert.deepEqual(take(), [{ kind: "spawn", generation: 5 }]);
  // 再有窗口 dom-ready 不会重复拉起。
  core.windowDomReady(2);
  assert.deepEqual(take(), []);
  bootReady(5);
  assert.deepEqual(
    take().map((effect) => [effect.kind, "windowId" in effect ? effect.windowId : null]),
    [
      ["reload", 1],
      ["issue", 2],
    ],
  );
});

test("a database startup retry issues an endpoint to every waiting window without a reload", () => {
  const { core, take } = setup();
  core.start();
  core.hostSpawned(1);
  core.windowDomReady(1);
  core.windowDomReady(2);
  core.hostStartupState(1, { phase: "failed", attemptId: "a" });
  assert.deepEqual(take(), [{ kind: "spawn", generation: 1 }]);

  // 任意窗口的 retry 经 relay 交给 Host；新尝试 ready 后两个窗口都拿到 endpoint。
  core.hostStartupState(1, { phase: "ready", attemptId: "b" });
  assert.deepEqual(
    take().map((effect) => [effect.kind, "windowId" in effect ? effect.windowId : null]),
    [
      ["issue", 1],
      ["issue", 2],
    ],
  );
});

test("waiting for readiness resolves on ready, times out, and stops at dispose", async () => {
  const { core, advance, bootReady } = setup();
  core.start();
  const early = core.waitUntilReady(10_000);
  const timedOut = core.waitUntilReady(1_000);
  advance(1_000);
  assert.equal(await timedOut, false);
  bootReady(1);
  assert.equal(await early, true);
  assert.equal(await core.waitUntilReady(10_000), true);

  core.hostExited(1, 1);
  const duringRestart = core.waitUntilReady(10_000);
  assert.equal(core.beginDispose(), false, "no live process during the backoff");
  assert.equal(await duringRestart, false);
  assert.equal(await core.waitUntilReady(10_000), false);
});

test("dispose stops restarts and a disposed exit is not unexpected", () => {
  const { core, take, advance, bootReady } = setup();
  core.start();
  bootReady(1);
  core.windowDomReady(1);
  take();
  assert.equal(core.beginDispose(), true);
  assert.equal(core.beginDispose(), false);
  core.hostExited(1, 0);
  advance(60_000);
  assert.deepEqual(take(), []);
  assert.equal(core.snapshot().phase, "disposed");
  // 退出后的 dom-ready 不再拉起或签发。
  core.windowDomReady(1);
  assert.deepEqual(take(), []);

  // 退避期间退出：取消计划中的重启，准备中的拉起也不再执行。
  const other = setup();
  other.core.start();
  other.bootReady(1);
  other.core.hostExited(1, 1);
  other.take();
  other.core.beginDispose();
  other.advance(500);
  assert.deepEqual(other.take(), []);
  assert.equal(other.core.shouldSpawn(2), false);
});

/** 连续崩溃到放弃：返回放弃时的代次。 */
function crashUntilGaveUp(harness: ReturnType<typeof setup>, firstGeneration = 1): number {
  let generation = firstGeneration;
  for (const delayMs of [500, 2_000, 5_000]) {
    harness.core.hostExited(generation, 1);
    harness.advance(delayMs);
    generation += 1;
    harness.core.hostSpawned(generation);
  }
  harness.core.hostExited(generation, 1);
  assert.equal(harness.core.snapshot().phase, "gave-up");
  return generation;
}

test("after giving up a new window shows the failure page instead of restarting", () => {
  const harness = setup();
  const { core, take, bootReady } = harness;
  core.start();
  bootReady(1);
  core.windowDomReady(1);
  const last = crashUntilGaveUp(harness);
  take();

  // 放弃期间新打开的主窗口（例如托盘重新打开）：只给它发失败页，不拉起。
  core.windowDomReady(7);
  assert.deepEqual(take(), [
    { kind: "notify", payload: { state: "failed", attempts: 3 }, windowId: 7 },
  ]);
  assert.equal(core.snapshot().phase, "gave-up");
  assert.equal(core.snapshot().generation, last);

  // 它的 Retry（失败页上的重新加载）才触发重启；就绪后 reload 仍停在失败页的窗口 1。
  core.windowDomReady(7);
  assert.deepEqual(take(), [{ kind: "spawn", generation: last + 1 }]);
  bootReady(last + 1);
  assert.deepEqual(
    take().map((effect) => [effect.kind, "windowId" in effect ? effect.windowId : null]),
    [
      ["reload", 1],
      ["issue", 7],
    ],
  );
});

test("a Retry after giving up starts a fresh restart budget", () => {
  const harness = setup();
  const { core, take, advance, bootReady } = harness;
  core.start();
  bootReady(1);
  core.windowDomReady(1);
  const last = crashUntilGaveUp(harness);
  core.windowDomReady(1);
  take();
  bootReady(last + 1);
  take();

  // Retry 之后（仍在上一轮崩溃的 60 s 内）再崩溃：从第 1 次自动重启重新计起，而不是立即放弃。
  advance(1_000);
  core.hostExited(last + 1, 1);
  assert.deepEqual(take(), [
    {
      kind: "notify",
      payload: { state: "restarting", attempt: 1, maxAttempts: 3, delayMs: 500 },
    },
  ]);
});

test("a host that fails to spawn counts as an unexpected exit of that generation", () => {
  const { core, take, advance, bootReady } = setup();
  core.start();
  core.windowDomReady(1);
  take();
  // 进程没起来（hostSpawned 之前就失败）：按意外退出进入退避重启，窗口继续等下一代。
  core.hostExited(1, -1);
  assert.deepEqual(take(), [
    {
      kind: "notify",
      payload: { state: "restarting", attempt: 1, maxAttempts: 3, delayMs: 500 },
    },
  ]);
  advance(500);
  assert.deepEqual(take(), [{ kind: "spawn", generation: 2 }]);
  bootReady(2);
  assert.deepEqual(
    take().map((effect) => [effect.kind, "windowId" in effect ? effect.windowId : null]),
    [["reload", 1]],
  );
});

test("a reload Main asked for that lands after giving up is not a Retry", () => {
  const harness = setup();
  const { core, take, advance, bootReady } = harness;
  core.start();
  bootReady(1);
  core.windowDomReady(1);
  core.windowDomReady(2);
  // 前 3 次崩溃都在旧页面 reload 完成之后；第 4 代就绪、Main 让两个窗口 reload 之后立即崩溃。
  let generation = 1;
  for (const delayMs of [500, 2_000, 5_000]) {
    core.hostExited(generation, 1);
    advance(delayMs);
    generation += 1;
    bootReady(generation);
    if (generation < 4) {
      core.windowDomReady(1);
      core.windowDomReady(2);
    }
  }
  take();
  core.hostExited(generation, 1);
  assert.deepEqual(take(), [{ kind: "notify", payload: { state: "failed", attempts: 3 } }]);

  // 这两次 dom-ready 是 Main 自己要的 reload：页面显示失败页，不自动拉起下一代。
  core.windowDomReady(2);
  core.windowDomReady(1);
  assert.deepEqual(take(), [
    { kind: "notify", payload: { state: "failed", attempts: 3 }, windowId: 2 },
    { kind: "notify", payload: { state: "failed", attempts: 3 }, windowId: 1 },
  ]);
  assert.equal(core.snapshot().phase, "gave-up");

  // 用户在失败页上点 Retry：这才拉起。
  core.windowDomReady(1);
  assert.deepEqual(take(), [{ kind: "spawn", generation: generation + 1 }]);
});
