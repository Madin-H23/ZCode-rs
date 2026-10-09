// 数据库启动状态中继（packages/desktop/src/main/databaseStartupRelay.ts）在 Host 退出之后的行为
// （docs/specs/rust-u4-app-host.md §2.8）：App 级 Rust Host 退出后，退避期间加载的页面发 `snapshot`
// 拿不到上一代的 failed/transport_closed（它属于下一代，应停在“正在启动”）；失败页的“退出”照常可用。
// Node 窗口 Host 的中继不变：退出后 `snapshot` 仍回放最后状态。
// 用法：node --import tsx --experimental-test-module-mocks --test packages/desktop/test/databaseStartupRelayExit.test.ts
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import test, { mock } from "node:test";
import { InternalChannels, type DatabaseStartupState } from "@zcode/shared";

const ipcMain = new EventEmitter();
mock.module("electron", { namedExports: { ipcMain } });
mock.module("../src/main/databaseStartupTelemetry.js", {
  namedExports: { reportDatabaseStartupState: () => {} },
});
const relay = await import("../src/main/databaseStartupRelay.js");

function fakeWindow(id: number) {
  const received: DatabaseStartupState[] = [];
  const events = new EventEmitter();
  return {
    received,
    isDestroyed: () => false,
    once: (name: string, listener: () => void) => events.once(name, listener),
    removeListener: (name: string, listener: () => void) => events.removeListener(name, listener),
    webContents: {
      id,
      isDestroyed: () => false,
      send: (_channel: string, state: DatabaseStartupState) => received.push(state),
    },
  };
}

function fakeHost() {
  const host = new EventEmitter() as EventEmitter & {
    posted: unknown[];
    postMessage(m: unknown): void;
  };
  host.posted = [];
  host.postMessage = (message) => host.posted.push(message);
  return host;
}

function readyState(startupId: string): DatabaseStartupState {
  const now = Date.now();
  return {
    schemaVersion: 1,
    startupId,
    attemptId: "attempt-1",
    sequence: 1,
    startedAt: now,
    updatedAt: now,
    phase: "ready",
    disk: [],
  };
}

const control = (sender: { webContents: unknown }, raw: unknown) =>
  ipcMain.emit(InternalChannels.DatabaseStartupControl, { sender: sender.webContents }, raw);

test("after the app host exits, a page loaded during the backoff gets no stale failure", () => {
  let quits = 0;
  relay.configureDatabaseStartupQuit(() => {
    quits += 1;
  });
  const first = fakeWindow(1);
  const host = fakeHost();
  const binding = relay.bindAppDatabaseStartupRelay(host as never, "gen-1", {
    listMainWindows: () => [first as never],
    isMainWindowSender: () => true,
  });
  binding.receive(readyState("gen-1"));
  assert.deepEqual(
    first.received.map((state) => state.phase),
    ["ready"],
  );

  // 意外退出：已打开的页面立即收到 failed/transport_closed。
  host.emit("exit", 1);
  assert.equal(first.received.at(-1)?.phase, "failed");
  assert.equal(first.received.at(-1)?.errorCode, "transport_closed");

  // 退避期间 reload 的页面问 snapshot：不回放上一代的失败。
  const reloaded = fakeWindow(2);
  control(reloaded, { action: "snapshot" });
  assert.deepEqual(reloaded.received, []);
  // 放弃重启后的失败页“退出”仍然有效。
  control(reloaded, { action: "exit" });
  assert.equal(quits, 1);

  // 下一代接管：新绑定的状态照常扇出并可被 snapshot。
  const next = fakeHost();
  const nextBinding = relay.bindAppDatabaseStartupRelay(next as never, "gen-2", {
    listMainWindows: () => [reloaded as never],
    isMainWindowSender: () => true,
  });
  nextBinding.receive(readyState("gen-2"));
  control(reloaded, { action: "snapshot" });
  assert.deepEqual(
    reloaded.received.map((state) => [state.startupId, state.phase]),
    [
      ["gen-2", "ready"],
      ["gen-2", "ready"],
    ],
  );
  control(reloaded, { action: "exit" });
  assert.equal(quits, 2, "the replaced binding no longer handles controls twice");
});

test("the Node window host relay still replays its last state after an exit", () => {
  const win = fakeWindow(3);
  const host = fakeHost();
  const binding = relay.bindDatabaseStartupRelay(win as never, host as never, "node-1");
  binding.receive(readyState("node-1"));
  host.emit("exit", 1);
  // 只看这个 Node 绑定的回放（上一个测试的 App 级绑定仍挂在同一个 ipcMain 上）。
  const ownStates = () => win.received.filter((state) => state.startupId === "node-1");
  const before = ownStates().length;
  control(win, { action: "snapshot" });
  assert.equal(ownStates().length, before + 1);
  assert.equal(ownStates().at(-1)?.errorCode, "transport_closed");
});
