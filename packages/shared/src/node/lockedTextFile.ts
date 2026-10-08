import { readFile } from "node:fs/promises";
import { atomicWritePrivateTextFile, withFileLock } from "./privateFilePersistence.js";

function isMissingFileError(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    (error as { code?: unknown }).code === "ENOENT"
  );
}

/**
 * 多进程共享文本配置（如 `~/.zcode/cli/config.json`）的读改写。
 *
 * 修复前 skills、commands、mcp-sync、hooks、plugin-sync、Main 的 MCP 用户目录都是不持锁的
 * “读 → 改 → 写”：两个进程（Node Host、Rust Host、Main）交错时，后写者用旧快照覆盖前者，
 * 另一方的修改静默丢失。这里让整个读改写持 `<file>.lock/` 跨进程锁（与 `setting.json` 的 S0
 * 修复、Rust Host `store::with_file_lock` 同一协议），锁内同目录临时文件 + rename（0600）。
 * `mutate` 收到当前文本（文件不存在为 `null`），返回 `null` 表示不写。
 */
export async function updateTextFileLocked(
  filePath: string,
  mutate: (current: string | null) => string | null | Promise<string | null>,
): Promise<void> {
  await withFileLock(filePath, async () => {
    let current: string | null;
    try {
      current = await readFile(filePath, "utf-8");
    } catch (error) {
      if (!isMissingFileError(error)) throw error;
      current = null;
    }
    const next = await mutate(current);
    if (next !== null) {
      await atomicWritePrivateTextFile(filePath, next);
    }
  });
}
