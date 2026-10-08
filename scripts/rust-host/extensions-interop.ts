// Rust Host 扩展服务与 TS 实现的跨进程互操作辅助（docs/specs/rust-host-extensions.md §3、§6）。
// 由 apps/zcode-host/crates/services/tests/extensions_interop.rs 启动；所有路径都在测试给的临时目录内。
//   race <home> <count> <prefix>        用 TS 服务交替写 config.json（命令开关、MCP upsert）
//   export-skills <home> <out>          用 TS skill-sync 导出全部用户技能归档
//   extract-skills <archive> <dir>      用 TS 解包 skill 归档
//   extract-plugins <archive> <dir>     用 TS 解包 plugin 归档
import { readFile, writeFile } from "node:fs/promises";

const [command, ...args] = process.argv.slice(2);

function useHome(home: string) {
  // 服务在模块加载时读取 HOME，必须先设置再动态导入。
  process.env.HOME = home;
  process.env.USERPROFILE = home;
  process.env.ZCODE_DATA_BASE_DIR = home;
}

if (command === "race") {
  const [home, count, prefix] = args as [string, string, string];
  useHome(home);
  const { createCommandsService } =
    await import("../../packages/services/src/commands/commandsService.js");
  const { createMcpSyncService } =
    await import("../../packages/services/src/mcp-sync/mcpSyncService.js");
  const commands = createCommandsService();
  const mcp = createMcpSyncService();
  // 模块加载完再通知对方开始，保证两个进程的写入真正交错。
  await writeFile(`${home}/.race-ready`, "1");
  for (let index = 0; index < Number(count); index += 1) {
    if (index % 2 === 0) {
      await commands.setCommandEnabled({
        commandId: "race",
        filePath: `${prefix}-${index}`,
        enabled: false,
      });
    } else {
      await mcp.saveMcpToUserDirectory({
        action: "upsert",
        name: `${prefix}-${index}`,
        config: { command: "x" },
      } as never);
    }
  }
} else if (command === "export-skills") {
  const [home, out] = args as [string, string];
  useHome(home);
  const { createSkillSyncService } =
    await import("../../packages/services/src/skill-sync/skillSyncService.js");
  const service = createSkillSyncService();
  const { candidates } = await service.listLocalUserSkillCandidates();
  const exported = await service.exportSkillsArchive({ skillIds: candidates.map((c) => c.id) });
  await writeFile(out, exported.archive);
} else if (command === "extract-skills") {
  const [archive, dir] = args as [string, string];
  const { extractSkillSyncArchive } =
    await import("../../packages/services/src/skill-sync/skillSyncArchive.js");
  await extractSkillSyncArchive(await readFile(archive), dir, { maxExtractedBytes: 1 << 20 });
} else if (command === "extract-plugins") {
  const [archive, dir] = args as [string, string];
  const { extractPluginSyncArchive } =
    await import("../../packages/services/src/plugin-sync/pluginSyncArchive.js");
  await extractPluginSyncArchive(await readFile(archive), dir, { maxExtractedBytes: 1 << 20 });
} else {
  console.error(`unknown command: ${command}`);
  process.exitCode = 2;
}
