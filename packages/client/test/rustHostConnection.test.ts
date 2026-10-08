// R0 的跨语言验收（docs/specs/rust-host-r0.md §6）：用 Renderer 实际使用的 TS 客户端连接真实的
// zcode-host，并用 @zcode/shared 的 zod schema 校验 Rust 发出的控制消息，防止两侧定义漂移。
import assert from "node:assert/strict";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface } from "node:readline";
import test from "node:test";
import { createWebSocketServiceConnection } from "../src/index.js";
import {
  RUST_HOST_CAPABILITY_SUBPROTOCOL_PREFIX,
  RUST_HOST_RPC_SUBPROTOCOL,
  databaseStartupStateSchema,
  hostAttachmentEndpointIssuedResponseSchema,
  hostEndpointReadyResponseSchema,
  hostLogResponseSchema,
  serviceEndpointPayloadSchema,
} from "@zcode/shared";

const binary = process.env.ZCODE_RUST_HOST_BINARY;
if (!binary) throw new Error("ZCODE_RUST_HOST_BINARY is required (run scripts/rust-host/test.mjs)");

interface Host {
  child: ChildProcessWithoutNullStreams;
  send: (message: unknown) => void;
  next: () => Promise<Record<string, unknown>>;
}

function startHost(): Host {
  const child = spawn(binary!, ["--control", "stdio", "--allowed-origin", "file://"], {
    stdio: ["pipe", "pipe", "pipe"],
  });
  const queued: Record<string, unknown>[] = [];
  const waiters: ((message: Record<string, unknown>) => void)[] = [];
  createInterface({ input: child.stdout }).on("line", (line) => {
    const message = JSON.parse(line) as Record<string, unknown>;
    // 结构化日志必须符合 Main 的 hostLogResponseSchema，校验后不交给测试步骤。
    if (message.type === "log") {
      hostLogResponseSchema.parse(message);
      return;
    }
    const waiter = waiters.shift();
    if (waiter) waiter(message);
    else queued.push(message);
  });
  return {
    child,
    send: (message) => child.stdin.write(`${JSON.stringify(message)}\n`),
    next: () => {
      const message = queued.shift();
      if (message) return Promise.resolve(message);
      return new Promise((resolve) => waiters.push(resolve));
    },
  };
}

async function initialize(host: Host): Promise<void> {
  host.send({ type: "init-local", databaseStartupId: "startup-ts" });
  const state = await host.next();
  assert.equal(state.type, "database-startup-state");
  const parsed = databaseStartupStateSchema.parse(state.state);
  assert.equal(parsed.phase, "ready");
  assert.equal(parsed.startupId, "startup-ts");
}

async function issueEndpoint(host: Host): Promise<{ url: string; capability: string }> {
  host.send({
    type: "issue-attachment-endpoint",
    requestId: "request-ts",
    attachmentId: "attachment-ts",
    clientMode: "desktop-continuous",
    scope: { kind: "local" },
  });
  const issued = hostAttachmentEndpointIssuedResponseSchema.parse(await host.next());
  assert.equal(issued.ok, true);
  if (!issued.ok) throw new Error(issued.error);
  const endpoint = { url: issued.url, capability: issued.capability };
  // Main 原样转交给 preload 的载荷也必须通过 Renderer 侧的校验。
  serviceEndpointPayloadSchema.parse({ databaseStartupId: "startup-ts", ...endpoint });
  return endpoint;
}

const protocols = (capability: string) => [
  RUST_HOST_RPC_SUBPROTOCOL,
  `${RUST_HOST_CAPABILITY_SUBPROTOCOL_PREFIX}${capability}`,
];

async function startedHost(): Promise<Host> {
  const host = startHost();
  hostEndpointReadyResponseSchema.parse(await host.next());
  return host;
}

test("renderer client gets immediate stub replies from the Rust Host", async () => {
  const host = await startedHost();
  try {
    await initialize(host);
    const { url, capability } = await issueEndpoint(host);
    const connection = await createWebSocketServiceConnection(url, {
      protocols: protocols(capability),
    });

    const started = performance.now();
    await assert.rejects(connection.services.settingService.get(), (error: Error) => {
      assert.equal(error.name, "NotImplemented");
      assert.equal((error as Error & { code?: string }).code, "fault.host.notImplemented");
      return true;
    });
    assert.ok(performance.now() - started < 100, "stub reply within 100 ms");

    assert.deepEqual(await connection.services.settingsSyncService.getFirstRunPromptState(), {
      handled: true,
    });
    // 订阅被接受且不报错；之后的调用照常应答。
    const subscription = connection.services.zcodeAgentService.onDynamicConversationFrame({
      workspacePath: "/w",
    })(() => assert.fail("stub events never fire"));
    await assert.rejects(connection.services.fileService.stat("/tmp"));
    subscription.dispose();
    connection.dispose();
  } finally {
    host.child.kill();
  }
});

test("pending calls are rejected as soon as the Host goes away", async () => {
  const host = await startedHost();
  try {
    // 不发 init-local：连接建立但 Initialize 未到，调用在客户端排队挂起。
    const { url, capability } = await issueEndpoint(host);
    const connection = await createWebSocketServiceConnection(url, {
      protocols: protocols(capability),
    });
    const pending = connection.services.settingService.get();
    host.child.kill();
    await assert.rejects(pending, /Service connection closed/);
  } finally {
    host.child.kill();
  }
});
