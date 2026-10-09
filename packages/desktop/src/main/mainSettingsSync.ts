// Main 内存里的设置派生状态（关闭到托盘、防休眠、更新渠道、菜单快捷键）的唯一更新入口
// （docs/specs/rust-u4-app-host.md §2.6）。
//
// Node 路径：今天的做法，Renderer 写完设置后发 SyncAppSettings（patch）。
// Rust 路径：只认 Host 的 setting-changed（完整快照，按写入顺序从同一条管道到达），与上一次快照
// 比较后只应用变化的字段；Renderer 的 SyncAppSettings 被忽略。两个来源不并存，所以同一次修改
// 的 IPC 通知与 Host 推送不会乱序覆盖。
import type { AppSettings } from "@zcode/shared";

type MainSettingsSyncSource = "renderer" | "host";

type SettingsPatch = Partial<AppSettings>;

/**
 * 比较用的字段值：没有快捷键覆盖与空覆盖表等价。基线由 Main 的 TS 读取得到，快照由 Host 的
 * SettingStore 得到，两边只要有一边把“没有覆盖”写成 `{}`，就不能当成一次快捷键修改
 * （否则重建菜单并向所有窗口广播 SettingsChanged）。
 */
function comparableValue(key: string, value: unknown): string | undefined {
  if (key === "shortcutBindings" && (value === undefined || value === null)) return "{}";
  return JSON.stringify(value);
}

/** 逐字段比较两个快照；被移除的字段记为 undefined。 */
export function diffAppSettings(
  previous: AppSettings | undefined,
  next: AppSettings,
): SettingsPatch {
  const patch: Record<string, unknown> = {};
  const before = new Map<string, unknown>(Object.entries(previous ?? {}));
  const after = new Map<string, unknown>(Object.entries(next));
  for (const key of new Set([...before.keys(), ...after.keys()])) {
    if (comparableValue(key, before.get(key)) !== comparableValue(key, after.get(key))) {
      patch[key] = after.get(key);
    }
  }
  if ("shortcutBindings" in patch && patch.shortcutBindings === undefined) {
    // 快捷键覆盖被整体清除时仍要重建菜单；syncImmediateAppSettings 以 `!== undefined` 判断是否变化。
    patch.shortcutBindings = {};
  }
  return patch as SettingsPatch;
}

export function createMainSettingsSync(options: {
  source: MainSettingsSyncSource;
  apply: (patch: SettingsPatch) => void;
}) {
  let last: AppSettings | undefined;
  return {
    /** Main 启动时直接读文件得到的设置；Main 的派生状态就是由它初始化的。 */
    setBaseline(settings: AppSettings): void {
      last = settings;
    },
    acceptRendererPatch(patch: SettingsPatch): void {
      if (options.source === "renderer") options.apply(patch);
    },
    acceptHostSnapshot(settings: AppSettings): void {
      if (options.source !== "host") return;
      const patch = diffAppSettings(last, settings);
      last = settings;
      if (Object.keys(patch).length > 0) options.apply(patch);
    },
  };
}
