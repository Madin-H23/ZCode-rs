// App 级 Rust Host owner（packages/desktop/src/main/rustAppHost.ts）的接线（docs/specs/rust-u4-app-host.md
// §2.1、§2.8）：状态机之外，owner 自己负责的两件事——拉起失败按这一代意外退出处理；窗口级的
// “Host 已可用”动作在 endpoint 交给当前页面之后才执行。Electron 与进程层用 mock 替换。
// 用法：node --import tsx --experimental-test-module-mocks --test packages/desktop/test/rustAppHostWiring.test.ts
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import test, { mock } from "node:test";

type Sent = { channel: string; payload: unknown };

function fakeWindow(id: number) {
  const sent: Sent[] = [];
  return {
    sent,
    isDestroyed: () => false,
    isFocused: () => false,
    webContents: {
      id,
      isDestroyed: () => false,
      send: (channel: string, payload: unknown) => sent.push({ channel, payload }),
      reload: () => sent.push({ channel: "reload", payload: null }),
    },
  };
}

const windows: Array<ReturnType<typeof fakeWindow>> = [];
type SpawnHooks = {
  onSpawned: (host: unknown) => void;
  onDatabaseStartupState: (state: { startupId: string; phase: string; attemptId: string }) => void;
  onExit: (code: number) => void;
};
let spawnBehavior: (startupId: string, hooks: SpawnHooks) => unknown = () => {
  throw new Error("spawn behavior not set");
};
const spawnCalls: string[] = [];
let endpointDelivery: () => Promise<void> = () => Promise.resolve();

mock.module("electron", {
  namedExports: { BrowserWindow: { getAllWindows: () => windows } },
});
mock.module("../src/main/resourceManagerWindow.js", {
  namedExports: { isMainApplicationWindowWebContents: () => true },
});
mock.module("../src/main/databaseStartupRelay.js", {
  namedExports: {
    bindAppDatabaseStartupRelay: (_child: unknown, startupId: string) => ({
      receive: () => {},
      startupId,
    }),
  },
});
mock.module("../src/main/desktopHostProcess.js", {
  namedExports: {
    spawnRustAppHostProcess: (
      _label: string,
      init: { databaseStartupId: string },
      _dependencies: unknown,
      hooks: SpawnHooks,
    ) => {
      spawnCalls.push(init.databaseStartupId);
      return spawnBehavior(init.databaseStartupId, hooks);
    },
    disposeHostProcessAndWait: async () => {},
  },
});
mock.module("../src/main/rustDesktopHost.js", {
  namedExports: {
    sendRustHostServiceEndpoint: async (
      win: ReturnType<typeof fakeWindow>,
      _host: unknown,
      startupId: string,
      _logger: unknown,
      isCurrentPage: () => boolean,
    ) => {
      await endpointDelivery();
      if (!isCurrentPage()) return false;
      win.webContents.send("endpoint", startupId);
      return true;
    },
  },
});

const { createRustAppHost } = await import("../src/main/rustAppHost.js");

class FakeHostProcess extends EventEmitter {
  readonly pid = 4242;
  killed = false;
  postMessage(): void {}
  kill(): boolean {
    this.killed = true;
    return true;
  }
  asUtilityProcess(): this {
    return this;
  }
}

function createOwner() {
  return createRustAppHost({
    logger: { info: () => {}, warn: () => {} },
    getDependencies: () => ({}) as never,
    prepareInit: async () => ({ deviceMid: "e2e" }) as never,
    findWindowIdForRemoteWorkspace: () => undefined,
    onSettingChanged: () => {},
    onOffPeakActiveCountChanged: () => {},
    disposingHostProcessTimers: new WeakMap(),
  });
}

const flush = async () => {
  for (let index = 0; index < 5; index += 1) await new Promise((resolve) => setImmediate(resolve));
};

function connectionStates(win: ReturnType<typeof fakeWindow>) {
  return win.sent
    .filter((entry) => entry.channel === "zcode:host-connection-state")
    .map((entry) => entry.payload);
}

test("a synchronous spawn failure is an unexpected exit of that generation, not a silent hang", async () => {
  mock.timers.enable({ apis: ["setTimeout"] });
  try {
    windows.length = 0;
    spawnCalls.length = 0;
    const win = fakeWindow(1);
    windows.push(win);
    spawnBehavior = () => {
      throw new Error("spawn EINVAL");
    };
    const owner = createOwner();
    owner.start();
    owner.attachWindow(win as never);
    await flush();
    assert.equal(spawnCalls.length, 1);
    assert.deepEqual(connectionStates(win), [
      { state: "restarting", attempt: 1, maxAttempts: 3, delayMs: 500 },
    ]);

    // 退避结束后拉起下一代；这次成功并就绪，窗口（属于失败那一代）被 reload 后重新接入。
    let hooks: SpawnHooks | undefined;
    const host = new FakeHostProcess();
    spawnBehavior = (_startupId, spawnHooks) => {
      hooks = spawnHooks;
      spawnHooks.onSpawned(host);
      return host;
    };
    mock.timers.tick(500);
    await flush();
    assert.equal(spawnCalls.length, 2);
    hooks?.onDatabaseStartupState({ startupId: spawnCalls[1]!, phase: "ready", attemptId: "a" });
    assert.ok(win.sent.some((entry) => entry.channel === "reload"));
  } finally {
    mock.timers.reset();
  }
});

test("window hooks run only after the endpoint reaches the current page", async () => {
  windows.length = 0;
  spawnCalls.length = 0;
  const win = fakeWindow(2);
  windows.push(win);
  let hooks: SpawnHooks | undefined;
  spawnBehavior = (_startupId, spawnHooks) => {
    hooks = spawnHooks;
    spawnHooks.onSpawned(new FakeHostProcess());
    return undefined;
  };
  const owner = createOwner();
  owner.start();
  await flush();

  // Host 尚未就绪：dom-ready 不触发窗口级动作。
  const attached: string[] = [];
  owner.attachWindow(win as never, () => attached.push("first page"));
  await flush();
  assert.deepEqual(attached, []);

  // 就绪后签发；签发完成之前页面 reload 了：旧页面的动作不执行，新页面拿到 endpoint 后执行一次。
  let deliver!: () => void;
  endpointDelivery = () =>
    new Promise<void>((resolve) => {
      deliver = resolve;
    });
  hooks?.onDatabaseStartupState({ startupId: spawnCalls[0]!, phase: "ready", attemptId: "a" });
  endpointDelivery = () => Promise.resolve();
  owner.attachWindow(win as never, () => attached.push("reloaded page"));
  await flush();
  deliver();
  await flush();
  assert.deepEqual(attached, ["reloaded page"]);
  assert.equal(win.sent.filter((entry) => entry.channel === "endpoint").length, 1);
});
