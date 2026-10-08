// 跨区域收尾：settings-sync 手动导入（docs/specs/rust-host-integration.md §2.2）。
// 设置页 Skills → 更多操作 → Import：对话框经 settings-sync.detect（categories:["skills"]）列出
// Claude Code 的全局技能；导入到 Global（默认 symlink）后经 settings-sync.importSelected 在
// ~/.zcode/skills 下建指向来源的软链，结果页显示 Imported，技能列表随之出现该技能。
import { lstat, mkdir, readlink, writeFile } from "node:fs/promises";
import { join } from "node:path";

import { openSettingsSection } from "../extensions-helpers.mjs";

const SKILL = "e2e-imported-skill";

export async function seed({ home }) {
  const dir = join(home, ".claude/skills", SKILL);
  await mkdir(dir, { recursive: true });
  await writeFile(
    join(dir, "SKILL.md"),
    `---\nname: ${SKILL}\ndescription: Imported from Claude Code in E2E\n---\nBody\n`,
  );
}

async function clickVisible(locator) {
  await locator.waitFor({ state: "visible", timeout: 30_000 });
  await locator.click({ trial: true });
  await locator.click();
}

export async function run({ page, shots, home }) {
  await openSettingsSection(page, "Skills", shots, "settings-sync");
  await clickVisible(page.getByRole("button", { name: "More actions", exact: true }).first());
  await clickVisible(page.locator('[data-settings-import-action="settings.skills.import.open"]'));
  const dialog = page.getByRole("dialog").last();
  try {
    // detect 的结果：Claude Code 来源下的技能（skills 默认勾选可导入项）。
    await dialog
      .getByText(SKILL, { exact: false })
      .first()
      .waitFor({ state: "visible", timeout: 30_000 });
  } catch {
    // 来源默认折叠时展开（“Show skills”）。
    const expand = dialog.getByRole("button", { name: "Show skills" }).first();
    if (await expand.isVisible().catch(() => false)) await clickVisible(expand);
  } finally {
    await page.screenshot({ path: join(shots, "settings-sync-02-dialog.png") });
  }
  await dialog
    .getByText(SKILL, { exact: false })
    .first()
    .waitFor({ state: "visible", timeout: 30_000 });
  console.info("[e2e:integration-settings-sync] detect lists the Claude Code skill");

  // 勾选可导入的技能（列表不预选），再导入到 Global。
  await clickVisible(dialog.getByRole("checkbox", { name: "Select all", exact: true }).first());
  await dialog.getByText("1/1 selected", { exact: true }).first().waitFor({ state: "visible" });

  const importButton = dialog
    .getByRole("button", { name: "Import to Global", exact: true })
    .first();
  await clickVisible(importButton);
  const done = dialog.getByRole("button", { name: "Done", exact: true }).first();
  try {
    await done.waitFor({ state: "visible", timeout: 30_000 });
    await dialog.getByText("Imported", { exact: false }).first().waitFor({ state: "visible" });
  } finally {
    await page.screenshot({ path: join(shots, "settings-sync-03-result.png") });
  }
  console.info("[e2e:integration-settings-sync] importSelected reports the skill as imported");

  const target = join(home, ".zcode/skills", SKILL);
  const stat = await lstat(target);
  if (!stat.isSymbolicLink()) throw new Error(`${target} is not a symlink`);
  const link = await readlink(target);
  if (link !== join(home, ".claude/skills", SKILL)) {
    throw new Error(`unexpected symlink target ${link}`);
  }
  console.info("[e2e:integration-settings-sync] ~/.zcode/skills holds a symlink to the source");

  await clickVisible(done);
  const listed = page.getByText(SKILL, { exact: true }).first();
  try {
    await listed.waitFor({ state: "visible", timeout: 30_000 });
    await listed.click({ trial: true });
  } finally {
    await page.screenshot({ path: join(shots, "settings-sync-04-list.png") });
  }
  console.info("[e2e:integration-settings-sync] skills list shows the imported skill");
}
