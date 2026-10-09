import { randomUUID } from "node:crypto";
import { ipcMain, type BrowserWindow, type UtilityProcess } from "electron";
import {
  HostMessageTypes,
  InternalChannels,
  databaseStartupControlSchema,
  type DatabaseStartupState,
} from "@zcode/shared";
import { reportDatabaseStartupState } from "./databaseStartupTelemetry.js";

let localStorageReady = false;
let quit: (() => void) | undefined;
export function configureDatabaseStartupQuit(handler: () => void): void {
  quit = handler;
}
const readyListeners = new Set<() => void>();
/** Main 只按 Host ready 调度既有 scheduler，不拥有迁移状态或账本。 */
export function onLocalDatabaseStartupReady(listener: () => void): void {
  if (localStorageReady) listener();
  else readyListeners.add(listener);
}

const windowBindings = new WeakMap<BrowserWindow, () => void>();
const hostStartupIds = new WeakMap<UtilityProcess, string>();
export function getDatabaseStartupPortPayload(
  child: UtilityProcess,
): { databaseStartupId: string } | undefined {
  const databaseStartupId = hostStartupIds.get(child);
  return databaseStartupId ? { databaseStartupId } : undefined;
}

/** 启动状态发给谁、接受谁的控制：Node 路径是绑定的那个窗口，App 级 Rust Host 是所有主窗口。 */
interface DatabaseStartupRelayTargets {
  deliver(state: DatabaseStartupState): void;
  /** `snapshot` 控制的应答只发给请求方。 */
  reply(sender: Electron.WebContents, state: DatabaseStartupState): void;
  acceptsControlFrom(sender: Electron.WebContents): boolean;
}

interface DatabaseStartupRelayBinding {
  receive: (state: DatabaseStartupState) => void;
  startupId: string;
  dispose: () => void;
}

function createDatabaseStartupRelayBinding(
  child: UtilityProcess,
  startupId: string,
  targets: DatabaseStartupRelayTargets,
): DatabaseStartupRelayBinding {
  hostStartupIds.set(child, startupId);
  let disposed = false;
  let latest: DatabaseStartupState | undefined;
  let exited = false;
  const applyState = (state: DatabaseStartupState) => {
    if (latest && state.startupId === latest.startupId && state.sequence <= latest.sequence) return;
    latest = state;
    if (!disposed) targets.deliver(state);
    try {
      reportDatabaseStartupState(state);
    } catch {
      /* 遥测故障不阻断启动。 */
    }
    if (state.phase === "ready" && !localStorageReady) {
      localStorageReady = true;
      for (const listener of readyListeners) listener();
      readyListeners.clear();
    }
  };
  const receive = (state: DatabaseStartupState) => {
    if (disposed || exited || state.startupId !== startupId) return;
    applyState(state);
  };
  const control = (event: Electron.IpcMainEvent, raw: unknown) => {
    if (disposed || !targets.acceptsControlFrom(event.sender)) return;
    const result = databaseStartupControlSchema.safeParse(raw);
    if (!result.success) return;
    if (result.data.action === "exit") {
      quit?.();
      return;
    }
    if (result.data.action === "snapshot" && latest) targets.reply(event.sender, latest);
    if (!exited)
      child.postMessage({ type: HostMessageTypes.DatabaseStartupControl, control: result.data });
  };
  const onExit = () => {
    exited = true;
    hostStartupIds.delete(child);
    if (!latest) {
      const now = Date.now();
      latest = {
        schemaVersion: 1,
        startupId,
        attemptId: randomUUID(),
        sequence: 0,
        startedAt: now,
        updatedAt: now,
        phase: "starting",
        disk: [],
      };
    }
    // 即使旧代曾 ready，退出后的 reload 也只能读到失败，不能用旧证明放行新端口。
    if (latest.phase !== "failed")
      applyState({
        ...latest,
        sequence: latest.sequence + 1,
        updatedAt: Date.now(),
        failedPhase: latest.phase,
        phase: "failed",
        errorCode: "transport_closed",
      });
  };
  const dispose = () => {
    if (disposed) return;
    disposed = true;
    ipcMain.removeListener(InternalChannels.DatabaseStartupControl, control);
    child.removeListener("exit", onExit);
    hostStartupIds.delete(child);
  };
  ipcMain.on(InternalChannels.DatabaseStartupControl, control);
  child.once("exit", onExit);
  return { receive, startupId, dispose };
}

function sendStartupState(webContents: Electron.WebContents, state: DatabaseStartupState): void {
  if (!webContents.isDestroyed()) webContents.send(InternalChannels.DatabaseStartupState, state);
}

/** Node 窗口 Host：启动状态只发给这个窗口，也只接受这个窗口的控制。 */
export function bindDatabaseStartupRelay(
  win: BrowserWindow,
  child: UtilityProcess,
  startupId = randomUUID(),
) {
  // 同窗口重建 Host 时，旧监听不能继续重放 ready 或接收用户控制命令。
  windowBindings.get(win)?.();
  const forward = (state: DatabaseStartupState) => {
    if (!win.isDestroyed()) sendStartupState(win.webContents, state);
  };
  const binding = createDatabaseStartupRelayBinding(child, startupId, {
    deliver: forward,
    reply: (_sender, state) => forward(state),
    acceptsControlFrom: (sender) => sender === win.webContents,
  });
  const dispose = () => {
    binding.dispose();
    win.removeListener("closed", dispose);
    if (windowBindings.get(win) === dispose) windowBindings.delete(win);
  };
  windowBindings.set(win, dispose);
  win.once("closed", dispose);
  return { receive: binding.receive, startupId };
}

let appBinding: DatabaseStartupRelayBinding | undefined;

/**
 * App 级 Rust Host（docs/specs/rust-u4-app-host.md §2.8）：每一代 Host 一个绑定，替换上一代；
 * 启动状态扇出到所有主窗口，任意主窗口的 retry / exit / snapshot 都被接受。
 */
export function bindAppDatabaseStartupRelay(
  child: UtilityProcess,
  startupId: string,
  windows: {
    listMainWindows: () => BrowserWindow[];
    isMainWindowSender: (sender: Electron.WebContents) => boolean;
  },
): { receive: (state: DatabaseStartupState) => void; startupId: string } {
  // 上一代的监听不能继续接收控制，也不能把旧代的 failed/transport_closed 重放给新页面。
  appBinding?.dispose();
  const binding = createDatabaseStartupRelayBinding(child, startupId, {
    deliver: (state) => {
      for (const win of windows.listMainWindows()) {
        if (!win.isDestroyed()) sendStartupState(win.webContents, state);
      }
    },
    reply: sendStartupState,
    acceptsControlFrom: windows.isMainWindowSender,
  });
  appBinding = binding;
  return { receive: binding.receive, startupId };
}
