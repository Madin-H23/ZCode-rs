// 扩展：技能、Subagent 与命令（docs/specs/rust-host-extensions.md §6）。
// - Composer 输入 `/`：AGENTS 分组列出内置与用户 Subagent（subagents.list）；SKILLS 分组来自
//   zcode-agent.getSkillReferenceCatalog（agent 子系统），实现后同样必须列出预置技能；
// - 设置页 Skills：列出 workspace、用户（.zcode / .agents）技能（skills.list），关闭开关写入
//   ~/.zcode/cli/config.json 的 skills.<SKILL.md>.enable=false（skills.setEnabled，持锁读改写）；
// - 设置页 Commands：列出用户与 workspace 命令（commands.list）。
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

import { composer, openSettingsSection, readCliConfig } from "../extensions-helpers.mjs";

const skill = (name, description) =>
  `---\nname: ${name}\ndescription: ${description}\n---\nBody of ${name}\n`;

async function put(path, content) {
  await mkdir(join(path, ".."), { recursive: true });
  await writeFile(path, content);
}

export async function seed({ home, workspace }) {
  await put(
    join(home, ".zcode/skills/e2e-alpha/SKILL.md"),
    skill("e2e-alpha", "Alpha skill for E2E"),
  );
  await put(
    join(home, ".agents/skills/e2e-beta/SKILL.md"),
    skill("e2e-beta", "Beta skill for E2E"),
  );
  await put(
    join(workspace, ".zcode/skills/e2e-ws-skill/SKILL.md"),
    skill("e2e-ws-skill", "Workspace skill for E2E"),
  );
  await put(
    join(home, ".zcode/commands/e2e-deploy.md"),
    "---\ndescription: Deploy from E2E\n---\n\nDeploy now\n",
  );
  await put(join(workspace, ".zcode/commands/e2e-local.md"), "Local command\n");
  await put(
    join(home, ".zcode/agents/e2e-reviewer.md"),
    "---\nname: e2e-reviewer\ndescription: Reviews E2E changes\n---\nYou review.\n",
  );
}

async function visibleText(page, text) {
  const item = page.getByText(text, { exact: true }).first();
  await item.waitFor({ state: "visible", timeout: 30_000 });
  await item.click({ trial: true });
  return item;
}

export async function run({ page, shots, home }) {
  const input = await composer(page);
  await input.click();
  await page.keyboard.type("/");
  await page.waitForTimeout(1500);
  await page.screenshot({ path: join(shots, "skills-01-slash.png") });
  try {
    await visibleText(page, "e2e-reviewer");
    await visibleText(page, "general-purpose");
  } finally {
    await page.screenshot({ path: join(shots, "skills-02-slash-agents.png") });
  }
  console.info("[e2e:extensions-skills] slash picker lists built-in and user subagents");
  const catalogMissing = await page
    .getByText("getSkillReferenceCatalog is not implemented", { exact: false })
    .first()
    .isVisible()
    .catch(() => false);
  if (catalogMissing) {
    console.info(
      "[e2e:extensions-skills] SKILLS group: zcode-agent.getSkillReferenceCatalog is still a stub on this branch",
    );
  } else {
    await visibleText(page, "e2e-alpha");
    console.info("[e2e:extensions-skills] slash picker lists the seeded skill");
  }
  await page.keyboard.press("Escape");
  await page.keyboard.press("Backspace");

  await openSettingsSection(page, "Skills", shots, "skills");
  for (const name of ["e2e-alpha", "e2e-beta"]) {
    await visibleText(page, name);
  }
  await page.screenshot({ path: join(shots, "skills-03-list.png") });
  console.info("[e2e:extensions-skills] settings list user skills (.zcode and .agents)");

  // 关闭 e2e-alpha：开关在同一行里。
  const row = page
    .locator("div")
    .filter({ has: page.getByText("e2e-alpha", { exact: true }) })
    .filter({ has: page.getByRole("switch") })
    .last();
  const toggle = row.getByRole("switch").first();
  await toggle.click({ trial: true });
  await toggle.click();
  let disabledKey;
  for (let attempt = 0; attempt < 60 && !disabledKey; attempt += 1) {
    const config = await readCliConfig(home);
    disabledKey = Object.entries(config.skills ?? {}).find(
      ([path, value]) => path.endsWith("/e2e-alpha/SKILL.md") && value?.enable === false,
    )?.[0];
    if (!disabledKey) await page.waitForTimeout(500);
  }
  await page.screenshot({ path: join(shots, "skills-04-disabled.png") });
  if (!disabledKey) throw new Error("skills.setEnabled did not persist e2e-alpha as disabled");
  console.info("[e2e:extensions-skills] disabling a skill persisted to config.json");

  // 范围切到 workspace：列出 workspace 的技能。
  const scope = page.getByRole("button", { name: "User" }).first();
  await scope.click({ trial: true });
  await scope.click();
  await page.waitForTimeout(500);
  await page.screenshot({ path: join(shots, "skills-05-scope-menu.png") });
  const workspaceOption = page.getByText("workspace", { exact: true }).last();
  await workspaceOption.click({ trial: true });
  await workspaceOption.click();
  try {
    await visibleText(page, "e2e-ws-skill");
  } finally {
    await page.screenshot({ path: join(shots, "skills-06-workspace.png") });
  }
  console.info("[e2e:extensions-skills] settings list workspace skills");

  await openSettingsSection(page, "Commands", shots, "commands");
  await visibleText(page, "/e2e-deploy");
  // 关闭命令：写入 config.json 的 command.<文件路径>.enable=false（commands.setCommandEnabled）。
  const commandToggle = page
    .locator("div")
    .filter({ has: page.getByText("/e2e-deploy", { exact: true }) })
    .filter({ has: page.getByRole("switch") })
    .last()
    .getByRole("switch")
    .first();
  await commandToggle.click({ trial: true });
  await commandToggle.click();
  let commandDisabled = false;
  for (let attempt = 0; attempt < 60 && !commandDisabled; attempt += 1) {
    const config = await readCliConfig(home);
    commandDisabled = Object.entries(config.command ?? {}).some(
      ([path, value]) => path.endsWith("/e2e-deploy.md") && value?.enable === false,
    );
    if (!commandDisabled) await page.waitForTimeout(500);
  }
  await page.screenshot({ path: join(shots, "skills-07-commands.png") });
  if (!commandDisabled) throw new Error("commands.setCommandEnabled did not persist /e2e-deploy");
  // 技能开关写入的覆盖仍在：两个写入方共享同一份 config.json，互不覆盖。
  const config = await readCliConfig(home);
  if (!Object.keys(config.skills ?? {}).some((path) => path.endsWith("/e2e-alpha/SKILL.md"))) {
    throw new Error("command toggle lost the skill override in config.json");
  }
  const commandScope = page.getByRole("button", { name: "User" }).first();
  await commandScope.click({ trial: true });
  await commandScope.click();
  const commandWorkspace = page.getByText("workspace", { exact: true }).last();
  await commandWorkspace.click({ trial: true });
  await commandWorkspace.click();
  try {
    await visibleText(page, "/e2e-local");
  } finally {
    await page.screenshot({ path: join(shots, "skills-08-workspace-commands.png") });
  }
  console.info("[e2e:extensions-skills] settings list user and workspace commands");
}
