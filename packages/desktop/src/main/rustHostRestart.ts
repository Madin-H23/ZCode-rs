// Rust Host 意外退出后的自动重启（docs/specs/rust-host-resilience.md §2.1、§4.1）。
//
// Main 是重启决定的唯一所有者：按窗口计数、退避，然后 reload 窗口，由 desktopWindowLifecycle
// 的 dom-ready 走与冷启动相同的路径拉起新 Host（init-local → ready → endpoint）。
// Renderer 只消费这里发出的进度通知，不自行决定重启。
import type { BrowserWindow } from "electron";
import { InternalChannels, type HostConnectionStatePayload } from "@zcode/shared";
import { releaseDatabaseStartupRelay } from "./databaseStartupRelay.js";
import { RustHostRestartPolicy } from "./rustHostRestartPolicy.js";

interface RestartLogger {
  info: (...args: unknown[]) => void;
  warn: (...args: unknown[]) => void;
}

const policies = new WeakMap<BrowserWindow, RustHostRestartPolicy>();

function notify(win: BrowserWindow, payload: HostConnectionStatePayload): void {
  if (!win.isDestroyed() && !win.webContents.isDestroyed())
    win.webContents.send(InternalChannels.HostConnectionState, payload);
}

/**
 * 窗口自己的本地 Rust Host 在没有经过 dispose 的情况下退出时调用。
 * `hasHost` 返回窗口此刻是否已有 Host：用户在退避期间手动 reload 时，dom-ready 已拉起新 Host，不再重复 reload。
 */
export function handleUnexpectedRustHostExit(options: {
  win: BrowserWindow;
  label: string;
  code: number;
  hasHost: () => boolean;
  logger: RestartLogger;
}): void {
  const { win, label, logger } = options;
  if (win.isDestroyed() || win.webContents.isDestroyed()) return;
  let policy = policies.get(win);
  if (!policy) {
    policy = new RustHostRestartPolicy();
    policies.set(win, policy);
  }
  const decision = policy.onUnexpectedExit(Date.now());
  if (decision.kind === "give-up") {
    logger.warn(
      `[rustHost] host (${label}) exited unexpectedly with code ${options.code}; giving up after ${decision.attempts} automatic restarts`,
    );
    notify(win, { state: "failed", attempts: decision.attempts });
    return;
  }
  logger.warn(
    `[rustHost] host (${label}) exited unexpectedly with code ${options.code}; restart ${decision.attempt}/${decision.maxAttempts} in ${decision.delayMs}ms`,
  );
  notify(win, {
    state: "restarting",
    attempt: decision.attempt,
    maxAttempts: decision.maxAttempts,
    delayMs: decision.delayMs,
  });
  setTimeout(() => {
    if (win.isDestroyed() || win.webContents.isDestroyed()) return;
    if (options.hasHost()) {
      logger.info(`[rustHost] window (${label}) already has a new host; skipping restart reload`);
      return;
    }
    // 旧代的 failed/transport_closed 不能在 reload 后被 snapshot 重放成失败页。
    releaseDatabaseStartupRelay(win);
    logger.info(`[rustHost] reloading window (${label}) to start a new host`);
    win.webContents.reload();
  }, decision.delayMs);
}
