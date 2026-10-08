# Rust Host：setting 数据目录迁移（setting）

日期：2026-10-09。上级规格：[rust-host.md](rust-host.md)；R1 的 `setting.get` / `update` / `ensureDefaultProject` 见该文 R1 一行与 [rust-host-r2.md](rust-host-r2.md)。分支：`feat/rust-host-p-datadir`（基于 `feat/rust-host` @ `548c7a7`）。

契约扫描发现两个成员在 Rust Host 里从未被引用。本文补上 R1 setting 部分缺的 `updateDataBaseDir`，并记录另一个成员保持 stub 的理由。

## 1. 范围

| 成员                                     | Node 依据                                                                                                                               | Rust Host                                                                                                   |
| ---------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `setting.updateDataBaseDir(newDir?)`     | `settingService.ts` 的 `updateDataBaseDir`；`paths.ts` 的 `getDataBaseDir`、`validateDataBaseDirTarget`、`copyDataDirectory`（`fs.cp`） | 实现：`settings/data_dir/`（`mod.rs` 流程、`guard.rs` 守卫、`copy.rs` 复制）                                |
| `zcode-task.setAssistantMessageFeedback` | `zcodeTaskServiceAdapter.ts`：读 legacy `getTaskSnapshot`，改内存里的副本后返回，不落盘                                                 | 保持 stub：依赖 Rust Host 有意不实现的 legacy 快照，UI 无调用方（[tasks 规格](rust-host-tasks.md) 第 5 节） |

R1 已实现的其他 setting 成员不变。

## 2. 产品规则

### 2.1 入口与参数

1. **UI**：设置页 → General → 数据存储路径（`DataBaseDirControl`）。“Choose folder” 经 `IPlatformService.selectDirectory`（Main 的 `dialog.showOpenDialog`）取得真实目录，只更新草稿；“Save” 调 `updateDataBaseDir(dir || undefined)`，选中目录等于 home 时传 `undefined`（恢复默认）。成功显示“需要重启”，非守卫错误一律显示“数据复制失败，路径未更改”，守卫错误（按 `code` 或消息里的错误码识别）显示“不能放在安装目录”。
2. **目标**：`target = newDir.trim()`；为空（含 `undefined`）时用 `os.homedir()`，即 `HostEnvironment.user_home`（`HOME`，Windows 上 `USERPROFILE`），不受 `ZCODE_DESKTOP_HOME_DIR` 影响。
3. **当前数据根**：`HostEnvironment.data_base_dir`（`ZCODE_DATA_BASE_DIR`，否则 `HOME`），与 Node Host 的 `getDataBaseDir()` 相同，在 Host 生命周期内不变。迁移成功后 Host 继续用旧目录，直到重启时 Main 读 `setting.json` 的 `dataBaseDir`、经 `ZCODE_DATA_BASE_DIR` 下发新目录（UI 的 `requiresRestart`）。所以同一次运行里再迁移一次，仍从启动时的数据根复制。

### 2.2 顺序

1. **校验**（2.3）。命中时抛 `Error`：message 为 `DATA_BASE_DIR_FORBIDDEN_WINDOWS_INSTALL_DIR: <forbiddenDir>`，`code` 为 `DATA_BASE_DIR_FORBIDDEN_WINDOWS_INSTALL_DIR`。不复制，也不写设置。
2. **复制**：当前数据根与 `target` 的字符串不同时复制（2.4），相同时跳过。前后各记一条 info 日志。
3. **写设置**：走 `SettingStore::update`，也就是 Host 内唯一的写路径（进程内队列、`setting.json` 跨进程锁、原子写）。patch 是 `{dataBaseDir: newDir}`，用原始参数，不是 `target`：
   - 字符串：schema 去掉首尾空白后写入；只有空白时报 `ZodError`。这时复制已经做完，与 Node 一致。
   - `undefined`：删除 `dataBaseDir` 字段。
   - 成功后 `SettingStore` 的 updated 事件带 `["dataBaseDir"]`（Node 经 observable 的 `this.update` 同样发 `onDidUpdate`）。
4. **失败**：复制或写设置的错误原样返回。已复制的文件不回滚；复制失败时 `setting.json` 不变。

### 2.3 Windows 安装目录守卫（`validateDataBaseDirTarget`）

1. **只在 Windows 生效**：调用点按 `cfg!(windows)` 传平台，其他平台直接通过。判定本身是纯函数，参数包括平台、环境、`appInstallDir`、cwd，任何平台都能单测。
2. **候选目录**（按顺序；规范化后重复的只保留第一个原文）：
   - `appInstallDir`（Host 不传）；
   - `ZCODE_WINDOWS_APP_INSTALL_DIR`（Main 下发）；
   - `win32.join(ProgramFiles, "ZCode")`、`ProgramFiles(x86)`、`ProgramW6432`；
   - `win32.join(LOCALAPPDATA, "Programs", "ZCode")`。

   读环境变量时先按原键取，去掉首尾空白后非空才用；否则不分大小写，取第一个非空的值。

3. **比较**：两边各自 trim → `win32.normalize` → 去掉尾部分隔符 → `win32.resolve`（相对路径按 Host 进程 cwd，盘符相对路径按 `=X:` 环境变量）→ 去掉尾部分隔符 → 小写。两者相等，或目标以 `候选 + "\"` 开头，即为命中；`forbiddenDir` 取命中候选的原文。
4. **路径语义**：Rust 按 Node `path.win32` 的 `normalize`、`resolve`、`join` 逐行移植（纯词法，包括 UNC、`\\?\` 与 `\\.\` 设备根、保留设备名），与真实 TS 对拍。

### 2.4 复制（`copyDataDirectory`）

Node 调用 `fs.cp(<old>/.zcode/v2, <new>/.zcode/v2, {recursive: true, force: false, filter})`。Rust 按 Node 24 的 `lib/internal/fs/cp/cp.js` 逐步移植，全部用 tokio 异步 IO。

1. **路径**：`path.join(base, ".zcode", "v2")`，按词法规范化，不解析软链。
2. **filter**：对每一层的每个条目执行，顶层的 `v2` 也不例外。
   - basename 是 `setting.json` 或以 `setting.json.` 开头的，不论在哪一层都跳过。这类文件是 bootstrap 文件和原子写的中间态，比如锁目录 `setting.json.lock`、临时文件 `setting.json.*.tmp`。
   - lstat 是软链的跳过。如果顶层 `v2` 本身就是软链，整个复制什么也不做，也不报错。
   - lstat 失败的放行，交给后续步骤处理。
3. **顶层检查**：对应 `checkPaths`、`checkParentPaths`、`checkParentDir`。
   - src 的 lstat 失败（例如不存在）时，报 `ENOENT: no such file or directory, lstat '<src>'`；
   - dest 已存在且与 src 是同一 inode：`ERR_FS_CP_EINVAL`（`src and dest cannot be the same`）；
   - src 是目录、dest 不是目录：`ERR_FS_CP_DIR_TO_NON_DIR`；src 不是目录、dest 是目录：`ERR_FS_CP_NON_DIR_TO_DIR`；
   - src 是目录，且 resolve 后的 dest 按路径段位于 src 之下：`ERR_FS_CP_EINVAL`（`cannot copy <src> to a subdirectory of self <dest>`）；
   - 从 dest 的父目录往上逐级 stat（跟随软链），遇到与 src 同一 inode 的同样报上一条；遇到不存在的父目录、src 的父目录或根目录时停止；
   - dest 的父目录不存在时 `mkdir -p`。
4. **逐项复制**：子项同样做 filter 和上面的类型冲突检查。
   - 目录：dest 不存在时先 mkdir，再递归，最后 chmod 成 src 的 mode；dest 已存在时直接合并，不改 mode。
   - 文件（含字符设备、块设备）：dest 不存在时 copyFile，再 chmod 成 src 的 mode；dest 已存在（非目录）时静默跳过，不覆盖。
   - socket、FIFO、未知类型：分别报 `ERR_FS_CP_SOCKET`、`ERR_FS_CP_FIFO_PIPE`、`ERR_FS_CP_UNKNOWN`。
5. **执行方式**：顺序执行，第一个错误即中止，已复制的部分保留。
6. **错误形态**：
   - 系统调用错误：`name` 为 `Error`，`code` 为 libuv 错误码，消息为 `CODE: 描述, syscall '路径'`；copyfile 的路径部分是 `'src' -> 'dest'`。
   - cp 自身的错误：`name` 为 `SystemError`，`code` 为 `ERR_FS_CP_*`，消息与 Node 的 SystemError 相同，即 `<标题>: cp returned <errno 名> (<细节>) <路径>`。

## 3. 状态所有者

| 状态 / 能力                     | 唯一所有者                                                                                     |
| ------------------------------- | ---------------------------------------------------------------------------------------------- |
| `setting.json` 的 `dataBaseDir` | `SettingStore`（Host 内唯一写路径；与 Main、其他 Host 共用跨进程锁）                           |
| 本次运行的数据根                | `HostEnvironment.data_base_dir`（启动时由 Main 下发的 `ZCODE_DATA_BASE_DIR` 决定，运行期不变） |
| 下次启动的数据根                | Main：启动时读 `setting.json` 的 `dataBaseDir` → `setDataBaseDir` → Host 环境变量              |
| 目录选择                        | Main 的原生对话框（`IPlatformService.selectDirectory`）                                        |
| 迁移进度与结果展示              | Renderer `DataBaseDirControl` 的局部状态（saving / saved / error），不是服务端事实             |

## 4. 事件顺序

```text
Renderer（设置页）          Main                          Rust Host（setting）                  文件系统
 │ Choose folder ─────────► dialog.showOpenDialog
 │ ◄────────────────────── 目录（只更新草稿）
 │ Save ── updateDataBaseDir(dir | undefined) ──────────► DataDirMove
 │                                                        ├─ target = newDir.trim() || user_home
 │                                                        ├─ 守卫（仅 Windows）── 命中 ─► Error(code)
 │                                                        ├─ current ≠ target ─► cp(.zcode/v2) ─► <target>/.zcode/v2
 │                                                        └─ SettingStore.update({dataBaseDir}) ─锁─► <home>/.zcode/v2/setting.json
 │ ◄──────────────────────────────────────────────────── ok / error
 │ 显示“需要重启”
重启 ─► Main 读 setting.json.dataBaseDir ─► 新 Host 的 ZCODE_DATA_BASE_DIR = <target>
```

## 5. 有意与 Node 不同之处

| Node 行为                                                  | Rust Host                                                          | 理由                                                              |
| ---------------------------------------------------------- | ------------------------------------------------------------------ | ----------------------------------------------------------------- |
| 参数为 `null` 时 target 回退 home，写设置时报 `ZodError`   | `null` 按 `undefined` 处理（删除 `dataBaseDir`）                   | RPC 解码不区分 `undefined` 与 `null`；UI 只传字符串或 `undefined` |
| `ZodError` 的 message 是 zod issues 的 JSON                | `invalid dataBaseDir`                                              | 与 R1 `setting.update` 相同；UI 只看是否失败                      |
| filter 放行后、cp 读取前条目变成软链（竞态）时按软链复制   | 跳过                                                               | filter 已经跳过所有软链，剩下的只是微秒级的竞态窗口               |
| Windows 上 `areIdentical` 比较 `dev` / `ino`               | Windows 上比较 `canonicalize` 后的路径；任一侧是软链即视为不同     | 标准库没有稳定的 file index；检查对象都是目录，结论相同           |
| 多个只有大小写不同的同名环境变量：按插入顺序取第一个非空值 | 按字典序取第一个非空值                                             | Windows 的环境变量不分大小写，同名键只会有一个                    |
| 不在常用表里的 errno 按 libuv 全表给出名字和描述           | 常用码（ENOENT、EACCES、EPERM、ENOTDIR 等）相同，其余报 `EIO` 文案 | UI 对非守卫错误统一显示“复制失败”                                 |

## 6. 验收

- `pnpm exec tsx scripts/rust-host/setting-data-dir-fixtures.ts [--check]`：在临时沙盒里用真实 TS 代码执行 `setting-data-dir-scenarios.ts` 的输入，生成 `settings/fixtures/data-dir-parity.json`，内容包括：
  - `path.win32` 的 normalize / resolve / join；
  - 守卫判定，覆盖安装目录本身、子目录、同前缀的兄弟目录、大小写、正斜杠、`..`、UNC、环境变量大小写与空白、候选去重、cwd 与 `=X:`，以及非 Windows 平台；
  - `copyDataDirectory` 的 10 个场景：新目录、合并已有目录、目录覆盖文件、文件覆盖目录、源目录不存在、目标在源目录内、同一目录、源 `v2` 是软链、目标根是指向源根的软链、FIFO；
  - `updateDataBaseDir` 的 5 个场景：去空白、恢复默认、目标等于当前根、只有空白、复制失败。

  每一项都记录返回值或错误（name、code、message）、落盘树（类型、mode、内容、软链目标），以及 `setting.json`。`scripts/rust-host/test.mjs` 先跑 `--check`。

- `cargo test -p zcode-host-services --lib settings::data_dir`：逐项复现 fixture，并补充以下单测：
  - 守卫的纯函数（Windows 判定、`cfg` 选择平台）；
  - 只读目录在子项复制完之后才 chmod；
  - updated 事件带 `dataBaseDir`；
  - 守卫命中时不复制、不写设置。

  所有测试只用临时目录。

- 桌面 E2E `setting-data-dir`（隔离 HOME 内的 Electron + Rust Host）：
  1. 设置页 → General → 数据存储路径 → Choose folder → Save，看到“需要重启”；
  2. 断言 `<target>/.zcode/v2` 有预置的数据文件、`provider_config.json` 与 `tasks-index.sqlite`，没有 `setting.json`；`<home>/.zcode/v2/setting.json` 的 `dataBaseDir` 等于 target；
  3. 重启后发一条消息：新任务出现在 `<target>` 的 tasks-index，而不在旧目录（Main 按 `dataBaseDir` 给新的 Rust Host 下发数据根）。

  原生目录对话框经 `--inspect` 连上 Main，替换 `dialog.showOpenDialog`；它是 `IPlatformService.selectDirectory` 在桌面端的唯一后端。
