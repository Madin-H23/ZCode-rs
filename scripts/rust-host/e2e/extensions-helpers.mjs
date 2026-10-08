// 扩展场景（extensions-*）共用的界面辅助：打开设置页某个分区、读取隔离 profile 的 CLI 配置。
import { readFile } from "node:fs/promises";
import { join } from "node:path";

export { composer } from "./helpers.mjs";

/** 从侧栏底部进入设置页，再点左侧导航的分区（按可见文字定位）。 */
export async function openSettingsSection(page, label, shots, prefix) {
  // 已在设置页时侧栏底部按钮变成“Back to workspace”，直接点分区即可。
  const inSettings = await page
    .getByText("Back to workspace", { exact: true })
    .first()
    .isVisible()
    .catch(() => false);
  if (!inSettings) {
    const settings = page.getByRole("button", { name: "Settings", exact: true }).first();
    await settings.waitFor({ state: "visible", timeout: 30_000 });
    await settings.click({ trial: true });
    await settings.click();
  }
  const entry = page.getByRole("button", { name: label, exact: true }).first();
  try {
    await entry.waitFor({ state: "visible", timeout: 30_000 });
  } catch (error) {
    await page.screenshot({ path: join(shots, `${prefix}-settings-missing.png`) });
    throw error;
  }
  await entry.click({ trial: true });
  await entry.click();
  await page.waitForTimeout(1000);
  await page.screenshot({ path: join(shots, `${prefix}-01-section.png`) });
}

export async function readCliConfig(home) {
  try {
    return JSON.parse(await readFile(join(home, ".zcode", "cli", "config.json"), "utf8"));
  } catch {
    return {};
  }
}
