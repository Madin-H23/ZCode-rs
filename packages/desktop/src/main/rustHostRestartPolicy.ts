// Rust Host 自动重启的有界策略（docs/specs/rust-host-resilience.md §2.1）。
//
// 纯逻辑，不依赖 Electron：App 级 Host 一个实例（docs/specs/rust-u4-app-host.md §2.8），记录意外
// 退出的时间；任意 windowMs 内最多自动重启 maxRestarts 次，第 n 次之前等待 backoffMs[n-1]。超出后
// 放弃，由用户在任意窗口点 Retry 触发一次重启。

export interface RustHostRestartPolicyOptions {
  maxRestarts?: number;
  windowMs?: number;
  backoffMs?: readonly number[];
}

export type RustHostRestartDecision =
  | { kind: "restart"; attempt: number; maxAttempts: number; delayMs: number }
  | { kind: "give-up"; attempts: number };

export const RUST_HOST_RESTART_DEFAULTS = {
  maxRestarts: 3,
  windowMs: 60_000,
  backoffMs: [500, 2_000, 5_000],
} as const;

export class RustHostRestartPolicy {
  private readonly maxRestarts: number;
  private readonly windowMs: number;
  private readonly backoffMs: readonly number[];
  private exits: number[] = [];

  constructor(options: RustHostRestartPolicyOptions = {}) {
    this.maxRestarts = options.maxRestarts ?? RUST_HOST_RESTART_DEFAULTS.maxRestarts;
    this.windowMs = options.windowMs ?? RUST_HOST_RESTART_DEFAULTS.windowMs;
    this.backoffMs = options.backoffMs ?? RUST_HOST_RESTART_DEFAULTS.backoffMs;
  }

  /** 记录一次意外退出并给出决定；`now` 由调用方注入，便于测试。 */
  onUnexpectedExit(now: number): RustHostRestartDecision {
    this.exits = this.exits.filter((at) => now - at < this.windowMs);
    this.exits.push(now);
    const attempt = this.exits.length;
    if (attempt > this.maxRestarts) return { kind: "give-up", attempts: attempt - 1 };
    const delayMs = this.backoffMs[Math.min(attempt, this.backoffMs.length) - 1] ?? 0;
    return { kind: "restart", attempt, maxAttempts: this.maxRestarts, delayMs };
  }

  /** 用户在失败页点 Retry：之后的意外退出重新从第 1 次自动重启计起（rust-u4-app-host.md §2.8）。 */
  reset(): void {
    this.exits = [];
  }
}
