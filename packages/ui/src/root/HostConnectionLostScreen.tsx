import { useZCodeIntl } from "@/i18n/IntlProvider.js";
import { Button } from "@/components/ui/button.js";
import { RootStartupLoading } from "@/root/RootStartupLoading.js";

/**
 * 本地服务（Rust Host）断开后的整页状态（docs/specs/rust-host-resilience.md §2.1）。
 * reconnecting：Main 正在自动重启，页面随后由 Main reload；failed：已放弃自动重启，由用户决定。
 */
export type HostConnectionLostState =
  | { state: "reconnecting"; attempt?: number; maxAttempts?: number }
  | { state: "failed" };

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
  const failed = connection.state === "failed";
  const label = intl.formatMessage({
    id: failed ? "startup.hostConnection.failed" : "startup.hostConnection.reconnecting",
  });
  return (
    <RootStartupLoading label={label} busy={!failed}>
      <div
        className="flex w-full max-w-md flex-col items-center gap-3 px-6 text-center"
        data-testid="host-connection-lost"
        data-state={connection.state}
      >
        <h1 className="text-ui-lg font-medium" role={failed ? "alert" : undefined}>
          {label}
        </h1>
        <p className="text-ui-base text-foreground-subtle">
          {intl.formatMessage({
            id: failed ? "startup.hostConnection.failedHelp" : "startup.hostConnection.help",
          })}
        </p>
        {!failed && connection.attempt && connection.maxAttempts ? (
          <p className="text-ui-caption text-foreground-subtle">
            {intl.formatMessage(
              { id: "startup.hostConnection.attempt" },
              { attempt: connection.attempt, max: connection.maxAttempts },
            )}
          </p>
        ) : null}
        <div className="flex flex-wrap justify-center gap-2">
          {failed ? (
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
