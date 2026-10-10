// Rust Host 断开后的整页状态（docs/specs/rust-host-resilience.md §2.1、§2.5）。
//
// 一旦进入就取代启动页与业务 Root：旧连接上的服务已不可用。状态由 `HostConnectionLostController`
// 唯一持有，只来自两处：连上之后 WebSocket 关闭（按 Close 的状态码决定等待 Main 还是自己重新接入），
// 或 Main 的重启进度通知。这里只负责渲染与接线。
import type { ReactNode } from "react";
import { HostConnectionLostScreen, type HostConnectionLostState, logger } from "@zcode/ui";
import { InternalChannels, hostConnectionStatePayloadSchema } from "@zcode/shared";
import {
  HostConnectionLostController,
  createSessionReattachHistory,
  type HostConnectionCloseEvent,
  type HostConnectionKind,
} from "./hostConnectionClose.js";

export interface HostConnectionLostView {
  readonly active: boolean;
  /** 连上之后某条连接关闭：Host 仍在运行时自己重新接入，否则等待 Main 的重启进度。 */
  onConnectionClosed: (close: HostConnectionCloseEvent, connection: HostConnectionKind) => void;
  /** 处理 preload 转发的重启进度；返回是否是这类消息。 */
  handleMessage: (event: MessageEvent) => boolean;
  render: () => void;
}

export function createHostConnectionLostView(options: {
  /** 用与启动页相同的外壳（错误边界、国际化）渲染整页。 */
  renderShell: (screen: ReactNode) => void;
  onExit: () => void;
}): HostConnectionLostView {
  const renderState = (state: HostConnectionLostState) =>
    options.renderShell(
      <HostConnectionLostScreen
        connection={state}
        onReload={() => controller.reloadByUser()}
        onExit={options.onExit}
      />,
    );
  const controller = new HostConnectionLostController({
    history: createSessionReattachHistory(() => window.sessionStorage),
    now: () => Date.now(),
    setTimer: (delayMs, fire) => {
      const timer = setTimeout(fire, delayMs);
      return () => clearTimeout(timer);
    },
    reload: () => window.location.reload(),
    render: renderState,
    warn: (message) => logger.lifecycle.warn(message),
  });
  return {
    get active() {
      return controller.state !== null;
    },
    onConnectionClosed: (close, connection) => controller.connectionClosed(close, connection),
    handleMessage(event) {
      if (event.source !== window || event.data?.type !== InternalChannels.HostConnectionState)
        return false;
      const result = hostConnectionStatePayloadSchema.safeParse(event.data.payload);
      if (result.success)
        controller.hostRestartState(
          result.data.state === "failed"
            ? { state: "failed" }
            : {
                state: "reconnecting",
                attempt: result.data.attempt,
                maxAttempts: result.data.maxAttempts,
              },
        );
      return true;
    },
    render() {
      if (controller.state) renderState(controller.state);
    },
  };
}
