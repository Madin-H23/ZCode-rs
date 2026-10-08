// Rust Host 的 MessagePort attachment 桥（docs/specs/rust-host-tasks.md §2.4）。
//
// Main 发起的 attachment（AttachServicePort，例如 web-remote-replayable）在 Node Host 里直接把
// MessagePort 交给 Host；Rust Host 不能接收 MessagePort，所以 Main 为它签发一个带同样
// clientMode / scope 的 WebSocket endpoint，再把端口与 WebSocket 逐条消息互转：
//   MessagePort（MessagePortProtocol：每条 postMessage 是一条完整 rpc 消息）
//   ⇄ SocketProtocol（WebSocket 二进制帧承载的字节流，与 Renderer 直连相同）。
// 任一侧关闭都会关闭另一侧，对端立即感知，而不是悬挂。
import type { MessageEvent, MessagePortMain } from "electron";
import { RUST_HOST_CAPABILITY_SUBPROTOCOL_PREFIX, RUST_HOST_RPC_SUBPROTOCOL } from "@zcode/shared";
import { Emitter, type ISocket, SocketProtocol, VSBuffer } from "@zcode/rpc";

export interface RustHostPortBridge {
  close(): void;
}

function wrapWebSocket(ws: WebSocket): ISocket {
  const onData = new Emitter<VSBuffer>();
  const onClose = new Emitter<void>();
  const onEnd = new Emitter<void>();
  ws.binaryType = "arraybuffer";
  ws.addEventListener("message", (event) => {
    if (event.data instanceof ArrayBuffer) onData.fire(VSBuffer.wrap(new Uint8Array(event.data)));
  });
  const ended = () => {
    onClose.fire();
    onEnd.fire();
  };
  ws.addEventListener("close", ended);
  ws.addEventListener("error", ended);
  return {
    onData: onData.event,
    onClose: onClose.event,
    onEnd: onEnd.event,
    write(buffer) {
      if (ws.readyState === WebSocket.OPEN) ws.send(buffer.buffer as Uint8Array<ArrayBuffer>);
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

/** 连接 endpoint 并开始双向转发；连接失败时关闭端口并抛出。 */
export async function bridgeMessagePortToRustHost(
  port: MessagePortMain,
  endpoint: { url: string; capability: string },
): Promise<RustHostPortBridge> {
  const ws = new WebSocket(endpoint.url, [
    RUST_HOST_RPC_SUBPROTOCOL,
    `${RUST_HOST_CAPABILITY_SUBPROTOCOL_PREFIX}${endpoint.capability}`,
  ]);
  try {
    await new Promise<void>((resolveOpen, rejectOpen) => {
      ws.addEventListener("open", () => resolveOpen(), { once: true });
      ws.addEventListener(
        "error",
        () => rejectOpen(new Error(`WebSocket failed: ${endpoint.url}`)),
        {
          once: true,
        },
      );
      ws.addEventListener(
        "close",
        (event) => rejectOpen(new Error(`WebSocket closed before ready (${event.code})`)),
        { once: true },
      );
    });
  } catch (error) {
    port.close();
    throw error;
  }
  const protocol = new SocketProtocol(wrapWebSocket(ws));
  let closed = false;
  const close = () => {
    if (closed) return;
    closed = true;
    port.close();
    ws.close();
    protocol.dispose();
  };
  protocol.onMessage((buffer) => {
    if (!closed) port.postMessage(buffer.buffer);
  });
  ws.addEventListener("close", close);
  port.on("message", (event: MessageEvent) => {
    // 只有 Uint8Array 是 rpc 二进制；MessagePort 的流控控制对象没有 WebSocket 对应物，丢弃。
    if (!closed && event.data instanceof Uint8Array) protocol.send(VSBuffer.wrap(event.data));
  });
  port.on("close", close);
  port.start();
  return { close };
}
