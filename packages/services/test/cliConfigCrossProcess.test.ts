import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const workerPath = fileURLToPath(new URL("./fixtures/cliConfigUpdateWorker.ts", import.meta.url));
const rounds = 12;
const workers = [
  { kind: "commands", prefix: "cmd-a" },
  { kind: "commands", prefix: "cmd-b" },
  { kind: "mcp", prefix: "mcp-a" },
  { kind: "mcp", prefix: "mcp-b" },
] as const;

function runWorker(home: string, kind: string, prefix: string, startAt: number): Promise<void> {
  return new Promise((resolve, reject) => {
    const child = spawn(
      process.execPath,
      ["--import", "tsx", workerPath, kind, prefix, String(rounds), String(startAt)],
      {
        env: { ...process.env, HOME: home, USERPROFILE: home, ZCODE_DATA_BASE_DIR: home },
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
      else reject(new Error(`worker ${prefix} exited with ${code}: ${stderr}`));
    });
  });
}

// config.json 同时由 Main、CLI 与 Node / Rust Host 写入；修复前各服务锁外读改写会互相覆盖。
test("concurrent config.json updates from several processes keep every key", async () => {
  const home = await mkdtemp(join(tmpdir(), "zcode-cli-config-cross-process-"));
  try {
    await mkdir(join(home, ".zcode", "cli"), { recursive: true });
    await writeFile(join(home, ".zcode", "cli", "config.json"), '{\n  "model": "keep"\n}\n');
    const startAt = Date.now() + 2_000;
    await Promise.all(
      workers.map((worker) => runWorker(home, worker.kind, worker.prefix, startAt)),
    );

    const persisted = JSON.parse(
      await readFile(join(home, ".zcode", "cli", "config.json"), "utf8"),
    ) as {
      model?: string;
      command?: Record<string, { enable?: boolean }>;
      mcp?: { servers?: Record<string, unknown> };
    };
    assert.equal(persisted.model, "keep");
    const lost: string[] = [];
    for (const worker of workers) {
      for (let round = 0; round < rounds; round += 1) {
        const key = `${worker.prefix}-${round}`;
        const present =
          worker.kind === "commands"
            ? persisted.command?.[key]?.enable === false
            : Boolean(persisted.mcp?.servers?.[key]);
        if (!present) lost.push(key);
      }
    }
    assert.deepEqual(lost, [], `keys overwritten by another process: ${lost.join(", ")}`);
  } finally {
    await rm(home, { recursive: true, force: true });
  }
});
