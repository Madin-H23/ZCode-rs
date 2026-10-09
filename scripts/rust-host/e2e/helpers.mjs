// 桌面 E2E 场景共用的界面与数据辅助函数。
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";

/** OpenAI 兼容流式响应的一帧。 */
export function sseChunk(delta, finish) {
  return `data: ${JSON.stringify({
    id: "mock",
    object: "chat.completion.chunk",
    created: 0,
    model: "mock-model",
    choices: [{ index: 0, delta, finish_reason: finish }],
    ...(finish ? { usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } } : {}),
  })}\n\n`;
}

/** 一次性回一段文本并结束流。 */
export function sseText(content) {
  return sseChunk({ role: "assistant", content }, null) + sseChunk({}, "stop") + "data: [DONE]\n\n";
}

/** 回一个工具调用并结束本轮（CLI 执行工具后会带着工具结果再请求一次）。 */
export function sseToolCall(name, args, id = `call_${name}_1`) {
  const call = {
    index: 0,
    id,
    type: "function",
    function: { name, arguments: JSON.stringify(args) },
  };
  return (
    sseChunk({ role: "assistant", tool_calls: [call] }, null) +
    sseChunk({}, "tool_calls") +
    "data: [DONE]\n\n"
  );
}

export function lastUserText(messages) {
  const user = [...messages].reverse().find((message) => message.role === "user");
  return typeof user?.content === "string" ? user.content : JSON.stringify(user?.content ?? "");
}

/** 当前可见的输入框（首页与会话页的 composer）。 */
export async function composer(page) {
  const candidates = [
    page.locator('textarea[placeholder*="Ask ZCode"]'),
    page.locator('[contenteditable="true"]'),
  ];
  for (let attempt = 0; attempt < 120; attempt += 1) {
    for (const candidate of candidates) {
      const first = candidate.first();
      if (await first.isVisible().catch(() => false)) return first;
    }
    await page.waitForTimeout(500);
  }
  throw new Error("composer not found");
}

/**
 * 提交 composer 里已经输入的内容。发送键可用之前，Enter 在 LexicalChatInput 里只会换行
 * （submitDisabled：runtime 还没报告执行能力、模型选择未就绪等），消息留在输入框里。先等发送键
 * 可点——用户看到的同一个信号——再按 Enter；runtime 越慢（例如调试构建），这段窗口越长。
 */
export async function submitComposer(page) {
  await page
    .locator('[data-testid="v4-composer-send"]:visible')
    .first()
    .click({ trial: true, timeout: 60_000 });
  await page.keyboard.press("Enter");
}

export async function sendMessage(page, text) {
  const input = await composer(page);
  await input.click();
  await page.keyboard.type(text);
  await submitComposer(page);
}

/** 只读打开隔离 HOME 里的 tasks-index.sqlite（WAL 下读者不阻塞 Host 的写入）。 */
export function readTaskRows(home) {
  const database = new DatabaseSync(join(home, ".zcode", "v2", "tasks-index.sqlite"), {
    readOnly: true,
  });
  try {
    return database.prepare("SELECT task_id, title, workspace_path, deleted FROM tasks").all();
  } finally {
    database.close();
  }
}
