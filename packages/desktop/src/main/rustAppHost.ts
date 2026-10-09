// App 级 Rust Host 的唯一所有者（docs/specs/rust-u4-app-host.md）。
//
// Rust 开关下整个 App 只有一个 Host 进程，所有主窗口接入它。这里持有当前代的进程句柄，把进程、
// 窗口与 IPC 事件交给 RustAppHostCore（纯状态机）决定拉起、签发、重启；同时是 Main 发起的
// setting.json 写入（HostSettingWriter）与 Host U4 控制消息的入口。窗口生命周期只调用
// attachWindow / detachWindow；其余 Main 代码经 hostForWindow 拿到窗口当前使用的 Host。
import { randomUUID } from "node:crypto";
import { BrowserWindow, type UtilityProcess as ElectronUtilityProcess } from "electron";
import {
  type AppSettings,
  HostMessageTypes,
  HostResponseTypes,
  InternalChannels,
  type RemoteTarget,
  formatZodError,
  rustAppHostControlResponseSchema,
} from "@zcode/shared";
import { bindAppDatabaseStartupRelay } from "./databaseStartupRelay.js";
import {
  disposeHostProcessAndWait,
  type HostInitMessage,
  type HostProcessDependencies,
  spawnRustAppHostProcess,
} from "./desktopHostProcess.js";
import { resolveHostRequestWindow } from "./hostRequestWindowRouting.js";
import {
  HostSettingWriter,
  type MainSettingStore,
  settingSettleBudgetMs,
} from "./hostSettingWriter.js";
import { isMainApplicationWindowWebContents } from "./resourceManagerWindow.js";
import { RustAppHostCore } from "./rustAppHostCore.js";
import { type RustHostProcess, sendRustHostServiceEndpoint } from "./rustDesktopHost.js";

/** App 级 Host 在资源管理器与日志里的 label；跨代不变，退出时先注销再由下一代注册。 */
const RUST_APP_HOST_LABEL = "app-host";
/** 与今天窗口 dom-ready 等待登录 shell 环境的上限相同。 */
const RUNTIME_PROCESS_ENV_WAIT_TIMEOUT_MS = 4_500;

type Logger = {
  info: (...args: unknown[]) => void;
  warn: (...args: unknown[]) => void;
};

/** `init-local` 的 App 级字段；`databaseStartupId` 每代由 owner 生成，不带 hostId / deliveryKind（§2.5）。 */
export type RustAppHostInit = Omit<
  HostInitMessage,
  "type" | "databaseStartupId" | "hostId" | "deliveryKind"
>;

interface RustAppHostOptions {
  logger: Logger;
  /** Host→Main 消息的处理依赖；在拉起时才取，避免与 Main 其余模块的初始化顺序耦合。 */
  getDependencies: () => HostProcessDependencies;
  /** 第一次拉起前解析一次并缓存：登录 shell 环境、启动 workspace 等（§2.1）。不应 reject。 */
  prepareInit: () => Promise<RustAppHostInit>;
  /** Bot 重连请求没有 windowId 时，找持有该 workspace route 的窗口（§2.7）。 */
  findWindowIdForRemoteWorkspace: (workspace: {
    workspacePath: string;
    workspaceIdentity: string;
    target: RemoteTarget;
  }) => number | undefined;
  onSettingChanged: (settings: AppSettings) => void;
  onOffPeakActiveCountChanged: (count: number) => void;
  disposingHostProcessTimers: WeakMap<ElectronUtilityProcess, ReturnType<typeof setTimeout>>;
}

function listMainWindows(): BrowserWindow[] {
  return BrowserWindow.getAllWindows().filter(
    (win) =>
      !win.isDestroyed() &&
      !win.webContents.isDestroyed() &&
      isMainApplicationWindowWebContents(win.webContents.id),
  );
}

function findMainWindow(windowId: number): BrowserWindow | undefined {
  return listMainWindows().find((win) => win.webContents.id === windowId);
}

function anyMainWindow(): BrowserWindow | undefined {
  const windows = listMainWindows();
  return windows.find((win) => win.isFocused()) ?? windows[0];
}

function startTimer(delayMs: number, fire: () => void): () => void {
  const timer = setTimeout(fire, delayMs);
  return () => clearTimeout(timer);
}

/** 等登录 shell 环境采集完成；超过上限或失败时用不执行 shell 的降级补丁（与窗口 Host 相同）。 */
export async function resolveRuntimeProcessEnvPatch(
  preparation: {
    patchPromise: Promise<Record<string, string>>;
    fallbackPatch: Record<string, string>;
  },
  logger: Logger,
): Promise<Record<string, string>> {
  let cancelTimer: () => void = () => {};
  const timeout = new Promise<Record<string, string>>((resolve) => {
    cancelTimer = startTimer(RUNTIME_PROCESS_ENV_WAIT_TIMEOUT_MS, () => {
      logger.warn(
        `[rustAppHost] runtime env prewarm exceeded ${RUNTIME_PROCESS_ENV_WAIT_TIMEOUT_MS}ms, using shell-free fallback`,
      );
      resolve(preparation.fallbackPatch);
    });
  });
  const captured = preparation.patchPromise.catch((error: unknown) => {
    logger.warn("[rustAppHost] runtime env prewarm failed, using shell-free fallback:", error);
    return preparation.fallbackPatch;
  });
  try {
    return await Promise.race([captured, timeout]);
  } finally {
    cancelTimer();
  }
}

export function createRustAppHost(options: RustAppHostOptions) {
  const { logger } = options;
  /** 当前代的进程；退出后置空，下一代拉起时替换。 */
  let current: { host: RustHostProcess; generation: number; startupId: string } | null = null;
  let initPromise: Promise<RustAppHostInit> | null = null;
  /** 每个窗口当前页面拿到 endpoint 之后要做的事（attachWindow 传入，随每次 dom-ready 替换）。 */
  const onPageAttached = new Map<number, () => void>();

  const core = new RustAppHostCore({
    spawn: (generation) => {
      // 修复原因：这里原来是 `void spawnGeneration(generation)`，拉起时同步抛出的异常变成无人处理的
      // rejection，状态机停在 running 却没有进程，窗口永远等不到 endpoint。拉起失败按这一代意外退出处理，
      // 交给重启策略。
      spawnGeneration(generation).catch((error: unknown) =>
        spawnFailed(generation, "spawning the host failed", error),
      );
    },
    issueEndpoint: (windowId, generation, pageToken) => {
      const win = findMainWindow(windowId);
      if (!win || !current || current.generation !== generation) return;
      const isCurrentPage = () => core.isCurrentPage(windowId, pageToken);
      void sendRustHostServiceEndpoint(
        win,
        current.host,
        current.startupId,
        logger,
        isCurrentPage,
      ).then((delivered) => {
        if (delivered && isCurrentPage()) onPageAttached.get(windowId)?.();
      });
    },
    reloadWindow: (windowId) => {
      const win = findMainWindow(windowId);
      if (!win) return;
      logger.info(`[rustAppHost] reloading window ${windowId} to attach to the new host`);
      win.webContents.reload();
    },
    notifyConnectionState: (payload, windowId) => {
      const targets =
        windowId === undefined ? listMainWindows() : [findMainWindow(windowId)].filter(Boolean);
      for (const win of targets) {
        win?.webContents.send(InternalChannels.HostConnectionState, payload);
      }
    },
    setTimer: startTimer,
    now: () => Date.now(),
    logger,
  });

  const settingWriter = new HostSettingWriter({
    waitUntilReady: (timeoutMs) => core.waitUntilReady(timeoutMs),
    post: (message) => {
      if (!current || !core.snapshot().processAlive) return false;
      current.host.postMessage(message);
      return true;
    },
    newRequestId: randomUUID,
    setTimer: startTimer,
    logger,
  });

  function handleControlMessage(message: unknown): void {
    const type = (message as { type?: unknown } | null)?.type;
    if (
      type !== HostResponseTypes.SettingWriteResult &&
      type !== HostResponseTypes.SettingChanged &&
      type !== HostResponseTypes.OffPeakActiveCount
    ) {
      return;
    }
    const parsed = rustAppHostControlResponseSchema.safeParse(message);
    if (!parsed.success) {
      logger.warn(`[rustAppHost] invalid ${String(type)}: ${formatZodError(parsed.error)}`);
      return;
    }
    if (parsed.data.type === HostResponseTypes.SettingWriteResult) {
      settingWriter.handleResult(parsed.data);
    } else if (parsed.data.type === HostResponseTypes.SettingChanged) {
      options.onSettingChanged(parsed.data.setting);
    } else {
      options.onOffPeakActiveCountChanged(parsed.data.count);
    }
  }

  /** 这一代没能拉起（或拉起中途失败）：结束可能已启动的进程，按意外退出交给状态机。 */
  function spawnFailed(generation: number, reason: string, error: unknown): void {
    logger.warn(`[rustAppHost] ${reason} for generation ${generation}:`, error);
    if (current?.generation === generation) {
      const host = current.host;
      current = null;
      settingWriter.hostExited();
      try {
        host.kill();
      } catch {
        // 进程可能已经不存在。
      }
    }
    core.hostExited(generation, -1);
  }

  async function spawnGeneration(generation: number): Promise<void> {
    let init: RustAppHostInit;
    try {
      init = await (initPromise ??= options.prepareInit());
    } catch (error) {
      initPromise = null;
      if (core.shouldSpawn(generation))
        spawnFailed(generation, "failed to prepare init-local", error);
      return;
    }
    if (!core.shouldSpawn(generation)) return;
    const startupId = randomUUID();
    let relay: ReturnType<typeof bindAppDatabaseStartupRelay> | undefined;
    spawnRustAppHostProcess(
      RUST_APP_HOST_LABEL,
      { type: HostMessageTypes.InitLocal, ...init, databaseStartupId: startupId },
      options.getDependencies(),
      {
        onSpawned: (host) => {
          current = { host, generation, startupId };
          // 先绑定启动中继，它的 exit 监听排在 owner 之前：所有窗口先收到 failed，再收到重启进度。
          relay = bindAppDatabaseStartupRelay(host.asUtilityProcess(), startupId, {
            listMainWindows,
            isMainWindowSender: (sender) => isMainApplicationWindowWebContents(sender.id),
          });
          host.on("message", handleControlMessage);
          core.hostSpawned(generation);
        },
        onDatabaseStartupState: (state) => {
          relay?.receive(state);
          if (state.startupId === startupId) core.hostStartupState(generation, state);
        },
        resolveRequestWindow: (request) =>
          resolveHostRequestWindow(request, {
            byWindowId: findMainWindow,
            preferred: request.workspace
              ? () => {
                  const windowId = options.findWindowIdForRemoteWorkspace(request.workspace!);
                  return windowId === undefined ? undefined : findMainWindow(windowId);
                }
              : undefined,
            anyMainWindow,
          }),
        onExit: (code) => {
          if (current?.generation === generation) current = null;
          settingWriter.hostExited();
          core.hostExited(generation, code);
        },
      },
    );
  }

  return {
    /** App ready 时调用一次。 */
    start(): void {
      core.start();
    },
    /**
     * 主窗口 dom-ready（含 reload）：为当前页面签发 endpoint，Host 未就绪时等就绪后签发。
     * `onAttached` 在 endpoint 交给这个页面之后执行（窗口级的“Host 已可用”动作，如补挂远端
     * attachment）；页面在此之前 reload 或关闭时不执行。
     */
    attachWindow(win: BrowserWindow, onAttached?: () => void): void {
      const windowId = win.webContents.id;
      if (onAttached) onPageAttached.set(windowId, onAttached);
      else onPageAttached.delete(windowId);
      core.windowDomReady(windowId);
    },
    /** 主窗口关闭或销毁：只断开这个窗口；Renderer 的 WebSocket 随页面一起关闭，Host 继续运行。 */
    detachWindow(webContentsId: number): void {
      onPageAttached.delete(webContentsId);
      core.windowClosed(webContentsId);
    },
    /** 窗口当前使用的 Host：窗口已接入且当前代进程存活时返回。 */
    hostForWindow(webContentsId: number): ElectronUtilityProcess | undefined {
      if (!current || !core.snapshot().processAlive || !core.hasWindow(webContentsId)) return;
      return current.host.asUtilityProcess();
    },
    /** Main 读设置仍直接读文件（Host 落盘后才回 ok）；写入只经 Host 的 SettingStore（§2.6）。 */
    createSettingStore(fileSettings: Pick<MainSettingStore, "get">): MainSettingStore {
      return {
        get: () => fileSettings.get(),
        update: (patch) => settingWriter.write(patch),
      };
    },
    /**
     * App 退出 / 安装更新：先等在途的设置写入结果，再发 dispose，最多等 3.5 s 后结束进程（不变）。
     * 修复原因：原来无上限地等写入结果（单个写入最长 10 s），超过普通退出的 4.5 s 预算；现在写入的
     * 等待与 dispose 共用同一个退出预算（settingSettleBudgetMs）。
     */
    async disposeAndWait(
      reason: string,
      policy: { forceKillDelayMs?: number; waitTimeoutMs?: number },
    ): Promise<void> {
      const startedAt = Date.now();
      const settled = settingWriter.closeAndSettle(settingSettleBudgetMs(policy));
      const live = core.beginDispose();
      if (!(await settled))
        logger.warn(
          `[rustAppHost] disposing with setting writes still in flight (${reason}); their results are unknown`,
        );
      const host = current?.host;
      if (!live || !host) return;
      logger.info(`[rustAppHost] disposing the app host (${reason}), pid=${host.pid ?? "unknown"}`);
      await disposeHostProcessAndWait(
        host.asUtilityProcess(),
        `${RUST_APP_HOST_LABEL}:${reason}`,
        options.disposingHostProcessTimers,
        logger,
        {
          ...policy,
          ...(policy.waitTimeoutMs === undefined
            ? {}
            : { waitTimeoutMs: Math.max(0, policy.waitTimeoutMs - (Date.now() - startedAt)) }),
        },
      );
    },
  };
}
