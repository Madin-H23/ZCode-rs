// 扩展：设置页 Memory（docs/specs/rust-host-extensions.md §6）。隔离 profile 里预置一个项目
// Memory，设置页经 memory.listProjectMemories 列出 workspace 与文件；磁盘上新增文件后点刷新，
// 列表重新扫描。（界面没有正文预览入口，readProjectMemoryFile 由 Rust 单测与对拍覆盖。）
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

import { composer, openSettingsSection } from "../extensions-helpers.mjs";

const PROJECT = "e2e-repo-0123456789abcdef";
const NOTE = "Remember to run the Rust host parity suite";

export async function seed({ home }) {
  const memory = join(home, ".zcode", "cli", "memories", "projects", PROJECT, "memory");
  await mkdir(memory, { recursive: true });
  await writeFile(join(memory, "MEMORY.md"), "# E2E memory index\n- [note](note.md)\n");
  await writeFile(join(memory, "note.md"), `${NOTE}\n`);
  // Memory 查看器只在开启 Workspace Memory 时加载（setting.json 的 memoryEnabled）。
  const settingPath = join(home, ".zcode", "v2", "setting.json");
  const setting = JSON.parse(await readFile(settingPath, "utf8"));
  await writeFile(settingPath, JSON.stringify({ ...setting, memoryEnabled: true }));
}

export async function run({ page, shots, home }) {
  await composer(page);
  await openSettingsSection(page, "Memory", shots, "memory");
  // 项目 label 由 id 去掉 16 位 hash 得到。
  const project = page.getByText("e2e-repo", { exact: true }).first();
  await project.waitFor({ state: "visible", timeout: 30_000 });
  await project.click({ trial: true });
  for (const name of ["MEMORY.md", "note.md"]) {
    const file = page.getByText(name, { exact: true }).first();
    await file.waitFor({ state: "visible", timeout: 30_000 });
    await file.click({ trial: true });
  }
  await page.getByText("2 memories").first().waitFor({ state: "visible", timeout: 10_000 });
  await page.screenshot({ path: join(shots, "memory-02-catalog.png") });

  // 新增文件后刷新：列表必须重新扫描磁盘，而不是沿用旧快照。
  const memory = join(home, ".zcode", "cli", "memories", "projects", PROJECT, "memory");
  await writeFile(join(memory, "extra.md"), `${NOTE} again\n`);
  const refresh = page.getByRole("button", { name: "Refresh" }).first();
  await refresh.click({ trial: true });
  await refresh.click();
  const extra = page.getByText("extra.md", { exact: true }).first();
  await extra.waitFor({ state: "visible", timeout: 30_000 });
  await extra.click({ trial: true });
  await page.getByText("3 memories").first().waitFor({ state: "visible", timeout: 10_000 });
  await page.screenshot({ path: join(shots, "memory-03-refreshed.png") });
  console.info("[e2e:extensions-memory] project memories listed and refreshed");
}
