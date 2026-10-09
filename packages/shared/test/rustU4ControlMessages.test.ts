// U4 App 级 Rust Host 控制消息的线上格式（docs/specs/rust-u4-app-host.md §4.1）。
// 这些 schema 是 Main 与 Rust Host 两侧的契约：Main 用它们发送 / 校验，Rust 侧按同一格式实现。
import assert from "node:assert/strict";
import test from "node:test";
import {
  hostBotRemoteWorkspaceReconnectRequestResponseSchema,
  hostBrowserExecuteRequestResponseSchema,
  hostIssueAttachmentEndpointMessageSchema,
  hostResponseMessageSchema,
  hostSettingWriteMessageSchema,
  rustAppHostControlResponseSchema,
} from "../src/index.js";

const issue = {
  type: "issue-attachment-endpoint",
  requestId: "r1",
  attachmentId: "a1",
  clientMode: "desktop-continuous",
  scope: { kind: "local" },
};

test("issue-attachment-endpoint accepts an optional integer windowId >= 1", () => {
  assert.equal(hostIssueAttachmentEndpointMessageSchema.safeParse(issue).success, true);
  assert.equal(
    hostIssueAttachmentEndpointMessageSchema.safeParse({ ...issue, windowId: 1 }).success,
    true,
  );
  for (const windowId of [0, -1, 1.5, "1", null]) {
    assert.equal(
      hostIssueAttachmentEndpointMessageSchema.safeParse({ ...issue, windowId }).success,
      false,
      `windowId=${String(windowId)} must be rejected`,
    );
  }
  assert.equal(
    hostIssueAttachmentEndpointMessageSchema.safeParse({ ...issue, extra: true }).success,
    false,
  );
});

test("setting-write carries an AppSettings patch where null clears a field", () => {
  const ok = hostSettingWriteMessageSchema.safeParse({
    type: "setting-write",
    requestId: "w1",
    patch: { desktopZoomLevel: 2, pendingPostUpdateReleaseNotes: null },
  });
  assert.equal(ok.success, true);
  // 值域按 appSettingsPatchSchema 校验。
  assert.equal(
    hostSettingWriteMessageSchema.safeParse({
      type: "setting-write",
      requestId: "w1",
      patch: { desktopZoomLevel: 99 },
    }).success,
    false,
  );
  // 未知字段、空 patch、非对象 patch、缺 requestId 都拒绝。
  for (const patch of [{ notASetting: true }, {}, null, [], "x"]) {
    assert.equal(
      hostSettingWriteMessageSchema.safeParse({ type: "setting-write", requestId: "w1", patch })
        .success,
      false,
      `patch=${JSON.stringify(patch)} must be rejected`,
    );
  }
  assert.equal(
    hostSettingWriteMessageSchema.safeParse({ type: "setting-write", patch: { locale: "en" } })
      .success,
    false,
  );
});

test("Host → Main U4 control messages parse strictly", () => {
  const result = rustAppHostControlResponseSchema.safeParse({
    type: "setting-write-result",
    requestId: "w1",
    ok: false,
    error: "locked",
  });
  assert.equal(result.success, true);
  assert.equal(
    rustAppHostControlResponseSchema.safeParse({ type: "setting-write-result", requestId: "w1" })
      .success,
    false,
  );

  const changed = rustAppHostControlResponseSchema.safeParse({
    type: "setting-changed",
    setting: {
      closeToTrayOnWindows: false,
      closeToTrayOnWindowsMigrationInitialized: true,
      desktopZoomLevel: 1,
    },
  });
  assert.equal(changed.success, true);
  if (changed.success && changed.data.type === "setting-changed") {
    // 快照按 appSettingsSchema 解析（与 Main 直接读文件相同的迁移与默认值）。
    assert.equal(changed.data.setting.closeToTrayOnWindows, false);
    assert.equal(changed.data.setting.desktopZoomLevel, 1);
    assert.equal(changed.data.setting.autoDownloadAndInstallUpdates, false);
  }
  assert.equal(
    rustAppHostControlResponseSchema.safeParse({ type: "setting-changed" }).success,
    false,
  );

  assert.equal(
    rustAppHostControlResponseSchema.safeParse({ type: "off-peak-active-count", count: 2 }).success,
    true,
  );
  for (const count of [-1, 0.5, "1"]) {
    assert.equal(
      rustAppHostControlResponseSchema.safeParse({ type: "off-peak-active-count", count }).success,
      false,
    );
  }
  // 这些消息不属于 Node Host 的响应联合。
  assert.equal(
    hostResponseMessageSchema.safeParse({ type: "off-peak-active-count", count: 1 }).success,
    false,
  );
});

test("browser-execute-request and bot reconnect request carry an optional windowId", () => {
  const browser = {
    type: "browser-execute-request",
    requestId: "b1",
    sessionId: "s1",
    command: { method: "getState" },
  };
  const parsedBrowser = hostBrowserExecuteRequestResponseSchema.safeParse(browser);
  assert.equal(parsedBrowser.success, true, JSON.stringify(parsedBrowser.error?.issues));
  const withWindow = hostBrowserExecuteRequestResponseSchema.safeParse({ ...browser, windowId: 3 });
  assert.equal(withWindow.success, true);
  assert.equal(withWindow.data?.windowId, 3);
  assert.equal(
    hostBrowserExecuteRequestResponseSchema.safeParse({ ...browser, windowId: 0 }).success,
    false,
  );

  const reconnect = {
    type: "bot-remote-workspace-reconnect-request",
    requestId: "r1",
    workspacePath: "/w",
    workspaceIdentity: "remote:wsl:/w",
    target: { kind: "wsl" },
  };
  const parsedReconnect = hostBotRemoteWorkspaceReconnectRequestResponseSchema.safeParse(reconnect);
  assert.equal(parsedReconnect.success, true, JSON.stringify(parsedReconnect.error?.issues));
  assert.equal(
    hostBotRemoteWorkspaceReconnectRequestResponseSchema.safeParse({ ...reconnect, windowId: 2 })
      .success,
    true,
  );
  assert.equal(
    hostBotRemoteWorkspaceReconnectRequestResponseSchema.safeParse({ ...reconnect, windowId: -2 })
      .success,
    false,
  );
});
