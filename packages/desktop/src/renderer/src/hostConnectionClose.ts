// Rust Host 连接断开后的页面状态与重新接入（docs/specs/rust-host-resilience.md §2.1、§2.5）。
//
// 纯逻辑，不依赖 React 与真实计时器：页面状态（断开 / 重新接入 / 多次中断 / Main 的重启进度）由这里
// 唯一持有，`hostConnectionLost.tsx` 只负责渲染与接线。Host 主动关闭连接时 Close 帧带状态码
// （docs/specs/rust-u2-gateway.md §2.6）：只有 4001 `outbound-backlog` 表示 Host 仍在运行，页面 reload
// 后重新接入同一个 Host；其余关闭照旧等待 Main。自动重新接入 60 s 内至多 3 次，次数存在窗口的
// sessionStorage，跨 reload 保留。
import { RUST_HOST_CLOSE_CODES } from "@zcode/shared";
import type { HostConnectionLostState } from "@zcode/ui";

/** 窗口的本地连接（业务服务），或某个远端 workspace 的 scope 连接。 */
export type HostConnectionKind = "local" | "remote-scope";

export interface HostConnectionCloseEvent {
  code: number;
  reason: string;
}

type HostConnectionCloseAction =
  /** Host 仍在运行：等 `delayMs` 后 reload 页面，重新接入。 */
  | { kind: "reattach"; attempt: number; maxAttempts: number; delayMs: number }
  /** 60 s 内自动重新接入的次数已用完（或无法记录次数）：停在“连接多次中断”页。 */
  | { kind: "give-up"; attempts: number }
  /** 按 Host 退出处理：显示“正在重连”，等待 Main。 */
  | { kind: "host-lost" }
  /** 不改变页面。 */
  | { kind: "ignore" };

/** 自动重新接入的计数窗口与第 n 次之前的等待（与 Host 自动重启的数值相同）。 */
export const HOST_REATTACH_WINDOW_MS = 60_000;
const HOST_REATTACH_DELAYS_MS: readonly number[] = [500, 2_000, 5_000];

const CODE_NAMES = new Map<number, string>(
  Object.entries(RUST_HOST_CLOSE_CODES).map(([name, code]): [number, string] => [code, name]),
);

/** 日志用：状态码与 reason（Host 给的原因只用于诊断，判断只看状态码）。 */
function describeHostConnectionClose(close: HostConnectionCloseEvent): string {
  const name = CODE_NAMES.get(close.code);
  return `${close.code}${name ? ` ${name}` : ""}${close.reason ? ` (${close.reason})` : ""}`;
}

/**
 * 连上之后的一次关闭 → 动作。`history` 是此前自动重新接入的时间（ms），null 表示存储不可用；
 * 返回的 `history` 是动作为 reattach 时要存回的记录（已去掉窗口外的旧记录、加上这一次）。
 */
export function decideHostConnectionClose(input: {
  close: HostConnectionCloseEvent;
  connection: HostConnectionKind;
  now: number;
  history: readonly number[] | null;
}): { action: HostConnectionCloseAction; history: number[] | null } {
  if (input.close.code !== RUST_HOST_CLOSE_CODES.outboundBacklog) {
    // 远端 scope 的断开与重连由 Main 的远端会话管理器负责；本地连接按 Host 退出处理（今天的行为）。
    return {
      action: input.connection === "local" ? { kind: "host-lost" } : { kind: "ignore" },
      history: null,
    };
  }
  const maxAttempts = HOST_REATTACH_DELAYS_MS.length;
  // 记录不可用时不能保证不循环：直接停在“连接多次中断”页，由用户决定。
  if (!input.history) return { action: { kind: "give-up", attempts: 0 }, history: null };
  const recent = input.history.filter(
    (at) => at <= input.now && input.now - at < HOST_REATTACH_WINDOW_MS,
  );
  if (recent.length >= maxAttempts)
    return { action: { kind: "give-up", attempts: recent.length }, history: null };
  const attempt = recent.length + 1;
  return {
    action: {
      kind: "reattach",
      attempt,
      maxAttempts,
      delayMs: HOST_REATTACH_DELAYS_MS[attempt - 1]!,
    },
    history: [...recent, input.now],
  };
}

/** 自动重新接入的记录（每个窗口一份，跨 reload 保留）。读写失败时 read 返回 null、write 返回 false。 */
export interface HostReattachHistory {
  read(): number[] | null;
  write(history: readonly number[]): boolean;
  clear(): void;
}

const HISTORY_KEY = "zcode.hostReattachHistory";

/** 存在 `sessionStorage`：每个窗口一份，随窗口关闭消失。访问本身可能抛错（被禁用的存储）。 */
export function createSessionReattachHistory(
  storage: () => Pick<Storage, "getItem" | "setItem" | "removeItem">,
): HostReattachHistory {
  return {
    read() {
      try {
        const raw = storage().getItem(HISTORY_KEY);
        if (raw === null) return [];
        const parsed: unknown = JSON.parse(raw);
        return Array.isArray(parsed)
          ? parsed.filter((at): at is number => typeof at === "number" && Number.isFinite(at))
          : [];
      } catch {
        return null;
      }
    },
    write(history) {
      try {
        storage().setItem(HISTORY_KEY, JSON.stringify(history));
        return true;
      } catch {
        return false;
      }
    },
    clear() {
      try {
        storage().removeItem(HISTORY_KEY);
      } catch {
        // 清不掉时下次读到的仍是旧记录，最多多停一次在“连接多次中断”页
      }
    },
  };
}

interface HostConnectionLostEffects {
  history: HostReattachHistory;
  now(): number;
  setTimer(delayMs: number, fire: () => void): () => void;
  reload(): void;
  render(state: HostConnectionLostState): void;
  warn(message: string): void;
}

/** 页面的断开状态：只消费连接关闭与 Main 的重启进度，一旦进入就取代业务界面。 */
export class HostConnectionLostController {
  private current: HostConnectionLostState | null = null;
  private cancelReattach: (() => void) | null = null;

  constructor(private readonly effects: HostConnectionLostEffects) {}

  get state(): HostConnectionLostState | null {
    return this.current;
  }

  /** 连上之后某条连接关闭。页面只按第一次进入断开状态的关闭决定一次。 */
  connectionClosed(close: HostConnectionCloseEvent, connection: HostConnectionKind): void {
    if (this.current) return;
    const { action, history } = decideHostConnectionClose({
      close,
      connection,
      now: this.effects.now(),
      history: this.effects.history.read(),
    });
    const described = describeHostConnectionClose(close);
    switch (action.kind) {
      case "ignore":
        return;
      case "host-lost":
        this.show({ state: "reconnecting" });
        return;
      case "give-up":
        this.effects.warn(
          `[host-connection] ${connection} connection closed by the Host: ${described}; automatic reattach exhausted (${action.attempts} in ${HOST_REATTACH_WINDOW_MS} ms)`,
        );
        this.show({ state: "closed" });
        return;
      case "reattach":
        // 记不下这一次就不能保证不循环：按次数已用完处理。
        if (!history || !this.effects.history.write(history)) {
          this.effects.warn(
            `[host-connection] ${connection} connection closed by the Host: ${described}; cannot record the reattach, not reloading`,
          );
          this.show({ state: "closed" });
          return;
        }
        this.effects.warn(
          `[host-connection] ${connection} connection closed by the Host: ${described}; reattaching (${action.attempt}/${action.maxAttempts}) in ${action.delayMs} ms`,
        );
        this.show({
          state: "reattaching",
          attempt: action.attempt,
          maxAttempts: action.maxAttempts,
        });
        this.cancelReattach = this.effects.setTimer(action.delayMs, () => {
          this.cancelReattach = null;
          this.effects.reload();
        });
    }
  }

  /** Main 的重启进度（Host 进程退出了）：取代重新接入，计划中的 reload 取消，由 Main 负责之后的 reload。 */
  hostRestartState(next: HostConnectionLostState): void {
    // 已放弃自动重启的结论不会被迟到的“正在重启”覆盖。
    if (this.current?.state === "failed") return;
    this.cancelReattach?.();
    this.cancelReattach = null;
    this.show(next);
  }

  /** 用户点“重新加载”：重新开始计数。 */
  reloadByUser(): void {
    this.effects.history.clear();
    this.effects.reload();
  }

  private show(next: HostConnectionLostState): void {
    this.current = next;
    this.effects.render(next);
  }
}
