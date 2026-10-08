// Rust Host 扩展服务（docs/specs/rust-host-extensions.md §6）的对拍场景。
// extensions-fixtures.ts 用真实 TS 服务在临时目录里执行这些调用，把输入与结果一起写进
// fixtures/extensions-parity.json；Rust 测试在同样的文件树上复现并逐项比较。
//
// 约定：路径与文件内容里的 `${ROOT}` 替换为临时根（已 realpath），`${HOME}` = `${ROOT}/home`，
// `${WS}` = `${ROOT}/ws`。参数里的字符串 `$ref:<调用序号>:<JSON pointer>` 取之前某次调用的原始结果，
// `$find:<调用序号>:<数组 pointer>:<字段>=<值>:<元素内 pointer>` 在数组里按字段查找元素后取值。

export interface ScenarioFile {
  path: string;
  content?: string;
  symlink?: string;
}

export interface ScenarioCall {
  channel: string;
  method: string;
  params?: unknown;
}

const skill = (name: string, description: string, body = "Body") =>
  `---\nname: ${name}\ndescription: ${description}\n---\n${body}\n`;

const agent = (name: string, extra = "") =>
  `---\nname: ${name}\ndescription: ${name} agent\n${extra}---\nYou are ${name}.\n`;

export const FILES: ScenarioFile[] = [
  // 用户级技能
  { path: "home/.zcode/skills/alpha/SKILL.md", content: skill("alpha", "Alpha skill") },
  {
    path: "home/.zcode/skills/group/beta/SKILL.md",
    content: "---\r\nname: beta\r\ndescription: 触发场景: 写代码\r\n---\r\nBeta body\r\n",
  },
  { path: "home/.zcode/skills/gamma/SKILL.md", content: "No frontmatter here\n" },
  {
    path: "home/.zcode/skills/gamma/_meta.json",
    content: '{"slug":" g ","version":"1.2","ownerId":"","publishedAt":5}',
  },
  { path: "home/.zcode/skills/node_modules/hidden/SKILL.md", content: skill("hidden", "x") },
  { path: "home/.agents/skills/alpha/SKILL.md", content: skill("alpha", "Shadowed") },
  {
    path: "home/.agents/skills/delta/SKILL.md",
    content: "---\nname: delta\ndescription: >\n  Folded\n  text\n---\n# Title\nDelta\n",
  },
  { path: "home/.agents/skills/linked", symlink: "${HOME}/linked-target" },
  { path: "home/linked-target/SKILL.md", content: skill("linked", "Linked skill") },
  // inline 插件（plugins.dirs）
  {
    path: "home/plugins/inline-one/.zcode-plugin/plugin.json",
    content: '{"name":"inline-one","version":" 1.0.0 ","description":"Inline plugin"}',
  },
  {
    path: "home/plugins/inline-one/skills/p-skill/SKILL.md",
    content: skill("p-skill", "Plugin skill"),
  },
  {
    path: "home/plugins/inline-one/commands/plug.md",
    content: "---\ndescription: Plugin command\n---\nRun plug\n",
  },
  { path: "home/plugins/inline-one/agents/helper.md", content: agent("helper") },
  // marketplace 安装的插件
  {
    path: "home/.zcode/cli/plugins/installed_plugins.json",
    content: JSON.stringify({
      plugins: [
        {
          id: "mk@market",
          marketplace: "market",
          name: "mk",
          installPath: "${HOME}/market-plugins/mk",
          scope: "user",
        },
      ],
    }),
  },
  {
    path: "home/market-plugins/mk/.claude-plugin/plugin.json",
    content: '{"name":"mk","skills":"./custom-skills","agents":["./extra"]}',
  },
  { path: "home/market-plugins/mk/custom-skills/m1/SKILL.md", content: skill("m1", "Market") },
  {
    path: "home/market-plugins/mk/extra/reviewer.md",
    content: agent("reviewer", "model: openai/gpt-5$high\ntools: Read, Grep\n"),
  },
  { path: "home/market-plugins/mk/commands/mk-cmd.md", content: "Market command\n" },
  // 本地 marketplace 源（镜像导出）
  {
    path: "home/local-market/marketplace.json",
    content: JSON.stringify({
      name: "local-market",
      description: "Local",
      metadata: { pluginRoot: "plugins" },
      plugins: [
        { name: "a", source: "./a", dependencies: ["b@local-market"] },
        { name: "b", version: "1" },
      ],
    }),
  },
  {
    path: "home/local-market/plugins/a/.zcode-plugin/plugin.json",
    content: '{"name":"a"}',
  },
  { path: "home/local-market/plugins/a/skills/a1/SKILL.md", content: skill("a1", "A") },
  {
    path: "home/local-market/plugins/b/.zcode-plugin/plugin.json",
    content: '{"name":"b"}',
  },
  // 命令
  {
    path: "home/.zcode/commands/deploy.md",
    content: "---\ndescription: Deploy it\n  carefully\nother: keep\n---\n\nDeploy now\n",
  },
  { path: "home/.zcode/commands/ns/sub.md", content: "Nested\n" },
  { path: "home/.agents/commands/agents-only.md", content: "Agents user command\n" },
  // Subagents（含旧 Provider 写法，list 前迁移）
  {
    path: "home/.zcode/agents/writer.md",
    content: agent(
      "writer",
      "model: builtin:zai/glm-4.6 # legacy\nthoughtLevel: high\ncolor: green\n",
    ),
  },
  {
    path: "home/.zcode/v2/agents-state.json",
    content: JSON.stringify({
      builtInModelOverrides: { Explore: "builtin:bigmodel/glm-5" },
      disabledAgentIds: ["user:user:writer", "user:user:ghost"],
    }),
  },
  // Project Memory
  {
    path: "home/.zcode/cli/memories/projects/repo-0123456789abcdef/memory/MEMORY.md",
    content: "# Index\n",
  },
  {
    path: "home/.zcode/cli/memories/projects/repo-0123456789abcdef/memory/note.md",
    content: "note",
  },
  { path: "home/.zcode/cli/memories/projects/repo-0123456789abcdef/memory/x.txt", content: "x" },
  // 旧格式 hooks（只读）
  {
    path: "home/.agents/settings.json",
    content: JSON.stringify({
      hooks: { Stop: [{ hooks: [{ type: "command", command: "echo legacy", timeoutMs: 2500 }] }] },
    }),
  },
  // 用户级 CLI 配置
  {
    path: "home/.zcode/cli/config.json",
    content: JSON.stringify(
      {
        model: "keep-me",
        plugins: {
          dirs: ["${HOME}/plugins/inline-one"],
          enabledPlugins: { "mk@market": true },
        },
        mcp: {
          servers: {
            fs: { command: "npx", args: ["-y", "@modelcontextprotocol/server-filesystem", "/x"] },
            legacy: { command: "legacy", enable: false },
          },
        },
        hooks: {
          events: {
            UserPromptSubmit: [{ hooks: [{ type: "command", command: "echo user" }] }],
          },
        },
      },
      null,
      2,
    ),
  },
  // workspace
  { path: "ws/.git/HEAD", content: "ref: refs/heads/main\n" },
  { path: "ws/.zcode/skills/ws-skill/SKILL.md", content: skill("ws-skill", "Workspace skill") },
  {
    path: "ws/.agents/skills/ws-agents-skill/SKILL.md",
    content: skill("ws-agents-skill", "Agents workspace skill"),
  },
  { path: "ws/.zcode/commands/local.md", content: "Local command\n" },
  { path: "ws/.agents/commands/ignored.md", content: "Ignored\n" },
  { path: "ws/.zcode/agents/ws-agent.md", content: agent("ws-agent", "maxTurns: 3\n") },
  {
    path: "ws/.zcode/config.json",
    content: JSON.stringify({
      hooks: {
        enabled: true,
        events: {
          PreToolUse: [
            {
              matcher: "Bash",
              hooks: [{ type: "command", command: "echo pre", timeout: 5, note: "x" }],
            },
          ],
          Stop: [
            { hooks: [{ type: "process", command: "node", args: ["a.js"], timeoutMs: 1500 }] },
          ],
        },
      },
      mcp: { servers: { wsmcp: { command: "y" } } },
    }),
  },
  {
    path: "ws/zcode.json",
    content: JSON.stringify({
      hooks: {
        events: {
          SessionStart: [{ hooks: [{ type: "command", command: "echo start", shell: "bash" }] }],
        },
      },
    }),
  },
];

const WS = "${WS}";
const HOME = "${HOME}";

export const CALLS: ScenarioCall[] = [
  /* 0 */ { channel: "skills", method: "list", params: { workspacePath: WS } },
  /* 1 */ {
    channel: "skills",
    method: "setEnabled",
    params: { workspacePath: WS, skillId: "$find:0:/skills:name=delta:/id", enabled: false },
  },
  /* 2 */ { channel: "skills", method: "list", params: { workspacePath: WS } },
  /* 3 */ {
    channel: "skills",
    method: "buildPromptContext",
    params: { workspacePath: WS, prompt: "use $alpha and $delta and $nope" },
  },
  /* 4 */ {
    channel: "skills",
    method: "copyToCommon",
    params: { workspacePath: WS, skillId: "$find:0:/skills:name=ws-agents-skill:/id" },
  },
  /* 5 */ {
    channel: "skills",
    method: "removeFromCommon",
    params: { workspacePath: WS, skillId: "$find:0:/skills:name=alpha:/id" },
  },
  /* 6 */ {
    channel: "skills",
    method: "deleteSkill",
    params: { workspacePath: WS, skillId: "$find:0:/skills:name=p-skill:/id" },
  },
  /* 7 */ {
    channel: "skills",
    method: "deleteSkill",
    params: { workspacePath: WS, skillId: "missing" },
  },
  /* 8 */ { channel: "skills", method: "list", params: { workspacePath: WS } },
  /* 9 */ { channel: "commands", method: "list", params: { workspacePath: WS } },
  /* 10 */ {
    channel: "commands",
    method: "writeCommandFile",
    params: {
      config: { name: "/new-cmd", prompt: "Do new", description: " New one ", argumentHint: "<x>" },
    },
  },
  /* 11 */ {
    channel: "commands",
    method: "setCommandEnabled",
    params: { commandId: "c", filePath: `${HOME}/.zcode/commands/deploy.md`, enabled: false },
  },
  /* 12 */ {
    channel: "commands",
    method: "updateCommandFile",
    params: {
      commandId: "c",
      oldFilePath: `${HOME}/.zcode/commands/deploy.md`,
      config: { name: "/deploy2", prompt: "Deploy v2", description: "D2" },
    },
  },
  /* 13 */ {
    channel: "commands",
    method: "deleteCommandFile",
    params: { commandId: "c", filePath: `${HOME}/.zcode/commands/new-cmd.md` },
  },
  /* 14 */ {
    channel: "commands",
    method: "writeCommandFile",
    params: { config: { name: "/local", prompt: "x" }, storageLevel: "project", workspacePath: WS },
  },
  /* 15 */ { channel: "commands", method: "list", params: { workspacePath: WS } },
  /* 16 */ { channel: "commands", method: "getPrimaryUserCommandsDirectory", params: {} },
  /* 17 */ { channel: "memory", method: "listProjectMemories" },
  /* 18 */ {
    channel: "memory",
    method: "readProjectMemoryFile",
    params: { workspaceId: "repo-0123456789abcdef", fileName: "MEMORY.md" },
  },
  /* 19 */ {
    channel: "memory",
    method: "readProjectMemoryFile",
    params: { workspaceId: "..", fileName: "MEMORY.md" },
  },
  /* 20 */ { channel: "plugins", method: "getOverview", params: { workspacePath: WS } },
  /* 21 */ { channel: "plugins", method: "installPlugin", params: { workspacePath: WS } },
  /* 22 */ { channel: "hooks", method: "loadHooks", params: { workspacePath: WS } },
  /* 23 */ {
    channel: "hooks",
    method: "saveHooks",
    params: { workspacePath: WS, hooks: "$ref:22:/hooks" },
  },
  /* 24 */ { channel: "hooks", method: "loadHooks", params: { workspacePath: WS } },
  /* 25 */ {
    channel: "mcp-sync",
    method: "loadMcpFromUserDirectory",
    params: { workspacePath: WS },
  },
  /* 26 */ {
    channel: "mcp-sync",
    method: "saveMcpToUserDirectory",
    params: { action: "set-enabled", name: "fs", enabled: false },
  },
  /* 27 */ {
    channel: "mcp-sync",
    method: "saveMcpToUserDirectory",
    params: {
      action: "upsert",
      name: "newsrv",
      config: { type: "http", url: "http://127.0.0.1:9/mcp" },
    },
  },
  /* 28 */ {
    channel: "mcp-sync",
    method: "saveMcpToUserDirectory",
    params: { action: "delete", name: "wsmcp", projectPath: WS },
  },
  /* 29 */ {
    channel: "mcp-sync",
    method: "saveMcpToUserDirectory",
    params: { action: "upsert", name: "nocfg" },
  },
  /* 30 */ { channel: "mcp-sync", method: "listLocalUserMcpCandidates" },
  /* 31 */ {
    channel: "mcp-sync",
    method: "listRemoteUserMcpStatuses",
    params: { names: ["FS", "nope"] },
  },
  /* 32 */ {
    channel: "mcp-sync",
    method: "exportMcpServers",
    params: { serverIds: ["$find:30:/candidates:name=fs:/id"] },
  },
  /* 33 */ {
    channel: "mcp-sync",
    method: "importMcpServers",
    params: {
      servers: [
        {
          name: "imported",
          config: {
            command: "npx",
            args: ["@modelcontextprotocol/server-filesystem", "/local/me/docs"],
          },
          enabled: true,
        },
        { name: "fs", config: { command: "z" }, enabled: true },
      ],
      localHomeDir: "/local/me",
    },
  },
  /* 34 */ { channel: "mcp-sync", method: "checkRemoteUserMcpWriteAccess" },
  /* 35 */ { channel: "subagents", method: "list", params: { workspacePath: WS } },
  /* 36 */ {
    channel: "subagents",
    method: "setEnabled",
    params: { agentId: "user:user:writer", enabled: true },
  },
  /* 37 */ {
    channel: "subagents",
    method: "setBuiltInModelOverride",
    params: {
      agentName: "general-purpose",
      modelSelection: {
        providerId: " openai ",
        modelId: "gpt",
        options: { reasoningLevel: "high" },
      },
    },
  },
  /* 38 */ {
    channel: "subagents",
    method: "setPluginAgentModelOverride",
    params: {
      agentId: "$find:35:/pluginAgents:name=mk:reviewer:/id",
      modelSelection: { providerId: "p", modelId: "m" },
    },
  },
  /* 39 */ {
    channel: "subagents",
    method: "createAgent",
    params: {
      config: {
        name: "new-agent",
        description: " D ",
        systemPrompt: " P ",
        tools: ["Read", "*"],
        color: "green",
        maxTurns: 4,
        mcpServers: [{ name: "srv", command: "x", args: ["-y"] }],
      },
      provider: "glm",
    },
  },
  /* 40 */ {
    channel: "subagents",
    method: "updateAgent",
    params: {
      agentId: "user:user:new-agent",
      oldFilePath: `${HOME}/.zcode/agents/new-agent.md`,
      config: { name: "renamed-agent", description: "D2", systemPrompt: "P2" },
      provider: "glm",
    },
  },
  /* 41 */ {
    channel: "subagents",
    method: "deleteAgent",
    params: {
      agentId: "user:user:renamed-agent",
      filePath: `${HOME}/.zcode/agents/renamed-agent.md`,
    },
  },
  /* 42 */ {
    channel: "subagents",
    method: "createAgent",
    params: { config: { name: "x", description: "d", systemPrompt: "p" }, provider: "glm" },
  },
  /* 43 */ {
    channel: "subagents",
    method: "list",
    params: { workspacePath: WS, mode: "settingsUserOnly" },
  },
  /* 44 */ { channel: "skill-sync", method: "listLocalUserSkillCandidates" },
  /* 45 */ {
    channel: "skill-sync",
    method: "listRemoteUserSkillStatuses",
    params: {
      directoryNames: ["group/beta", "missing"],
      skills: [{ directoryName: "missing", name: "DELTA" }],
    },
  },
  /* 46 */ {
    channel: "skill-sync",
    method: "listRemoteUserSkillStatuses",
    params: { directoryNames: ["../escape"] },
  },
  /* 47 */ {
    channel: "skill-sync",
    method: "exportSkillsArchive",
    params: { skillIds: ["$find:44:/candidates:name=beta:/id"] },
  },
  /* 48 */ {
    channel: "skill-sync",
    method: "importSkillsArchive",
    params: { archive: "$ref:47:/archive" },
  },
  /* 49 */ { channel: "skill-sync", method: "checkRemoteUserSkillWriteAccess" },
  /* 50 */ { channel: "plugin-sync", method: "listLocalUserPluginCandidates" },
  /* 51 */ {
    channel: "plugin-sync",
    method: "exportPluginsArchive",
    params: { pluginIds: ["$ref:50:/candidates/0/id"] },
  },
  /* 52 */ {
    channel: "plugin-sync",
    method: "importPluginsArchive",
    params: { archive: "$ref:51:/archive" },
  },
  /* 53 */ {
    channel: "plugin-sync",
    method: "listRemoteUserPluginStatuses",
    params: {
      plugins: [
        { pluginId: "inline-one@inline", directoryName: "inline-one" },
        { pluginId: "other@inline", directoryName: "other" },
      ],
    },
  },
  /* 54 */ { channel: "plugin-sync", method: "checkRemoteUserPluginWriteAccess" },
  /* 55 */ {
    channel: "plugin-sync",
    method: "exportMarketplaceSourceArchive",
    params: {
      marketplaceId: "local-market",
      pluginNames: ["a"],
      source: { source: "directory", path: `${HOME}/local-market` },
    },
  },
  /* 56 */ {
    channel: "plugin-sync",
    method: "importMarketplaceSourceArchive",
    params: { archive: "$ref:55:/archive" },
  },
];

/** 全部调用结束后比较内容的文件（相对 ROOT）；不存在记为 null。 */
export const DUMPS = [
  "home/.zcode/cli/config.json",
  "ws/.zcode/config.json",
  "home/.zcode/v2/agents-state.json",
  "home/.zcode/agents/writer.md",
  "home/.zcode/commands/deploy.md",
  "home/.zcode/commands/deploy2.md",
  "home/.zcode/commands/new-cmd.md",
  "ws/.zcode/commands/local.md",
  "home/.zcode/v2/skills-audit.log",
  "ws/.zcode/skills/ws-agents-skill/SKILL.md",
  "home/.zcode/skills/alpha/SKILL.md",
];
