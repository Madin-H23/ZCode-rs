// Main 的 MessagePort ⇄ WebSocket 桥（packages/desktop/src/main/rustHostPortBridge.ts）对真实 Rust Host 的
// 验收（docs/specs/rust-host-tasks.md §6、rust-host-remote.md §6）：签发 web-remote-replayable endpoint
// （本地与远端 scope），经桥用 MessagePortProtocol 的 ChannelClient 调用，hello 报告 replayable 投递；
// 关闭端口后 WebSocket 一并关闭。
// 用法：ZCODE_RUST_HOST_BINARY=<zcode-host> node --import tsx --test packages/desktop/test/rustHostPortBridge.test.ts
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import { existsSync, mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createInterface } from "node:readline";
import test from "node:test";
import { ChannelClient, MessagePortProtocol, type MessagePortLike } from "@zcode/rpc";
import { hostAttachmentEndpointIssuedResponseSchema } from "@zcode/shared";
import { bridgeMessagePortToRustHost } from "../src/main/rustHostPortBridge.js";

const binary = process.env.ZCODE_RUST_HOST_BINARY;
if (!binary) throw new Error("ZCODE_RUST_HOST_BINARY is required (run scripts/rust-host/test.mjs)");

type Payload = unknown;

/** 一对互连的假端口：main 侧模仿 Electron MessagePortMain，peer 侧模仿 DOM MessagePort。 */
function fakeChannel() {
  const main = new EventEmitter() as EventEmitter & {
    postMessage(data: Payload): void;
    start(): void;
    close(): void;
  };
  const peerListeners = new Set<(e: { data: Payload }) => void>();
  let closed = false;
  const peer: MessagePortLike = {
    addEventListener: (_type, listener) => peerListeners.add(listener as never),
    removeEventListener: (_type, listener) => peerListeners.delete(listener as never),
    postMessage: (data) => queueMicrotask(() => main.emit("message", { data })),
    start: () => {},
    close: () => main.close(),
  };
  main.postMessage = (data) => queueMicrotask(() => peerListeners.forEach((l) => l({ data })));
  main.start = () => {};
  main.close = () => {
    if (closed) return;
    closed = true;
    main.emit("close");
  };
  return { main, peer, isClosed: () => closed };
}

test("replayable attachment bridged from a MessagePort reaches the Rust Host", async () => {
  // 临时 home 与数据根：不读写开发机真实的 ~/.zcode。
  const home = mkdtempSync(join(tmpdir(), "zcode-port-bridge-"));
  const child = spawn(binary, ["--control", "stdio", "--allowed-origin", "file://"], {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, HOME: home, ZCODE_DESKTOP_HOME_DIR: home, ZCODE_DATA_BASE_DIR: home },
  });
  const messages: Record<string, unknown>[] = [];
  const waiters: (() => void)[] = [];
  createInterface({ input: child.stdout }).on("line", (line) => {
    const message = JSON.parse(line) as Record<string, unknown>;
    if (message.type === "log") return;
    messages.push(message);
    waiters.splice(0).forEach((wake) => wake());
  });
  const next = async (type: string) => {
    for (;;) {
      const index = messages.findIndex((m) => m.type === type);
      if (index >= 0) return messages.splice(index, 1)[0]!;
      await new Promise<void>((wake) => waiters.push(wake));
    }
  };
  const send = (message: unknown) => child.stdin.write(`${JSON.stringify(message)}\n`);
  try {
    await next("host-endpoint-ready");
    send({ type: "init-local", databaseStartupId: "startup-bridge" });
    send({
      type: "issue-attachment-endpoint",
      requestId: "r1",
      attachmentId: "bot-attachment",
      clientMode: "web-remote-replayable",
      scope: { kind: "local" },
    });
    const issued = hostAttachmentEndpointIssuedResponseSchema.parse(
      await next("attachment-endpoint-issued"),
    );
    if (!issued.ok) throw new Error(issued.error);

    const { main, peer, isClosed } = fakeChannel();
    const bridge = await bridgeMessagePortToRustHost(main as never, issued);
    const client = new ChannelClient(new MessagePortProtocol(peer));
    const hello = (await client
      .getChannel("zcode-agent")
      .call("helloConversationV4", { workspacePath: "/w" })) as Record<string, any>;
    assert.equal(hello.clientMode, "web-remote-replayable");
    assert.equal(hello.deliveryProfile, "replayable");
    assert.equal(hello.capabilities.nativeDialogs, false);

    bridge.close();
    assert.equal(isClosed(), true);
    client.dispose();
  } finally {
    child.kill();
    rmSync(home, { recursive: true, force: true });
  }
});

const root = resolve(import.meta.dirname, "../../..");
const serverBundle = join(root, "packages/server/dist/remote/zcode-server.cjs");

// 远端 scope（Bot 远程运行时、手机远控，docs/specs/rust-host-remote.md §6）：Main 的 AttachServicePort
// 带远端 scope 时同样签发 endpoint 后桥接。远端由测试连接器在本机拉起本仓库构建的 zcode-server.cjs
// （scripts/rust-host/remote-connector-local.ts），出站 HTTP 指向已关闭的本机端口。
test(
  "remote-scope replayable attachment bridged from a MessagePort reaches the remote server",
  {
    skip: existsSync(serverBundle)
      ? false
      : "needs the remote server bundle: pnpm --filter @zcode/server build:remote",
  },
  async () => {
    const temp = mkdtempSync(join(tmpdir(), "zcode-remote-bridge-"));
    const home = join(temp, "home");
    const remoteHome = join(temp, "remote-home");
    const workspace = join(temp, "workspace");
    for (const dir of [home, remoteHome, workspace]) mkdirSync(dir, { recursive: true });
    writeFileSync(join(workspace, "hello.txt"), "hello over the bridge\n");
    const workspacePath = realpathSync(workspace);
    const tsxLoader = pathToFileURL(join(root, "node_modules/tsx/dist/loader.mjs")).href;
    const child = spawn(binary, ["--control", "stdio", "--allowed-origin", "file://"], {
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
        HOME: home,
        ZCODE_DESKTOP_HOME_DIR: home,
        ZCODE_DATA_BASE_DIR: home,
        // 产品连接器的位置换成测试连接器：Electron 换成当前 node，并预加载 tsx 运行 TS 源码。
        ZCODE_HOST_NODE_EXEC_PATH: process.execPath,
        ZCODE_REMOTE_CONNECTOR_PATH: join(root, "scripts/rust-host/remote-connector-local.ts"),
        NODE_OPTIONS: `--import=${tsxLoader}`,
        ZCODE_TEST_REMOTE_NODE: process.execPath,
        ZCODE_TEST_REMOTE_SERVER_BUNDLE: serverBundle,
        ZCODE_TEST_REMOTE_HOME: remoteHome,
        ZCODE_TEST_REMOTE_DEAD_PROXY: "http://127.0.0.1:9",
      },
    });
    const messages: Record<string, unknown>[] = [];
    const waiters: (() => void)[] = [];
    createInterface({ input: child.stdout }).on("line", (line) => {
      const message = JSON.parse(line) as Record<string, unknown>;
      if (message.type === "log") return;
      messages.push(message);
      waiters.splice(0).forEach((wake) => wake());
    });
    const next = async (type: string) => {
      for (;;) {
        const index = messages.findIndex((m) => m.type === type);
        if (index >= 0) return messages.splice(index, 1)[0]!;
        await new Promise<void>((wake) => waiters.push(wake));
      }
    };
    const send = (message: unknown) => child.stdin.write(`${JSON.stringify(message)}\n`);
    try {
      await next("host-endpoint-ready");
      send({ type: "init-local", databaseStartupId: "startup-remote-bridge" });
      // 远端控制消息在服务装好（ready）之后才有处理者，与 Main 在窗口就绪后才发起连接一致。
      for (;;) {
        const state = (await next("database-startup-state")) as { state: { phase: string } };
        if (state.state.phase === "ready") break;
      }
      send({
        type: "connect-remote-workspace",
        requestId: "connect-1",
        target: { kind: "docker", container: "bridge-test" },
        remoteAssets: {},
        workspacePath,
      });
      const connected = (await next("remote-workspace-connected")) as {
        descriptor: { remoteSessionId: string; workspaceIdentity: string };
      };
      const scope = {
        kind: "remote",
        remoteSessionId: connected.descriptor.remoteSessionId,
        workspacePath,
        workspaceIdentity: connected.descriptor.workspaceIdentity,
      };
      // scope 与 logical session 不一致时拒签，绝不回落到本地服务。
      send({
        type: "issue-attachment-endpoint",
        requestId: "wrong",
        attachmentId: "bot-remote-wrong",
        clientMode: "web-remote-replayable",
        scope: { ...scope, workspaceIdentity: workspacePath },
      });
      const rejected = hostAttachmentEndpointIssuedResponseSchema.parse(
        await next("attachment-endpoint-issued"),
      );
      assert.equal(rejected.ok, false);

      send({
        type: "issue-attachment-endpoint",
        requestId: "remote",
        attachmentId: "bot-remote",
        clientMode: "web-remote-replayable",
        scope,
      });
      const issued = hostAttachmentEndpointIssuedResponseSchema.parse(
        await next("attachment-endpoint-issued"),
      );
      if (!issued.ok) throw new Error(issued.error);
      const { main, peer, isClosed } = fakeChannel();
      await bridgeMessagePortToRustHost(main as never, issued);
      const client = new ChannelClient(new MessagePortProtocol(peer));
      const hello = (await client
        .getChannel("zcode-agent")
        .call("helloConversationV4", { workspacePath })) as Record<string, any>;
      assert.equal(hello.clientMode, "web-remote-replayable");
      const text = (await client
        .getChannel("file")
        .call("readTextFile", { path: join(workspacePath, "hello.txt") })) as { content: string };
      assert.equal(text.content, "hello over the bridge\n");

      // 释放 logical session：Host 关闭它的 attachment，桥随之关闭端口。
      send({
        type: "dispose-remote-workspace-session",
        requestId: "dispose-1",
        remoteSessionId: scope.remoteSessionId,
      });
      await next("remote-workspace-closed");
      for (let attempt = 0; attempt < 50 && !isClosed(); attempt += 1) {
        await new Promise((wake) => setTimeout(wake, 20));
      }
      assert.equal(isClosed(), true);
      client.dispose();
    } finally {
      child.kill();
      rmSync(temp, { recursive: true, force: true });
    }
  },
);
