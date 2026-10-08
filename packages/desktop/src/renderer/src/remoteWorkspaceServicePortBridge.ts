import {
  InternalChannels,
  scopedServiceEndpointPayloadSchema,
  type RemoteTarget,
  type ScopedServiceEndpointPayload,
} from "@zcode/shared";

export interface RemoteWorkspaceServicePortRegistration {
  attachmentId: string;
  port: MessagePort;
  sessionId: string;
  target: RemoteTarget;
}

export function parseRemoteWorkspaceServicePortMessage(
  event: MessageEvent,
): RemoteWorkspaceServicePortRegistration | null {
  if (
    typeof event.data !== "object" ||
    event.data === null ||
    !("type" in event.data) ||
    event.data.type !== InternalChannels.ScopedServicePort
  ) {
    return null;
  }
  const port = event.ports[0];
  const attachmentId = typeof event.data.attachmentId === "string" ? event.data.attachmentId : null;
  const sessionId = typeof event.data.sessionId === "string" ? event.data.sessionId : null;
  const target = "target" in event.data ? (event.data.target as RemoteTarget | null) : null;
  if (!port || !attachmentId || !sessionId || !target) return null;
  return { attachmentId, port, sessionId, target };
}

/** Rust Host 的远端 scope endpoint（docs/specs/rust-host-remote.md §4.2）；preload 已校验，这里再校验一次。 */
export type RemoteWorkspaceServiceEndpointRegistration = ScopedServiceEndpointPayload;

export function parseRemoteWorkspaceServiceEndpointMessage(
  event: MessageEvent,
): RemoteWorkspaceServiceEndpointRegistration | null {
  if (
    event.source !== window ||
    typeof event.data !== "object" ||
    event.data === null ||
    event.data.type !== InternalChannels.ScopedServiceEndpoint
  ) {
    return null;
  }
  const { type: _type, ...payload } = event.data as Record<string, unknown>;
  const parsed = scopedServiceEndpointPayloadSchema.safeParse(payload);
  return parsed.success ? parsed.data : null;
}

export function notifyRemoteWorkspaceServicePortReady(
  payload: Pick<RemoteWorkspaceServicePortRegistration, "attachmentId" | "sessionId">,
  postMessage: (message: unknown, targetOrigin: string) => void = window.postMessage.bind(window),
): void {
  postMessage(
    {
      type: InternalChannels.ScopedServicePortReady,
      attachmentId: payload.attachmentId,
      sessionId: payload.sessionId,
    },
    "*",
  );
}
