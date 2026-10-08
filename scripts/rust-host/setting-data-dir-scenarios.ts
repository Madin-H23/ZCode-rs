// setting.updateDataBaseDir 对拍 fixture 的场景数据（docs/specs/rust-host-setting.md §6），
// 由 setting-data-dir-fixtures.ts 用真实 TS 代码执行；Rust 侧从生成的 fixture 读取同一份数据。
// ---------------------------------------------------------------------------
// path.win32：cwd 固定为 C:\work，D: 盘的当前目录（`=D:`）为 D:\dcwd。
// ---------------------------------------------------------------------------

export const WIN32_CWD = "C:\\work";
export const WIN32_DRIVE_CWDS: Record<string, string> = { "=D:": "D:\\dcwd" };

export const WIN32_INPUTS = [
  "",
  ".",
  "..",
  "/",
  "\\",
  "\\\\",
  "\\\\\\x",
  "C:",
  "c:",
  "C:\\",
  "C:\\a\\b",
  "C:/a/../b/",
  "C:\\a\\.\\b\\\\c\\\\",
  "C:\\..\\..",
  "C:foo",
  "D:foo\\..\\bar",
  "E:rel",
  "\\\\server\\share",
  "\\\\server\\share\\x\\..\\..",
  "//server/share/",
  "\\\\server",
  "\\\\?\\C:\\x",
  "\\\\?\\COM1:x",
  "\\\\.\\PHYSICALDRIVE0",
  "\\foo\\..\\..\\bar",
  "foo/bar/..",
  "..\\..\\x",
  "ZCode",
  "CON",
  "CONx",
  "CON:",
  "COM1:x",
  "com\u00b9:y",
  "a:b",
  "x:\\y:z",
  "1:\\x",
  "  C:\\x  ",
  "C:\\Program Files\\ZCode.",
  "C:\\Program Files\\\u00e9t\u00e9",
];

export const WIN32_JOINS: string[][] = [
  ["C:\\Program Files", "ZCode"],
  ["c:\\program files\\", "ZCode"],
  ["C:\\Users\\u\\AppData\\Local", "Programs", "ZCode"],
  ["//server", "share"],
  ["\\\\server\\share", "x"],
  ["///x", "y"],
  ["", ""],
  ["a", "", "b"],
  ["CON:", "x"],
  ["a", "b:c"],
  ["C:", "x"],
  ["/a/", "/b/"],
];

// ---------------------------------------------------------------------------
// validateDataBaseDirTarget
// ---------------------------------------------------------------------------

const PF = {
  ProgramFiles: "C:\\Program Files",
  "ProgramFiles(x86)": "C:\\Program Files (x86)",
  ProgramW6432: "C:\\Program Files",
  LOCALAPPDATA: "C:\\Users\\u\\AppData\\Local",
};

export interface ValidationCase {
  name: string;
  target: string;
  env: Record<string, string>;
  appInstallDir?: string;
  platform?: string;
  cwd?: string;
}

export const VALIDATION: ValidationCase[] = [
  { name: "install dir itself", target: "C:\\Program Files\\ZCode", env: PF },
  { name: "inside, forward slashes, lower case", target: "c:/program files/zcode/data", env: PF },
  { name: "sibling with same prefix", target: "C:\\Program Files\\ZCodeData", env: PF },
  { name: "dot-dot leaves install dir", target: "C:\\Program Files\\ZCode\\..\\Other", env: PF },
  { name: "trailing separator", target: "C:\\Program Files\\ZCode\\", env: PF },
  { name: "dot segments", target: "C:\\Program Files\\.\\ZCode\\.", env: PF },
  { name: "trailing dot name", target: "C:\\Program Files\\ZCode.", env: PF },
  { name: "padded with spaces", target: "  C:\\Program Files\\ZCode  ", env: PF },
  { name: "x86", target: "C:\\Program Files (x86)\\ZCode\\x", env: PF },
  { name: "local app data", target: "C:\\Users\\u\\AppData\\Local\\Programs\\ZCode", env: PF },
  { name: "other drive", target: "D:\\Data", env: PF },
  {
    name: "lower-case env keys",
    target: "C:\\PF\\ZCode\\x",
    env: { programfiles: "C:\\PF" },
  },
  {
    name: "blank exact key falls back to case-insensitive match",
    target: "E:\\Apps\\ZCode",
    env: { ProgramFiles: "   ", programfiles: "E:\\Apps" },
  },
  {
    name: "custom install dir with trailing separator",
    target: "d:\\tools\\zcode custom\\data",
    env: { ZCODE_WINDOWS_APP_INSTALL_DIR: "  D:\\Tools\\ZCode Custom\\  " },
  },
  {
    name: "app install dir option wins",
    target: "F:\\ZCode",
    env: { ...PF, ZCODE_WINDOWS_APP_INSTALL_DIR: "F:\\ZCode\\" },
    appInstallDir: "F:\\ZCode",
  },
  {
    name: "UNC install dir",
    target: "//server/share/zcode/x",
    env: { ZCODE_WINDOWS_APP_INSTALL_DIR: "\\\\server\\share\\ZCode" },
  },
  { name: "verbatim prefix", target: "\\\\?\\C:\\Program Files\\ZCode", env: PF },
  {
    name: "duplicate candidates keep the first spelling",
    target: "C:\\PROGRAM FILES\\ZCODE",
    env: { ProgramFiles: "C:\\Program Files", ProgramW6432: "c:\\program files\\" },
  },
  { name: "rooted path takes the cwd drive", target: "\\Program Files\\ZCode", env: PF },
  {
    name: "relative path resolves against cwd",
    target: "ZCode\\data",
    env: PF,
    cwd: "C:\\Program Files",
  },
  {
    name: "drive-relative path uses =C:",
    target: "C:ZCode",
    env: { ...PF, "=C:": "C:\\Program Files" },
  },
  {
    name: "drive-relative path without =X: falls back to drive root",
    target: "C:ZCode",
    env: PF,
    cwd: "D:\\x",
  },
  { name: "no candidates", target: "C:\\Program Files\\ZCode", env: {} },
  { name: "not windows (darwin)", target: "C:\\Program Files\\ZCode", env: PF, platform: "darwin" },
  { name: "not windows (linux)", target: "C:\\Program Files\\ZCode", env: PF, platform: "linux" },
];

// ---------------------------------------------------------------------------
// copyDataDirectory / updateDataBaseDir
// ---------------------------------------------------------------------------

/** 沙盒内的条目：路径相对沙盒根；symlink 的 target 里 `<ROOT>` 换成沙盒根。 */
export type Entry =
  | { path: string; type: "dir"; mode?: number }
  | { path: string; type: "file"; content: string; mode?: number }
  | { path: string; type: "symlink"; target: string }
  | { path: string; type: "fifo" };

const dir = (path: string, mode?: number): Entry => ({ path, type: "dir", mode });
const file = (path: string, content: string, mode?: number): Entry => ({
  path,
  type: "file",
  content,
  mode,
});
const link = (path: string, target: string): Entry => ({ path, type: "symlink", target });

const V2 = "old/.zcode/v2";
/** 一棵典型的数据目录：嵌套、空目录、只读目录、各种 setting.json 变体与软链。 */
const DATA_TREE: Entry[] = [
  dir(V2, 0o700),
  file(`${V2}/tasks-index.sqlite`, "sqlite bytes", 0o600),
  file(`${V2}/provider_config.json`, '{"schemaVersion":1}', 0o644),
  file(`${V2}/setting.json`, '{"dataBaseDir":"<old>"}'),
  dir(`${V2}/setting.json.lock`),
  file(`${V2}/setting.json.lock/owner`, "pid"),
  file(`${V2}/setting.json.123.tmp`, "partial"),
  file(`${V2}/setting.jsonx`, "not a bootstrap file"),
  dir(`${V2}/sessions`, 0o750),
  dir(`${V2}/sessions/abc123`),
  file(`${V2}/sessions/abc123/task.json`, '{"id":"task"}', 0o640),
  file(`${V2}/sessions/abc123/setting.json`, "nested bootstrap name"),
  dir(`${V2}/empty`, 0o711),
  dir(`${V2}/read-only`, 0o555),
  file(`${V2}/read-only/frozen.txt`, "frozen", 0o444),
  file(`${V2}/script.sh`, "#!/bin/sh\n", 0o755),
  link(`${V2}/linked-file`, "<ROOT>/outside/file.txt"),
  link(`${V2}/linked-dir`, "<ROOT>/outside"),
  link(`${V2}/dangling`, "<ROOT>/missing"),
  dir("outside"),
  file("outside/file.txt", "outside"),
];

export interface CopyCase {
  name: string;
  kind: "copy";
  tree: Entry[];
  from: string;
  to: string;
  /** 结束后转储的目录（相对沙盒根）。 */
  dump: string[];
}

export interface UpdateCase {
  name: string;
  kind: "update";
  tree: Entry[];
  /** Host 启动时的数据根（相对沙盒根）。 */
  current: string;
  /** updateDataBaseDir 的参数；null 表示 undefined。`<ROOT>` 换成沙盒根。 */
  newDir: string | null;
  dump: string[];
}

const SEEDED_SETTINGS = JSON.stringify({
  dataBaseDir: "<ROOT>/custom",
  recentProjects: ["/work/a"],
  closeToTrayOnWindowsMigrationInitialized: true,
  messageStreamShowReasoningMigrationInitialized: true,
});

export const CASES: Array<CopyCase | UpdateCase> = [
  {
    name: "fresh target",
    kind: "copy",
    tree: DATA_TREE,
    from: "old",
    to: "new",
    dump: ["new"],
  },
  {
    name: "merge into existing target",
    kind: "copy",
    tree: [
      ...DATA_TREE,
      dir("new/.zcode/v2", 0o755),
      file("new/.zcode/v2/provider_config.json", "existing wins", 0o600),
      dir("new/.zcode/v2/sessions", 0o700),
      file("new/.zcode/v2/sessions/keep.json", "target only"),
      file("new/.zcode/v2/linked-file", "regular file where source has a symlink"),
      file("new/.zcode/v2/setting.json", "target bootstrap stays"),
    ],
    from: "old",
    to: "new",
    dump: ["new"],
  },
  {
    name: "directory over existing file",
    kind: "copy",
    tree: [
      dir(V2),
      dir(`${V2}/sessions`),
      file(`${V2}/sessions/a.json`, "a"),
      dir("new/.zcode/v2"),
      file("new/.zcode/v2/sessions", "a file"),
    ],
    from: "old",
    to: "new",
    dump: ["new"],
  },
  {
    name: "file over existing directory",
    kind: "copy",
    tree: [
      dir(V2),
      file(`${V2}/tasks-index.sqlite`, "db"),
      dir("new/.zcode/v2/tasks-index.sqlite"),
    ],
    from: "old",
    to: "new",
    dump: ["new"],
  },
  {
    name: "missing source",
    kind: "copy",
    tree: [dir("old")],
    from: "old",
    to: "new",
    dump: ["new"],
  },
  {
    name: "target inside source",
    kind: "copy",
    tree: [dir(V2), file(`${V2}/a.txt`, "a")],
    from: "old",
    to: "old/.zcode/v2/nested",
    dump: ["old/.zcode/v2"],
  },
  {
    name: "same directory spelled differently",
    kind: "copy",
    tree: [dir(V2), file(`${V2}/a.txt`, "a")],
    from: "old",
    to: "old/./",
    dump: ["old/.zcode/v2"],
  },
  {
    name: "source v2 is a symlink",
    kind: "copy",
    tree: [
      dir("real/v2"),
      file("real/v2/a.txt", "a"),
      dir("old/.zcode"),
      link(V2, "<ROOT>/real/v2"),
    ],
    from: "old",
    to: "new",
    dump: ["new"],
  },
  {
    name: "target base is a symlink to the source base",
    kind: "copy",
    tree: [dir(V2), file(`${V2}/a.txt`, "a"), link("new", "<ROOT>/old")],
    from: "old",
    to: "new",
    dump: ["old/.zcode/v2"],
  },
  {
    name: "fifo in source",
    kind: "copy",
    // 目标 v2 预先存在：出错时新建目录还没 chmod，mode 会随 umask 变化。
    tree: [dir(V2), { path: `${V2}/pipe`, type: "fifo" }, dir("new/.zcode/v2")],
    from: "old",
    to: "new",
    dump: ["new"],
  },
  {
    name: "update: padded target",
    kind: "update",
    tree: [...DATA_TREE, file("home/.zcode/v2/setting.json", SEEDED_SETTINGS)],
    current: "old",
    newDir: "  <ROOT>/new  ",
    dump: ["new", "home/.zcode/v2/setting.json"],
  },
  {
    name: "update: reset to home",
    kind: "update",
    tree: [
      dir("custom/.zcode/v2"),
      file("custom/.zcode/v2/tasks-index.sqlite", "custom db", 0o600),
      file("custom/.zcode/v2/setting.json", "stale copy"),
      file("home/.zcode/v2/setting.json", SEEDED_SETTINGS),
      file("home/.zcode/v2/tasks-index.sqlite", "home db"),
    ],
    current: "custom",
    newDir: null,
    dump: ["home"],
  },
  {
    name: "update: target equals current root",
    kind: "update",
    tree: [file("home/.zcode/v2/setting.json", SEEDED_SETTINGS)],
    current: "same",
    newDir: "<ROOT>/same",
    dump: ["same", "home/.zcode/v2/setting.json"],
  },
  {
    name: "update: blank target copies home then fails validation",
    kind: "update",
    tree: [
      dir("custom/.zcode/v2"),
      file("custom/.zcode/v2/a.txt", "a"),
      file("home/.zcode/v2/setting.json", SEEDED_SETTINGS),
    ],
    current: "custom",
    newDir: "   ",
    dump: ["home"],
  },
  {
    name: "update: copy failure leaves settings untouched",
    kind: "update",
    tree: [file("home/.zcode/v2/setting.json", SEEDED_SETTINGS)],
    current: "missing",
    newDir: "<ROOT>/new",
    dump: ["new", "home/.zcode/v2/setting.json"],
  },
];
