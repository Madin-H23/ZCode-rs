// Rust Host 反向请求参数校验的对拍 fixture（docs/specs/rust-host-integration.md §2.5）。
// 用真实 zod 4 schema（packages/shared 的 zcode-protocol）对下面的输入执行 safeParse，记录解析结果或
// `error.flatten()`；Rust 侧 support/zod 逐项复现。用法：pnpm exec tsx scripts/rust-host/zod-flatten-fixtures.ts [--check]
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { format } from "oxfmt";
import type { z } from "zod";
import {
  zcodeAutomationCheckTaskBindingParamsSchema,
  zcodeAutomationCreateParamsSchema,
  zcodeAutomationDeleteParamsSchema,
  zcodeAutomationListParamsSchema,
  zcodeAutomationUpdateParamsSchema,
  zcodeOffPeakCreateParamsSchema,
  zcodeOffPeakListParamsSchema,
} from "../../packages/shared/src/zcode-protocol/index.js";

const root = resolve(import.meta.dirname, "../..");
const fixturePath = resolve(
  root,
  "apps/zcode-host/crates/services/src/support/fixtures/zod-flatten.json",
);

// list 两个 schema 的调用方传 `request.params ?? {}`（zcodeAgentService.ts）。
const SCHEMAS: Record<string, { schema: z.ZodType; nullishAsEmpty?: boolean }> = {
  offPeakCreate: { schema: zcodeOffPeakCreateParamsSchema },
  offPeakList: { schema: zcodeOffPeakListParamsSchema, nullishAsEmpty: true },
  automationCreate: { schema: zcodeAutomationCreateParamsSchema },
  automationUpdate: { schema: zcodeAutomationUpdateParamsSchema },
  automationList: { schema: zcodeAutomationListParamsSchema, nullishAsEmpty: true },
  automationCheckTaskBinding: { schema: zcodeAutomationCheckTaskBindingParamsSchema },
  automationDelete: { schema: zcodeAutomationDeleteParamsSchema },
};

const UNDEFINED = Symbol("undefined");
const daily = { cronExpr: "x", prompt: "p", intervalUnit: "daily" };
const CASES: [string, unknown][] = [
  ["offPeakCreate", UNDEFINED],
  ["offPeakCreate", []],
  ["offPeakCreate", "x"],
  ["offPeakCreate", {}],
  ["offPeakCreate", { title: " t ", prompt: "p" }],
  [
    "offPeakCreate",
    {
      title: "t",
      prompt: "p",
      permissionMode: "plan",
      model: " GLM ",
      thoughtLevel: "high",
      boundSessionId: "s",
    },
  ],
  ["offPeakCreate", { title: " ", prompt: 1, permissionMode: "auto", extra: 1, z: 2 }],
  ["offPeakCreate", { title: "t", prompt: "p", model: "", thoughtLevel: null, boundSessionId: 3 }],
  ["offPeakCreate", { title: "t", prompt: "p", extra: true }],
  ["offPeakList", UNDEFINED],
  ["offPeakList", {}],
  ["offPeakList", { a: 1 }],
  ["offPeakList", []],
  ["automationCreate", UNDEFINED],
  ["automationCreate", { cronExpr: " 0 9 * * * ", prompt: "hi" }],
  [
    "automationCreate",
    {
      title: "",
      cronExpr: "x",
      prompt: "p",
      relativeDelayMinutes: 5,
      modelSelection: { providerId: " p ", modelId: "m", options: { reasoningLevel: " high " } },
      mode: "plan",
      targetTaskId: "t",
      botDeliveryTarget: {
        provider: "feishu",
        botId: " b ",
        providerUserId: "u",
        chatType: "group",
      },
      recurring: false,
      maxRuns: 3,
    },
  ],
  ["automationCreate", { ...daily, interval: 3, recurring: true }],
  [
    "automationCreate",
    {
      cronExpr: "x",
      prompt: "p",
      relativeDelayMinutes: 1.5,
      maxRuns: 0,
      interval: 300,
      mode: "x",
      modelSelection: { providerId: "", modelId: 1, options: { x: 1 } },
    },
  ],
  ["automationCreate", { ...daily, recurring: false, maxRuns: 2 }],
  ["automationCreate", { ...daily, cronExpr: "", recurring: false }],
  ["automationCreate", { ...daily, title: 1 }],
  ["automationCreate", { ...daily, extra: 1 }],
  ["automationCreate", { ...daily, mode: "zz" }],
  ["automationCreate", { ...daily, maxRuns: 1.5, recurring: true }],
  ["automationCreate", { ...daily, relativeDelayMinutes: 999999 }],
  ["automationCreate", { ...daily, relativeDelayMinutes: -3 }],
  ["automationCreate", { ...daily, relativeDelayMinutes: "x" }],
  ["automationCreate", { ...daily, modelSelection: { providerId: "", modelId: "m" } }],
  ["automationCreate", { ...daily, modelSelection: { providerId: 1, modelId: "m" } }],
  ["automationCreate", { ...daily, modelSelection: { providerId: "p", modelId: "m", x: 1 } }],
  ["automationCreate", { ...daily, intervalUnit: "bogus" }],
  ["automationCreate", { ...daily, interval: "3" }],
  ["automationCreate", { ...daily, interval: 0 }],
  ["automationCreate", { ...daily, interval: 2.5 }],
  ["automationCreate", { ...daily, maxRuns: null }],
  ["automationCreate", { cronExpr: 5, prompt: "p", extra: 1, intervalUnit: "daily" }],
  [
    "automationCreate",
    {
      cronExpr: "x",
      prompt: "p",
      modelSelection: "s",
      botDeliveryTarget: {
        provider: "feishu",
        botId: " ",
        providerUserId: "u",
        chatType: "group",
        z: 1,
      },
    },
  ],
  ["automationCreate", { cronExpr: "x", prompt: "p", botDeliveryTarget: { provider: "x" } }],
  [
    "automationCreate",
    { cronExpr: "x", prompt: "p", relativeDelayMinutes: 2, intervalUnit: "daily", interval: 4 },
  ],
  ["automationUpdate", { automationId: "a" }],
  ["automationUpdate", { automationId: " a ", title: " t " }],
  ["automationUpdate", { automationId: "a", maxRuns: null }],
  ["automationUpdate", { automationId: "a", maxRuns: null, recurring: true }],
  ["automationUpdate", { automationId: "a", maxRuns: 3, recurring: true }],
  ["automationUpdate", { automationId: "a", maxRuns: -1, relativeDelayMinutes: 2 }],
  ["automationUpdate", { automationId: " ", maxRuns: null, recurring: "y" }],
  ["automationUpdate", { maxRuns: 2, recurring: true }],
  ["automationUpdate", { automationId: "a", intervalUnit: "weekly", interval: 2, maxRuns: 4 }],
  [
    "automationUpdate",
    { automationId: "a", intervalUnit: "weekly", interval: 2, maxRuns: null, recurring: true },
  ],
  ["automationUpdate", { automationId: "a", intervalUnit: "weekly", recurring: false }],
  ["automationList", UNDEFINED],
  ["automationList", { x: 1, y: 2 }],
  ["automationCheckTaskBinding", { targetTaskId: " t " }],
  ["automationCheckTaskBinding", { targetTaskId: "" }],
  ["automationDelete", { automationId: 1 }],
  ["automationDelete", UNDEFINED],
];

const cases = CASES.map(([name, input]) => {
  const entry = SCHEMAS[name]!;
  const isUndefined = input === UNDEFINED;
  const value = isUndefined ? undefined : input;
  const parsed = entry.schema.safeParse(entry.nullishAsEmpty ? (value ?? {}) : value);
  return {
    schema: name,
    ...(isUndefined ? { undefined: true } : { input }),
    expected: parsed.success ? { ok: parsed.data } : { flatten: parsed.error.flatten() },
  };
});

const content = `${JSON.stringify({ source: "scripts/rust-host/zod-flatten-fixtures.ts", cases }, null, 2)}\n`;
const formatted = await format(fixturePath, content);
if (formatted.errors.length) throw new Error("Cannot format zod-flatten.json");
if (process.argv.includes("--check")) {
  const current = await readFile(fixturePath, "utf8").catch(() => "");
  if (current !== formatted.code) {
    console.error(
      "[zod-flatten-fixtures] zod-flatten.json is stale; run pnpm exec tsx scripts/rust-host/zod-flatten-fixtures.ts",
    );
    process.exitCode = 1;
  } else {
    console.log(`[zod-flatten-fixtures] up to date: ${cases.length} cases`);
  }
} else {
  await mkdir(resolve(fixturePath, ".."), { recursive: true });
  await writeFile(fixturePath, formatted.code);
  console.log(`[zod-flatten-fixtures] wrote: ${cases.length} cases`);
}
