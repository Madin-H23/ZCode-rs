// U4：setting.json 的唯一写入方是 Host（docs/specs/rust-u4-app-host.md §2.6、§6）；Node Host 路径
// （--host node）跑同一个场景，验证重构后的 Main 行为不变。
//
// 1. Main 发起的写入：账户菜单 → 界面缩放 → 放大，走 Main 的缩放命令（与快捷键、View 菜单同一入口）。
//    缩放档位落到 setting.json（Rust 开关下经 Host 的 setting-write）；页面的缩放随之变化；
//    冷启动后 Main 从 setting.json 恢复这个档位。
// 2. Main 发起的写入：窗口尺寸（resize 防抖后持久化），冷启动后按保存的尺寸建窗。
// 3. Renderer 发起的写入到达 Main 的派生状态：设置页打开“保持电脑运行”，Main 持有 powerSaveBlocker
//    （Rust 开关下经 Host 的 setting-changed，Node 路径经 SyncAppSettings）；关闭后释放。
import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { promisify } from "node:util";

import { openSettingsSection } from "../extensions-helpers.mjs";
import { sseText } from "../helpers.mjs";
import { hostLogLines, waitFor, waitUsable } from "../resilience-helpers.mjs";

export const mockRules = [
  { name: "default", match: () => true, respond: ({ response }) => response.end(sseText("ok")) },
];

const readSettings = async (home) =>
  JSON.parse(await readFile(join(home, ".zcode", "v2", "setting.json"), "utf8"));

const zoomFactor = (page) => page.evaluate(() => window.devicePixelRatio);

/** 账户菜单里的子菜单项。 */
async function accountMenu(page, submenu, item) {
  await page.getByTestId("login-trigger").first().click();
  await page.getByText(submenu, { exact: true }).last().hover();
  await page.getByText(submenu, { exact: true }).last().click();
  // 菜单项里还有快捷键标签，按可访问名称（以文字开头）定位。
  const entry = page.getByRole("menuitem", { name: new RegExp(`^${item}`) }).last();
  await entry.waitFor({ state: "visible", timeout: 10_000 });
  await entry.click();
  await page.keyboard.press("Escape").catch(() => {});
}

const WINDOW_SIZE = { width: 1180, height: 760 };

const MAIN_WINDOW_BOUNDS = `(() => {
  const win = require("electron").BrowserWindow.getAllWindows()
    .find((candidate) => !candidate.isDestroyed() && candidate.webContents.getURL().includes("/renderer/index.html"));
  const [width, height] = win.getSize();
  return { width, height, zoomFactor: win.webContents.getZoomFactor() };
})()`;

const RESIZE_MAIN_WINDOW = `(() => {
  const win = require("electron").BrowserWindow.getAllWindows()
    .find((candidate) => !candidate.isDestroyed() && candidate.webContents.getURL().includes("/renderer/index.html"));
  if (win.isMaximized()) win.unmaximize();
  win.setSize(${WINDOW_SIZE.width}, ${WINDOW_SIZE.height});
  return win.getSize();
})()`;

async function powerAssertionHeld(pid) {
  if (process.platform !== "darwin") return null;
  const { stdout } = await promisify(execFile)("pmset", ["-g", "assertions"]);
  return stdout
    .split("\n")
    .some(
      (line) => line.includes(`pid ${pid}(`) && /NoIdleSleep|PreventUserIdleSystemSleep/.test(line),
    );
}

export async function run(context) {
  const { shots, home, mainPid } = context;
  let page = context.page;
  await waitUsable(page);
  const initial = await readSettings(home);
  if (initial.desktopZoomLevel)
    throw new Error(`unexpected seeded zoom ${initial.desktopZoomLevel}`);

  // 1. 缩放命令（Main 发起的写入）。
  const factorBefore = await zoomFactor(page);
  await accountMenu(page, "Interface zoom", "Zoom in");
  const zoomed = await waitFor(
    async () => {
      const settings = await readSettings(home);
      return typeof settings.desktopZoomLevel === "number" && settings.desktopZoomLevel > 0
        ? settings.desktopZoomLevel
        : null;
    },
    { timeout: 15_000, what: "desktopZoomLevel in setting.json" },
  );
  await waitFor(async () => (await zoomFactor(page)) > factorBefore, {
    what: "the page zoom to grow",
  });
  await page.screenshot({ path: join(shots, "01-zoomed.png") });
  console.info(`[e2e] zoom command persisted desktopZoomLevel=${zoomed}; page zoom followed`);

  // 2. 窗口尺寸（Main 在 resize 防抖后写入）。
  await context.main().evaluate(RESIZE_MAIN_WINDOW);
  const savedSize = await waitFor(
    async () => {
      const size = (await readSettings(home)).desktopWindowSize;
      return size?.width === WINDOW_SIZE.width && size?.height === WINDOW_SIZE.height ? size : null;
    },
    { timeout: 15_000, what: "desktopWindowSize in setting.json" },
  );
  console.info(`[e2e] window size persisted ${JSON.stringify(savedSize)}`);

  // 3. Renderer 的设置修改到达 Main 的派生状态。
  await openSettingsSection(page, "General", shots, "settings");
  const keepAwake = page.getByRole("switch", { name: "Keep computer running" }).first();
  await keepAwake.waitFor({ state: "visible", timeout: 30_000 });
  if ((await keepAwake.getAttribute("aria-checked")) === "true")
    throw new Error("keep-awake is already on");
  await keepAwake.click();
  await waitFor(async () => (await readSettings(home)).keepAwakeWhileRunning === true, {
    what: "keepAwakeWhileRunning in setting.json",
  });
  await waitFor(
    async () =>
      (await hostLogLines(home)).some((line) =>
        line.includes("[keep-awake] powerSaveBlocker started"),
      ),
    { timeout: 15_000, what: "Main to start the powerSaveBlocker" },
  );
  const held = await powerAssertionHeld(mainPid());
  if (held === false) throw new Error("no power assertion held by the Electron Main");
  await page.screenshot({ path: join(shots, "02-keep-awake.png") });
  console.info(
    `[e2e] renderer setting reached Main: powerSaveBlocker started${held ? " (pmset shows the assertion)" : ""}`,
  );
  await keepAwake.click();
  await waitFor(
    async () =>
      (await hostLogLines(home)).some((line) =>
        line.includes("[keep-awake] powerSaveBlocker stopped"),
      ),
    { timeout: 15_000, what: "Main to stop the powerSaveBlocker" },
  );
  console.info("[e2e] turning it off released the powerSaveBlocker");

  // 冷启动：Main 从 setting.json 恢复缩放与窗口尺寸（缩放档位 → Chromium 缩放倍率）。
  const beforeRestart = await context.main().evaluate(MAIN_WINDOW_BOUNDS);
  if (!(beforeRestart.zoomFactor > 1))
    throw new Error(`zoom factor not raised: ${beforeRestart.zoomFactor}`);
  page = await context.relaunch();
  await waitUsable(page);
  const restored = await context.main().evaluate(MAIN_WINDOW_BOUNDS);
  if (Math.abs(restored.zoomFactor - beforeRestart.zoomFactor) > 1e-6)
    throw new Error(
      `zoom not restored: factor ${restored.zoomFactor} (before restart ${beforeRestart.zoomFactor}, level ${zoomed})`,
    );
  if (restored.width !== WINDOW_SIZE.width || restored.height !== WINDOW_SIZE.height)
    throw new Error(`window size not restored: ${JSON.stringify(restored)}`);
  await page.screenshot({ path: join(shots, "03-after-restart.png") });
  console.info(
    `[e2e] after a cold restart zoom factor ${restored.zoomFactor} (level ${zoomed}) and size ${restored.width}x${restored.height} came back`,
  );
}
