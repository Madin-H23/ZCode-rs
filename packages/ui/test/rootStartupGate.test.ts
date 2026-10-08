import assert from "node:assert/strict";
import test from "node:test";
import {
  isProviderStartupSyncPending,
  shouldResolveProviderStartupState,
} from "../src/lib/rootStartupGate.js";

const ready = {
  providerFamilyDomainMigrationComplete: true,
  modelSelectionViewHydrated: true,
  appSettingsHydrated: true,
};

test("provider startup sync waits for the app settings snapshot", () => {
  // reload 后迁移流程的 settingService.get 可能先于 useSettings 的那次返回：
  // 设置快照未加载时门禁必须继续等待，否则会用 null 的 providerFamilyDomain 误开登录页。
  assert.equal(isProviderStartupSyncPending({ ...ready, appSettingsHydrated: false }), true);
  assert.equal(
    shouldResolveProviderStartupState({
      providerStartupSyncPending: isProviderStartupSyncPending({
        ...ready,
        appSettingsHydrated: false,
      }),
      providerAvailabilityStartupCheckCompleted: false,
    }),
    true,
  );
});

test("provider startup sync completes once every input is ready", () => {
  assert.equal(isProviderStartupSyncPending(ready), false);
  assert.equal(
    isProviderStartupSyncPending({ ...ready, providerFamilyDomainMigrationComplete: false }),
    true,
  );
  assert.equal(isProviderStartupSyncPending({ ...ready, modelSelectionViewHydrated: false }), true);
});
