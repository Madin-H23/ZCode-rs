// Rust Host 自动重启策略（docs/specs/rust-host-resilience.md §2.1、§6）。
import assert from "node:assert/strict";
import test from "node:test";
import { RustHostRestartPolicy } from "../src/main/rustHostRestartPolicy.js";

test("restarts with backoff and gives up after three exits within the window", () => {
  const policy = new RustHostRestartPolicy();
  assert.deepEqual(policy.onUnexpectedExit(0), {
    kind: "restart",
    attempt: 1,
    maxAttempts: 3,
    delayMs: 500,
  });
  assert.equal(policy.onUnexpectedExit(1_000).kind, "restart");
  const third = policy.onUnexpectedExit(2_000);
  assert.deepEqual(third, { kind: "restart", attempt: 3, maxAttempts: 3, delayMs: 5_000 });
  assert.deepEqual(policy.onUnexpectedExit(3_000), { kind: "give-up", attempts: 3 });
  // 放弃之后仍在窗口内的退出继续放弃，不会重新开始计数。
  assert.equal(policy.onUnexpectedExit(4_000).kind, "give-up");
});

test("exits older than the window no longer count", () => {
  const policy = new RustHostRestartPolicy();
  policy.onUnexpectedExit(0);
  policy.onUnexpectedExit(10_000);
  policy.onUnexpectedExit(20_000);
  // 第一次退出已过 60 s：窗口内只剩两次，本次是第 3 次。
  assert.deepEqual(policy.onUnexpectedExit(60_000), {
    kind: "restart",
    attempt: 3,
    maxAttempts: 3,
    delayMs: 5_000,
  });
  // 长时间稳定运行后重新从第 1 次开始。
  assert.equal((policy.onUnexpectedExit(500_000) as { attempt: number }).attempt, 1);
});

test("custom limits reuse the last backoff step", () => {
  const policy = new RustHostRestartPolicy({ maxRestarts: 4, backoffMs: [10, 20] });
  const delays = [0, 1, 2, 3].map((at) => {
    const decision = policy.onUnexpectedExit(at);
    return decision.kind === "restart" ? decision.delayMs : -1;
  });
  assert.deepEqual(delays, [10, 20, 20, 20]);
});
