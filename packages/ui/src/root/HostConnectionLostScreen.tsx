import { useZCodeIntl } from "@/i18n/IntlProvider.js";
import { Button } from "@/components/ui/button.js";
import { RootStartupLoading } from "@/root/RootStartupLoading.js";

/**
 * 本地服务（Rust Host）断开后的整页状态（docs/specs/rust-host-resilience.md §2.1、§2.5）。
 * reconnecting：Host 退出，Main 正在自动重启，页面随后由 Main reload；failed：已放弃自动重启，由用户决定。
 * reattaching：Host 仍在运行、关闭了这个窗口的连接，页面稍后自己 reload 重新接入；closed：短时间内
 * 多次这样关闭，不再自动 reload，由用户决定。
 */
export type HostConnectionLostState =
  | { state: "reconnecting"; attempt?: number; maxAttempts?: number }
  | { state: "reattaching"; attempt: number; maxAttempts: number }
  | { state: "closed" }
  | { state: "failed" };

const MESSAGES = {
  reconnecting: {
    label: "startup.hostConnection.reconnecting",
    help: "startup.hostConnection.help",
    attempt: "startup.hostConnection.attempt",
  },
  reattaching: {
    label: "startup.hostConnection.reconnecting",
    help: "startup.hostConnection.reattachHelp",
    attempt: "startup.hostConnection.reattachAttempt",
  },
  closed: {
    label: "startup.hostConnection.closed",
    help: "startup.hostConnection.closedHelp",
    attempt: null,
  },
  failed: {
    label: "startup.hostConnection.failed",
    help: "startup.hostConnection.failedHelp",
    attempt: null,
  },
} as const;

export function HostConnectionLostScreen({
  connection,
  onReload,
  onExit,
}: {
  connection: HostConnectionLostState;
  onReload: () => void;
  onExit: () => void;
}) {
  const { intl } = useZCodeIntl();
  const messages = MESSAGES[connection.state];
  // 需要用户决定的两种状态：不再转圈，提供“重新加载”。
  const settled = connection.state === "failed" || connection.state === "closed";
  const label = intl.formatMessage({ id: messages.label });
  const attempt =
    connection.state === "reconnecting" || connection.state === "reattaching"
      ? connection
      : undefined;
  return (
    <RootStartupLoading label={label} busy={!settled}>
      <div
        className="flex w-full max-w-md flex-col items-center gap-3 px-6 text-center"
        data-testid="host-connection-lost"
        data-state={connection.state}
        data-attempt={attempt?.attempt}
      >
        <h1 className="text-ui-lg font-medium" role={settled ? "alert" : undefined}>
          {label}
        </h1>
        <p className="text-ui-base text-foreground-subtle">
          {intl.formatMessage({ id: messages.help })}
        </p>
        {messages.attempt && attempt?.attempt && attempt.maxAttempts ? (
          <p className="text-ui-caption text-foreground-subtle">
            {intl.formatMessage(
              { id: messages.attempt },
              { attempt: attempt.attempt, max: attempt.maxAttempts },
            )}
          </p>
        ) : null}
        <div className="flex flex-wrap justify-center gap-2">
          {settled ? (
            <Button data-testid="host-connection-reload" onClick={onReload}>
              {intl.formatMessage({ id: "startup.hostConnection.reload" })}
            </Button>
          ) : null}
          <Button variant="ghost" onClick={onExit}>
            {intl.formatMessage({ id: "startup.global.exit" })}
          </Button>
        </div>
      </div>
    </RootStartupLoading>
  );
}
