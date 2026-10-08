import {
  Emitter,
  VSBuffer,
  SocketProtocol,
  ChannelClient,
  type IMessagePassingProtocol,
  type ISocket,
} from "@zcode/rpc";
import type { IServiceAccessor } from "@zcode/services";
import { RemoteServiceAccess } from "./remoteServiceAccess.js";

export interface WebSocketConnectionCloseEvent {
  code: number;
  reason: string;
  wasClean: boolean;
}

interface WebSocketConnectionOptions {
  onClose?: (event: WebSocketConnectionCloseEvent) => void;
  onOpenSocket?: (socket: WebSocket) => void;
  /** WebSocket 子协议；Rust Host 用它传递 rpc 版本与一次性 capability（浏览器不能设请求头）。 */
  protocols?: string[];
}

export interface WebSocketServiceConnection {
  services: IServiceAccessor;
  dispose: (reason?: Error) => void;
}

function wrapBrowserWebSocket(ws: WebSocket): ISocket {
  const onData = new Emitter<VSBuffer>();
  const onClose = new Emitter<void>();
  const onEnd = new Emitter<void>();

  ws.binaryType = "arraybuffer";
  ws.addEventListener("message", (e) => {
    onData.fire(VSBuffer.wrap(new Uint8Array(e.data as ArrayBuffer)));
  });
  ws.addEventListener("close", () => {
    onClose.fire();
    onEnd.fire();
  });
  ws.addEventListener("error", () => {
    onClose.fire();
    onEnd.fire();
  });

  return {
    onData: onData.event,
    onClose: onClose.event,
    onEnd: onEnd.event,
    write(buffer: VSBuffer) {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(buffer.buffer as Uint8Array<ArrayBuffer>);
      }
    },
    end() {
      ws.close();
    },
    drain() {
      return Promise.resolve();
    },
    dispose() {
      ws.close();
    },
  };
}

export function connectViaWebSocket(
  wsUrl: string,
  options?: WebSocketConnectionOptions,
): Promise<IServiceAccessor> {
  return openWebSocket(wsUrl, options).then(({ ws }) =>
    connectViaProtocol(new SocketProtocol(wrapBrowserWebSocket(ws))),
  );
}

/**
 * 带明确生命周期的 WebSocket service connection（Rust Host 用）。
 *
 * 与 connectViaWebSocket 的区别：连接关闭时立即用错误拒绝所有挂起的调用，
 * 不让调用方无限等待（docs/specs/rust-host-r0.md §2 规则 5）。
 */
export async function createWebSocketServiceConnection(
  wsUrl: string,
  options?: WebSocketConnectionOptions,
): Promise<WebSocketServiceConnection> {
  let dispose: ((reason?: Error) => void) | undefined;
  const { ws } = await openWebSocket(wsUrl, {
    ...options,
    onClose: (event) => {
      dispose?.(
        new Error(
          event.reason
            ? `Service connection closed: ${event.reason}`
            : `Service connection closed (${event.code})`,
        ),
      );
      options?.onClose?.(event);
    },
  });
  const client = new ChannelClient(new SocketProtocol(wrapBrowserWebSocket(ws)));
  let disposed = false;
  dispose = (reason?: Error) => {
    if (disposed) return;
    disposed = true;
    client.dispose(reason);
    ws.close();
  };
  return { services: new RemoteServiceAccess(client), dispose };
}

function openWebSocket(
  wsUrl: string,
  options?: WebSocketConnectionOptions,
): Promise<{ ws: WebSocket }> {
  return new Promise((resolve, reject) => {
    const ws = options?.protocols ? new WebSocket(wsUrl, options.protocols) : new WebSocket(wsUrl);
    let settled = false;

    ws.addEventListener("error", () => {
      if (!settled) {
        reject(new Error(`WebSocket connection failed: ${wsUrl}`));
      }
    });
    ws.addEventListener("close", (event) => {
      options?.onClose?.({
        code: event.code,
        reason: event.reason,
        wasClean: event.wasClean,
      });

      if (!settled) {
        reject(
          new Error(
            event.reason
              ? `WebSocket closed before ready: ${event.reason}`
              : `WebSocket closed before ready (${event.code})`,
          ),
        );
      }
    });

    ws.addEventListener("open", () => {
      settled = true;
      options?.onOpenSocket?.(ws);
      resolve({ ws });
    });
  });
}

export function connectViaProtocol(protocol: IMessagePassingProtocol): IServiceAccessor {
  const client = new ChannelClient(protocol);
  return new RemoteServiceAccess(client);
}
