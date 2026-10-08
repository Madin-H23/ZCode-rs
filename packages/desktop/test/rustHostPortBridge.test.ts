// Main 的 MessagePort ⇄ WebSocket 桥（packages/desktop/src/main/rustHostPortBridge.ts）对真实 Rust Host 的
// 验收（docs/specs/rust-host-tasks.md §6）：签发 web-remote-replayable endpoint，经桥用 MessagePortProtocol
// 的 ChannelClient 调用 zcode-agent，hello 报告 replayable 投递；关闭端口后 WebSocket 一并关闭。
// 用法：ZCODE_RUST_HOST_BINARY=<zcode-host> node --import tsx --test packages/desktop/test/rustHostPortBridge.test.ts
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
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
