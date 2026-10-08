import {
  databaseStartupPortPayloadSchema,
  serviceEndpointPayloadSchema,
  type DatabaseStartupState,
} from "@zcode/shared";

/** 业务 Root 的连接来源：Node Host 的 MessagePort，或 Rust Host 的 WebSocket endpoint。 */
export type ServiceConnector =
  | { kind: "port"; port: MessagePort }
  | { kind: "endpoint"; url: string; capability: string };

/** 页面只做同代准入；ready(A) 不能与连接(B) 拼成一次完成事实。 */
export class DatabaseStartupAdmission {
  state: DatabaseStartupState | null = null;
  private pending?: { startupId: string; connector: ServiceConnector };

  acceptState(state: DatabaseStartupState): boolean {
    if (this.state?.startupId === state.startupId && this.state.sequence >= state.sequence)
      return false;
    this.state = state;
    return true;
  }

  acceptPort(payload: unknown, port: MessagePort): void {
    const parsed = databaseStartupPortPayloadSchema.safeParse(payload);
    if (!parsed.success) {
      port.close();
      return;
    }
    this.replacePending({
      startupId: parsed.data.databaseStartupId,
      connector: { kind: "port", port },
    });
  }

  acceptEndpoint(payload: unknown): void {
    const parsed = serviceEndpointPayloadSchema.safeParse(payload);
    if (!parsed.success) return;
    const { databaseStartupId, url, capability } = parsed.data;
    this.replacePending({
      startupId: databaseStartupId,
      connector: { kind: "endpoint", url, capability },
    });
  }

  takeReadyConnector(): ServiceConnector | undefined {
    if (this.state?.phase !== "ready" || this.pending?.startupId !== this.state.startupId) return;
    const { connector } = this.pending;
    this.pending = undefined;
    return connector;
  }

  private replacePending(next: { startupId: string; connector: ServiceConnector }): void {
    const previous = this.pending?.connector;
    if (
      previous?.kind === "port" &&
      (next.connector.kind !== "port" || next.connector.port !== previous.port)
    )
      previous.port.close();
    this.pending = next;
  }
}
