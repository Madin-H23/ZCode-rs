// Rust Host 主动关闭连接后的页面状态与重新接入（docs/specs/rust-host-resilience.md §2.5）。
// 用法：node --import tsx --test packages/desktop/test/hostConnectionClose.test.ts
import assert from "node:assert/strict";
import test from "node:test";
import {
  HOST_REATTACH_WINDOW_MS,
  HostConnectionLostController,
  createSessionReattachHistory,
  decideHostConnectionClose,
  type HostReattachHistory,
} from "../src/renderer/src/hostConnectionClose.js";

const backlog = { code: 4001, reason: "outbound-backlog" };

test("only the outbound-backlog close reattaches; other closes keep today's behaviour", () => {
  const decide = (code: number, connection: "local" | "remote-scope") =>
    decideHostConnectionClose({ close: { code, reason: "" }, connection, now: 0, history: [] })
      .action.kind;
  assert.equal(decide(4001, "local"), "reattach");
  assert.equal(decide(4001, "remote-scope"), "reattach");
  for (const code of [4002, 4003, 4004, 4005, 1011, 1006, 1000, 4999, 3000]) {
    assert.equal(decide(code, "local"), "host-lost", `local ${code}`);
    assert.equal(decide(code, "remote-scope"), "ignore", `remote ${code}`);
  }
});

test("at most three automatic reattaches within 60 s, with 500 ms / 2 s / 5 s backoff", () => {
  let history: number[] = [];
  const delays: number[] = [];
  for (const now of [1_000, 2_000, 3_000]) {
    const decided = decideHostConnectionClose({
      close: backlog,
      connection: "local",
      now,
      history,
    });
    assert.equal(decided.action.kind, "reattach");
    if (decided.action.kind === "reattach") delays.push(decided.action.delayMs);
    history = decided.history!;
  }
  assert.deepEqual(delays, [500, 2_000, 5_000]);
  assert.deepEqual(history, [1_000, 2_000, 3_000]);
  assert.deepEqual(
    decideHostConnectionClose({ close: backlog, connection: "local", now: 4_000, history }).action,
    { kind: "give-up", attempts: 3 },
  );
  // 第一条记录滑出 60 s 窗口后，又可以自动重新接入一次（第 3 次，等 5 s）。
  const later = decideHostConnectionClose({
    close: backlog,
    connection: "local",
    now: 1_000 + HOST_REATTACH_WINDOW_MS,
    history,
  });
  assert.deepEqual(later.action, { kind: "reattach", attempt: 3, maxAttempts: 3, delayMs: 5_000 });
  assert.deepEqual(later.history, [2_000, 3_000, 1_000 + HOST_REATTACH_WINDOW_MS]);
});

test("without a usable history the window never reloads by itself", () => {
  assert.deepEqual(
    decideHostConnectionClose({ close: backlog, connection: "local", now: 0, history: null })
      .action,
    { kind: "give-up", attempts: 0 },
  );
});

test("the session-storage history survives reads and tolerates broken storage", () => {
  const items = new Map<string, string>();
  const storage = {
    getItem: (key: string) => items.get(key) ?? null,
    setItem: (key: string, value: string) => void items.set(key, value),
    removeItem: (key: string) => void items.delete(key),
  };
  const history = createSessionReattachHistory(() => storage);
  assert.deepEqual(history.read(), []);
  assert.equal(history.write([1, 2]), true);
  assert.deepEqual(createSessionReattachHistory(() => storage).read(), [1, 2]);
  items.set("zcode.hostReattachHistory", '["x", 3, null]');
  assert.deepEqual(history.read(), [3]);
  history.clear();
  assert.deepEqual(history.read(), []);

  const broken = createSessionReattachHistory(() => {
    throw new Error("SecurityError");
  });
  assert.equal(broken.read(), null);
  assert.equal(broken.write([1]), false);
  broken.clear();
});

function harness(history: HostReattachHistory = memoryHistory()) {
  const rendered: unknown[] = [];
  const timers: Array<{ delayMs: number; fire: () => void; cancelled: boolean }> = [];
  let reloads = 0;
  let now = 10_000;
  const controller = new HostConnectionLostController({
    history,
    now: () => now,
    setTimer: (delayMs, fire) => {
      const timer = { delayMs, fire, cancelled: false };
      timers.push(timer);
      return () => {
        timer.cancelled = true;
      };
    },
    reload: () => {
      reloads += 1;
    },
    render: (state) => rendered.push(state),
    warn: () => undefined,
  });
  return {
    controller,
    rendered,
    timers,
    reloads: () => reloads,
    advance: (ms: number) => {
      now += ms;
    },
  };
}

function memoryHistory(): HostReattachHistory & { stored: number[] } {
  const box = { stored: [] as number[] };
  return Object.assign(box, {
    read: () => [...box.stored],
    write: (next: readonly number[]) => {
      box.stored = [...next];
      return true;
    },
    clear: () => {
      box.stored = [];
    },
  });
}

test("a backlog close shows the reattaching page, then reloads after the delay", () => {
  const page = harness();
  page.controller.connectionClosed(backlog, "local");
  assert.deepEqual(page.rendered, [{ state: "reattaching", attempt: 1, maxAttempts: 3 }]);
  assert.equal(page.timers.length, 1);
  assert.equal(page.timers[0]!.delayMs, 500);
  assert.equal(page.reloads(), 0);
  page.timers[0]!.fire();
  assert.equal(page.reloads(), 1);
});

test("reloaded pages share the count: the fourth close within 60 s shows the closed page", () => {
  const history = memoryHistory();
  for (const attempt of [1, 2, 3]) {
    // 每次 reload 都是一个新页面（新的 controller），计数来自窗口的存储。
    const page = harness(history);
    page.controller.connectionClosed(backlog, "local");
    assert.deepEqual(page.rendered.at(-1), { state: "reattaching", attempt, maxAttempts: 3 });
    page.timers[0]!.fire();
  }
  const last = harness(history);
  last.controller.connectionClosed(backlog, "local");
  assert.deepEqual(last.rendered, [{ state: "closed" }]);
  assert.equal(last.timers.length, 0, "no reload is scheduled");
  // 用户点“重新加载”：清空计数后 reload，之后的越过上限重新从第 1 次计。
  last.controller.reloadByUser();
  assert.equal(last.reloads(), 1);
  assert.deepEqual(history.stored, []);
  const fresh = harness(history);
  fresh.controller.connectionClosed(backlog, "local");
  assert.deepEqual(fresh.rendered, [{ state: "reattaching", attempt: 1, maxAttempts: 3 }]);
});

test("an unrecordable reattach does not reload", () => {
  const page = harness({ read: () => [], write: () => false, clear: () => undefined });
  page.controller.connectionClosed(backlog, "local");
  assert.deepEqual(page.rendered, [{ state: "closed" }]);
  assert.equal(page.timers.length, 0);
});

test("only the first close decides; ignored remote closes do not", () => {
  const page = harness();
  page.controller.connectionClosed({ code: 4004, reason: "scope-closed" }, "remote-scope");
  assert.deepEqual(page.rendered, []);
  page.controller.connectionClosed(backlog, "remote-scope");
  page.controller.connectionClosed(backlog, "local");
  page.controller.connectionClosed({ code: 1006, reason: "" }, "local");
  assert.deepEqual(page.rendered, [{ state: "reattaching", attempt: 1, maxAttempts: 3 }]);
  assert.equal(page.timers.length, 1);
});

test("an abnormal close waits for Main, as before", () => {
  const page = harness();
  page.controller.connectionClosed({ code: 1006, reason: "" }, "local");
  assert.deepEqual(page.rendered, [{ state: "reconnecting" }]);
  assert.equal(page.timers.length, 0);
  page.controller.hostRestartState({ state: "reconnecting", attempt: 1, maxAttempts: 3 });
  page.controller.hostRestartState({ state: "failed" });
  page.controller.hostRestartState({ state: "reconnecting", attempt: 2, maxAttempts: 3 });
  assert.deepEqual(page.rendered.at(-1), { state: "failed" }, "failed is not overwritten");
});

test("Main's restart progress cancels a scheduled reattach", () => {
  const page = harness();
  page.controller.connectionClosed(backlog, "local");
  page.controller.hostRestartState({ state: "reconnecting", attempt: 1, maxAttempts: 3 });
  assert.equal(page.timers[0]!.cancelled, true);
  assert.deepEqual(page.rendered.at(-1), { state: "reconnecting", attempt: 1, maxAttempts: 3 });
});
