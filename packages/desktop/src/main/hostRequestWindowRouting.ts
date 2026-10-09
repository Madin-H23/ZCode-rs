// Host → Main 请求按窗口路由（docs/specs/rust-u4-app-host.md §2.2、§2.7）。
//
// App 级 Host 被多个主窗口共享，需要回到某个窗口的请求（browser-execute-request、Bot 重连）
// 带上发起方的 windowId：有 windowId 时只交给这个窗口，窗口已不存在就回失败；没有 windowId
// 时先用调用方给出的优先窗口（如持有该 workspace route 的窗口），再退到任意一个主窗口。
// 纯逻辑，窗口的查找由调用方注入。

export type HostRequestWindowResolution<W> = { ok: true; window: W } | { ok: false; error: string };

interface HostRequestWindowLookup<W> {
  /** 按窗口 id（webContents.id）找仍然存活的主窗口。 */
  byWindowId(windowId: number): W | undefined;
  /** 没有 windowId 时优先使用的窗口。 */
  preferred?: () => W | undefined;
  /** 任意一个存活的主窗口（调用方决定顺序，例如先取聚焦窗口）。 */
  anyMainWindow(): W | undefined;
}

export function resolveHostRequestWindow<W>(
  request: { windowId?: number },
  lookup: HostRequestWindowLookup<W>,
): HostRequestWindowResolution<W> {
  if (request.windowId !== undefined) {
    const window = lookup.byWindowId(request.windowId);
    return window
      ? { ok: true, window }
      : { ok: false, error: `发起请求的窗口已关闭，windowId=${request.windowId}` };
  }
  const window = lookup.preferred?.() ?? lookup.anyMainWindow();
  return window ? { ok: true, window } : { ok: false, error: "没有可用的主窗口" };
}
