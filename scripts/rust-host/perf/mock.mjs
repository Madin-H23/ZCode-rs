// App 级性能测量的 mock 模型规则（OpenAI 兼容 SSE，经 e2e/profile.mjs 的 startMockModel 应答）。
//
// 提示词约定（用户消息的开头）：
//   perf:reply:<tag>            短回复：40 段、每段间隔 20 ms（约 0.8 s）
//   perf:stream:<tag>:<段数>    长流：每段间隔 20 ms
//   perf:tool:<tag>             工具负载：先调用 Bash（seq 1 200000），再 Read 一个大文件的前 2000 行，最后回复
//   perf:hist:<序号>            长历史的一轮：一次性回一段约 3 KB 的 Markdown；每 10 轮先 Read 一次 README
// 带 tools 的请求才是主对话；其他请求（标题生成等）一律回一个短标题。
//
// 可见文本里的标记词让页面探针（page-probes.mjs）能在 DOM 里定位进度：
//   qa<tag>s  第一段     qa<tag>e  最后一段     qm<tag>x<序号>  每 10 段一个，mock 记下它的发送时刻
import { join } from "node:path";

import { sseChunk, sseText, sseToolCall } from "../e2e/helpers.mjs";

export const CHUNK_INTERVAL_MS = 20;
const WORDS = ["lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit"];

export const startMarker = (tag) => `qa${tag}s`;
export const endMarker = (tag) => `qa${tag}e`;
export const progressMarker = (tag, index) => `qm${tag}x${String(index).padStart(4, "0")}`;
export const historyMarker = (index) => `qah${index}e`;
/** 长历史夹具的轮数（history-fixture.mjs）。 */
export const HISTORY_TURNS = 120;
/** 工具负载读取的大文件（workspace 里，seed 时写入）。 */
export const BIG_FILE = "big-file.txt";

/** 每段的文本：标记段、段落换行与普通词交替。 */
function chunkText(tag, index, total) {
  if (index === 0) return `${startMarker(tag)} `;
  if (index === total - 1) return ` ${endMarker(tag)}`;
  if (index % 10 === 0) return `${progressMarker(tag, index)} `;
  return index % 40 === 39
    ? `${WORDS[index % WORDS.length]}.\n\n`
    : `${WORDS[index % WORDS.length]} `;
}

/** 某个 tag 的流应有的全部标记，按出现顺序。 */
export function expectedMarkers(tag, total) {
  const markers = [startMarker(tag)];
  for (let index = 10; index < total - 1; index += 10) markers.push(progressMarker(tag, index));
  markers.push(endMarker(tag));
  return markers;
}

const PROMPT = /perf:(reply|stream|tool|hist):([A-Za-z0-9]+)(?::(\d+))?/;
const messageText = (message) =>
  typeof message.content === "string" ? message.content : JSON.stringify(message.content ?? "");

/**
 * 本轮的提示词 `perf:<kind>:<tag>[:<数字>]`，以及它之后已有几条工具结果（判断工具负载走到了哪一步）。
 * 提示词所在的用户消息不一定是最后一条：长会话里 runtime 会在它后面追加用户角色的提醒（例如 Node CLI
 * 在第 90 轮左右），所以从后往前找第一条带提示词的用户消息。
 */
function turnOf(messages) {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    if (messages[index].role !== "user") continue;
    const match = messageText(messages[index]).match(PROMPT);
    if (!match) continue;
    const toolResults = messages
      .slice(index + 1)
      .filter((message) => message.role === "tool").length;
    return { kind: match[1], tag: match[2], count: Number(match[3] ?? 0), toolResults };
  }
  return null;
}

function historyReply(index) {
  const paragraph = Array.from(
    { length: 60 },
    (_, word) => WORDS[(index + word) % WORDS.length],
  ).join(" ");
  return [
    `## Step ${index}`,
    "",
    paragraph,
    "",
    `- item one for step ${index}`,
    `- item two for step ${index}`,
    `- item three for step ${index}`,
    "",
    "```ts",
    ...Array.from(
      { length: 24 },
      (_, line) => `export const value${index}_${line} = compute(${line}, "${WORDS[line % 8]}");`,
    ),
    "```",
    "",
    paragraph,
    "",
    historyMarker(index),
  ].join("\n");
}

/**
 * 规则表。`log` 收集 mock 侧的观测：`sent[tag]` = [{ marker, sentAt }]（标记段写出的时刻），
 * `toolResults[tag]` = 模型收到的工具结果文本长度（正确性检查用）。
 */
export function perfMockRules(log) {
  const stream = async ({ response, sleep }, tag, total) => {
    const sent = (log.sent[tag] = []);
    for (let index = 0; index < total && !response.destroyed; index += 1) {
      const text = chunkText(tag, index, total);
      response.write(
        sseChunk(index === 0 ? { role: "assistant", content: text } : { content: text }, null),
      );
      if (index === 0 || index === total - 1 || index % 10 === 0)
        sent.push({ marker: text.trim(), sentAt: Date.now() });
      if (index < total - 1) await sleep(CHUNK_INTERVAL_MS);
    }
    response.end(sseChunk({}, "stop") + "data: [DONE]\n\n");
  };
  return [
    {
      // 标题生成等旁路请求：回提示词本身，侧栏标题因此总是提示词（场景按它点选会话）。
      name: "side-request",
      match: ({ hasTools }) => !hasTools,
      respond: ({ response, text }) =>
        response.end(sseText(text.match(/perf:[a-z]+:[A-Za-z0-9:]+/)?.[0] ?? "Perf task")),
    },
    {
      name: "reply",
      match: ({ messages }) => turnOf(messages)?.kind === "reply",
      respond: (context) => stream(context, turnOf(context.messages).tag, 40),
    },
    {
      name: "stream",
      match: ({ messages }) => turnOf(messages)?.kind === "stream",
      respond: (context) => {
        const { tag, count } = turnOf(context.messages);
        return stream(context, tag, count);
      },
    },
    {
      name: "tool",
      match: ({ messages }) => turnOf(messages)?.kind === "tool",
      respond: (context) => {
        const { response, messages, workspace } = context;
        const { tag, toolResults: step } = turnOf(messages);
        if (step > 0) {
          const result = messages.at(-1);
          const content =
            typeof result.content === "string" ? result.content : JSON.stringify(result.content);
          (log.toolResults[tag] ??= []).push(content.length);
        }
        if (step === 0)
          return response.end(
            sseToolCall(
              "Bash",
              { command: "seq 1 200000", description: "Print numbers" },
              `call_bash_${tag}`,
            ),
          );
        if (step === 1)
          return response.end(
            // 显式 limit：不带 limit 时 TS CLI 对超过 256 KB 的文件直接拒绝（READ_MAX_FILE_SIZE_BYTES），
            // 两种 runtime 读同样的 2000 行。
            sseToolCall(
              "Read",
              { file_path: join(workspace, BIG_FILE), offset: 1, limit: 2000 },
              `call_read_${tag}`,
            ),
          );
        return stream(context, tag, 40);
      },
    },
    {
      name: "history",
      match: ({ messages }) => turnOf(messages)?.kind === "hist",
      respond: ({ response, messages, workspace }) => {
        const { tag, toolResults } = turnOf(messages);
        const index = Number(tag);
        if (index % 10 === 0 && toolResults === 0)
          return response.end(
            sseToolCall("Read", { file_path: join(workspace, "README.md") }, `call_hist_${index}`),
          );
        log.sent[`h${index}`] = [{ marker: historyMarker(index), sentAt: Date.now() }];
        return response.end(sseText(historyReply(index)));
      },
    },
    {
      name: "default",
      match: () => true,
      respond: ({ response }) => response.end(sseText("ok")),
    },
  ];
}
