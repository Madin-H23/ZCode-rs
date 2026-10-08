import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const workerPath = fileURLToPath(new URL("./fixtures/settingUpdateWorker.ts", import.meta.url));

// 每个进程独占一个字段；任何一个最终不是 true，都说明被别的进程用旧快照整文件写回覆盖了。
const fields = [
  "terminalInheritSystemProfile",
  "embeddedBrowserAllowInsecureCertificates",
  "computerUseComposerEntryHidden",
  "taskAutoArchiveEnabled",
  "keepAwakeWhileRunning",
  "messageStreamShowTodos",
] as const;
const rounds = 8;

function runWorker(home: string, field: string, startAt: number): Promise<void> {
  return new Promise((resolve, reject) => {
    const child = spawn(
      process.execPath,
      ["--import", "tsx", workerPath, field, String(rounds), String(startAt)],
      {
        env: { ...process.env, HOME: home, USERPROFILE: home, ZCODE_DESKTOP_HOME_DIR: home },
        stdio: ["ignore", "ignore", "pipe"],
      },
    );
    let stderr = "";
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk: string) => {
      stderr += chunk;
    });
    child.on("error", reject);
    child.on("exit", (code) => {
      if (code === 0) resolve();
      else reject(new Error(`worker ${field} exited with ${code}: ${stderr}`));
    });
  });
}

test("concurrent setting updates from several processes keep every field", async () => {
  const home = await mkdtemp(join(tmpdir(), "zcode-setting-cross-process-"));
  try {
    // 子进程加载 tsx 需要时间，统一起跑点保证各进程的读改写真正重叠。
    const startAt = Date.now() + 2_000;
    await Promise.all(fields.map((field) => runWorker(home, field, startAt)));

    const persisted = JSON.parse(
      await readFile(join(home, ".zcode", "v2", "setting.json"), "utf8"),
    ) as Record<string, unknown>;
    const lost = fields.filter((field) => persisted[field] !== true);
    assert.deepEqual(lost, [], `fields overwritten by another process: ${lost.join(", ")}`);
  } finally {
    await rm(home, { recursive: true, force: true });
  }
});
