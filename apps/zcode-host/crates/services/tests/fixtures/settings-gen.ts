// 设置语义差分夹具：用真实的 TS schema / patch 规范化 / 旧账号迁移计算期望结果，Rust 逐条比对。
// 重新生成：在仓库根目录执行
//   node --import tsx apps/zcode-host/crates/services/tests/fixtures/settings-gen.ts \
//     > apps/zcode-host/crates/services/tests/fixtures/settings.json
import {
  appSettingsPatchSchema,
  appSettingsSchema,
} from "../../../../../../packages/shared/src/validationAppSettings.ts";
import { normalizeSettingsPatch } from "../../../../../../packages/services/src/setting/normalizeSettingsPatch.ts";
import {
  migrateLegacyAccountConnectionSettings,
  readIncompleteLegacyTeamConnections,
} from "../../../../../../packages/services/src/setting/legacyAccountConnectionSettings.ts";

const flags = {
  closeToTrayOnWindowsMigrationInitialized: true,
  messageStreamShowReasoningMigrationInitialized: true,
};
const ssh = { kind: "ssh", host: " h ", username: "u", port: 22, extra: 1 };

const reads: [string, unknown][] = [
  ["empty", {}],
  ["flags-only", flags],
  ["not-object-null", null],
  ["not-object-array", []],
  ["string", "x"],
  [
    "legacy-mixed",
    {
      locale: "en-US",
      closeToTrayOnWindows: false,
      messageStreamShowReasoning: false,
      unknownField: 1,
      lastOpenTabs: ["/a", "/b"],
      desktopWindowSize: { width: 1, height: 1, maximized: false },
    },
  ],
  ["respects-flags", { ...flags, closeToTrayOnWindows: false, messageStreamShowReasoning: false }],
  ["blank-proxy", { ...flags, httpProxy: "   " }],
  [
    "trimmed-strings",
    { ...flags, httpProxy: "  http://a:1  ", terminalFontFamily: " Menlo ", dataBaseDir: " /d " },
  ],
  ["zoom-out-of-range", { ...flags, desktopZoomLevel: 9 }],
  ["zoom-ok", { ...flags, desktopZoomLevel: -3 }],
  ["archive-days-bad", { ...flags, taskAutoArchiveOlderThanDays: 400 }],
  ["archive-days-float", { ...flags, taskAutoArchiveOlderThanDays: 7.5 }],
  ["occupation-bad", { ...flags, onboardingOccupation: "pilot" }],
  ["occupation-null", { ...flags, onboardingOccupation: null }],
  ["occupation-ok", { ...flags, onboardingOccupation: "developer" }],
  ["null-optional", { ...flags, proactiveSuggestionsEnabled: null }],
  ["null-defaulted", { ...flags, memoryEnabled: null }],
  ["skipped-beta", { ...flags, skippedElectronUpdateVersions: { beta: "1" } }],
  ["skipped-ok", { ...flags, skippedElectronUpdateVersions: { stable: " 1.2.3 " } }],
  ["endpoint-normalized", { ...flags, zcodeEndpointOrigin: " https://x.example.com/path?q=1 " }],
  ["endpoint-dropped", { ...flags, zcodeEndpointOrigin: "ftp://x" }],
  ["endpoint-port", { ...flags, zcodeEndpointOrigin: "http://LOCALHOST:8080/a" }],
  [
    "window-size-ok",
    { ...flags, desktopWindowSize: { width: 800, height: 900, maximized: true, z: 1 } },
  ],
  ["viewport-bad", { ...flags, embeddedBrowserViewportPreference: { mode: "x" } }],
  [
    "viewport-ok",
    {
      ...flags,
      embeddedBrowserViewportPreference: {
        mode: "responsive",
        viewport: { width: 400, height: 900 },
        zoom: "75",
      },
    },
  ],
  [
    "viewport-extra-key",
    {
      ...flags,
      embeddedBrowserViewportPreference: {
        mode: "normal",
        viewport: { width: 400, height: 900 },
        zoom: "75",
        x: 1,
      },
    },
  ],
  ["shell-auto", { ...flags, integratedTerminalShell: { mode: "auto", extra: 1 } }],
  [
    "shell-ok",
    {
      ...flags,
      integratedTerminalShell: { mode: "shell", dialect: "cmd", id: " a ", label: "b", path: "c" },
    },
  ],
  [
    "shell-bad",
    {
      ...flags,
      integratedTerminalShell: { mode: "shell", dialect: "pwsh", id: "a", label: "b", path: "c" },
    },
  ],
  [
    "selections-ok",
    {
      ...flags,
      providerFamilyConnectionSelections: { zai: { kind: "start-plan" }, other: { kind: "x" } },
    },
  ],
  [
    "selections-team",
    {
      ...flags,
      providerFamilyConnectionSelections: {
        bigmodel: {
          kind: "team-coding-plan",
          productId: " p ",
          organizationId: "o",
          projectId: "j",
        },
      },
    },
  ],
  [
    "selections-extra",
    { ...flags, providerFamilyConnectionSelections: { zai: { kind: "start-plan", x: 1 } } },
  ],
  [
    "domain",
    {
      ...flags,
      providerFamilyDomain: "zai",
      providerFamilyDomainUpdatedAt: 5,
      providerFamilyDomainMigrated: true,
    },
  ],
  ["domain-bad", { ...flags, providerFamilyDomain: "other" }],
  [
    "sessions",
    {
      ...flags,
      lastWorkspaceSession: [
        { kind: "local", workspacePath: "/a", extra: 1 },
        { kind: "local", workspacePath: "/c", workspacePurpose: "conversation" },
        {
          kind: "remote",
          workspacePath: "/r",
          target: { ...ssh, resourcePackages: { selectedPackageIds: ["x"] } },
          lastOpenedAt: 3,
          lastConnectionStatus: "connected",
          y: 2,
        },
        {
          kind: "remote",
          workspacePath: "/w",
          target: { kind: "wsl", distro: "Ubuntu", user: " me " },
          lastOpenedAt: 0,
          lastConnectionStatus: "failed",
          lastConnectionError: "e",
        },
        {
          kind: "remote",
          workspacePath: "/d",
          target: { kind: "docker", container: "c1" },
          lastOpenedAt: 1,
          lastConnectionStatus: "connected",
          workspaceIdentity: "id",
        },
      ],
    },
  ],
  [
    "session-empty-path",
    { ...flags, lastWorkspaceSession: [{ kind: "local", workspacePath: "" }] },
  ],
  [
    "session-unknown-kind",
    { ...flags, lastWorkspaceSession: [{ kind: "cloud", workspacePath: "/x" }] },
  ],
  [
    "legacy-history",
    {
      ...flags,
      lastWorkspaceSession: [
        { kind: "remote", historyId: "h1" },
        { kind: "remote", historyId: "missing" },
      ],
      remoteWorkspaceHistory: [
        {
          id: "h1",
          workspacePath: "/h",
          target: { kind: "docker", container: "c" },
          lastOpenedAt: 2,
          lastConnectionStatus: "failed",
          localWorkspacePath: "",
        },
      ],
      lastOpenTabs: ["/h", "/x"],
    },
  ],
  [
    "records",
    {
      ...flags,
      shortcutBindings: { a: ["x", "y"] },
      lastActiveTaskByWorkspace: { "/a": "t1" },
      recentProjects: ["/p"],
    },
  ],
  ["records-bad", { ...flags, shortcutBindings: { a: "x" } }],
  [
    "release-notes",
    {
      ...flags,
      pendingPostUpdateReleaseNotes: {
        version: "1",
        title: " t ",
        markdown: "m",
        releaseNotesByLocale: { "en-US": { title: "T", markdown: "M" } },
      },
    },
  ],
  [
    "release-notes-bad-locale",
    {
      ...flags,
      pendingPostUpdateReleaseNotes: {
        version: "1",
        title: "t",
        markdown: "m",
        releaseNotesByLocale: { fr: { title: "T", markdown: "M" } },
      },
    },
  ],
  ["locale-preference-kept", { ...flags, locale: "en-US", localePreference: "system" }],
  [
    "legacy-account",
    {
      ...flags,
      modelProviderFamilySelectedKeys: {
        zai: "coding-plan:builtin:zai-coding-plan",
        bigmodel: "team-plan:builtin:bigmodel-coding-plan:p%201:o:j",
      },
      modelProviderFamilyModes: { zai: "oauth" },
    },
  ],
  [
    "legacy-account-apikey",
    {
      ...flags,
      modelProviderFamilySelectedKeys: { zai: "coding-plan:builtin:zai-coding-plan" },
      modelProviderFamilyModes: { zai: "apiKey" },
    },
  ],
  [
    "legacy-account-incomplete",
    { ...flags, modelProviderFamilySelectedKeys: { zai: "team-plan:builtin:zai-coding-plan:p:j" } },
  ],
];

const current = appSettingsSchema.parse({
  ...flags,
  locale: "zh-CN",
  httpProxy: "http://p",
  recentProjects: ["/a"],
  providerFamilyDomain: "zai",
});
const patches: [string, Record<string, unknown>][] = [
  ["locale", { locale: "en-US" }],
  ["locale-with-preference", { locale: "en-US", localePreference: "system" }],
  ["clear-proxy", { httpProxy: "   " }],
  ["trim-font", { terminalFontFamily: " Menlo " }],
  ["shell-auto-clears", { integratedTerminalShell: { mode: "auto" } }],
  [
    "shell-trim",
    {
      integratedTerminalShell: {
        mode: "shell",
        dialect: "git-bash",
        id: " i ",
        label: " l ",
        path: " p ",
      },
    },
  ],
  ["domain-empty", { providerFamilyDomain: "" }],
  ["domain-blank", { providerFamilyDomain: "  " }],
  ["unknown-key", { bogus: 1, memoryEnabled: true }],
  ["null-rejected", { desktopZoomLevel: null }],
  ["occupation-null", { onboardingOccupation: null }],
  ["endpoint-bad", { zcodeEndpointOrigin: "nope" }],
  ["endpoint-good", { zcodeEndpointOrigin: "https://a.example/x" }],
  ["recent", { recentProjects: ["/x", "/y"] }],
  ["invalid-type", { memoryEnabled: "yes" }],
];

function mergeWith(patch: Record<string, unknown>): unknown {
  const parsed = appSettingsPatchSchema.safeParse(normalizeSettingsPatch(patch as never));
  if (!parsed.success) return { error: true };
  return { merged: appSettingsSchema.parse({ ...current, ...parsed.data }) };
}

process.stdout.write(
  `${JSON.stringify(
    {
      reads: reads.map(([name, raw]) => {
        const result = appSettingsSchema.safeParse(migrateLegacyAccountConnectionSettings(raw));
        return {
          name,
          raw,
          expected: result.success ? result.data : null,
          incompleteLegacyTeam: readIncompleteLegacyTeamConnections(raw).length > 0,
        };
      }),
      current,
      patches: patches.map(([name, patch]) => ({ name, patch, expected: mergeWith(patch) })),
    },
    null,
    2,
  )}\n`,
);
