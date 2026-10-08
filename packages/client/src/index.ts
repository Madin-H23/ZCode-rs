export { RemoteServiceAccess } from "./remoteServiceAccess.js";
export {
  connectViaProtocol,
  connectViaWebSocket,
  createWebSocketServiceConnection,
} from "./websocket.js";
export type { WebSocketConnectionCloseEvent, WebSocketServiceConnection } from "./websocket.js";
export { connectViaMessagePort, createMessagePortServiceConnection } from "./messageport.js";
export type { MessagePortServiceConnection } from "./messageport.js";
