// 图片附件：会话中的 composer 加入一张没有本地路径的图片（同粘贴截图），走
// attachmentBegin/Chunk/Commit 上传；发送后模型请求里带着这张图片。
// 有本地路径的文件走 zero-copy（直接把路径交给 CLI），不经过上传事务，所以这里用内存文件。
import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { crc32, deflateSync } from "node:zlib";

import { sendMessage, sseText, submitComposer } from "../helpers.mjs";
import { waitUsable } from "../resilience-helpers.mjs";

/** 生成一张合法的 16×16 红色 PNG（CRC 与压缩都由 node:zlib 计算，CLI 会解码并预处理图片）。 */
function redPng(size = 16) {
  const chunk = (type, data) => {
    const length = Buffer.alloc(4);
    length.writeUInt32BE(data.length);
    const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(body));
    return Buffer.concat([length, body, crc]);
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header.set([8, 2, 0, 0, 0], 8); // 8 位 RGB
  const row = Buffer.concat([Buffer.from([0]), Buffer.alloc(size * 3, Buffer.from([255, 0, 0]))]);
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(Buffer.concat(Array.from({ length: size }, () => row)))),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

const hasImage = (body) => body.includes("image_url") || body.includes("data:image/png");

export const mockRules = [
  {
    name: "image",
    match: ({ hasTools, text }) => hasTools && text.includes("describe the image"),
    respond: ({ response, parsed, messages }) => {
      const received = hasImage(JSON.stringify(parsed));
      if (!received)
        console.info(`[e2e] last user message: ${JSON.stringify(messages.at(-1)).slice(0, 1500)}`);
      response.end(sseText(received ? "IMAGE_RECEIVED_OK" : "NO_IMAGE_IN_REQUEST"));
    },
  },
  {
    name: "echo",
    match: () => true,
    respond: ({ response, text }) => response.end(sseText(`Reply to: ${text}`)),
  },
];

export async function seed({ home }) {
  // mock 模型声明支持图片输入，否则 CLI 以“模型不支持该附件格式”拒绝发送。
  const path = join(home, ".zcode", "v2", "provider_config.json");
  const config = JSON.parse(await readFile(path, "utf8"));
  for (const rule of config.config.modelConfigRules.providerModelRules) {
    rule.config.properties = { inputFormat: { supportsImage: true } };
  }
  await writeFile(path, JSON.stringify(config));
}

export async function run({ page, shots, model }) {
  // 先建会话：上传事务需要 sessionId。
  await waitUsable(page);
  await sendMessage(page, "start a session");
  await page.getByText("Reply to: start a session").first().waitFor({ timeout: 60_000 });
  const input = await waitUsable(page);
  await page
    .locator('input[type="file"]')
    .first()
    .setInputFiles({ name: "red.png", mimeType: "image/png", buffer: redPng() });
  await page.screenshot({ path: join(shots, "01-attached.png") });
  await input.click();
  await page.keyboard.type("please describe the image");
  await submitComposer(page);
  try {
    await page
      .getByText(/IMAGE_RECEIVED_OK|NO_IMAGE_IN_REQUEST/)
      .first()
      .waitFor({ timeout: 60_000 });
  } finally {
    await page.screenshot({ path: join(shots, "02-after-send.png") });
    // 发送失败时展开错误详情，便于定位是哪个调用失败。
    const details = page.getByRole("button", { name: "Show details" }).first();
    if (await details.isVisible().catch(() => false)) {
      await details.click();
      await page.screenshot({ path: join(shots, "03-error-details.png") });
      console.info(`[e2e] send error: ${(await page.locator("body").innerText()).slice(0, 2000)}`);
    }
  }
  if (!(await page.getByText("IMAGE_RECEIVED_OK").first().isVisible())) {
    throw new Error("the model request did not contain the image");
  }
  const request = model.requests.find((entry) => entry.body.includes("describe the image"));
  console.info(`[e2e] model received the image (request bytes=${request?.body.length ?? 0})`);
}
