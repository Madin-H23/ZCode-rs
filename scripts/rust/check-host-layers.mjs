#!/usr/bin/env node
// Rust Host 的 crate 分层（docs/specs/rust-u2-host-structure.md §2、§3 规则 1）：
// 一个 crate 只能依赖层号更小的 Host crate；同层之间互不依赖。Cargo 只能发现环，发现不了
// “第 5 层的三个 crate 互相依赖”这类越层但无环的依赖。新增 crate 或跨层依赖要先改规格和这张表。
import { readdir, readFile } from "node:fs/promises";
import { resolve } from "node:path";

const root = resolve(import.meta.dirname, "../../apps/zcode-host/crates");
// 目录名 → 层号。0–2 是与业务无关的基础（协议、控制消息、RPC 工具）；3 起是规格第 2 节的表。
const LAYERS = {
  rpc: 0,
  control: 1,
  kit: 2,
  base: 3,
  settings: 4,
  providers: 5,
  sessions: 6,
  account: 7,
  extensions: 7,
  workspace: 7,
  automation: 8,
  remote: 9,
  services: 10,
  "host-core": 11,
  "zcode-host": 12,
};

/** `[dependencies]`、`[dev-dependencies]`、`[target.….dependencies]` 等段里出现的包名。 */
function dependencyNames(manifest) {
  const names = new Set();
  let inDependencies = false;
  for (const line of manifest.split("\n")) {
    const section = line.match(/^\s*\[([^\]]+)\]\s*$/);
    if (section) {
      inDependencies = /(^|\.)(dev-|build-)?dependencies$/.test(section[1]);
      continue;
    }
    const entry = inDependencies && line.match(/^\s*([A-Za-z0-9_-]+)\s*(?:\.|=)/);
    if (entry) names.add(entry[1]);
  }
  return names;
}

const failures = [];
const packages = new Map();
for (const entry of await readdir(root, { withFileTypes: true })) {
  if (!entry.isDirectory()) continue;
  const manifest = await readFile(resolve(root, entry.name, "Cargo.toml"), "utf8");
  const name = manifest.match(/^\s*name\s*=\s*"([^"]+)"/m)?.[1];
  if (!name) failures.push(`${entry.name}: package name not found`);
  else packages.set(name, { directory: entry.name, manifest });
  if (!(entry.name in LAYERS)) {
    failures.push(
      `${entry.name}: not in the layer table (docs/specs/rust-u2-host-structure.md §2)`,
    );
  }
}
for (const directory of Object.keys(LAYERS)) {
  if (![...packages.values()].some((item) => item.directory === directory)) {
    failures.push(`${directory}: listed in the layer table but the crate does not exist`);
  }
}
for (const [name, { directory, manifest }] of packages) {
  const layer = LAYERS[directory];
  if (layer === undefined) continue;
  for (const dependency of dependencyNames(manifest)) {
    const target = packages.get(dependency);
    if (!target || dependency === name) continue;
    const targetLayer = LAYERS[target.directory];
    if (targetLayer === undefined || targetLayer < layer) continue;
    failures.push(
      `${directory} (layer ${layer}) depends on ${target.directory} (layer ${targetLayer}): ` +
        "a crate may only depend on lower layers",
    );
  }
}

if (failures.length) {
  for (const failure of failures) console.error(failure);
  process.exitCode = 1;
} else console.log("Rust Host layers: OK");
