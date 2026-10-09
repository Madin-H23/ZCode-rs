// Node Host 路径下 reload 回退会让同一个窗口短暂有两个 Host（旧 Host 正在 dispose，新 Host 已拉起）。
// U4 的 Main 侧把远端连接的归属从“窗口”改成“发起请求的 Host”（desktopRemoteSessions.ts 的
// ensureHostListener）：一个 Host 退出只清它自己的等待中连接与 route，不误伤同窗口新 Host 上的连接。
// 用法：node --import tsx --experimental-test-module-mocks --test packages/desktop/test/desktopRemoteSessionsHostExit.test.ts
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import test, { mock } from "node:test";

class FakeHost extends EventEmitter {
  readonly posted: Array<Record<string, unknown>> = [];
  constructor(readonly pid: number) {
    super();
  }
  postMessage(message: Record<string, unknown>): void {
    this.posted.push(message);
  }
  /** Host 发给某个请求的最后一条 connect 消息的 requestId。 */
  connectRequestIds(): string[] {
    return this.posted
      .filter((message) => message.type === "connect-remote-workspace")
      .map((message) => String(message.requestId));
  }
}

const target = { kind: "docker", container: "e2e" } as const;

function descriptor(remoteSessionId: string, workspacePath: string) {
  return {
    remoteSessionId,
    target,
    workspacePath,
    workspaceIdentity: `${workspacePath}@docker`,
    generation: 1,
  };
}

test("a host exit only clears the connects and routes that host owns", async () => {
  mock.module("electron", {
    namedExports: {
      app: { isPackaged: false },
      BrowserWindow: { getAllWindows: () => [] },
      MessageChannelMain: class {},
    },
  });
  const { createRemoteWorkspaceSessionManager } =
    await import("../src/main/desktopRemoteSessions.js");

  const win = {
    isDestroyed: () => false,
    webContents: { id: 1, isDestroyed: () => false, send: () => {}, postMessage: () => {} },
  };
  const oldHost = new FakeHost(101);
  const newHost = new FakeHost(102);
  let windowHost: FakeHost = oldHost;
  const manager = createRemoteWorkspaceSessionManager({
    logger: { info: () => {}, warn: () => {}, error: () => {} },
    resolveWindowHost: () => windowHost as never,
    resolveRemoteAssetDirs: () => ({}) as never,
  });

  // 旧 Host 上：一个已连上的 route（/old）和一个等待中的连接。
  const oldConnected = manager.createRemoteWorkspaceSession(win as never, target, "old-connected");
  const oldPending = manager.createRemoteWorkspaceSession(win as never, target, "old-pending");
  oldConnected.catch(() => {});
  oldPending.catch(() => {});
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(oldHost.connectRequestIds(), ["old-connected", "old-pending"]);
  oldHost.emit("message", {
    type: "remote-workspace-connected",
    requestId: "old-connected",
    descriptor: descriptor("session-old", "/old"),
  });

  // reload 回退：窗口换到新 Host，新 Host 上同样有一个 route（/new）和一个等待中的连接。
  windowHost = newHost;
  const newConnected = manager.createRemoteWorkspaceSession(win as never, target, "new-connected");
  const newPending = manager.createRemoteWorkspaceSession(win as never, target, "new-pending");
  newConnected.catch(() => {});
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(newHost.connectRequestIds(), ["new-connected", "new-pending"]);
  newHost.emit("message", {
    type: "remote-workspace-connected",
    requestId: "new-connected",
    descriptor: descriptor("session-new", "/new"),
  });
  const lookup = (workspacePath: string) =>
    manager.findWindowIdForRemoteWorkspace({
      workspacePath,
      workspaceIdentity: `${workspacePath}@docker`,
      target,
    });
  assert.equal(lookup("/old"), 1);
  assert.equal(lookup("/new"), 1);

  // 旧 Host 退出：只拒绝它的等待中连接、只撤掉它的 route。
  oldHost.emit("exit", 0);
  await assert.rejects(oldPending, /Local Host 已退出/);
  assert.equal(lookup("/old"), undefined);
  assert.equal(lookup("/new"), 1, "the new host's route survives the old host's exit");

  // 新 Host 的等待中连接仍按 (Host, requestId) 找得到：它的结果照常交回发起方。
  newHost.emit("message", {
    type: "remote-workspace-connect-failed",
    requestId: "new-pending",
    error: "probe failed on the new host",
  });
  await assert.rejects(newPending, /probe failed on the new host/);

  // 同一个 requestId 的结果只认发出它的 Host：旧 Host 迟到的消息不会落到新 Host 的请求上。
  const late = manager.createRemoteWorkspaceSession(win as never, target, "shared-id");
  await new Promise((resolve) => setImmediate(resolve));
  oldHost.emit("message", {
    type: "remote-workspace-connect-failed",
    requestId: "shared-id",
    error: "from the old host",
  });
  newHost.emit("message", {
    type: "remote-workspace-connect-failed",
    requestId: "shared-id",
    error: "from the new host",
  });
  await assert.rejects(late, /from the new host/);
});
