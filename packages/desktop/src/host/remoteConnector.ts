// 远端连接器（docs/specs/rust-host-remote.md §2.1）：Rust Host 为每个远端连接拉起一个本进程
// （Electron 以 ELECTRON_RUN_AS_NODE 运行），由它用原有的 @zcode/server/remote 完成探测、部署、
// 启动远端 zcode-server 与 hello 握手，之后把远端 stdio 字节与 Host 之间原样转发。
//
// 部署链（CDN 清单、SHA 去重、远端部署锁、node-pty、agent bundle）与 ssh2 认证语义保持逐字节不变；
// Host 只拿到一条远端 @zcode/rpc 字节流，以及本进程提供的 backend 能力（附件上传与物化）。
import { ChannelServer, Emitter, ProxyChannel, VSBuffer } from "@zcode/rpc";
import type { IRemoteBackend, StdioStream } from "@zcode/server/remote";
import type { RemoteRuntimeNetworkOptions } from "@zcode/server/remote/connect.js";
import type { HelloMessage, RemoteTarget, ZCodePromptAttachment } from "@zcode/shared";
import { stringifyHostLogArg } from "./hostLog.js";
import { createRemotePromptAttachmentTransferService } from "./promptAttachmentTransferService.js";
import { materializeRemotePromptAttachments } from "./remotePromptAttachments.js";
import {
  REMOTE_CONNECTOR_STREAM,
  RemoteConnectorFrameReader,
  encodeRemoteConnectorControl,
  encodeRemoteConnectorFrame,
} from "./remoteConnectorProtocol.js";

/** 关闭远端 stdio 后等待其退出的上限，与 Node Host 的 disposeAndWait 相同。 */
const DISPOSE_TIMEOUT_MS = 5_000;

export interface RemoteConnectRequest {
  target: RemoteTarget;
  remoteAssets: Record<string, unknown>;
  network: { httpProxy?: string; noProxy?: string; caCertPath?: string };
  remoteRuntimeNetwork?: RemoteRuntimeNetworkOptions;
}

export interface OpenedRemoteServer {
  hello: HelloMessage;
  stream: StdioStream;
  /** hello 行之后、同一块数据里的剩余字节（属于 rpc 字节流）。 */
  remaining: Buffer | null;
  backend: Pick<IRemoteBackend, "exec" | "upload">;
  onDidDisconnect?: (listener: (event: { reason: string; error?: Error }) => void) => {
    dispose(): void;
  };
  dispose(): void;
  disposeAndWait(): Promise<void>;
}

export type OpenRemoteServer = (
  request: RemoteConnectRequest,
  signal: AbortSignal,
) => Promise<OpenedRemoteServer>;

/** 产品路径：与 Node Host 的 setupRemoteConnection + connectRemote 相同的步骤，只是不包成 ChannelClient。 */
export const openDeployedRemoteServer: OpenRemoteServer = async (request, signal) => {
  const [remote, connect, platform, services, shared] = await Promise.all([
    import("@zcode/server/remote"),
    import("@zcode/server/remote/connect.js"),
    import("@zcode/server/remote/remotePlatformSupport.js"),
    import("@zcode/services/node"),
    import("@zcode/shared"),
  ]);
  const { target } = request;
  const backend = await remote.createRemoteBackend(target);
  let backendDisposed = false;
  const disposeBackend = () => {
    if (backendDisposed) return;
    backendDisposed = true;
    backend.dispose();
  };
  const network = services.createHostApiNetworkTransport(async () => request.network);
  // SSH 由窗口级 registry 串行复用并可取消；WSL / Docker 的取消等连接自然结束后再释放（同 Node Host）。
  const abortable = target.kind === "ssh";
  const aborted = () => abortable && signal.aborted;
  const onAbort = () => {
    if (abortable) disposeBackend();
  };
  signal.addEventListener("abort", onAbort, { once: true });
  try {
    console.log("detecting remote env...");
    const env = await backend.detect();
    if (aborted()) throw new Error("Remote connection canceled");
    console.log("detected:", env);
    platform.assertSupportedRemoteEnvironment(env);
    const log = (...args: unknown[]) => console.log(...args);
    const remoteRuntimeNetwork = await connect.resolveRemoteRuntimeNetwork(
      backend,
      request.remoteRuntimeNetwork,
      log,
    );
    const options = {
      ...request.remoteAssets,
      remoteAssetNetwork: { fetch: network.fetch },
      remoteRuntimeNetwork,
      ...(abortable ? { signal } : {}),
      appVersion: shared.ZCODE_VERSION,
      remoteRuntimeEnv: remote.pickRemoteRuntimeEnv(process.env),
      assetInstallMode: target.kind === "ssh" ? target.assetInstallMode : undefined,
      deployLockMode: target.kind === "ssh" ? ("caller-serialized" as const) : ("remote" as const),
    };
    console.log("deploying server...");
    await remote.deployServer(backend, env, options);
    if (aborted()) throw new Error("Remote connection canceled");
    console.log("deploy complete");
    console.log("launching remote server...");
    const stream = await backend.exec(
      connect.buildRemoteServerCommand(options, remoteRuntimeNetwork),
    );
    if (aborted()) throw new Error("Remote connection canceled");
    stream.stderr.on("data", (chunk: Buffer) => {
      // 远端 zcode-server 的服务日志走 stderr：转成连接日志，便于排查 sqlite 初始化与锁冲突。
      console.log(`[remote] ${chunk.toString().trimEnd()}`);
    });
    console.log("performing handshake...");
    const { hello, remaining } = await remote.performHandshake(stream, `desktop-${Date.now()}`);
    console.log("handshake done, server version:", hello.version);
    return {
      hello,
      stream,
      remaining,
      backend,
      onDidDisconnect: backend.onDidDisconnect
        ? (listener) => backend.onDidDisconnect!(listener)
        : undefined,
      dispose: disposeBackend,
      async disposeAndWait() {
        if (backendDisposed) return;
        backendDisposed = true;
        if (backend.disposeAndWait) await backend.disposeAndWait();
        else backend.dispose();
      },
    };
  } catch (error) {
    disposeBackend();
    throw error;
  } finally {
    signal.removeEventListener("abort", onAbort);
    await network.disposeAndWait().catch(() => undefined);
  }
};

interface ConnectorIo {
  stdin: NodeJS.ReadableStream;
  stdout: NodeJS.WritableStream;
  exit(code: number): void;
}

/** 运行一个连接器：读 Host 的帧，建连后双向转发，收到 dispose 或 stdin EOF 时收口并退出。 */
export function runRemoteConnector(io: ConnectorIo, open: OpenRemoteServer): void {
  const write = (frame: Buffer) => {
    io.stdout.write(frame);
  };
  const control = (message: Record<string, unknown>) =>
    write(encodeRemoteConnectorControl(message));
  const forwardConsole =
    (level: "info" | "warn" | "error") =>
    (...args: unknown[]) => {
      const message = args.map((arg) => stringifyHostLogArg(arg)).join(" ");
      if (message.trim()) control({ type: "log", level, message });
    };
  // stdout 只承载帧：远端模块的 console 输出改为 log 帧。
  console.log = forwardConsole("info");
  console.info = forwardConsole("info");
  console.warn = forwardConsole("warn");
  console.error = forwardConsole("error");
  console.debug = () => undefined;

  const abort = new AbortController();
  const backendMessages = new Emitter<VSBuffer>();
  let connecting = false;
  let opened: OpenedRemoteServer | null = null;
  let closedSent = false;
  let exiting = false;

  const finish = (code: number) => {
    if (exiting) return;
    exiting = true;
    io.stdout.write(Buffer.alloc(0), () => io.exit(code));
  };
  const reportClosed = (exitCode: number, error?: string) => {
    if (closedSent) return;
    closedSent = true;
    control({ type: "closed", exitCode, ...(error ? { error } : {}) });
  };
  const dispose = async () => {
    if (exiting) return;
    abort.abort();
    const current = opened;
    if (!current) {
      // 连接仍在进行：SSH 已随 abort 释放 backend；WSL / Docker 等建连结束后在 connect 分支里释放。
      if (!connecting) finish(0);
      return;
    }
    opened = null;
    // stdin.end 必须先于任何 await，让远端 stdio server 立即收到 EOF。
    current.stream.stdin.end();
    const streamClosed = new Promise<void>((resolve) => current.stream.onClose(() => resolve()));
    await Promise.race([
      streamClosed,
      new Promise<void>((resolve) => setTimeout(resolve, DISPOSE_TIMEOUT_MS)),
    ]);
    await current.disposeAndWait().catch(() => undefined);
    finish(0);
  };

  const connect = async (request: RemoteConnectRequest) => {
    if (connecting || opened) return;
    connecting = true;
    let server: OpenedRemoteServer;
    try {
      server = await open(request, abort.signal);
    } catch (error) {
      connecting = false;
      control({ type: "failed", error: error instanceof Error ? error.message : String(error) });
      finish(1);
      return;
    }
    connecting = false;
    if (abort.signal.aborted) {
      // Host 已取消：迟到的成功结果立即释放，不复活连接。
      server.stream.stdin.end();
      await server.disposeAndWait().catch(() => undefined);
      finish(0);
      return;
    }
    opened = server;
    control({ type: "connected", hello: server.hello });
    if (server.remaining && server.remaining.length > 0) {
      write(encodeRemoteConnectorFrame(REMOTE_CONNECTOR_STREAM.server, server.remaining));
    }
    server.stream.stdout.on("data", (chunk: Buffer) => {
      write(encodeRemoteConnectorFrame(REMOTE_CONNECTOR_STREAM.server, chunk));
    });
    server.stream.onClose((code) => reportClosed(code));
    server.onDidDisconnect?.((event) => {
      // SSH keepalive 发现半开连接时远端 stdio 未必立刻关闭：并入同一条关闭上报（退出码 -1）。
      reportClosed(-1, event.error?.message);
    });
    const channelServer = new ChannelServer(
      {
        send: (buffer: VSBuffer) =>
          write(encodeRemoteConnectorFrame(REMOTE_CONNECTOR_STREAM.backend, buffer.buffer)),
        onMessage: backendMessages.event,
      },
      "remote-connector",
    );
    channelServer.registerChannel(
      "prompt-attachment-transfer",
      ProxyChannel.fromService(
        createRemotePromptAttachmentTransferService(server.backend, {
          onJanitorError: (error) => console.warn("remote prompt attachment janitor failed", error),
        }),
      ),
    );
    channelServer.registerChannel(
      "remote-backend",
      ProxyChannel.fromService({
        async materializePromptAttachments(input: {
          taskId: string;
          traceId: string;
          content: string;
          attachments?: ZCodePromptAttachment[];
        }) {
          const result = await materializeRemotePromptAttachments(input, {
            backend: server.backend,
          });
          return { content: result.content, attachments: result.attachments };
        },
        async upload(localPath: string, remotePath: string) {
          await server.backend.upload(localPath, remotePath);
        },
      }),
    );
  };

  const reader = new RemoteConnectorFrameReader();
  io.stdin.on("data", (chunk: Buffer) => {
    let frames: Array<{ stream: number; payload: Buffer }>;
    try {
      frames = reader.push(chunk);
    } catch (error) {
      console.error("invalid frame from Host:", error);
      void dispose();
      return;
    }
    for (const { stream, payload } of frames) {
      if (stream === REMOTE_CONNECTOR_STREAM.server) {
        opened?.stream.stdin.write(payload);
        continue;
      }
      if (stream === REMOTE_CONNECTOR_STREAM.backend) {
        backendMessages.fire(VSBuffer.wrap(new Uint8Array(payload)));
        continue;
      }
      let message: { type?: unknown } & Partial<RemoteConnectRequest>;
      try {
        message = JSON.parse(payload.toString("utf8")) as typeof message;
      } catch {
        continue;
      }
      if (message.type === "connect" && message.target) {
        void connect({
          target: message.target,
          remoteAssets: message.remoteAssets ?? {},
          network: message.network ?? {},
          ...(message.remoteRuntimeNetwork
            ? { remoteRuntimeNetwork: message.remoteRuntimeNetwork }
            : {}),
        });
      } else if (message.type === "dispose") {
        void dispose();
      }
    }
  });
  // Host 退出或关闭管道：等同 dispose。
  io.stdin.on("end", () => void dispose());
  io.stdin.on("error", () => void dispose());
}
