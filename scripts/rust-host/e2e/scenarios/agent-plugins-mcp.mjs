// agent 子系统：插件页与 MCP 设置页（docs/specs/rust-host-agent.md §5.1、rust-cli-architecture.md
// 「Workspace 作用域」）。两个控制面 lane（plugin-management、mcp-status）的 CLI 工作目录都是
// <dataBaseDir>/.zcode/plugin-workspace，真实 workspace 只作为请求参数，两个 lane 同时存活。
// 隔离 profile 预置：
// - 用户级 marketplace 插件（installed_plugins.json + plugins.enabledPlugins）；
// - workspace 项目配置 <workspace>/.zcode/config.json：一个 inline 插件目录，以及一个 stdio MCP
//   server（本地 node 脚本，只回应 initialize / tools/list，不联网）。
// 断言：插件页列出这两个插件（plugin-management lane 按真实 workspace 读到项目插件）；MCP 页的
// server 行由 mcp-status lane 探测为 connected、工具数 1。
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

import { openSettingsSection } from "../extensions-helpers.mjs";
import { sseText } from "../helpers.mjs";

const STORE_PLUGIN = "e2e-store-plugin";
const STORE_PLUGIN_ID = `${STORE_PLUGIN}@e2e-market`;
const WORKSPACE_PLUGIN = "e2e-workspace-plugin";
const MCP_SERVER = "e2e-echo";

export const mockRules = [
  {
    name: "default",
    match: () => true,
    respond: ({ response }) => response.end(sseText("ok")),
  },
];

async function put(path, content) {
  await mkdir(join(path, ".."), { recursive: true });
  await writeFile(path, content);
}

/** 最小的 stdio MCP server：一个 echo 工具。 */
const MCP_SCRIPT = `
import { createInterface } from "node:readline";
const reply = (m, result) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, result }) + "\\n");
createInterface({ input: process.stdin }).on("line", (line) => {
  const m = JSON.parse(line);
  if (m.method === "initialize")
    reply(m, { protocolVersion: "2025-11-25", serverInfo: { name: "e2e-echo", version: "1" }, capabilities: { tools: {} } });
  else if (m.method === "server/discover")
    process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, error: { code: -32601, message: "legacy" } }) + "\\n");
  else if (m.method === "tools/list")
    reply(m, { tools: [{ name: "echo", description: "Echo", inputSchema: { type: "object", properties: {} } }] });
});
`;

export async function seed({ home, workspace, temp }) {
  const plugins = join(home, ".zcode", "cli", "plugins");
  const store = join(plugins, "cache", "e2e-market", STORE_PLUGIN, "1.0.0");
  await put(
    join(store, ".zcode-plugin", "plugin.json"),
    JSON.stringify({ name: STORE_PLUGIN, version: "1.0.0", description: "E2E store plugin" }),
  );
  // 已安装记录按 CLI 的 installed_plugins.json v1 格式（缺 version 字段的记录会被当成缺包）。
  await put(
    join(plugins, "installed_plugins.json"),
    JSON.stringify({
      version: 1,
      plugins: [
        {
          id: STORE_PLUGIN_ID,
          name: STORE_PLUGIN,
          marketplace: "e2e-market",
          version: "1.0.0",
          installPath: store,
          installedAt: "2026-09-01T00:00:00.000Z",
          scope: "user",
        },
      ],
    }),
  );
  await put(
    join(home, ".zcode", "cli", "config.json"),
    `${JSON.stringify({ plugins: { enabledPlugins: { [STORE_PLUGIN_ID]: true } } }, null, 2)}\n`,
  );
  const inline = join(temp, "plugins-src", WORKSPACE_PLUGIN);
  await put(
    join(inline, ".zcode-plugin", "plugin.json"),
    JSON.stringify({
      name: WORKSPACE_PLUGIN,
      version: "1.0.0",
      description: "E2E workspace plugin",
    }),
  );
  const script = join(temp, "mcp-echo.mjs");
  await writeFile(script, MCP_SCRIPT);
  await put(
    join(workspace, ".zcode", "config.json"),
    `${JSON.stringify(
      {
        plugins: { dirs: [inline] },
        mcp: {
          servers: { [MCP_SERVER]: { type: "stdio", command: process.execPath, args: [script] } },
        },
      },
      null,
      2,
    )}\n`,
  );
}

async function waitVisible(page, locator, shots, name) {
  try {
    await locator.waitFor({ state: "visible", timeout: 60_000 });
    await locator.click({ trial: true });
  } finally {
    await page.screenshot({ path: join(shots, name) });
  }
}

/** 插件页的作用域菜单：User 或某个 workspace 标签。 */
async function selectScope(page, label, shots, name) {
  const trigger = page.locator('[data-plugin-scope-trigger="true"]').first();
  await trigger.waitFor({ state: "visible", timeout: 30_000 });
  await trigger.click();
  const option = page.getByRole("menuitemradio", { name: label, exact: true }).first();
  await waitVisible(page, option, shots, name);
  await option.click();
}

export async function run({ page, shots }) {
  await openSettingsSection(page, "Plugins", shots, "plugins-mcp");
  // User 作用域：plugin-management lane 的 plugins/list（带真实 workspace、configScope user）。
  await waitVisible(
    page,
    page.getByText(/e2e[- ]store[- ]plugin/i).first(),
    shots,
    "plugins-mcp-02-user-scope.png",
  );
  console.info("[e2e:agent-plugins-mcp] user scope lists the marketplace plugin");

  // workspace 作用域：CLI 的 cwd 是 plugin-workspace，项目插件只能按请求里的 workspace 读到。
  await selectScope(page, "workspace", shots, "plugins-mcp-03-scope-menu.png");
  await waitVisible(
    page,
    page.getByText(/e2e[- ]workspace[- ]plugin/i).first(),
    shots,
    "plugins-mcp-04-workspace-scope.png",
  );
  await waitVisible(
    page,
    page.getByText(/e2e[- ]store[- ]plugin/i).first(),
    shots,
    "plugins-mcp-05-workspace-inherits-user.png",
  );
  console.info("[e2e:agent-plugins-mcp] workspace scope lists the project and marketplace plugins");

  // MCP：workspace 的 server 由 mcp-status lane 按真实 workspace 连接探测。
  const tab = page.getByRole("tab", { name: /^MCP/ }).first();
  await tab.waitFor({ state: "visible", timeout: 30_000 });
  await tab.click();
  const row = page.locator(`[data-testid="mcp-server-row-${MCP_SERVER}"]`).first();
  await waitVisible(page, row, shots, "plugins-mcp-06-mcp-row.png");
  let status = "";
  let tools = "";
  for (let attempt = 0; attempt < 120; attempt += 1) {
    status = (await row.getAttribute("data-mcp-status")) ?? "";
    tools = (await row.getAttribute("data-mcp-tool-count")) ?? "";
    if (status === "connected" && tools === "1") break;
    await page.waitForTimeout(500);
  }
  await page.screenshot({ path: join(shots, "plugins-mcp-07-mcp-status.png") });
  if (status !== "connected" || tools !== "1") {
    throw new Error(
      `MCP server not connected by the mcp-status lane: status=${status} tools=${tools}`,
    );
  }
  console.info("[e2e:agent-plugins-mcp] workspace MCP server connected with 1 tool");
}
