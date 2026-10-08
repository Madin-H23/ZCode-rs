// settings-sync 手动导入的对拍 fixture（docs/specs/rust-host-integration.md §2.2）。
// 在临时沙盒里按 TREE 建一棵外部 Agent 来源树，用真实 TS 服务（createSettingsSyncService）依次执行 OPS
// （detect / importSelected），记录每步结果与最终 `~/.zcode`、`<workspace>/.zcode` 的落盘状态；
// 沙盒根路径统一替换成 `<ROOT>`。Rust 侧 settings_sync/parity_tests.rs 用同一棵树逐项复现。
// 用法：pnpm exec tsx scripts/rust-host/settings-sync-fixtures.ts [--check]
import {
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  readlink,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { format } from "oxfmt";
import type { ISettingService } from "../../packages/services/src/setting/setting.js";
import { createSettingsSyncService } from "../../packages/services/src/settings-sync/settingsSyncService.js";

const root = resolve(import.meta.dirname, "../..");
const fixturePath = resolve(
  root,
  "apps/zcode-host/crates/services/src/settings_sync/fixtures/parity.json",
);

const json = (value: unknown) => JSON.stringify(value);

/** 来源树：路径相对沙盒根，内容里的 `<ROOT>` 换成沙盒根。 */
export const TREE: Record<string, string> = {
  "home/.claude/skills/alpha/SKILL.md": "---\nname: Alpha\nversion: 1.0\n---\nbody\n",
  "home/.claude/skills/beta/SKILL.md": '---\r\nname: "Beta"\r\n---\r\n',
  "home/.claude/skills/nested/deep/gamma/SKILL.md": "no frontmatter\n",
  "home/.claude/skills/node_modules/x/SKILL.md": "---\nname: excluded\n---\n",
  "home/.claude/skills/broken/SKILL.md": "---\nname: [bad\nversion: '2'\n---\n",
  "home/.claude/skills/num/SKILL.md": "---\nname: 42\nversion: 1.5\n---\n",
  "home/.codex/skills/alpha2/SKILL.md": "---\nname: alpha\n---\n",
  "home/.codex/skills/Zeta/SKILL.md": "---\nversion: 3\n---\nzeta body\n",
  "home/.codex/skills/Zeta/notes/extra.txt": "extra\n",
  "ws/.claude/skills/proj-skill/SKILL.md": "---\nname: Proj Skill\ndescription: x\n---\n",
  "ws/skills/openclaw-skill/SKILL.md": "---\nname: claw\n---\n",
  "home/.zcode/skills/existing/SKILL.md": "---\nname: beta\n---\n",
  "home/.claude/commands/review.md":
    "---\ndescription: Review code\nargument-hint: <file>\n---\nReview $ARGUMENTS\n",
  "home/.claude/commands/git/commit.md": "Commit everything\n",
  "home/.claude/commands/.hidden/x.md": "hidden\n",
  "home/.claude/commands/README.MD": "readme\n",
  "home/.claude/commands/notes.txt": "not a command\n",
  "home/.codex/commands/deploy.md": "---\ndescription: |\n  multi\n  line\n---\nDeploy\n",
  "ws/.claude/commands/local.md": "Local\n",
  "home/.zcode/commands/review.md": "existing\n",
  "home/.claude/plugins/p1/.claude-plugin/plugin.json": json({
    name: "plugin-one",
    version: " 1.2.0 ",
  }),
  "home/.claude/plugins/p1/skills/s/SKILL.md": "plugin skill\n",
  "home/.claude/plugins/p2/.zcode-plugin/plugin.json": json({ name: " " }),
  "home/.claude/plugins/p3/.codex-plugin/plugin.json": json({ name: "three" }),
  "home/.claude/plugins/no-manifest/readme.txt": "x\n",
  "home/.claude/plugins/.hidden/.claude-plugin/plugin.json": json({ name: "hidden" }),
  "home/somewhere/three-copy/.claude-plugin/plugin.json": json({ name: "Three" }),
  "ws/.codex/plugins/wp/.claude-plugin/plugin.json": json({ name: "ws-plugin" }),
  "home/.zcode/cli/config.json": `${JSON.stringify(
    { theme: "dark", plugins: { dirs: ["<ROOT>/home/somewhere/three-copy", 7] } },
    null,
    2,
  )}\n`,
  "home/.claude/settings.json": json({
    mcpServers: {
      github: { command: "npx", args: ["-y", "gh"], timeout: 5, env: { A: "1" } },
      " ": { command: "blank" },
      bad: 1,
    },
    other: true,
  }),
  "ws/.mcp.json": json({ mcpServers: { "local-db": { type: "stdio", command: "db" } } }),
  "home/.codex/config.toml":
    '[mcp_servers.search]\ncommand = "search"\nargs = ["--x"]\nstartup_timeout_sec = 10\n\n[mcp_servers.alpha]\nurl = "http://127.0.0.1:1"\n',
  "home/.config/opencode/opencode.json": json({
    mcp: {
      oc: { type: "local", command: ["node", "srv.js", "--port"], enabled: true },
      remote1: { type: "remote", url: "https://example.invalid" },
      loc2: { type: "local", command: " run " },
      plain: { command: ["x"], type: "stdio" },
    },
  }),
  "home/.agents/mcp.json": json({ mcpServers: { GitHub: { command: "other" } } }),
  "ws/.zcode/config.json": json({ mcp: { servers: { "local-db": { command: "old" } } } }),
};

type Op = Record<string, unknown>;
const WS = "<ROOT>/ws";
const detect = (categories: string[], extra: Op = {}): Op => ({
  op: "detect",
  request: { workspacePath: WS, intent: "manualImport", categories, ...extra },
});
const importSelected = (selections: Op[], extra: Op = { workspacePath: WS }): Op => ({
  op: "importSelected",
  request: { ...extra, selections },
});

export const OPS: Op[] = [
  detect(["skills"]),
  detect(["commands"]),
  detect(["plugins"]),
  detect(["mcpServers"]),
  detect(["skills"], { workspacePath: undefined }),
  detect(["skills"], { intent: "firstRun" }),
  detect([]),
  detect(["providers"]),
  detect(["mcpServers", "commands"]),
  importSelected([
    {
      agent: "claudeCode",
      category: "skills",
      sourceScope: "global",
      targetScope: "global",
      importMode: "symlink",
      skillPaths: ["<ROOT>/home/.claude/skills/alpha", "<ROOT>/home/.claude/skills/beta"],
    },
    { agent: "codexCli", category: "skills", targetScope: "global", importMode: "copy" },
    {
      agent: "claudeCode",
      category: "skills",
      sourceScope: "project",
      targetScope: "project",
    },
    { agent: "openClaw", category: "skills", importMode: "copy" },
  ]),
  importSelected(
    [{ agent: "claudeCode", category: "skills", sourceScope: "global", targetScope: "project" }],
    {},
  ),
  importSelected([
    { agent: "claudeCode", category: "commands", importMode: "symlink" },
    { agent: "codexCli", category: "commands", targetScope: "project", importMode: "copy" },
  ]),
  importSelected([
    { agent: "claudeCode", category: "plugins", importMode: "symlink" },
    { agent: "claudeCode", category: "plugins", importMode: "symlink" },
    { agent: "codexCli", category: "plugins", targetScope: "project", importMode: "copy" },
  ]),
  importSelected([
    { agent: "claudeCode", category: "mcpServers" },
    { agent: "agents", category: "mcpServers" },
    { agent: "codexCli", category: "mcpServers", targetScope: "project" },
    { agent: "openCode", category: "mcpServers", mcpServerPaths: [] },
    { agent: "openCode", category: "mcpServers" },
    { agent: "claudeCode", category: "providers" },
  ]),
  detect(["skills"]),
  detect(["commands"]),
  detect(["plugins"]),
  detect(["mcpServers"]),
];

function substitute(value: unknown, rootDir: string): unknown {
  return JSON.parse(JSON.stringify(value ?? null).replaceAll("<ROOT>", rootDir));
}

async function dumpTree(base: string, rootDir: string): Promise<unknown[]> {
  const entries: unknown[] = [];
  async function visit(dir: string): Promise<void> {
    let names: string[];
    try {
      names = (await readdir(dir)).sort();
    } catch {
      return;
    }
    for (const name of names) {
      const path = join(dir, name);
      const stat = await lstat(path);
      const rel = relative(rootDir, path);
      if (stat.isSymbolicLink()) {
        entries.push({ path: rel, type: "symlink", target: await readlink(path) });
      } else if (stat.isDirectory()) {
        entries.push({ path: rel, type: "dir" });
        await visit(path);
      } else {
        entries.push({ path: rel, type: "file", content: await readFile(path, "utf8") });
      }
    }
  }
  await visit(base);
  return entries;
}

async function runScenario(rootDir: string) {
  for (const [path, content] of Object.entries(TREE)) {
    const target = join(rootDir, path);
    await mkdir(dirname(target), { recursive: true });
    await writeFile(target, content.replaceAll("<ROOT>", rootDir));
  }
  const savedHome = process.env.HOME;
  const savedProfile = process.env.USERPROFILE;
  process.env.HOME = join(rootDir, "home");
  process.env.USERPROFILE = join(rootDir, "home");
  try {
    const service = createSettingsSyncService({ settingService: {} as ISettingService });
    const results: unknown[] = [];
    for (const op of OPS) {
      const request = substitute(op.request, rootDir) as never;
      const result =
        op.op === "detect" ? await service.detect(request) : await service.importSelected(request);
      results.push(result);
    }
    const tree = [
      ...(await dumpTree(join(rootDir, "home/.zcode"), rootDir)),
      ...(await dumpTree(join(rootDir, "ws/.zcode"), rootDir)),
    ];
    return JSON.parse(JSON.stringify({ results, tree }).replaceAll(rootDir, "<ROOT>")) as Record<
      string,
      unknown
    >;
  } finally {
    process.env.HOME = savedHome;
    process.env.USERPROFILE = savedProfile;
  }
}

const dir = await mkdtemp(join(tmpdir(), "zcode-settings-sync-fixtures-"));
try {
  const scenario = await runScenario(dir);
  const content = `${JSON.stringify(
    {
      source: "scripts/rust-host/settings-sync-fixtures.ts",
      sourceTree: TREE,
      ops: OPS,
      ...scenario,
    },
    null,
    2,
  )}\n`;
  const formatted = await format(fixturePath, content);
  if (formatted.errors.length) throw new Error("Cannot format parity.json");
  if (process.argv.includes("--check")) {
    const current = await readFile(fixturePath, "utf8").catch(() => "");
    if (current !== formatted.code) {
      console.error(
        "[settings-sync-fixtures] parity.json is stale; run pnpm exec tsx scripts/rust-host/settings-sync-fixtures.ts",
      );
      process.exitCode = 1;
    } else {
      console.log(`[settings-sync-fixtures] up to date: ${OPS.length} ops`);
    }
  } else {
    await mkdir(dirname(fixturePath), { recursive: true });
    await writeFile(fixturePath, formatted.code);
    console.log(`[settings-sync-fixtures] wrote: ${OPS.length} ops`);
  }
} finally {
  await rm(dir, { recursive: true, force: true });
}
