// Rust 检查与测试脚本依赖的 cargo 子命令（docs/specs/rust-u0-foundation.md §2）。
// 缺失时直接给出安装命令，不悄悄退回 cargo test 等另一条路径。
import { execFile } from "node:child_process";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);

const INSTALL = {
  nextest: "cargo install --locked cargo-nextest",
  deny: "cargo install --locked cargo-deny",
  // 1.11.0 是仍支持仓库固定工具链（rust-toolchain.toml）的最新版本。
  shear: "cargo install --locked cargo-shear --version 1.11.0",
};

export async function requireCargoTools(names) {
  const missing = [];
  for (const name of names) {
    try {
      await execFileAsync("cargo", [name, "--version"]);
    } catch {
      missing.push(name);
    }
  }
  if (missing.length) {
    const hints = missing.map((name) => `  ${INSTALL[name]}`).join("\n");
    throw new Error(`缺少 cargo 子命令：${missing.join(", ")}。请先安装：\n${hints}`);
  }
}
