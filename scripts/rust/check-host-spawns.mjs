#!/usr/bin/env node
// Host 中已迁移到任务作用域的 crate（docs/specs/rust-u2-gateway.md §3、rust-u2-task-scope.md §2 第 5 条）：
// 非测试代码不得直接启动 tokio 任务，也不得取运行时句柄自行 spawn，任务一律经 zcode-async 的
// Spawner / owned / blocking 启动。其余 crate 迁移后加入 MIGRATED；全部迁移后改由 clippy.toml 的
// disallowed-methods 强制，这个脚本随之删除。
import { resolve } from "node:path";
import { SPAWN, findSpawns } from "./task-spawns.mjs";

const crates = resolve(import.meta.dirname, "../../apps/zcode-host/crates");
const MIGRATED = ["gateway", "host-core", "zcode-host"];
const pattern = new RegExp(`${SPAWN.source}|\\bHandle\\s*::\\s*(?:try_)?current\\b`);

const failures = [];
for (const crate of MIGRATED) {
  for (const file of await findSpawns(resolve(crates, crate), { base: crates, pattern })) {
    failures.push(`${file}: starts a task outside zcode-async (use a Spawner, owned or blocking)`);
  }
}
if (failures.length) {
  for (const failure of failures) console.error(failure);
  process.exitCode = 1;
} else console.log(`Rust Host task spawns (${MIGRATED.join(", ")}): OK`);
