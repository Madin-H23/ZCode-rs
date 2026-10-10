// R0 的跨语言验收（docs/specs/rust-host-r0.md §6）：用 Renderer 实际使用的 TS 客户端连接真实的
// zcode-host，并用 @zcode/shared 的 zod schema 校验 Rust 发出的控制消息，防止两侧定义漂移。
import assert from "node:assert/strict";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import test from "node:test";
import {
  createWebSocketServiceConnection,
  type WebSocketConnectionCloseEvent,
} from "../src/index.js";
import {
  RUST_HOST_CAPABILITY_SUBPROTOCOL_PREFIX,
  RUST_HOST_CLOSE_CODES,
  RUST_HOST_RPC_SUBPROTOCOL,
  databaseStartupStateSchema,
  hostAttachmentEndpointIssuedResponseSchema,
  hostEndpointReadyResponseSchema,
  hostLogResponseSchema,
  hostOffPeakActiveCountResponseSchema,
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
  // 测试 Host 使用临时 home 与数据根，绝不读写开发机真实的 ~/.zcode。
  const home = mkdtempSync(join(tmpdir(), "zcode-rust-host-"));
  const child = spawn(binary!, ["--control", "stdio", "--allowed-origin", "file://"], {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      HOME: home,
      USERPROFILE: home,
      ZCODE_DESKTOP_HOME_DIR: home,
      ZCODE_DATA_BASE_DIR: home,
    },
  });
  child.on("exit", () => rmSync(home, { recursive: true, force: true }));
  const queued: Record<string, unknown>[] = [];
  const waiters: ((message: Record<string, unknown>) => void)[] = [];
  createInterface({ input: child.stdout }).on("line", (line) => {
    const message = JSON.parse(line) as Record<string, unknown>;
    // 结构化日志必须符合 Main 的 hostLogResponseSchema，校验后不交给测试步骤。
    if (message.type === "log") {
      hostLogResponseSchema.parse(message);
      return;
    }
    // 调度器对执行中闲时任务数的上报与测试步骤无关、随时可能到达：按 Main 的 schema 校验后同样不交给步骤。
    if (message.type === "off-peak-active-count") {
      hostOffPeakActiveCountResponseSchema.parse(message);
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
  // starting → preparing_host_storage（tasks-index 迁移）→ starting_services → ready；
  // 每一条都必须通过 Main / Renderer 使用的 strict schema。
  const phases: string[] = [];
  for (;;) {
    const state = await host.next();
    assert.equal(state.type, "database-startup-state");
    const parsed = databaseStartupStateSchema.parse(state.state);
    assert.equal(parsed.startupId, "startup-ts");
    phases.push(parsed.phase);
    if (parsed.phase === "ready" || parsed.phase === "failed") break;
  }
  assert.equal(phases[0], "starting");
  assert.ok(phases.includes("preparing_host_storage"));
  assert.deepEqual(phases.slice(-2), ["starting_services", "ready"]);
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
    // 桌面本地 Host 不实现 provider-provisioning-target（Node 只在远端 / server 目标注册），一直是契约 stub。
    const envelope = {} as Parameters<
      typeof connection.services.providerProvisioningTargetService.apply
    >[0];
    await assert.rejects(
      connection.services.providerProvisioningTargetService.apply(envelope),
      (error: Error) => {
        assert.equal(error.name, "NotImplemented");
        assert.equal((error as Error & { code?: string }).code, "fault.host.notImplemented");
        return true;
      },
    );
    assert.ok(performance.now() - started < 100, "stub reply within 100 ms");

    // 已实现的 service：临时 home 下返回默认设置。
    assert.equal((await connection.services.settingService.get()).locale, "zh-CN");

    // 已移植：新 profile 尚未处理首次迁移提示。
    assert.equal(
      (await connection.services.settingsSyncService.getFirstRunPromptState()).handled,
      false,
    );
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

/**
 * Host 主动关闭连接时 Close 帧带状态码与原因（docs/specs/rust-u2-gateway.md §2.6）：Renderer 的客户端在
 * onClose 里拿到的值与 `RUST_HOST_CLOSE_CODES` 一致（防止两侧定义漂移）。越过出站上限（4001）需要调试
 * 构建的小上限，见 crates/zcode-host/tests/suite/close_reasons.rs 与桌面 E2E gateway-close-recovery。
 */
test("the Host names why it closed a connection in the WebSocket Close frame", async () => {
  const host = await startedHost();
  try {
    await initialize(host);
    const closed = async (attachmentId: string, trigger: () => void) => {
      host.send({
        type: "issue-attachment-endpoint",
        requestId: `issue-${attachmentId}`,
        attachmentId,
        clientMode: "desktop-continuous",
        scope: { kind: "local" },
      });
      const issued = hostAttachmentEndpointIssuedResponseSchema.parse(await host.next());
      if (!issued.ok) throw new Error(issued.error);
      let resolveClose!: (event: WebSocketConnectionCloseEvent) => void;
      const event = new Promise<WebSocketConnectionCloseEvent>((resolve) => {
        resolveClose = resolve;
      });
      const connection = await createWebSocketServiceConnection(issued.url, {
        protocols: protocols(issued.capability),
        onClose: resolveClose,
      });
      // 连接已就绪（[200] 之后的调用有应答）再触发关闭。
      await connection.services.settingService.get();
      trigger();
      return event;
    };
    const detached = await closed("detach-ts", () =>
      host.send({ type: "detach-service-port", attachmentId: "detach-ts" }),
    );
    assert.deepEqual(
      { code: detached.code, reason: detached.reason },
      { code: RUST_HOST_CLOSE_CODES.detached, reason: "detached" },
    );
    const shutdown = await closed("dispose-ts", () => host.send({ type: "dispose" }));
    assert.deepEqual(
      { code: shutdown.code, reason: shutdown.reason },
      { code: RUST_HOST_CLOSE_CODES.hostShutdown, reason: "host-shutdown" },
    );
  } finally {
    host.child.kill();
  }
});
