import { describe, expect, it } from "vitest";
import {
  BROWSER_GUEST_RECOVERY_HEALTHY_MS,
  MAX_BROWSER_GUEST_AUTO_RECOVERIES,
  decideBrowserGuestRecovery,
  isRecoverableBrowserGuestExitReason,
} from "@/embeddedBrowserHelpers.js";

describe("isRecoverableBrowserGuestExitReason", () => {
  it("被 Chromium 异常终止的原因可自动恢复", () => {
    for (const reason of ["abnormal-exit", "killed", "crashed", "oom", "memory-eviction"]) {
      expect(isRecoverableBrowserGuestExitReason(reason)).toBe(true);
    }
  });

  it("launch/integrity 失败、正常退出与未知原因不自动恢复", () => {
    for (const reason of ["launch-failed", "integrity-failure", "normal-exit", "unknown-reason", ""]) {
      expect(isRecoverableBrowserGuestExitReason(reason)).toBe(false);
    }
  });
});

describe("decideBrowserGuestRecovery", () => {
  const now = 1_700_000_000_000;

  it("首次异常退出允许重建并计 1 次", () => {
    const decision = decideBrowserGuestRecovery({
      consecutiveRecoveries: 0,
      lastRecoveryAt: null,
      now,
    });
    expect(decision.allowRebuild).toBe(true);
    expect(decision.consecutiveRecoveries).toBe(1);
  });

  it("连续重建全部放行至上限，达到上限即闸住且不递增", () => {
    let count = 0;
    for (let index = 0; index < MAX_BROWSER_GUEST_AUTO_RECOVERIES; index += 1) {
      const decision = decideBrowserGuestRecovery({
        consecutiveRecoveries: count,
        lastRecoveryAt: now + index * 1_000,
        now: now + index * 1_000 + 500,
      });
      expect(decision.allowRebuild).toBe(true);
      count = decision.consecutiveRecoveries;
    }
    expect(count).toBe(MAX_BROWSER_GUEST_AUTO_RECOVERIES);

    const blocked = decideBrowserGuestRecovery({
      consecutiveRecoveries: count,
      lastRecoveryAt: now + 10_000,
      now: now + 10_500,
    });
    expect(blocked.allowRebuild).toBe(false);
    expect(blocked.consecutiveRecoveries).toBe(count);
  });

  it("换代周期远小于健康时长时，无限循环被确定性闸住", () => {
    // 实测换代周期 1~2 分钟：每轮恢复后 90 秒即再崩，计数只增不清。
    let count = 0;
    let lastRecoveryAt: number | null = null;
    let blockedAt = -1;
    for (let cycle = 0; cycle < 10; cycle += 1) {
      const decision = decideBrowserGuestRecovery({
        consecutiveRecoveries: count,
        lastRecoveryAt,
        now: now + cycle * 90_000,
      });
      if (!decision.allowRebuild) {
        blockedAt = cycle;
        break;
      }
      count = decision.consecutiveRecoveries;
      lastRecoveryAt = now + cycle * 90_000;
    }
    expect(blockedAt).toBe(MAX_BROWSER_GUEST_AUTO_RECOVERIES);
  });

  it("guest 稳定存活超过健康时长后，偶发崩溃不误伤", () => {
    const decision = decideBrowserGuestRecovery({
      consecutiveRecoveries: 2,
      lastRecoveryAt: now - BROWSER_GUEST_RECOVERY_HEALTHY_MS - 1,
      now,
    });
    expect(decision.allowRebuild).toBe(true);
    expect(decision.consecutiveRecoveries).toBe(1);
  });

  it("边界：恰好等于健康时长即清零", () => {
    const decision = decideBrowserGuestRecovery({
      consecutiveRecoveries: 2,
      lastRecoveryAt: now - BROWSER_GUEST_RECOVERY_HEALTHY_MS,
      now,
    });
    expect(decision.allowRebuild).toBe(true);
    expect(decision.consecutiveRecoveries).toBe(1);
  });

  it("未达健康时长继续累计，不清零", () => {
    const decision = decideBrowserGuestRecovery({
      consecutiveRecoveries: 2,
      lastRecoveryAt: now - BROWSER_GUEST_RECOVERY_HEALTHY_MS + 1,
      now,
    });
    expect(decision.allowRebuild).toBe(true);
    expect(decision.consecutiveRecoveries).toBe(3);
  });
});
