// setting.updateDataBaseDir（docs/specs/rust-host-setting.md §6）：设置页 → General → 数据存储路径。
// “Choose folder” 走 IPlatformService.selectDirectory，即 Main 的 dialog.showOpenDialog。原生对话框
// 无法驱动，所以经 harness 的 Main inspector（context.main()，e2e/main-process.mjs）把 showOpenDialog 换成直接返回隔离 HOME 内的目标目录，
// 其余（草稿、Save、Rust Host 的守卫 → 复制 → 写设置、“需要重启”提示）都走真实界面与真实链路。
// 重启后 Main 按 setting.json 的 dataBaseDir 给 Rust Host 下发新目录，新任务写进新目录的 tasks-index。
import { access, mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

import { openSettingsSection } from "../extensions-helpers.mjs";
import { readTaskRows, sendMessage, sseText } from "../helpers.mjs";

const MARKER = "e2e-data-dir/nested/marker.txt";
const MARKER_CONTENT = "copied by updateDataBaseDir\n";
const REPLY = "Reply after the data directory moved";
const PROMPT = "hello from the new data dir";

export const mockRules = [
  { name: "default", match: () => true, respond: ({ response }) => response.end(sseText(REPLY)) },
];

export async function seed({ home }) {
  const marker = join(home, ".zcode/v2", MARKER);
  await mkdir(join(marker, ".."), { recursive: true });
  await writeFile(marker, MARKER_CONTENT);
}

/** 把 Main 的 dialog.showOpenDialog 换成直接选中 `directory`（selectDirectory 的唯一后端）。 */
async function stubDirectoryDialog(main, directory) {
  const value = await main.evaluate(`(() => {
    const { createRequire } = process.getBuiltinModule("node:module");
    const { dialog } = createRequire(process.cwd() + "/")("electron");
    dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [${JSON.stringify(directory)}] });
    return "stubbed";
  })()`);
  if (value !== "stubbed") throw new Error(`dialog stub not installed: ${value}`);
}

async function clickHittable(locator) {
  await locator.waitFor({ state: "visible", timeout: 30_000 });
  await locator.scrollIntoViewIfNeeded();
  await locator.click({ trial: true });
  await locator.click();
}

async function exists(path) {
  return access(path).then(
    () => true,
    () => false,
  );
}

export async function run({ page, shots, home, relaunch, main }) {
  const target = join(home, "zcode-data-target");
  await mkdir(target, { recursive: true });
  await stubDirectoryDialog(main(), target);
  console.info("[e2e:setting-data-dir] Main directory dialog stubbed");

  await openSettingsSection(page, "General", shots, "data-dir");
  const input = page.getByTestId("settings-data-base-dir-input");
  await input.scrollIntoViewIfNeeded();
  await page.screenshot({ path: join(shots, "data-dir-02-control.png") });

  // Choose folder：对话框返回目标目录，只更新草稿。
  await clickHittable(page.getByTestId("settings-data-base-dir-browse"));
  await page.waitForFunction(
    ([testId, expected]) => document.querySelector(`[data-testid="${testId}"]`)?.value === expected,
    ["settings-data-base-dir-input", target],
    { timeout: 30_000 },
  );
  if (await exists(join(target, ".zcode"))) throw new Error("browsing must not copy anything");
  console.info("[e2e:setting-data-dir] draft shows the chosen directory");

  // Save：Rust Host 守卫 → 复制 .zcode/v2 → 写 setting.json；界面提示需要重启。
  await clickHittable(page.getByTestId("settings-data-base-dir-save"));
  const status = page.locator('[data-testid="settings-data-base-dir-status"][data-state="saved"]');
  try {
    await status.waitFor({ state: "visible", timeout: 60_000 });
    await status.click({ trial: true });
  } finally {
    await page.screenshot({ path: join(shots, "data-dir-03-saved.png") });
  }
  console.info("[e2e:setting-data-dir] UI reports saved, restart required");

  const copied = join(target, ".zcode/v2");
  const marker = await readFile(join(copied, MARKER), "utf8");
  if (marker !== MARKER_CONTENT) throw new Error(`marker not copied: ${marker}`);
  const providerConfig = await readFile(join(copied, "provider_config.json"), "utf8");
  if (providerConfig !== (await readFile(join(home, ".zcode/v2/provider_config.json"), "utf8"))) {
    throw new Error("provider_config.json differs after the copy");
  }
  if (!(await exists(join(copied, "tasks-index.sqlite")))) {
    throw new Error("tasks-index.sqlite was not copied");
  }
  if (await exists(join(copied, "setting.json"))) {
    throw new Error("setting.json must stay at the bootstrap location");
  }
  const settings = JSON.parse(await readFile(join(home, ".zcode/v2/setting.json"), "utf8"));
  if (settings.dataBaseDir !== target) {
    throw new Error(`setting.json dataBaseDir = ${JSON.stringify(settings.dataBaseDir)}`);
  }
  console.info("[e2e:setting-data-dir] .zcode/v2 copied (bootstrap excluded), dataBaseDir saved");

  // 重启：Main 读 dataBaseDir，新的 Rust Host 用新目录；新任务写进新目录的 tasks-index。
  // （mock 也回答标题生成请求，标题会变，所以按 task_id 比较。）
  const existing = new Set(readTaskRows(home).map((row) => row.task_id));
  const next = await relaunch();
  await sendMessage(next, PROMPT);
  try {
    await next.getByText(REPLY).first().waitFor({ state: "visible", timeout: 60_000 });
  } finally {
    await next.screenshot({ path: join(shots, "data-dir-04-after-restart.png") });
  }
  let added = [];
  for (let attempt = 0; attempt < 40 && added.length === 0; attempt += 1) {
    added = readTaskRows(target).filter((row) => !existing.has(row.task_id));
    if (added.length === 0) await sleep(500);
  }
  if (added.length === 0) throw new Error("new task missing from the moved tasks-index");
  const addedIds = new Set(added.map((row) => row.task_id));
  if (readTaskRows(home).some((row) => addedIds.has(row.task_id))) {
    throw new Error("new task was written to the old data directory");
  }
  console.info("[e2e:setting-data-dir] after restart the Rust Host writes to the new data dir");
}
