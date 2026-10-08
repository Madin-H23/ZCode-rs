// Rust Host 断开后的整页状态（docs/specs/rust-host-resilience.md §2.1）。
//
// 一旦进入就取代启动页与业务 Root：旧连接上的服务已不可用，页面只等待 Main 的 reload（自动重启）
// 或用户的重新加载 / 退出。状态只来自两处：连上之后 WebSocket 关闭，或 Main 的重启进度通知。
import type { ReactNode } from "react";
import { HostConnectionLostScreen, type HostConnectionLostState } from "@zcode/ui";
import { InternalChannels, hostConnectionStatePayloadSchema } from "@zcode/shared";

export interface HostConnectionLostView {
  readonly active: boolean;
  /** 连上之后连接关闭：Main 随后会发重启进度。 */
  onConnectionClosed: () => void;
  /** 处理 preload 转发的重启进度；返回是否是这类消息。 */
  handleMessage: (event: MessageEvent) => boolean;
  render: () => void;
}

export function createHostConnectionLostView(options: {
  /** 用与启动页相同的外壳（错误边界、国际化）渲染整页。 */
  renderShell: (screen: ReactNode) => void;
  onExit: () => void;
}): HostConnectionLostView {
  let current: HostConnectionLostState | null = null;
  const render = () => {
    if (!current) return;
    options.renderShell(
      <HostConnectionLostScreen
        connection={current}
        onReload={() => window.location.reload()}
        onExit={options.onExit}
      />,
    );
  };
  const show = (next: HostConnectionLostState) => {
    // 已放弃自动重启的结论不会被迟到的“正在重启”覆盖。
    if (current?.state === "failed") return;
    current = next;
    render();
  };
  return {
    get active() {
      return current !== null;
    },
    onConnectionClosed: () => show(current ?? { state: "reconnecting" }),
    handleMessage(event) {
      if (event.source !== window || event.data?.type !== InternalChannels.HostConnectionState)
        return false;
      const result = hostConnectionStatePayloadSchema.safeParse(event.data.payload);
      if (result.success)
        show(
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
    render,
  };
}
