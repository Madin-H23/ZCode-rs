// 扩展：插件贡献的资源（docs/specs/rust-host-extensions.md §6）。隔离 profile 里预置一个从
// marketplace 安装并启用的插件（installed_plugins.json + plugins.enabledPlugins）：
// - Composer `/` 与设置页 Subagents 列出插件 agent（subagents.list 的插件发现），为它选模型写入
//   agents-state.json 的 pluginAgentModelSelectionOverrides（setPluginAgentModelOverride）；
// - 插件页、以及技能 / 命令页里的插件分组按 plugin-management（agent 子系统，CLI 的 plugin lane）的
//   已安装列表聚合：这里只确认页面可用并记录列表结果，插件技能 / 命令的发现由对拍测试覆盖。
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

import { composer, openSettingsSection } from "../extensions-helpers.mjs";

const PLUGIN_ID = "e2e-plugin@e2e-market";

async function put(path, content) {
  await mkdir(join(path, ".."), { recursive: true });
  await writeFile(path, content);
}

export async function seed({ home }) {
  const plugins = join(home, ".zcode", "cli", "plugins");
  const root = join(plugins, "cache", "e2e-market", "e2e-plugin", "1.0.0");
  await put(
    join(root, ".claude-plugin", "plugin.json"),
    JSON.stringify({ name: "e2e-plugin", version: "1.0.0", description: "E2E plugin" }),
  );
  await put(
    join(root, "skills", "e2e-plugin-skill", "SKILL.md"),
    "---\nname: e2e-plugin-skill\ndescription: Skill shipped by the E2E plugin\n---\nBody\n",
  );
  await put(
    join(root, "commands", "e2e-plugin-cmd.md"),
    "---\ndescription: Command shipped by the E2E plugin\n---\nRun it\n",
  );
  await put(
    join(root, "agents", "e2e-plugin-agent.md"),
    "---\nname: e2e-plugin-agent\ndescription: Agent shipped by the E2E plugin\n---\nYou help.\n",
  );
  await put(
    join(plugins, "installed_plugins.json"),
    JSON.stringify({
      plugins: [
        {
          id: PLUGIN_ID,
          marketplace: "e2e-market",
          name: "e2e-plugin",
          installPath: root,
          scope: "user",
        },
      ],
    }),
  );
  await put(
    join(home, ".zcode", "cli", "config.json"),
    `${JSON.stringify({ plugins: { enabledPlugins: { [PLUGIN_ID]: true } } }, null, 2)}\n`,
  );
}

async function visibleText(page, text, options = { exact: true }) {
  const item = page.getByText(text, options).first();
  await item.waitFor({ state: "visible", timeout: 30_000 });
  await item.click({ trial: true });
}

export async function run({ page, shots, home }) {
  // Composer `/` 的 AGENTS 分组来自 subagents.list：插件 agent 以裸名别名出现（唯一且不冲突）。
  const input = await composer(page);
  await input.click();
  await page.keyboard.type("/e2e-plugin");
  try {
    await visibleText(page, "e2e-plugin-agent");
  } finally {
    await page.screenshot({ path: join(shots, "plugins-01-slash-agent.png") });
  }
  console.info("[e2e:extensions-plugins] plugin subagent offered in the slash picker");
  await page.keyboard.press("Escape");
  for (let index = 0; index < "/e2e-plugin".length; index += 1) {
    await page.keyboard.press("Backspace");
  }

  await openSettingsSection(page, "Subagents", shots, "plugin-subagents");
  await visibleText(page, "E2e Plugin");
  await visibleText(page, "e2e-plugin-agent");
  console.info("[e2e:extensions-plugins] plugin subagent listed in its plugin group");
  const row = page
    .locator("div")
    .filter({ has: page.getByText("e2e-plugin-agent", { exact: true }) })
    .filter({ has: page.getByText("Inherit default", { exact: true }) })
    .last();
  const picker = row.getByText("Inherit default", { exact: true }).first();
  await picker.click({ trial: true });
  await picker.click();
  await page.waitForTimeout(500);
  await page.screenshot({ path: join(shots, "plugins-02-model-menu.png") });
  // 模型菜单按 provider 分组：先展开 Mock 子菜单再选模型。
  const provider = page.getByText("Mock", { exact: true }).last();
  await provider.hover();
  await page.waitForTimeout(800);
  await page.screenshot({ path: join(shots, "plugins-02b-submenu.png") });
  // 子菜单在指针移动时会重新定位（Playwright 判为不稳定）：用键盘进入子菜单并选第一项。
  const option = page.locator('span[title="mock-model"]').first();
  await option.waitFor({ state: "visible", timeout: 10_000 });
  await page.keyboard.press("ArrowRight");
  await page.keyboard.press("Enter");
  const statePath = join(home, ".zcode", "v2", "agents-state.json");
  let override;
  for (let attempt = 0; attempt < 60 && !override; attempt += 1) {
    try {
      const state = JSON.parse(await readFile(statePath, "utf8"));
      override =
        state.pluginAgentModelSelectionOverrides?.["plugin:e2e-plugin@e2e-market:e2e-plugin-agent"];
    } catch {
      // 还没写入
    }
    if (!override) await page.waitForTimeout(500);
  }
  await page.screenshot({ path: join(shots, "plugins-03-model-override.png") });
  if (override?.modelId !== "mock-model") {
    throw new Error(`plugin subagent override not persisted: ${JSON.stringify(override)}`);
  }
  console.info("[e2e:extensions-plugins] plugin subagent model override persisted");

  await openSettingsSection(page, "Plugins", shots, "plugin-page");
  const heading = page.getByRole("button", { name: "Browse plugins" }).first();
  await heading.waitFor({ state: "visible", timeout: 30_000 });
  await heading.click({ trial: true });
  await page.waitForTimeout(1500);
  await page.screenshot({ path: join(shots, "plugins-04-plugins-page.png") });
  const listed = await page
    .getByText("e2e-plugin", { exact: true })
    .first()
    .isVisible()
    .catch(() => false);
  console.info(
    `[e2e:extensions-plugins] Plugins page rendered; plugin-management lists the seeded plugin: ${listed}`,
  );
}
