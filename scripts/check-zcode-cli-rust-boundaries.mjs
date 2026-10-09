import { readdir, readFile } from "node:fs/promises";
import { resolve, relative } from "node:path";
import { findSpawns } from "./rust/task-spawns.mjs";

const app = resolve(import.meta.dirname, "../apps/zcode-cli-rust");
const root = resolve(app, "crates");
const failures = [];

// 任务只经 zcode-async 启动（docs/specs/rust-u2-task-scope.md §2 第 5 条、§4.1）。clippy.toml 由所有
// Rust workspace 共用、Host 尚未迁移，runtime 先由这里扫描非测试代码强制。
const SPAWN_EXCEPTIONS = new Set([
  // 实例任务本身（Host 需要 JoinHandle）与嵌入方进程级的日志清理；Host 传入自己的作用域后去掉。
  "src/embed.rs",
]);

async function checkSpawns(directory) {
  for (const file of await findSpawns(directory, { base: app, exceptions: SPAWN_EXCEPTIONS })) {
    failures.push(`${file}: starts a task outside zcode-async (use a Spawner, owned or blocking)`);
  }
}

async function walk(directory) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) {
      await walk(path);
      continue;
    }
    if (!path.endsWith(".rs")) continue;
    const source = await readFile(path, "utf8");
    const file = relative(root, path).replaceAll("\\", "/");
    const crate = file.split("/", 1)[0];
    const code = source.replace(/\/\/[^\n]*/g, "");
    if (source.split("\n").length > 400) failures.push(`${file}: exceeds 400 lines`);
    if (
      crate === "domain" &&
      /\b(?:tokio|reqwest|rusqlite|zcode_cli_(?:state|model|tools|host))\s*::|\bstd\s*::\s*(?:fs|process|net)\b/.test(
        code,
      )
    )
      failures.push(`${file}: domain imports runtime or IO`);
    if (
      crate === "protocol" &&
      /\bzcode_cli_(?:core|core_api|state|model|tools|host)\s*::/.test(code)
    )
      failures.push(`${file}: protocol imports core or adapters`);
    if (
      crate === "core" &&
      /\bzcode_cli_(?:state|model|tools|host|app_server|headless|tui)\s*::/.test(code)
    )
      failures.push(`${file}: core imports an adapter or frontend`);
    // 前端只经传输契约（core-api/protocol/domain）与 runtime 交互，不能持有 core 内部状态。
    if (
      ["tui", "app-server", "headless"].includes(crate) &&
      /\bzcode_cli_(?:core|state|model|tools|host)\s*::/.test(code)
    )
      failures.push(`${file}: frontend imports core or an adapter`);
    // 网络出口是最底层基础设施：不依赖任何内部 crate，前端也不直接发起网络请求。
    if (crate === "net" && /\bzcode_cli_\w+\s*::/.test(code))
      failures.push(`${file}: net imports an internal crate`);
    if (["tui", "app-server", "headless"].includes(crate) && /\bzcode_cli_net\s*::/.test(code))
      failures.push(`${file}: frontend imports network egress`);
    if (
      ["state", "model", "tools", "host", "net", "plugins"].includes(crate) &&
      /\bzcode_cli_(?:core|app_server|headless|tui)\s*::/.test(code)
    )
      failures.push(`${file}: adapter imports core or frontend`);
  }
}
await walk(root);
for (const directory of ["crates", "src"]) await checkSpawns(resolve(app, directory));
if (failures.length) {
  for (const failure of failures) console.error(failure);
  process.exitCode = 1;
} else console.log("Rust workspace boundaries: OK");
