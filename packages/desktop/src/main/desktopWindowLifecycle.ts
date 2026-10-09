import { getDatabaseStartupPortPayload } from "./databaseStartupRelay.js";
import { randomUUID } from "node:crypto";
import { app, BrowserWindow, Menu, MessageChannelMain } from "electron";
import type { UtilityProcess as ElectronUtilityProcess } from "electron";
import { HostMessageTypes, InternalChannels, PlatformChannels, type Locale } from "@zcode/shared";
import { scheduleArmsBrowserPerfLoadNudge } from "./armsBrowserPerfLoadNudge.js";
import { createBrowserWindow } from "./desktopWindowChrome.js";
import type { HostInitMessage, WindowBootstrapOptions } from "./desktopHostProcess.js";
import type { StartupWorkspaceWarmupTarget } from "./startupWorkspace.js";
import { handleDarwinWindowCloseRequest } from "./desktopDarwinCloseBehavior.js";
import {
  parseWindowUnreadCount,
  sumWindowUnreadCounts,
  syncAppUnreadBadge,
} from "./unreadBadge.js";
import { attachDesktopWindowSizePersistence, type DesktopWindowSize } from "./desktopWindowSize.js";
import {
  registerMainApplicationWindow,
  unregisterMainApplicationWindow,
} from "./resourceManagerWindow.js";

const DEFAULT_RUNTIME_PROCESS_ENV_WAIT_TIMEOUT_MS = 4_500;

/** Node 路径：每个主窗口一个 Host，dom-ready 时拉起，reload 时为存活的 Host 补挂端口。 */
export interface WindowOwnedHostOptions {
  kind: "window";
  windowHostProcessMap: Map<number, ElectronUtilityProcess>;
  spawnHostProcess: (
    win: BrowserWindow,
    label: string,
    initMessage: HostInitMessage,
  ) => ElectronUtilityProcess;
  disposeHostProcess: (
    child: ElectronUtilityProcess,
    label: string,
    forceKillDelayMs?: number,
  ) => void;
  agentWarmupTargets?: readonly StartupWorkspaceWarmupTarget[];
  agentSpawnFallbackCwd: string;
  /** Main 模块初始化期已开始的异步环境采集；通常在 renderer dom-ready 前完成。 */
  runtimeProcessEnvPatchPromise?: Promise<Record<string, string>>;
  /** 不执行 shell 即可计算的完整降级 patch；预热失败/超时时仍要注入 Local Host。 */
  runtimeProcessEnvFallbackPatch: Record<string, string>;
  /** 仅供启动门禁和测试注入；超过该时间必须 fail-open 创建 Local Host。 */
  runtimeProcessEnvWaitTimeoutMs?: number;
  /**
   * 首个 Local Host 创建前的有界灰度裁决门。
   *
   * 缺省（undefined）时完全不触发 await，dom-ready handler 同步执行——保证既有调用方
   * 与测试零回归。仅 desktop main 注入：在 spawnLocalHost 之前等待一次 rollout 裁决，
   * 避免冷启动快照 { enabled:false } 被烤进首 Host env 后无法被异步成功结果覆盖。
   */
  awaitFirstHostSpawnDecision?: () => Promise<void>;
}

/**
 * Rust 开关：App 级 Host 由 rustAppHost.ts 拥有（docs/specs/rust-u4-app-host.md §2.1）。
 * 窗口 dom-ready 只申请接入，关闭只断开自己；Host 的拉起、就绪与重启不归窗口管。
 */
interface AppHostWindowOptions {
  kind: "app";
  appHost: {
    /** `onAttached` 在 Host 就绪、endpoint 交给这个页面之后执行。 */
    attachWindow(win: BrowserWindow, onAttached: () => void): void;
    detachWindow(webContentsId: number): void;
  };
}

export function createWindow(options: {
  iconPath: string;
  preloadPath: string;
  logger: { info: (...args: unknown[]) => void; warn: (...args: unknown[]) => void };
  forceQuitRef: { current: boolean };
  handleBeforeClose?: (win: BrowserWindow, label: string) => boolean;
  host: WindowOwnedHostOptions | AppHostWindowOptions;
  syncAutoUpdaterStateToWindow: (win: BrowserWindow) => void;
  syncReadyUpdateToWindow: (win: BrowserWindow) => void;
  syncPostUpdateReleaseNotesToWindow: (win: BrowserWindow) => void;
  disposeRemoteWorkspaceSessionsForWindow: (
    windowId: number,
    reason: string,
    disposeOptions?: { releaseInHost?: boolean },
  ) => void;
  reattachRemoteWorkspaceSessionsForWindow: (win: BrowserWindow, reason: string) => void;
  bootstrap?: WindowBootstrapOptions;
  deviceMid: string;
  initialDesktopZoomLevel?: number;
  initialWindowSize?: DesktopWindowSize;
  currentApplicationLocale?: () => Locale;
  persistWindowSize?: (state: DesktopWindowSize) => Promise<void>;
  /** Local Host map insertion completed; presentation facts can now be replayed safely. */
  onHostProcessReady?: (windowKey: number) => void;
  resolveBrowserViewOwner?: Parameters<typeof createBrowserWindow>[0]["resolveBrowserViewOwner"];
}) {
  const win = createBrowserWindow({
    iconPath: options.iconPath,
    preloadPath: options.preloadPath,
    bootstrap: {
      restoreSession: options.bootstrap?.restoreSession ?? true,
      supportsSettings: options.bootstrap?.supportsSettings ?? true,
      initialWorkspacePath: options.bootstrap?.initialWorkspacePath,
      initialWorkspacePurpose: options.bootstrap?.initialWorkspacePurpose,
      unavailableWorkspacePath: options.bootstrap?.unavailableWorkspacePath,
    },
    logger: options.logger,
    deviceMid: options.deviceMid,
    initialDesktopZoomLevel: options.initialDesktopZoomLevel,
    initialWindowSize: options.initialWindowSize,
    currentApplicationLocale: options.currentApplicationLocale,
    resolveBrowserViewOwner: options.resolveBrowserViewOwner,
  });
  const label = `local-${win.webContents.id}`;

  if (options.persistWindowSize) {
    attachDesktopWindowSizePersistence(win, options.persistWindowSize, (error) => {
      options.logger.warn("[desktop-window] failed to persist main window size", error);
    });
  }

  if (process.platform === "darwin") {
    win.on("close", (event) => {
      if (
        handleDarwinWindowCloseRequest({
          win,
          forceQuit: options.forceQuitRef.current,
          label,
          logger: options.logger,
        })
      ) {
        event.preventDefault();
      }
    });
  } else if (options.handleBeforeClose) {
    win.on("close", (event) => {
      if (options.handleBeforeClose?.(win, label)) {
        event.preventDefault();
      }
    });
  }

  const wcId = win.webContents.id;
  const browserWindowId = win.id;
  const host = options.host;
  // 资源遥测据此把主窗口 renderer 归 renderer_main；辅助窗口与 DevTools 归 chromium_other。
  registerMainApplicationWindow(wcId);
  let domReadyGeneration = 0;
  let cancelRuntimeProcessEnvWait: (() => void) | null = null;
  scheduleArmsBrowserPerfLoadNudge(win.webContents);
  win.webContents.on("dom-ready", async () => {
    cancelRuntimeProcessEnvWait?.();
    cancelRuntimeProcessEnvWait = null;
    const currentDomReadyGeneration = ++domReadyGeneration;
    options.logger.info(`[createWindow] dom-ready fired (${label})`);

    if (process.platform === "win32" && !win.isDestroyed()) {
      win.show();
      win.focus();
    }

    if (host.kind === "app") {
      // App 级 Host：首次加载与 reload 都只为当前页面申请接入（Host 未就绪时由 owner 等就绪后签发）。
      // 修复原因：原来在 dom-ready 立即通知“Host 可用”并补挂远端 attachment，不管 Host 是否就绪
      // （退避期间同样执行），reload 也记成 renderer-ready。改为 endpoint 交给这个页面之后再做，
      // 与 Node 路径“Host 就位后再做”的顺序一致。
      const reason = currentDomReadyGeneration === 1 ? "renderer-ready" : "renderer-reload";
      host.appHost.attachWindow(win, () => {
        options.onHostProcessReady?.(wcId);
        // 修复原因：原 Rust 分支在 reload 时直接返回，没有为窗口重新挂远端 workspace 的 attachment。
        options.reattachRemoteWorkspaceSessionsForWindow(win, `${label}:${reason}`);
      });
      options.syncAutoUpdaterStateToWindow(win);
      options.syncReadyUpdateToWindow(win);
      options.syncPostUpdateReleaseNotesToWindow(win);
      return;
    }

    const oldChild = host.windowHostProcessMap.get(wcId);
    // renderer 刷新（reload）
    // 曾经无条件杀掉旧 host 进程再重建——host 连带 CLI agent 一起死，运行中的会话直接消失，
    // 这正是「会话身份易失」病根。host/CLI 的生命周期属于窗口而非
    // renderer 加载周期：reload 只需给存活的 host 补挂一条新 RPC MessagePort
    // （复用 web 远控的 AttachServicePort 通道），renderer 重新订阅即可恢复投影。
    // 旧端口的 ChannelServer 会随 renderer 上下文销毁触发 close 自行回收。
    if (oldChild && oldChild.pid !== undefined) {
      try {
        const startupPayload = getDatabaseStartupPortPayload(oldChild);
        if (!startupPayload) throw new Error("Previous Host startup binding is unavailable");
        const { port1, port2 } = new MessageChannelMain();
        oldChild.postMessage(
          {
            type: HostMessageTypes.AttachServicePort,
            requestId: randomUUID(),
            attachmentId: randomUUID(),
            clientMode: "desktop-continuous",
            scope: { kind: "local" },
          },
          [port2],
        );
        win.webContents.postMessage(InternalChannels.ServicePort, startupPayload, [port1]);
        options.logger.info(
          `[createWindow] renderer reloaded, reattached to existing host (${label}), pid=${oldChild.pid}`,
        );
        options.syncAutoUpdaterStateToWindow(win);
        options.syncReadyUpdateToWindow(win);
        options.syncPostUpdateReleaseNotesToWindow(win);
        options.reattachRemoteWorkspaceSessionsForWindow(win, `${label}:renderer-reload`);
        return;
      } catch (error) {
        options.logger.warn(
          `[createWindow] reattach to existing host failed (${label}), falling back to respawn:`,
          error,
        );
      }
    }
    if (oldChild) {
      options.logger.info(
        `[createWindow] killing previous host process for (${label}), pid=${oldChild.pid ?? "unknown"}`,
      );
      host.disposeHostProcess(oldChild, `${label}:reload`, 150);
    }

    // 首个 Local Host 创建前的有界灰度裁决门。用 `if` 守卫而非 `await cb?.()`——
    // cb 缺省时不触发任何 await，async handler 同步跑完，保证既有调用方与测试零回归。
    // 仅在需要 spawn 新 Host 的路径上等待（reattach 早退路径已在上方 return，不触发）。
    if (host.awaitFirstHostSpawnDecision) {
      await host.awaitFirstHostSpawnDecision();
    }

    const spawnLocalHost = (runtimeProcessEnvPatch: Record<string, string>) => {
      if (currentDomReadyGeneration !== domReadyGeneration || win.isDestroyed()) {
        return;
      }
      const primaryWarmupTarget = host.agentWarmupTargets?.[0];
      const child = host.spawnHostProcess(win, label, {
        type: HostMessageTypes.InitLocal,
        deviceMid: options.deviceMid,
        workspacePath: primaryWarmupTarget?.workspacePath,
        workspaceIdentity: primaryWarmupTarget?.workspaceIdentity,
        ...(host.agentWarmupTargets && host.agentWarmupTargets.length > 0
          ? { agentWarmupTargets: [...host.agentWarmupTargets] }
          : {}),
        runtimeProcessEnvPatch,
        // 同一窗口会后台索引所有已恢复 workspace，不只索引启动时的 active workspace。
        // fallback 必须跟随 local Host 生命周期常驻，否则非 active 历史目录被删除后会用失效 cwd 反复 spawn。
        agentSpawnFallbackCwd: host.agentSpawnFallbackCwd,
      });
      host.windowHostProcessMap.set(wcId, child);
      options.onHostProcessReady?.(wcId);
      options.syncAutoUpdaterStateToWindow(win);
      options.syncReadyUpdateToWindow(win);
      options.syncPostUpdateReleaseNotesToWindow(win);
      options.reattachRemoteWorkspaceSessionsForWindow(win, `${label}:renderer-ready`);
    };

    if (!host.runtimeProcessEnvPatchPromise) {
      spawnLocalHost(host.runtimeProcessEnvFallbackPatch);
      return;
    }
    let settled = false;
    const waitTimeoutMs =
      host.runtimeProcessEnvWaitTimeoutMs ?? DEFAULT_RUNTIME_PROCESS_ENV_WAIT_TIMEOUT_MS;
    let timeout: ReturnType<typeof setTimeout> | null = null;
    const cancelWait = () => {
      if (settled) {
        return;
      }
      settled = true;
      if (timeout) {
        clearTimeout(timeout);
      }
    };
    const completeWait = (runtimeProcessEnvPatch: Record<string, string>) => {
      if (settled) {
        return;
      }
      settled = true;
      if (timeout) {
        clearTimeout(timeout);
      }
      if (cancelRuntimeProcessEnvWait === cancelWait) {
        cancelRuntimeProcessEnvWait = null;
      }
      spawnLocalHost(runtimeProcessEnvPatch);
    };
    cancelRuntimeProcessEnvWait = cancelWait;
    timeout = setTimeout(() => {
      options.logger.warn(
        `[createWindow] runtime env prewarm exceeded ${waitTimeoutMs}ms after dom-ready (${label}), using shell-free fallback`,
      );
      completeWait(host.runtimeProcessEnvFallbackPatch);
    }, waitTimeoutMs);
    void host.runtimeProcessEnvPatchPromise.then(completeWait, (error) => {
      options.logger.warn(
        `[createWindow] runtime env prewarm failed (${label}), using shell-free fallback:`,
        error,
      );
      // 旧 rejection 分支传 undefined，Host 随后又同步执行同一个 login shell，
      // 可能把 Main 的白屏转移成 Host 卡死。Main 路径始终传入预计算 fallback patch。
      completeWait(host.runtimeProcessEnvFallbackPatch);
    });
  });

  win.on("closed", () => {
    unregisterMainApplicationWindow(wcId);
    cancelRuntimeProcessEnvWait?.();
    cancelRuntimeProcessEnvWait = null;
    if (host.kind === "app") {
      // App 级 Host 继续运行：只断开这个窗口，并让 Host 释放这个窗口的远端 logical session。
      options.logger.info(`[createWindow] window closed, detaching from the app host (${label})`);
      host.appHost.detachWindow(wcId);
      options.disposeRemoteWorkspaceSessionsForWindow(wcId, `${label}:window-closed`, {
        releaseInHost: true,
      });
      return;
    }
    options.logger.info(`[createWindow] window closed, killing host process (${label})`);
    const child = host.windowHostProcessMap.get(wcId);
    if (child) {
      host.disposeHostProcess(child, `${label}:window-closed`);
      host.windowHostProcessMap.delete(wcId);
    }
    options.disposeRemoteWorkspaceSessionsForWindow(wcId, `${label}:window-closed`);
  });

  return win;
}

export function showCurrentWindowFromDock(primaryWindowCoordinator: {
  ensurePrimaryWindow(reason: string): Promise<void>;
}) {
  if (process.platform === "darwin") {
    app.show();
  }

  void primaryWindowCoordinator.ensurePrimaryWindow("dock-show-current-window");
}

export function focusWorkspaceInExistingWindow(
  path: string,
  windowWorkspaceMap: Map<number, Set<string>>,
  options?: { skipWindowId?: number },
): { activated: boolean; winId?: number } {
  for (const [winId, pathSet] of windowWorkspaceMap) {
    if (options?.skipWindowId === winId) {
      continue;
    }
    if (!pathSet.has(path)) {
      continue;
    }

    const existingWin = BrowserWindow.fromId(winId);
    if (existingWin && !existingWin.isDestroyed()) {
      if (existingWin.isMinimized()) {
        existingWin.restore();
      }
      existingWin.focus();
      existingWin.webContents.send(PlatformChannels.FocusTab, path);
      return { activated: true, winId };
    }

    windowWorkspaceMap.delete(winId);
  }

  return { activated: false };
}

export function syncApplicationUnreadBadge(windowUnreadCountMap: Map<number, number>) {
  syncAppUnreadBadge({
    platform: process.platform,
    totalUnreadCount: sumWindowUnreadCounts(windowUnreadCountMap),
    setBadgeCount: (count) => {
      app.setBadgeCount(count);
    },
  });
}

export function handleWindowUnreadCountSync(
  win: BrowserWindow | null,
  payload: unknown,
  windowUnreadCountMap: Map<number, number>,
  logger: { warn: (...args: unknown[]) => void },
) {
  const unreadCount = parseWindowUnreadCount(payload);
  if (unreadCount == null) {
    logger.warn("[sync-window-unread-count] invalid payload:", payload);
    return false;
  }

  if (!win) {
    return false;
  }

  if (unreadCount === 0) {
    windowUnreadCountMap.delete(win.id);
  } else {
    windowUnreadCountMap.set(win.id, unreadCount);
  }
  syncApplicationUnreadBadge(windowUnreadCountMap);
  return true;
}

export function configureDockMenu(getLabel: () => string, onShowCurrentWindow: () => void) {
  if (process.platform !== "darwin" || app.dock == null) {
    return;
  }

  const dockMenu = Menu.buildFromTemplate([
    {
      label: getLabel(),
      click: onShowCurrentWindow,
    },
  ]);

  app.dock.setMenu(dockMenu);
}

export function handleDesktopWindowCloseRequest(options: {
  platform: NodeJS.Platform;
  forceQuit: boolean;
  explicitQuitRequested?: boolean;
  closeToTrayOnWindows?: boolean;
  isLastWindow: boolean;
  label: string;
  logger: { info: (...args: unknown[]) => void };
  shouldConfirmQuit?: boolean;
  confirmQuit: () => boolean;
  requestQuit: () => void;
  hideWindow?: () => void;
}) {
  if (
    options.platform === "win32" &&
    options.closeToTrayOnWindows &&
    !options.forceQuit &&
    !options.explicitQuitRequested
  ) {
    options.logger.info(`[createWindow] window close hidden to tray (${options.label})`);
    options.hideWindow?.();
    return true;
  }

  if (options.platform === "darwin" || options.forceQuit || !options.isLastWindow) {
    return false;
  }

  if (options.shouldConfirmQuit === false) {
    options.logger.info(
      `[createWindow] last window close skipped confirmation, quitting app (${options.label})`,
    );
    options.requestQuit();
    return true;
  }

  if (!options.confirmQuit()) {
    options.logger.info(`[createWindow] last window close canceled by user (${options.label})`);
    return true;
  }

  options.logger.info(
    `[createWindow] last window close confirmed, quitting app (${options.label})`,
  );
  options.requestQuit();
  return true;
}
