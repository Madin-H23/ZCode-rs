import { createRemoteWorkspaceDisconnectedError, registerRemoteWorkspaceSession } from "@zcode/ui";
import {
  createMessagePortServiceConnection,
  createWebSocketServiceConnection,
} from "@zcode/client";
import { RUST_HOST_CAPABILITY_SUBPROTOCOL_PREFIX, RUST_HOST_RPC_SUBPROTOCOL } from "@zcode/shared";
import type { IServiceAccessor } from "@zcode/services";
import { buildRemoteWorkspaceSessionServices } from "./remoteWorkspaceSessionServices.js";
import {
  notifyRemoteWorkspaceServicePortReady,
  type RemoteWorkspaceServiceEndpointRegistration,
  type RemoteWorkspaceServicePortRegistration,
} from "./remoteWorkspaceServicePortBridge.js";

export type RemoteWorkspaceServiceRegistration =
  | RemoteWorkspaceServicePortRegistration
  | RemoteWorkspaceServiceEndpointRegistration;

function registerRemoteWorkspaceServicePort(
  base: IServiceAccessor,
  params: RemoteWorkspaceServicePortRegistration,
): void {
  const remoteConnection = createMessagePortServiceConnection(params.port);
  registerRemoteWorkspaceSession({
    sessionId: params.sessionId,
    target: params.target,
    services: buildRemoteWorkspaceSessionServices(base, remoteConnection.services),
    dispose: (reason) =>
      remoteConnection.dispose(reason ?? createRemoteWorkspaceDisconnectedError()),
  });
  // canonical workspace bind 会换代 remote-scoped port。
  // 只有 store 已注册新 services 后才能确认 ready，bind IPC 返回后的调用方才可重新读取并使用新代 services。
  notifyRemoteWorkspaceServicePortReady(params);
}

/** 连上之后的 WebSocket 关闭（状态码与 Host 给的原因，docs/specs/rust-u2-gateway.md §2.6）。 */
export type RemoteScopeConnectionClosed = (close: { code: number; reason: string }) => void;

/**
 * Rust Host：远端 scope 经 WebSocket 直连（docs/specs/rust-host-remote.md §4.2）。注册与 ready ACK
 * 与 MessagePort 路径相同；握手失败时不回 ACK，由 Main 的 ready 超时收口。连上之后的关闭交给
 * `onClosed`（页面据状态码决定是否重新接入，rust-host-resilience.md §2.5）。
 */
function registerRemoteWorkspaceServiceEndpoint(
  base: IServiceAccessor,
  params: RemoteWorkspaceServiceEndpointRegistration,
  onClosed?: RemoteScopeConnectionClosed,
): void {
  let connected = false;
  void createWebSocketServiceConnection(params.url, {
    protocols: [
      RUST_HOST_RPC_SUBPROTOCOL,
      `${RUST_HOST_CAPABILITY_SUBPROTOCOL_PREFIX}${params.capability}`,
    ],
    onClose: (event) => {
      if (connected) onClosed?.(event);
    },
  }).then(
    (remoteConnection) => {
      connected = true;
      registerRemoteWorkspaceSession({
        sessionId: params.sessionId,
        target: params.target,
        services: buildRemoteWorkspaceSessionServices(base, remoteConnection.services),
        dispose: (reason) =>
          remoteConnection.dispose(reason ?? createRemoteWorkspaceDisconnectedError()),
      });
      notifyRemoteWorkspaceServicePortReady(params);
    },
    () => undefined,
  );
}

/** 把 Main 投递的远端 scope（Node Host 的 MessagePort 或 Rust Host 的 endpoint）注册为远端会话服务。 */
export function registerRemoteWorkspaceServiceConnection(
  base: IServiceAccessor,
  params: RemoteWorkspaceServiceRegistration,
  onClosed?: RemoteScopeConnectionClosed,
): void {
  if ("port" in params) {
    registerRemoteWorkspaceServicePort(base, params);
  } else {
    registerRemoteWorkspaceServiceEndpoint(base, params, onClosed);
  }
}
