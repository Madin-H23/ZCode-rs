// App 级 Rust Host 的生命周期状态机（docs/specs/rust-u4-app-host.md §2.1、§2.8）。
//
// 纯逻辑，不依赖 Electron：决定何时拉起 Host、何时为哪个窗口签发 endpoint、意外退出后
// 重启 / 放弃 / 重试，以及哪些窗口需要 reload。进程、窗口与 IPC 的执行由 rustAppHost.ts
// 通过 effects 注入。Host 的每次拉起是一代（generation），旧代的事件一律忽略。
import type { DatabaseStartupState, HostConnectionStatePayload } from "@zcode/shared";
import { RustHostRestartPolicy } from "./rustHostRestartPolicy.js";

type RustAppHostPhase =
  /** App 尚未拉起 Host。 */
  | "idle"
  /** 当前代已决定拉起（进程可能仍在准备环境，或已运行但尚未 ready）。 */
  | "running"
  /** 意外退出后等待退避结束再拉起下一代。 */
  | "restart-scheduled"
  /** 60 s 内意外退出过多，已放弃自动重启；失败页所在窗口的 reload（Retry）触发一次重启。 */
  | "gave-up"
  | "disposing"
  | "disposed";

interface RustAppHostCoreEffects {
  /** 拉起第 generation 代 Host；进程真正启动后调用 `hostSpawned(generation)`。 */
  spawn(generation: number): void;
  /** 为窗口当前页面签发 endpoint；pageToken 用来丢弃发给已被 reload 的旧页面的结果。 */
  issueEndpoint(windowId: number, generation: number, pageToken: number): void;
  reloadWindow(windowId: number): void;
  /** 发给 `windowId` 这个主窗口；省略时发给所有主窗口（不只是已接入的窗口）。 */
  notifyConnectionState(payload: HostConnectionStatePayload, windowId?: number): void;
  setTimer(delayMs: number, fire: () => void): () => void;
  now(): number;
  logger: {
    info: (...args: unknown[]) => void;
    warn: (...args: unknown[]) => void;
  };
}

interface WindowEntry {
  /** 每次 dom-ready 换一个新值；签发结果只交给仍是这个值的页面。 */
  pageToken: number;
  /** 当前页面还没有拿到 endpoint。 */
  needsEndpoint: boolean;
  /** 页面属于已退出的那一代 Host（显示断开 / 失败页），下一代 ready 后要 reload。 */
  stale: boolean;
  /** Main 已让这个页面 reload、还没等到它的 dom-ready：下一次 dom-ready 来自 Main，不是用户操作。 */
  reloadRequested: boolean;
}

interface RustAppHostCoreSnapshot {
  phase: RustAppHostPhase;
  generation: number;
  /** 当前代已至少 ready 过一次（存储准备完成、服务已安装）。 */
  ready: boolean;
  /** 当前代的进程已启动且尚未退出。 */
  processAlive: boolean;
}

export class RustAppHostCore {
  private phase: RustAppHostPhase = "idle";
  private generation = 0;
  private ready = false;
  private processAlive = false;
  private readonly readyAttempts = new Set<string>();
  private readonly windows = new Map<number, WindowEntry>();
  private readonly readyWaiters = new Set<(ready: boolean) => void>();
  private cancelRestart: (() => void) | null = null;
  private nextPageToken = 0;
  /** 放弃前自动重启过的次数，给放弃期间新打开的窗口显示同一失败页。 */
  private gaveUpAttempts = 0;

  constructor(
    private readonly effects: RustAppHostCoreEffects,
    private readonly policy = new RustHostRestartPolicy(),
  ) {}

  snapshot(): RustAppHostCoreSnapshot {
    return {
      phase: this.phase,
      generation: this.generation,
      ready: this.ready,
      processAlive: this.processAlive,
    };
  }

  /** App ready：拉起唯一的 Host。只生效一次。 */
  start(): void {
    if (this.phase !== "idle") return;
    this.spawnNext("app-ready");
  }

  /** 准备环境期间可能已开始退出或换代；只有仍是当前代且在运行阶段才真正拉起进程。 */
  shouldSpawn(generation: number): boolean {
    return this.phase === "running" && generation === this.generation && !this.processAlive;
  }

  hostSpawned(generation: number): void {
    if (generation !== this.generation) return;
    this.processAlive = true;
  }

  /** 主窗口 dom-ready（含 reload）：为这个页面签发一个 endpoint；Host 未就绪时等就绪后签发。 */
  windowDomReady(windowId: number): void {
    if (this.phase === "disposing" || this.phase === "disposed") return;
    const previous = this.windows.get(windowId);
    const entry: WindowEntry = {
      pageToken: ++this.nextPageToken,
      needsEndpoint: true,
      stale: false,
      reloadRequested: false,
    };
    this.windows.set(windowId, entry);
    if (this.phase === "idle") {
      this.spawnNext("window-dom-ready");
      return;
    }
    if (this.phase === "gave-up") {
      if (previous?.stale && !previous.reloadRequested) {
        // 失败页的 Retry（页面自身 location.reload）：触发一次 App 级重启，不经过重启策略。
        // 修复原因：原来放弃后的任何 dom-ready（包括新打开的窗口）都会重启，且不清空计数，
        // Retry 之后一次崩溃就直接再次放弃。Retry 是用户明确要求重来，从第 1 次自动重启重新计起。
        this.policy.reset();
        this.spawnNext(`retry-from-window-${windowId}`);
        return;
      }
      // 不是 Retry 的 dom-ready 不触发重启，页面显示同一失败页，它的 Retry 才触发（§2.8）：
      // - 放弃期间新打开的主窗口；
      // - 修复原因：新一代就绪后 Main 让旧页面 reload，reload 完成前这一代又崩溃并放弃时，这次
      //   reload 的 dom-ready 曾被当成用户的 Retry，自动拉起下一代——启动后很快崩溃的 Host 会因此
      //   无限重启（E2E resilience-host-crash 两个窗口时复现：第 4 次崩溃后窗口 2 的 reload 拉起了第 5 代）。
      entry.stale = true;
      entry.needsEndpoint = false;
      this.effects.logger.info(
        `[rustAppHost] window ${windowId} loaded after giving up (${previous?.reloadRequested ? "reload requested by Main" : "new page"}); showing the failure page`,
      );
      this.effects.notifyConnectionState(
        { state: "failed", attempts: this.gaveUpAttempts },
        windowId,
      );
      return;
    }
    if (this.phase === "running" && this.ready) this.issue(windowId, entry);
  }

  /** 窗口关闭或销毁：只忘掉这个窗口；Host 继续运行。 */
  windowClosed(windowId: number): void {
    this.windows.delete(windowId);
  }

  hasWindow(windowId: number): boolean {
    return this.windows.has(windowId);
  }

  isCurrentPage(windowId: number, pageToken: number): boolean {
    return this.windows.get(windowId)?.pageToken === pageToken;
  }

  /** 当前代每个启动尝试第一次 ready 时签发（rust-host-resilience.md §2.2 的规则不变）。 */
  hostStartupState(
    generation: number,
    state: Pick<DatabaseStartupState, "phase" | "attemptId">,
  ): void {
    if (generation !== this.generation || this.phase !== "running") return;
    if (state.phase !== "ready" || this.readyAttempts.has(state.attemptId)) return;
    this.readyAttempts.add(state.attemptId);
    if (!this.ready) {
      this.ready = true;
      // 重启后的新代就绪：属于旧代的页面统一 reload，在 dom-ready 时重新接入（§2.8）。
      for (const [windowId, entry] of this.windows) {
        if (!entry.stale) continue;
        entry.stale = false;
        entry.needsEndpoint = false;
        entry.reloadRequested = true;
        this.effects.reloadWindow(windowId);
      }
      for (const resolve of Array.from(this.readyWaiters)) resolve(true);
    }
    for (const [windowId, entry] of this.windows) {
      if (entry.needsEndpoint) this.issue(windowId, entry);
    }
  }

  hostExited(generation: number, code: number): void {
    if (generation !== this.generation) return;
    this.processAlive = false;
    if (this.phase === "disposing") {
      this.phase = "disposed";
      return;
    }
    if (this.phase !== "running") return;
    // 未经 dispose 的退出都是意外退出（rust-host-resilience.md §2.1），退出码不参与判断。
    this.ready = false;
    for (const entry of this.windows.values()) {
      entry.stale = true;
      entry.needsEndpoint = false;
    }
    const decision = this.policy.onUnexpectedExit(this.effects.now());
    if (decision.kind === "give-up") {
      this.phase = "gave-up";
      this.gaveUpAttempts = decision.attempts;
      this.effects.logger.warn(
        `[rustAppHost] host generation ${generation} exited unexpectedly with code ${code}; giving up after ${decision.attempts} automatic restarts`,
      );
      this.effects.notifyConnectionState({ state: "failed", attempts: decision.attempts });
      return;
    }
    this.phase = "restart-scheduled";
    this.effects.logger.warn(
      `[rustAppHost] host generation ${generation} exited unexpectedly with code ${code}; restart ${decision.attempt}/${decision.maxAttempts} in ${decision.delayMs}ms`,
    );
    this.effects.notifyConnectionState({
      state: "restarting",
      attempt: decision.attempt,
      maxAttempts: decision.maxAttempts,
      delayMs: decision.delayMs,
    });
    this.cancelRestart = this.effects.setTimer(decision.delayMs, () => {
      this.cancelRestart = null;
      if (this.phase === "restart-scheduled") this.spawnNext(`restart-${decision.attempt}`);
    });
  }

  /** 等当前代 ready；超时、开始退出时得到 false。 */
  waitUntilReady(timeoutMs: number): Promise<boolean> {
    if (this.phase === "running" && this.ready) return Promise.resolve(true);
    if (this.phase === "disposing" || this.phase === "disposed") return Promise.resolve(false);
    return new Promise<boolean>((resolve) => {
      let cancelTimer: (() => void) | undefined;
      const settle = (ready: boolean) => {
        if (!this.readyWaiters.delete(settle)) return;
        cancelTimer?.();
        resolve(ready);
      };
      this.readyWaiters.add(settle);
      cancelTimer = this.effects.setTimer(timeoutMs, () => settle(false));
    });
  }

  /** App 退出：不再重启；返回是否还有存活的进程需要 dispose。 */
  beginDispose(): boolean {
    if (this.phase === "disposing" || this.phase === "disposed") return false;
    this.cancelRestart?.();
    this.cancelRestart = null;
    this.phase = this.processAlive ? "disposing" : "disposed";
    for (const resolve of Array.from(this.readyWaiters)) resolve(false);
    return this.processAlive;
  }

  private spawnNext(reason: string): void {
    this.generation += 1;
    this.ready = false;
    this.processAlive = false;
    this.readyAttempts.clear();
    this.phase = "running";
    this.effects.logger.info(
      `[rustAppHost] starting host generation ${this.generation} (${reason})`,
    );
    this.effects.spawn(this.generation);
  }

  private issue(windowId: number, entry: WindowEntry): void {
    entry.needsEndpoint = false;
    this.effects.issueEndpoint(windowId, this.generation, entry.pageToken);
  }
}
