// 跨进程写 ~/.zcode/cli/config.json 的子进程：等到统一起跑时间后，用真实服务写入自己独占的键
// （命令开关或 MCP server）。父测试据此检查其他进程的整文件写回有没有覆盖掉这些键。
import { setTimeout as sleep } from "node:timers/promises";

const [kind, prefix, roundsArg, startAtArg] = process.argv.slice(2);
const rounds = Number(roundsArg);
const startAt = Number(startAtArg);
if (!kind || !prefix || !Number.isInteger(rounds) || rounds < 1 || !Number.isFinite(startAt)) {
  throw new Error("usage: cliConfigUpdateWorker <commands|mcp> <prefix> <rounds> <startAtMs>");
}

// 服务在模块加载时读取 HOME，父进程已经把 HOME 指向临时目录。
const { createCommandsService } = await import("../../src/commands/commandsService.js");
const { createMcpSyncService } = await import("../../src/mcp-sync/mcpSyncService.js");
const commands = createCommandsService();
const mcp = createMcpSyncService();

await sleep(Math.max(0, startAt - Date.now()));
for (let round = 0; round < rounds; round += 1) {
  const key = `${prefix}-${round}`;
  if (kind === "commands") {
    await commands.setCommandEnabled({ commandId: key, filePath: key, enabled: false });
  } else {
    await mcp.saveMcpToUserDirectory({ action: "upsert", name: key, config: { command: "x" } });
  }
}
