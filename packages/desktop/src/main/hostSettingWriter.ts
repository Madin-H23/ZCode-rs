// Main 发起的 setting.json 写入（docs/specs/rust-u4-app-host.md §2.6）。
//
// Rust 开关下 setting.json 的唯一写入方是 Host 的 SettingStore：Main 原来直接写文件的位置都经
// `MainSettingStore.update` 走到这里，发 `setting-write` 并等 `setting-write-result`。Host 不可用
// （启动中、重启中）时最多等 10 s 就绪，超时记 warn 并放弃这次写入，不回落到直接写文件。
// 纯逻辑：Host 的就绪、消息发送与定时器由调用方注入。
import {
  type AppSettings,
  type HostSettingWriteMessage,
  HostMessageTypes,
  formatZodError,
  hostSettingWriteMessageSchema,
} from "@zcode/shared";

/** 等待 Host 就绪与等待写入结果的上限。 */
export const HOST_SETTING_WRITE_TIMEOUT_MS = 10_000;

/**
 * 退出时等在途设置写入结果的上限（§2.6）：退出预算（waitTimeoutMs）先留出 dispose 的强杀等待
 * （forceKillDelayMs），剩下的才给写入，且不超过写入本身的 10 s 上限。
 */
export function settingSettleBudgetMs(policy: {
  forceKillDelayMs?: number;
  waitTimeoutMs?: number;
}): number {
  const budget = (policy.waitTimeoutMs ?? 0) - (policy.forceKillDelayMs ?? 0);
  return Math.max(0, Math.min(HOST_SETTING_WRITE_TIMEOUT_MS, budget));
}

/** Main 读写设置的统一入口：Node 路径是今天的 settingService，Rust 路径读文件、写经 Host。 */
export interface MainSettingStore {
  get(): Promise<AppSettings>;
  update(patch: Partial<AppSettings>): Promise<void>;
}

interface HostSettingWriterPorts {
  waitUntilReady(timeoutMs: number): Promise<boolean>;
  /** 发给当前代 Host；没有存活的 Host 时返回 false。 */
  post(message: HostSettingWriteMessage): boolean;
  newRequestId(): string;
  setTimer(delayMs: number, fire: () => void): () => void;
  logger: { warn: (...args: unknown[]) => void };
}

interface PendingWrite {
  resolve: () => void;
  reject: (error: Error) => void;
  cancelTimer: () => void;
  settled: Promise<void>;
}

/** JSON 会丢掉值为 undefined 的键；线上用 null 表示“清除这个字段”。 */
export function encodeSettingWritePatch(patch: Partial<AppSettings>): Record<string, unknown> {
  return Object.fromEntries(
    Object.entries(patch).map(([key, value]) => [key, value === undefined ? null : value]),
  );
}

export class HostSettingWriter {
  private readonly pending = new Map<string, PendingWrite>();
  private closed = false;

  constructor(private readonly ports: HostSettingWriterPorts) {}

  async write(patch: Partial<AppSettings>): Promise<void> {
    const fields = Object.keys(patch);
    if (fields.length === 0) return;
    if (this.closed) throw this.abandon(fields, "the app is shutting down");
    const parsed = hostSettingWriteMessageSchema.safeParse({
      type: HostMessageTypes.SettingWrite,
      requestId: this.ports.newRequestId(),
      patch: encodeSettingWritePatch(patch),
    });
    if (!parsed.success) {
      throw new Error(`invalid setting-write patch: ${formatZodError(parsed.error)}`);
    }
    const ready = await this.ports.waitUntilReady(HOST_SETTING_WRITE_TIMEOUT_MS);
    if (!ready || this.closed) {
      throw this.abandon(
        fields,
        this.closed
          ? "the app is shutting down"
          : `the host was not ready within ${HOST_SETTING_WRITE_TIMEOUT_MS}ms`,
      );
    }
    const message = parsed.data;
    let resolveWrite!: () => void;
    let rejectWrite!: (error: Error) => void;
    const result = new Promise<void>((resolve, reject) => {
      resolveWrite = resolve;
      rejectWrite = reject;
    });
    const entry: PendingWrite = {
      resolve: resolveWrite,
      reject: rejectWrite,
      cancelTimer: () => {},
      settled: result.then(
        () => {},
        () => {},
      ),
    };
    this.pending.set(message.requestId, entry);
    entry.cancelTimer = this.ports.setTimer(HOST_SETTING_WRITE_TIMEOUT_MS, () => {
      this.settle(
        message.requestId,
        this.abandon(fields, `no setting-write-result within ${HOST_SETTING_WRITE_TIMEOUT_MS}ms`),
      );
    });
    if (!this.ports.post(message)) {
      this.settle(message.requestId, this.abandon(fields, "the host is not running"));
    }
    return result;
  }

  handleResult(result: { requestId: string; ok: boolean; error?: string }): void {
    this.settle(
      result.requestId,
      result.ok ? undefined : new Error(result.error || "setting write failed in the host"),
    );
  }

  /** 当前代退出：在途写入不会再有结果。 */
  hostExited(): void {
    for (const requestId of Array.from(this.pending.keys())) {
      this.settle(requestId, new Error("setting write lost: the host exited"));
    }
  }

  /**
   * App 退出与安装更新：拒绝新写入，等在途写入的结果后再 dispose Host（§2.6）。
   * 最多等 `timeoutMs`（由退出预算决定）；返回在途写入是否都已有结果。超时的写入仍按原有
   * 10 s 上限或 Host 退出得到结果，这里只是不再等它。
   */
  async closeAndSettle(timeoutMs: number): Promise<boolean> {
    this.closed = true;
    if (this.pending.size === 0) return true;
    const settled = Promise.all(Array.from(this.pending.values(), (entry) => entry.settled));
    let cancelTimer = () => {};
    const timedOut = new Promise<false>((resolve) => {
      cancelTimer = this.ports.setTimer(Math.max(0, timeoutMs), () => resolve(false));
    });
    try {
      return await Promise.race([settled.then(() => true as const), timedOut]);
    } finally {
      cancelTimer();
    }
  }

  private settle(requestId: string, error?: Error): void {
    const entry = this.pending.get(requestId);
    if (!entry) return;
    this.pending.delete(requestId);
    entry.cancelTimer();
    if (error) entry.reject(error);
    else entry.resolve();
  }

  private abandon(fields: string[], reason: string): Error {
    this.ports.logger.warn(
      `[setting-write] abandoned a setting write (${fields.join(", ")}): ${reason}`,
    );
    return new Error(`setting write abandoned: ${reason}`);
  }
}
