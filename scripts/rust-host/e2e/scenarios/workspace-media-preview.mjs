// 工作区场景：媒体预览（docs/specs/rust-host-workspace.md §2.3）。
// 命令中心打开工作区里的音频 → media-preview.prepare 经 Main 授权规范路径 → Renderer 用
// zcode-media:// URL 播放；<audio> 必须读出元数据（Main 只给已登记的规范路径供数）。
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

import { composer } from "../helpers.mjs";
import { dumpControls } from "./workspace-commit-message.mjs";

/** 0.5 s、8 kHz、8 bit 单声道的静音 WAV：Chromium 能解码，<audio> 不会因格式错误被替换。 */
function silentWav() {
  const samples = 4000;
  const header = Buffer.alloc(44);
  header.write("RIFF", 0);
  header.writeUInt32LE(36 + samples, 4);
  header.write("WAVEfmt ", 8);
  header.writeUInt32LE(16, 16);
  header.writeUInt16LE(1, 20); // PCM
  header.writeUInt16LE(1, 22); // mono
  header.writeUInt32LE(8000, 24);
  header.writeUInt32LE(8000, 28);
  header.writeUInt16LE(1, 32);
  header.writeUInt16LE(8, 34);
  header.write("data", 36);
  header.writeUInt32LE(samples, 40);
  return Buffer.concat([header, Buffer.alloc(samples, 128)]);
}

const AUDIO_BYTES = silentWav();

export async function seed({ workspace }) {
  await mkdir(join(workspace, "media"), { recursive: true });
  await writeFile(join(workspace, "media", "clip.wav"), AUDIO_BYTES);
}

export async function run({ page, shots }) {
  await composer(page);
  try {
    // 打开文件的入口是命令中心（侧栏 Search ⌘K）的 Files 范围，结果来自 file.searchWorkspaceFiles。
    await page
      .getByRole("button", { name: /^Search/ })
      .first()
      .click({ timeout: 60_000 });
    const search = page.getByPlaceholder("Search actions, tasks, or files").first();
    await search.waitFor({ state: "visible", timeout: 10_000 });
    await page.getByText("Files", { exact: true }).last().click({ timeout: 10_000 });
    await search.fill("clip.wav");
    await page.getByText("clip.wav").last().waitFor({ state: "visible", timeout: 30_000 });
    await page.keyboard.press("Enter");
  } catch (error) {
    await dumpControls(page, "open file");
    throw error;
  } finally {
    await page.screenshot({ path: join(shots, "01-open-file.png") });
  }

  let src = "";
  try {
    const handle = await page.waitForFunction(
      () => document.querySelector("audio,video")?.getAttribute("src") || "",
      undefined,
      { timeout: 30_000 },
    );
    src = await handle.jsonValue();
  } catch (error) {
    await dumpControls(page, "media preview");
    throw error;
  } finally {
    await page.screenshot({ path: join(shots, "02-media-preview.png") });
  }
  if (!src.startsWith("zcode-media://local/preview?path=")) {
    throw new Error(`media preview did not use the local media protocol: ${src}`);
  }
  // zcode-media 协议不开放 fetch；用媒体元素本身验证：Main 只给已登记的规范路径供数，
  // 元数据能读出（时长 0.5 s）就说明授权与协议链路都通了。
  const duration = await page
    .waitForFunction(
      () => {
        const media = document.querySelector("audio,video");
        return media && media.readyState >= 1 && !media.error ? media.duration : 0;
      },
      undefined,
      { timeout: 30_000 },
    )
    .then((handle) => handle.jsonValue());
  if (Math.abs(duration - 0.5) > 0.05) throw new Error(`unexpected media duration ${duration}`);
  console.info(`[e2e] media preview played through Main-authorized ${src.slice(0, 48)}…`);
}
