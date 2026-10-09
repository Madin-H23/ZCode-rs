// Rust Host 开发开关与进程 adapter（docs/specs/rust-host-r0.md）。
//
// adapter 对 Main 其余代码表现为 Electron UtilityProcess：postMessage 写入 Rust Host 的 stdin，
// stdout 上的每行 JSON 以 "message" 事件交回，进程退出发 "exit"。U4 起 Rust Host 是 App 级的
// （docs/specs/rust-u4-app-host.md），由 rustAppHost.ts 拥有；BroadcastHub、taskRealtimeBus 不再接入。
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { randomUUID } from "node:crypto";
import { EventEmitter } from "node:events";
import { resolve } from "node:path";
import { createInterface } from "node:readline";
import type { Readable } from "node:stream";
import { app, type BrowserWindow, type MessagePortMain, type UtilityProcess } from "electron";
import {
  type HostIssueAttachmentEndpointMessage,
  HostMessageTypes,
  HostResponseTypes,
  InternalChannels,
  type ServiceEndpointPayload,
  hostAttachServicePortMessageSchema,
  hostAttachmentEndpointIssuedResponseSchema,
  hostDetachServicePortMessageSchema,
  hostEndpointReadyResponseSchema,
} from "@zcode/shared";
import { bridgeMessagePortToRustHost, type RustHostPortBridge } from "./rustHostPortBridge.js";

export const RUST_DESKTOP_HOST_ENV = "ZCODE_DESKTOP_HOST";
export const RUST_HOST_BINARY_ENV = "ZCODE_RUST_HOST_BINARY";
/**
 * 给 Rust Host 的 Electron 可执行文件路径，只用来以 Node 模式运行远端连接器。Rust Host 的 agent runtime
 * 缺省在 Host 进程内运行，不再借 Electron 运行 CLI bundle（docs/specs/rust-u3-single-process.md §2.2）。
 */
const RUST_HOST_NODE_EXEC_PATH_ENV = "ZCODE_HOST_NODE_EXEC_PATH";
/** 远端 workspace 的连接器脚本（docs/specs/rust-host-remote.md §2.1）。 */
const RUST_HOST_REMOTE_CONNECTOR_PATH_ENV = "ZCODE_REMOTE_CONNECTOR_PATH";

interface RustHostLogger {
  info: (...args: unknown[]) => void;
  warn: (...args: unknown[]) => void;
}

interface ServiceEndpoint {
  url: string;
  capability: string;
}

/** 只在开发构建生效；打包版本不读这个开关，发布行为与今天一致。 */
export function isRustDesktopHostEnabled(): boolean {
  return !app.isPackaged && process.env[RUST_DESKTOP_HOST_ENV]?.trim() === "rust";
}

function resolveRustHostBinary(): string {
  const configured = process.env[RUST_HOST_BINARY_ENV]?.trim();
  if (configured) return configured;
  const executable = process.platform === "win32" ? "zcode-host.exe" : "zcode-host";
  // 开发态 app.getAppPath() 指向 packages/desktop。
  return resolve(app.getAppPath(), "../../apps/zcode-host/target/debug", executable);
}

/** 与 loadWindow 的页面来源一致：开发服务器的 origin，或用文件加载时的 file://。 */
function resolveAllowedRendererOrigin(): string {
  const devUrl = process.env["ELECTRON_RENDERER_URL"];
  return !app.isPackaged && devUrl ? new URL(devUrl).origin : "file://";
}

export class RustHostProcess extends EventEmitter {
  /** stdout 是控制通道，不作为日志流暴露给 hostLogRelay。 */
  readonly stdout = null;
  readonly stderr: Readable;
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly endpointReady: Promise<number>;
  private resolveEndpointReady!: (port: number) => void;
  private rejectEndpointReady!: (error: Error) => void;
  private readonly pendingEndpoints = new Map<
    string,
    { resolve: (endpoint: ServiceEndpoint) => void; reject: (error: Error) => void }
  >();
  private exited = false;
  /** Main 发起的 MessagePort attachment（按 attachmentId）经 WebSocket 桥接到 Rust Host。 */
  private readonly portBridges = new Map<string, RustHostPortBridge>();

  constructor(
    binary: string,
    args: string[],
    env: NodeJS.ProcessEnv,
    private readonly logger: RustHostLogger,
  ) {
    super();
    this.endpointReady = new Promise<number>((resolveReady, rejectReady) => {
      this.resolveEndpointReady = resolveReady;
      this.rejectEndpointReady = rejectReady;
    });
    // 没有请求 endpoint 就退出时，这个拒绝无人等待，不应变成未处理的 rejection。
    this.endpointReady.catch(() => {});
    this.child = spawn(binary, args, { env, stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
    this.stderr = this.child.stderr;
    createInterface({ input: this.child.stdout }).on("line", (line) => this.onControlLine(line));
    // 进程退出后的写入会触发 EPIPE；退出由 "exit" 统一上报，这里不重复处理。
    this.child.stdin.on("error", () => {});
    this.child.on("error", (error) => {
      this.logger.warn(`[rustHost] failed to run ${binary}:`, error.message);
      this.finish(-1);
    });
    this.child.on("exit", (code, signal) => this.finish(code ?? (signal ? 1 : 0)));
  }

  get pid(): number | undefined {
    return this.child.pid;
  }

  /** Main 其余代码按 UtilityProcess 使用；只用到 pid、postMessage、kill、stdout/stderr 与事件。 */
  asUtilityProcess(): UtilityProcess {
    return this as unknown as UtilityProcess;
  }

  postMessage(message: unknown, transfer?: MessagePortMain[]): void {
    if (transfer && transfer.length > 0) {
      const attach = hostAttachServicePortMessageSchema.safeParse(message);
      if (attach.success && transfer.length === 1) {
        // Main 发起的 attachment（本地 web-remote-replayable 与手机远控的远端 scope；Bot 远端运行时在 Host
        // 内解析 logical session，不经这里，见 docs/specs/rust-host-remote.md §2.9）：
        // 签发同 clientMode / scope 的 endpoint 后桥接；远端 scope 由 Host 校验 logical session
        // （docs/specs/rust-host-remote.md §2.4）。
        void this.attachServicePort(
          attach.data.attachmentId,
          attach.data.clientMode,
          attach.data.scope,
          transfer[0]!,
        );
        return;
      }
      // 不是 AttachServicePort 的端口转移没有对应的 Rust 接入。关闭端口让对端立即感知，而不是一直等待。
      for (const port of transfer) port.close();
      const type = (message as { type?: unknown } | null)?.type;
      this.logger.warn(`[rustHost] MessagePort attachment is not supported yet: ${String(type)}`);
      return;
    }
    const detach = hostDetachServicePortMessageSchema.safeParse(message);
    if (detach.success && this.portBridges.has(detach.data.attachmentId)) {
      this.portBridges.get(detach.data.attachmentId)?.close();
      this.portBridges.delete(detach.data.attachmentId);
      return;
    }
    if (this.exited || !this.child.stdin.writable) return;
    this.child.stdin.write(`${JSON.stringify(message)}\n`);
  }

  private async attachServicePort(
    attachmentId: string,
    clientMode: HostIssueAttachmentEndpointMessage["clientMode"],
    scope: HostIssueAttachmentEndpointMessage["scope"],
    port: MessagePortMain,
  ): Promise<void> {
    // 同一 attachmentId 重新 attach 时替换旧桥（与 Node Host 的 attachment registry 一致）。
    this.portBridges.get(attachmentId)?.close();
    this.portBridges.delete(attachmentId);
    try {
      const endpoint = await this.requestServiceEndpoint({ attachmentId, clientMode, scope });
      const bridge = await bridgeMessagePortToRustHost(port, endpoint);
      this.portBridges.set(attachmentId, bridge);
      this.logger.info(
        `[rustHost] attached service port, attachmentId=${attachmentId}, clientMode=${clientMode}, scope=${scope.kind}`,
      );
    } catch (error) {
      port.close();
      this.logger.warn(
        `[rustHost] failed to attach service port, attachmentId=${attachmentId}:`,
        error instanceof Error ? error.message : String(error),
      );
    }
  }

  kill(): boolean {
    return this.child.kill();
  }

  async requestServiceEndpoint(
    options?: Partial<
      Pick<HostIssueAttachmentEndpointMessage, "attachmentId" | "clientMode" | "scope" | "windowId">
    >,
  ): Promise<ServiceEndpoint> {
    await this.endpointReady;
    const request: HostIssueAttachmentEndpointMessage = {
      type: HostMessageTypes.IssueAttachmentEndpoint,
      requestId: randomUUID(),
      attachmentId: options?.attachmentId ?? randomUUID(),
      clientMode: options?.clientMode ?? "desktop-continuous",
      scope: options?.scope ?? { kind: "local" },
      // 主窗口的 Renderer attachment 带窗口身份（docs/specs/rust-u4-app-host.md §2.2）。
      ...(options?.windowId === undefined ? {} : { windowId: options.windowId }),
    };
    return new Promise<ServiceEndpoint>((resolveEndpoint, rejectEndpoint) => {
      this.pendingEndpoints.set(request.requestId, {
        resolve: resolveEndpoint,
        reject: rejectEndpoint,
      });
      this.postMessage(request);
    });
  }

  private onControlLine(line: string): void {
    if (!line.trim()) return;
    let message: unknown;
    try {
      message = JSON.parse(line);
    } catch {
      this.logger.warn("[rustHost] ignored a non-JSON control line");
      return;
    }
    const type = (message as { type?: unknown } | null)?.type;
    if (type === HostResponseTypes.HostEndpointReady) {
      const parsed = hostEndpointReadyResponseSchema.safeParse(message);
      if (parsed.success) this.resolveEndpointReady(parsed.data.wsPort);
      else this.logger.warn("[rustHost] invalid host-endpoint-ready");
      return;
    }
    if (type === HostResponseTypes.AttachmentEndpointIssued) {
      const parsed = hostAttachmentEndpointIssuedResponseSchema.safeParse(message);
      if (!parsed.success) {
        this.logger.warn("[rustHost] invalid attachment-endpoint-issued");
        return;
      }
      const pending = this.pendingEndpoints.get(parsed.data.requestId);
      this.pendingEndpoints.delete(parsed.data.requestId);
      if (parsed.data.ok)
        pending?.resolve({ url: parsed.data.url, capability: parsed.data.capability });
      else pending?.reject(new Error(parsed.data.error));
      return;
    }
    this.emit("message", message);
  }

  private finish(code: number): void {
    if (this.exited) return;
    this.exited = true;
    const error = new Error(`Rust Host exited with code ${code}`);
    this.rejectEndpointReady(error);
    for (const pending of this.pendingEndpoints.values()) pending.reject(error);
    this.pendingEndpoints.clear();
    for (const bridge of this.portBridges.values()) bridge.close();
    this.portBridges.clear();
    this.emit("exit", code);
  }
}

export function asRustHostProcess(child: unknown): RustHostProcess | null {
  return child instanceof RustHostProcess ? child : null;
}

export function spawnRustHostProcess(
  label: string,
  env: NodeJS.ProcessEnv,
  logger: RustHostLogger,
): RustHostProcess {
  const binary = resolveRustHostBinary();
  logger.info(`[rustHost] starting ${binary} for (${label})`);
  return new RustHostProcess(
    binary,
    ["--control", "stdio", "--allowed-origin", resolveAllowedRendererOrigin()],
    {
      ...env,
      // 远端 workspace 的探测、部署与启动仍用 TS 实现，由连接器进程承担（与 host/index.js 同一产物目录）；
      // Rust Host 不是 Node，连接器借 Electron 以 ELECTRON_RUN_AS_NODE 运行。
      [RUST_HOST_NODE_EXEC_PATH_ENV]: process.execPath,
      [RUST_HOST_REMOTE_CONNECTOR_PATH_ENV]: resolve(
        import.meta.dirname,
        "../host/remoteConnector.js",
      ),
    },
    logger,
  );
}

/**
 * 为主窗口当前页面签发一个 Renderer endpoint 并下发（docs/specs/rust-u4-app-host.md §2.1）。
 * `isCurrentPage` 为 false 说明签发期间窗口已 reload 或关闭，结果丢弃。返回是否交给了当前页面。
 */
export async function sendRustHostServiceEndpoint(
  win: BrowserWindow,
  host: RustHostProcess,
  databaseStartupId: string,
  logger: RustHostLogger,
  isCurrentPage: () => boolean,
): Promise<boolean> {
  try {
    const endpoint = await host.requestServiceEndpoint({ windowId: win.webContents.id });
    if (win.isDestroyed() || win.webContents.isDestroyed() || !isCurrentPage()) return false;
    const payload: ServiceEndpointPayload = { databaseStartupId, ...endpoint };
    win.webContents.send(InternalChannels.ServiceEndpoint, payload);
    return true;
  } catch (error) {
    logger.warn(
      "[rustHost] failed to issue a renderer endpoint:",
      error instanceof Error ? error.message : String(error),
    );
    return false;
  }
}
