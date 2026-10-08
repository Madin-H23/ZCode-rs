import { updateTextFileLocked } from "@zcode/shared/node";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** 宽松解析：读不到、不是 JSON 或不是对象时视为 `{}`（skills、commands、hooks 的既有读取语义）。 */
export function parseJsonRecordLenient(raw: string | null): Record<string, unknown> {
  if (raw === null) return {};
  try {
    const parsed = JSON.parse(raw) as unknown;
    return isRecord(parsed) ? parsed : {};
  } catch {
    return {};
  }
}

/**
 * 持跨进程锁读改写 JSON 对象文件（`~/.zcode/cli/config.json` 等由 Main、CLI、Node / Rust Host
 * 共同写入的文件）。`mutate` 返回下一个对象，或 `false` 表示不写；落盘为
 * `${JSON.stringify(value, null, 2)}\n`。锁与原子写的原因见 `updateTextFileLocked`。
 */
export async function updateJsonRecordFileLocked(
  filePath: string,
  mutate: (
    current: Record<string, unknown>,
  ) => Record<string, unknown> | false | Promise<Record<string, unknown> | false>,
  parse: (raw: string | null) => Record<string, unknown> = parseJsonRecordLenient,
): Promise<void> {
  await updateTextFileLocked(filePath, async (raw) => {
    const next = await mutate(parse(raw));
    return next === false ? null : `${JSON.stringify(next, null, 2)}\n`;
  });
}
