// U4 Main 侧的设置写入与派生状态同步（docs/specs/rust-u4-app-host.md §2.6），以及 Host 请求
// 按 windowId 的窗口路由（§2.2、§2.7）。
import assert from "node:assert/strict";
import test from "node:test";
import { appSettingsSchema, type AppSettings, type HostSettingWriteMessage } from "@zcode/shared";
import {
  HOST_SETTING_WRITE_TIMEOUT_MS,
  HostSettingWriter,
  encodeSettingWritePatch,
  settingSettleBudgetMs,
} from "../src/main/hostSettingWriter.js";
import { resolveAppShutdownPolicy } from "../src/main/appShutdownPolicy.js";
import { createMainSettingsSync, diffAppSettings } from "../src/main/mainSettingsSync.js";
import { resolveHostRequestWindow } from "../src/main/hostRequestWindowRouting.js";

function writerHarness(options: { ready?: boolean; hostRunning?: boolean } = {}) {
  const posted: HostSettingWriteMessage[] = [];
  const warnings: string[] = [];
  const timers: Array<{ ms: number; fire: () => void; cancelled: boolean }> = [];
  let readiness: ((ready: boolean) => void) | undefined;
  let sequence = 0;
  const writer = new HostSettingWriter({
    waitUntilReady: () =>
      options.ready === undefined
        ? new Promise<boolean>((resolve) => {
            readiness = resolve;
          })
        : Promise.resolve(options.ready),
    post: (message) => {
      if (options.hostRunning === false) return false;
      posted.push(message);
      return true;
    },
    newRequestId: () => `w${++sequence}`,
    setTimer: (ms, fire) => {
      const timer = { ms, fire, cancelled: false };
      timers.push(timer);
      return () => {
        timer.cancelled = true;
      };
    },
    logger: { warn: (...args) => warnings.push(args.map(String).join(" ")) },
  });
  return {
    writer,
    posted,
    warnings,
    becomeReady: (ready: boolean) => readiness?.(ready),
    fireTimers: () => {
      for (const timer of timers.splice(0)) if (!timer.cancelled) timer.fire();
    },
    /** 只触发某个时长的定时器（区分写入超时与退出等待）。 */
    fireTimer: (ms: number) => {
      const due = timers.filter((timer) => timer.ms === ms && !timer.cancelled);
      for (const timer of due) {
        timers.splice(timers.indexOf(timer), 1);
        timer.fire();
      }
      return due.length;
    },
  };
}

const flush = () => new Promise((resolve) => setImmediate(resolve));

test("undefined clears a field on the wire", () => {
  assert.deepEqual(
    encodeSettingWritePatch({ pendingPostUpdateReleaseNotes: undefined, desktopZoomLevel: 1 }),
    { pendingPostUpdateReleaseNotes: null, desktopZoomLevel: 1 },
  );
});

test("a write waits for the host, sends setting-write and resolves on an ok result", async () => {
  const { writer, posted, becomeReady } = writerHarness();
  const write = writer.write({ desktopZoomLevel: 2 });
  await flush();
  assert.deepEqual(posted, [], "nothing is sent before the host is ready");
  becomeReady(true);
  await flush();
  assert.deepEqual(posted, [
    { type: "setting-write", requestId: "w1", patch: { desktopZoomLevel: 2 } },
  ]);
  writer.handleResult({ requestId: "w1", ok: true });
  await write;
});

test("a failed result rejects with the host error and unknown results are ignored", async () => {
  const { writer } = writerHarness({ ready: true });
  const write = writer.write({ desktopZoomLevel: 2 });
  await flush();
  writer.handleResult({ requestId: "other", ok: true });
  writer.handleResult({ requestId: "w1", ok: false, error: "setting.json is locked" });
  await assert.rejects(write, /setting.json is locked/);
});

test("the host not becoming ready within 10 s abandons the write with a warning", async () => {
  const { writer, posted, warnings } = writerHarness({ ready: false });
  await assert.rejects(writer.write({ desktopZoomLevel: 2 }), /not ready within 10000ms/);
  assert.deepEqual(posted, []);
  assert.equal(warnings.length, 1);
  assert.match(warnings[0] ?? "", /desktopZoomLevel/);
});

test("a missing result times out and a host exit fails in-flight writes", async () => {
  const timeout = writerHarness({ ready: true });
  const late = timeout.writer.write({ desktopZoomLevel: 1 });
  await flush();
  timeout.fireTimers();
  await assert.rejects(late, new RegExp(`within ${HOST_SETTING_WRITE_TIMEOUT_MS}ms`));
  // 超时之后迟到的结果不再生效。
  timeout.writer.handleResult({ requestId: "w1", ok: true });

  const exit = writerHarness({ ready: true });
  const lost = exit.writer.write({ desktopZoomLevel: 1 });
  await flush();
  exit.writer.hostExited();
  await assert.rejects(lost, /host exited/);

  const stopped = writerHarness({ ready: true, hostRunning: false });
  await assert.rejects(stopped.writer.write({ desktopZoomLevel: 1 }), /not running/);
});

test("closing for dispose waits for in-flight results and rejects new writes", async () => {
  const { writer, posted } = writerHarness({ ready: true });
  const inFlight = writer.write({ autoDownloadAndInstallUpdates: true });
  await flush();
  assert.equal(posted.length, 1);
  let closed = false;
  const closing = writer.closeAndSettle(1_500).then((settled) => {
    closed = true;
    return settled;
  });
  await flush();
  assert.equal(closed, false, "dispose waits for the in-flight result");
  await assert.rejects(writer.write({ desktopZoomLevel: 1 }), /shutting down/);
  writer.handleResult({ requestId: "w1", ok: true });
  await inFlight;
  assert.equal(await closing, true);
  assert.equal(closed, true);
});

test("closing for dispose waits no longer than the quit budget", async () => {
  const { writer, fireTimer } = writerHarness({ ready: true });
  const inFlight = writer.write({ desktopZoomLevel: 1 });
  await flush();
  const closing = writer.closeAndSettle(500);
  await flush();
  // 退出预算到了：不再等这次写入（它仍按 10 s 上限或 Host 退出得到结果）。
  assert.equal(fireTimer(500), 1);
  assert.equal(await closing, false);
  writer.hostExited();
  await assert.rejects(inFlight, /host exited/);
  // 没有在途写入时立即返回，不设定时器。
  assert.equal(await writer.closeAndSettle(500), true);
  assert.equal(fireTimer(500), 0);
});

test("the settle budget leaves the dispose wait inside the quit budget", () => {
  // Windows 普通退出 4.5 s 预算：强杀等待 4 s，写入最多等 0.5 s。
  assert.equal(settingSettleBudgetMs(resolveAppShutdownPolicy("normal", "win32")), 500);
  assert.equal(settingSettleBudgetMs(resolveAppShutdownPolicy("normal", "darwin")), 1_500);
  assert.equal(settingSettleBudgetMs(resolveAppShutdownPolicy("update-install", "win32")), 1_500);
  assert.equal(settingSettleBudgetMs({}), 0);
  assert.equal(
    settingSettleBudgetMs({ forceKillDelayMs: 0, waitTimeoutMs: 60_000 }),
    HOST_SETTING_WRITE_TIMEOUT_MS,
  );
});

test("an invalid patch is rejected before anything is sent", async () => {
  const { writer, posted } = writerHarness({ ready: true });
  await assert.rejects(writer.write({ desktopZoomLevel: 99 }), /invalid setting-write patch/);
  assert.deepEqual(posted, []);
});

function settings(patch: Partial<AppSettings>): AppSettings {
  return appSettingsSchema.parse({
    closeToTrayOnWindowsMigrationInitialized: true,
    messageStreamShowReasoningMigrationInitialized: true,
    ...patch,
  });
}

test("host snapshots apply only changed fields, in arrival order", () => {
  const applied: Array<Partial<AppSettings>> = [];
  const sync = createMainSettingsSync({ source: "host", apply: (patch) => applied.push(patch) });
  sync.setBaseline(settings({ closeToTrayOnWindows: true, keepAwakeWhileRunning: false }));

  sync.acceptHostSnapshot(settings({ closeToTrayOnWindows: true, keepAwakeWhileRunning: true }));
  sync.acceptHostSnapshot(settings({ closeToTrayOnWindows: true, keepAwakeWhileRunning: true }));
  sync.acceptHostSnapshot(settings({ closeToTrayOnWindows: false, keepAwakeWhileRunning: false }));
  assert.deepEqual(applied, [
    { keepAwakeWhileRunning: true },
    { closeToTrayOnWindows: false, keepAwakeWhileRunning: false },
  ]);

  // Rust 路径忽略 Renderer 的 SyncAppSettings：迟到的旧 patch 不能盖掉 Host 的新快照。
  sync.acceptRendererPatch({ keepAwakeWhileRunning: true });
  assert.equal(applied.length, 2);
});

test("the renderer stays the source on the Node path", () => {
  const applied: Array<Partial<AppSettings>> = [];
  const sync = createMainSettingsSync({
    source: "renderer",
    apply: (patch) => applied.push(patch),
  });
  sync.acceptHostSnapshot(settings({ keepAwakeWhileRunning: true }));
  sync.acceptRendererPatch({ keepAwakeWhileRunning: true });
  assert.deepEqual(applied, [{ keepAwakeWhileRunning: true }]);
});

test("an empty shortcut table equals no shortcut overrides", () => {
  // Main 的基线（TS 读取）与 Host 快照（Rust SettingStore）对“没有覆盖”的写法不同时，不能当成一次
  // 快捷键修改去重建菜单并向所有窗口广播。
  assert.deepEqual(
    diffAppSettings(settings({}), settings({ shortcutBindings: {} })),
    {},
    "absent → {}",
  );
  assert.deepEqual(
    diffAppSettings(settings({ shortcutBindings: {} }), settings({})),
    {},
    "{} → absent",
  );
  const applied: Array<Partial<AppSettings>> = [];
  const sync = createMainSettingsSync({ source: "host", apply: (patch) => applied.push(patch) });
  sync.setBaseline(settings({}));
  sync.acceptHostSnapshot(settings({ shortcutBindings: {}, desktopZoomLevel: 1 }));
  assert.deepEqual(applied, [{ desktopZoomLevel: 1 }]);
});

test("clearing all shortcut overrides still reports a shortcut change", () => {
  const patch = diffAppSettings(
    settings({ shortcutBindings: { "app.newTask": ["Mod+N"] } }),
    settings({}),
  );
  assert.deepEqual(patch.shortcutBindings, {});
});

test("requests route by windowId and fail cleanly when that window is gone", () => {
  const windows = new Map([
    [1, "w1"],
    [2, "w2"],
  ]);
  const lookup = {
    byWindowId: (id: number) => windows.get(id),
    anyMainWindow: () => windows.values().next().value,
  };
  assert.deepEqual(resolveHostRequestWindow({ windowId: 2 }, lookup), { ok: true, window: "w2" });
  const gone = resolveHostRequestWindow({ windowId: 3 }, lookup);
  assert.equal(gone.ok, false);
  assert.match(gone.ok ? "" : gone.error, /windowId=3/);

  // 没有 windowId：先用优先窗口（持有 route 的窗口），再退到任意主窗口。
  assert.deepEqual(resolveHostRequestWindow({}, { ...lookup, preferred: () => "w2" }), {
    ok: true,
    window: "w2",
  });
  assert.deepEqual(resolveHostRequestWindow({}, { ...lookup, preferred: () => undefined }), {
    ok: true,
    window: "w1",
  });
  windows.clear();
  assert.equal(resolveHostRequestWindow({}, lookup).ok, false);
});
