import { readdir, readFile } from "node:fs/promises";
import { resolve, relative } from "node:path";

const app = resolve(import.meta.dirname, "../apps/zcode-cli-rust");
const root = resolve(app, "crates");
const failures = [];

// 任务只经 zcode-async 启动（docs/specs/rust-u2-task-scope.md §2 第 5 条、§4.1）。clippy.toml 由所有
// Rust workspace 共用、Host 尚未迁移，runtime 先由这里扫描非测试代码强制。
const SPAWN_EXCEPTIONS = new Set([
  // 实例任务本身（Host 需要 JoinHandle）与嵌入方进程级的日志清理；Host 传入自己的作用域后去掉。
  "src/embed.rs",
]);
const SPAWN =
  /\btokio\s*::\s*(?:task\s*::\s*)?(?:spawn|spawn_blocking|spawn_local|JoinSet)\b|\btask\s*::\s*(?:spawn|spawn_blocking|spawn_local)\s*\(|(?<![.\w])(?:spawn_blocking|spawn_local)\s*\(|\bHandle\s*::\s*current\s*\(\s*\)\s*\.\s*spawn/;

/** 测试文件：`tests.rs`、`*_tests.rs`，以及 tests/、examples/、benches/ 目录下的文件。 */
function isTestFile(file) {
  return /(?:^|\/)(?:tests|examples|benches)\//.test(file) || /(?:^|\/|_)tests\.rs$/.test(file);
}

/** 去掉 `#[cfg(test)]` 标注的条目（rustfmt 格式：条目以同一缩进的 `}` 结束）。 */
function withoutTestItems(code) {
  const lines = code.split("\n");
  const kept = [];
  for (let index = 0; index < lines.length; index++) {
    const attribute = /^(\s*)#\[cfg\(test\)\]\s*$/.exec(lines[index]);
    if (!attribute) {
      kept.push(lines[index]);
      continue;
    }
    let item = index + 1;
    while (item < lines.length && /^\s*#\[/.test(lines[item])) item++;
    let end = item;
    if (/\{\s*$/.test(lines[item] ?? "")) {
      while (end < lines.length && lines[end] !== `${attribute[1]}}`) end++;
    }
    index = end;
  }
  return kept.join("\n");
}

async function checkSpawns(directory) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) {
      await checkSpawns(path);
      continue;
    }
    const file = relative(app, path).replaceAll("\\", "/");
    if (!path.endsWith(".rs") || isTestFile(file) || SPAWN_EXCEPTIONS.has(file)) continue;
    const code = withoutTestItems((await readFile(path, "utf8")).replace(/\/\/[^\n]*/g, ""));
    if (SPAWN.test(code))
      failures.push(
        `${file}: starts a task outside zcode-async (use a Spawner, owned or blocking)`,
      );
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
      /\bzcode_cli_(?:state|model|tools|host|delivery|app_server|headless|tui)\s*::/.test(code)
    )
      failures.push(`${file}: core imports an adapter or frontend`);
    // 投递 crate 由前端与 Host 共用（docs/specs/rust-u3-stage-b.md §3.2）：只经传输契约与领域模型工作
    // （它自己的集成测试经公开名 zcode_cli_delivery 引用它）。
    if (
      crate === "delivery" &&
      /\bzcode_cli_(?!core_api\b|domain\b|protocol\b|delivery\b)\w+\s*::/.test(code)
    )
      failures.push(`${file}: delivery imports a crate other than core-api, domain or protocol`);
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
      /\bzcode_cli_(?:core|delivery|app_server|headless|tui)\s*::/.test(code)
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
