// AskUserQuestion：模型提问 → 用户在界面作答 → 答案作为工具结果回到模型，模型继续。
import { join } from "node:path";

import { sendMessage, sseText, sseToolCall } from "../helpers.mjs";
import { waitUsable } from "../resilience-helpers.mjs";

const QUESTION = "Which color do you prefer?";

function toolResultText(messages) {
  const content = messages.at(-1)?.content;
  return typeof content === "string" ? content : JSON.stringify(content ?? "");
}

export const mockRules = [
  {
    name: "answer",
    match: ({ hasTools, messages }) => hasTools && messages.at(-1)?.role === "tool",
    respond: ({ response, messages }) => {
      const result = toolResultText(messages);
      response.end(
        sseText(result.includes("Blue") ? "ANSWER_BLUE_OK" : `ANSWER_UNEXPECTED ${result.slice(0, 200)}`),
      );
    },
  },
  {
    name: "ask",
    match: ({ hasTools, text }) => hasTools && text.includes("ask me"),
    respond: ({ response }) =>
      response.end(
        sseToolCall("AskUserQuestion", {
          questions: [
            {
              question: QUESTION,
              header: "Color",
              options: [
                { label: "Red", description: "A warm color" },
                { label: "Blue", description: "A cool color" },
              ],
              multiSelect: false,
            },
          ],
        }),
      ),
  },
  {
    name: "echo",
    match: () => true,
    respond: ({ response, text }) => response.end(sseText(`Reply to: ${text}`)),
  },
];

export async function run({ page, shots, model }) {
  await waitUsable(page);
  await sendMessage(page, "please ask me a question");
  await page.getByText(QUESTION).first().waitFor({ timeout: 60_000 });
  await page.screenshot({ path: join(shots, "01-question.png") });
  const blue = page.getByText("Blue", { exact: true }).first();
  await blue.click({ trial: true });
  await blue.click();
  await page.screenshot({ path: join(shots, "02-selected.png") });
  // 单题单选通常选中即提交；否则点提交按钮。
  const answered = page.getByText("ANSWER_BLUE_OK").first();
  if (!(await answered.isVisible().catch(() => false))) {
    const submit = page.getByRole("button", { name: /^(Submit|Send|Confirm)$/ }).first();
    if (await submit.isVisible().catch(() => false)) await submit.click();
  }
  try {
    await answered.waitFor({ timeout: 60_000 });
  } finally {
    await page.screenshot({ path: join(shots, "03-answered.png") });
  }
  if (!model.requests.some((request) => request.body.includes("Blue")))
    throw new Error("the answer never reached the model");
  console.info("[e2e] the user's answer reached the model and the turn continued");
}
