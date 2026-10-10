// 合并二进制的 CLI 入口（docs/specs/rust-host-cli-boundary.md §3.4）：`zcode-host` 在做任何 Host 初始化
// 之前分派——可执行文件名是 `zcode-cli-rust`，或第一个参数是 `cli` 时，作为 CLI 运行。
//
// harness 用它做两件事：
//   1. 每次 Rust Host 运行前检查两种 CLI 入口（`zcode-host cli --version` 与以 `zcode-cli-rust` 为名的
//      符号链接 `--version`）输出 CLI 的版本、退出码 0、stderr 为空；
//   2. `--runtime child --cli merged` 时，Host 为每个 workspace 启动的 CLI 子进程就是这个符号链接。
import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";

const run = promisify(execFile);

/** 本仓库调试构建的 Host 二进制（与 Main 缺省使用的是同一个）。 */
export function hostBinary(root) {
  return join(root, "apps/zcode-host/target/debug/zcode-host");
}

/** 独立 CLI 的版本（apps/zcode-cli-rust/Cargo.toml 的 `[workspace.package] version`）。 */
async function cliVersion(root) {
  const manifest = await readFile(join(root, "apps/zcode-cli-rust/Cargo.toml"), "utf8");
  const section = manifest.slice(manifest.indexOf("[workspace.package]"));
  const version = section.match(/^version = "([^"]+)"/m)?.[1];
  if (!version) throw new Error("apps/zcode-cli-rust/Cargo.toml has no workspace version");
  return version;
}

/** 在临时目录里建一个指向调试构建 Host 的 `zcode-cli-rust` 符号链接；dispose() 删除它。 */
export async function mergedCliLink(root) {
  const dir = await mkdtemp(join(tmpdir(), "zcode-rust-host-e2e-cli-"));
  const path = join(dir, "zcode-cli-rust");
  await symlink(hostBinary(root), path);
  return { path, dispose: () => rm(dir, { recursive: true, force: true }) };
}

/** 两种 CLI 入口都输出 CLI 的版本并以 0 退出。 */
export async function assertCliEntry(root, link) {
  const version = await cliVersion(root);
  const cases = [
    { label: "zcode-host cli --version", file: hostBinary(root), args: ["cli", "--version"] },
    { label: "zcode-cli-rust (symlink) --version", file: link, args: ["--version"] },
  ];
  for (const { label, file, args } of cases) {
    const { stdout, stderr } = await run(file, args, { timeout: 30_000 }).catch((error) => {
      throw new Error(`${label} exited ${error.code}: ${error.stderr ?? error.message}`);
    });
    if (stdout !== `${version}\n` || stderr !== "")
      throw new Error(
        `${label}: expected "${version}\\n" on stdout and no stderr, got ${JSON.stringify({ stdout, stderr })}`,
      );
  }
  console.info(
    `[e2e] CLI entry of the merged binary: \`zcode-host cli --version\` and \`zcode-cli-rust --version\` print ${version}, exit 0`,
  );
}
