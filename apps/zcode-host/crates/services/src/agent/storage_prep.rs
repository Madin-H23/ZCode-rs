//! 启动期的会话库准备（docs/specs/rust-host-agent.md §2.11；hostDatabaseStartup.ts、
//! storagePreparationProcesses.ts、zcodeAgentStorageProcess.ts）。
//!
//! ```text
//! 对每个候选目录：spawn <cli> --prepare-storage
//!   stdout: startup/storagePath{path} ─► realpath ─► stdin: {"method":"startup/storagePathReady","reuse"}
//!           startup/storageState{phase…} ─► 报告 preparing_session_storage/<phase>（failed 记录失败）
//!           startup/storagePrepared ─► 关 stdin
//!   exit 0 且已 prepared、无失败 ─► 记住真实路径（后续同一真实库回 reuse:true）
//! 任何非法帧 / 首帧 30 s 未到 / 非零退出 ─► 失败，回收进程树
//! ```
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use zcode_host_kit::log;

use super::command::{self, AgentCommand};
use super::process;
use super::schema::{self, Field, Ty};

const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LINE_BYTES: usize = 65_536;

/// 一次进度报告（转成 `preparing_session_storage` 的 DatabaseStartupState）。
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub database_phase: &'static str,
    pub database_id: String,
    pub migration: Option<Value>,
    pub final_database: bool,
}

/// 准备失败：已分类的 `errorCode` 与可跨进程传输的细节。
#[derive(Debug, Clone, PartialEq)]
pub struct PrepareError {
    pub error_code: &'static str,
    pub sqlite_code: Option<i64>,
    pub system_code: Option<String>,
    pub migration_id: Option<String>,
    /// `(databaseId, migration facts)`：失败帧带的迁移事实。
    pub migration: Option<(String, Value)>,
    pub message: String,
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.error_code)
    }
}

const ERROR_CODES: &[&str] = &[
    "storage_full",
    "permission_denied",
    "io_error",
    "out_of_memory",
    "corrupt",
    "open_failed",
    "lock_timeout",
    "checksum_mismatch",
    "sql_failed",
    "startup_status_timeout",
    "transport_closed",
    "unsupported_runtime",
];

fn static_code(kind: &str) -> Option<&'static str> {
    ERROR_CODES.iter().copied().find(|code| *code == kind)
}

/// classifyDatabaseStartupError：原始 SQLite / 系统码优先，再看 kind；禁止从文案推断。
pub fn classify(kind: &str, sqlite_code: Option<i64>, system_code: Option<&str>) -> &'static str {
    let primary = sqlite_code.map(|code| code & 0xff);
    let code = system_code.unwrap_or_default();
    if primary == Some(13) || matches!(code, "ENOSPC" | "EDQUOT") {
        return "storage_full";
    }
    if matches!(primary, Some(3 | 8)) || matches!(code, "EACCES" | "EPERM") {
        return "permission_denied";
    }
    if primary == Some(10) || code == "EIO" {
        return "io_error";
    }
    if primary == Some(7) || matches!(code, "ENOMEM" | "ERR_WORKER_OUT_OF_MEMORY") {
        return "out_of_memory";
    }
    if matches!(primary, Some(11 | 26)) {
        return "corrupt";
    }
    let mut fallback = "sql_failed";
    if primary == Some(14) {
        fallback = "open_failed";
    }
    if primary == Some(5) {
        fallback = "lock_timeout";
    }
    match static_code(kind) {
        Some(kind) if kind != "sql_failed" => kind,
        _ => fallback,
    }
}

fn valid_system_code(code: &str) -> bool {
    (1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_migration_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl PrepareError {
    /// storagePreparationProcesses.ts statusError。
    pub fn status(kind: &str, details: Option<&Value>) -> Self {
        let sqlite_code = details.and_then(|d| d["sqliteCode"].as_i64());
        let system_code = details
            .and_then(|d| d["systemCode"].as_str())
            .map(str::to_owned);
        let migration_id = details
            .and_then(|d| d["migrationId"].as_str())
            .filter(|id| valid_migration_id(id))
            .map(str::to_owned);
        let migration = details.and_then(|d| {
            let facts = d.get("migration").filter(|facts| facts.is_object())?;
            Some((d["databaseId"].as_str()?.to_owned(), facts.clone()))
        });
        Self {
            error_code: classify(kind, sqlite_code, system_code.as_deref()),
            sqlite_code,
            system_code: system_code.filter(|code| valid_system_code(code)),
            migration_id,
            migration,
            message: format!("Storage preparation failed: {kind}"),
        }
    }

    fn io(error: &std::io::Error) -> Self {
        let system_code = process::errno_code(error).map(str::to_owned);
        Self {
            error_code: classify("", None, system_code.as_deref()),
            sqlite_code: None,
            system_code,
            migration_id: None,
            migration: None,
            message: error.to_string(),
        }
    }
}

/// 候选目录（hostDatabaseStartup.ts）：warmup 目标，否则 InitLocal.workspacePath，否则 fallback cwd；
/// 不是目录的回退到 fallback cwd（与普通 Agent 的 cwd 规则相同），去重保序。
pub async fn candidate_directories(
    warmup: &[String],
    workspace_path: Option<&str>,
    fallback_cwd: &Path,
) -> Vec<PathBuf> {
    let mut candidates: Vec<String> = warmup.to_vec();
    if candidates.is_empty()
        && let Some(path) = workspace_path.filter(|path| !path.is_empty())
    {
        candidates.push(path.to_owned());
    }
    if candidates.is_empty() {
        candidates.push(fallback_cwd.display().to_string());
    }
    let mut directories: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        let path = PathBuf::from(&candidate);
        let is_dir = tokio::fs::metadata(&path)
            .await
            .is_ok_and(|meta| meta.is_dir());
        let cwd = if is_dir {
            path
        } else {
            fallback_cwd.to_owned()
        };
        if !directories.contains(&cwd) {
            directories.push(cwd);
        }
    }
    directories
}

/// 准备进程的命令：原生 CLI 加 `--prepare-storage`；TS bundle 以存储入口运行
/// `app-server --stdio --prepare-storage --cwd <dir>`（Node 在 Worker 里跑同一入口）。
fn preparation_command(command: &AgentCommand, cwd: &Path) -> Option<AgentCommand> {
    if !command.supports_storage_startup {
        return None;
    }
    let mut prep = command.clone();
    prep.cwd = Some(command.cwd.clone().unwrap_or_else(|| cwd.to_owned()));
    match &command.storage_entry {
        Some(entry) => {
            prep.args = vec![
                entry.display().to_string(),
                "app-server".into(),
                "--stdio".into(),
                "--prepare-storage".into(),
                "--cwd".into(),
                cwd.display().to_string(),
            ];
        }
        None => prep.args.push("--prepare-storage".into()),
    }
    Some(prep)
}

const PATH_PARAMS: &[Field] = &[Field {
    name: "path",
    ty: Ty::Str {
        min: 1,
        max: Some(32_768),
    },
    required: true,
}];

const NONEMPTY128: Ty = Ty::Str {
    min: 1,
    max: Some(128),
};
const MIGRATION_FACTS: &[Field] = &[
    Field {
        name: "kind",
        ty: Ty::Enum(&["none", "initialize", "upgrade"]),
        required: true,
    },
    Field {
        name: "executedCount",
        ty: Ty::Int {
            nonnegative: true,
            positive: false,
        },
        required: true,
    },
    Field {
        name: "committedCount",
        ty: Ty::Int {
            nonnegative: true,
            positive: false,
        },
        required: true,
    },
];

/// zcodeStorageStartupStateSchema（strict）。
fn is_storage_state(params: &Value) -> bool {
    let Some(object) = params.as_object() else {
        return false;
    };
    let mut rest = object.clone();
    let migration = rest.remove("migration");
    const STATE: &[Field] = &[
        Field {
            name: "schemaVersion",
            ty: Ty::NumLiteral(1.0),
            required: true,
        },
        Field {
            name: "attemptId",
            ty: NONEMPTY128,
            required: true,
        },
        Field {
            name: "sequence",
            ty: Ty::Int {
                nonnegative: false,
                positive: true,
            },
            required: true,
        },
        Field {
            name: "databaseId",
            ty: NONEMPTY128,
            required: true,
        },
        Field {
            name: "databaseKind",
            ty: Ty::Enum(&["session", "tasks-index"]),
            required: true,
        },
        Field {
            name: "phase",
            ty: Ty::Enum(&[
                "checking",
                "waiting_for_lock",
                "migrating",
                "committing",
                "ready",
                "failed",
            ]),
            required: true,
        },
        Field {
            name: "elapsedMs",
            ty: Ty::Num { nonnegative: true },
            required: true,
        },
        Field {
            name: "completed",
            ty: Ty::Int {
                nonnegative: true,
                positive: false,
            },
            required: false,
        },
        Field {
            name: "total",
            ty: Ty::Int {
                nonnegative: true,
                positive: false,
            },
            required: false,
        },
        Field {
            name: "errorCode",
            ty: Ty::Enum(ERROR_CODES),
            required: false,
        },
        Field {
            name: "sqliteCode",
            ty: Ty::Int {
                nonnegative: false,
                positive: false,
            },
            required: false,
        },
        Field {
            name: "systemCode",
            ty: Ty::Str {
                min: 0,
                max: Some(64),
            },
            required: false,
        },
        Field {
            name: "migrationId",
            ty: Ty::Str {
                min: 0,
                max: Some(128),
            },
            required: false,
        },
    ];
    if !schema::check_object(&rest, STATE) {
        return false;
    }
    if let Some(migration) = migration {
        let Some(facts) = migration.as_object() else {
            return false;
        };
        let mut facts = facts.clone();
        let last = facts.remove("lastAppliedMigrationId");
        if !schema::check_object(&facts, MIGRATION_FACTS)
            || last.is_some_and(|id| !(id.is_null() || id.as_str().is_some_and(valid_migration_id)))
        {
            return false;
        }
        let executed = facts["executedCount"].as_f64().unwrap_or(0.0);
        let committed = facts["committedCount"].as_f64().unwrap_or(0.0);
        if committed > executed || (facts["kind"] == "none" && executed != 0.0) {
            return false;
        }
    }
    // failed 必须带 errorCode。
    object["phase"] != "failed" || object.contains_key("errorCode")
}

enum Frame {
    Path(String),
    Prepared,
    State(Value),
}

fn parse_frame(line: &str) -> Option<Frame> {
    let value: Value = serde_json::from_str(line).ok()?;
    let object = value.as_object()?;
    if object.len() != 2 || !object.contains_key("params") {
        return None;
    }
    let params = &object["params"];
    match object.get("method")?.as_str()? {
        "startup/storagePath" => params
            .as_object()
            .filter(|map| schema::check_object(map, PATH_PARAMS))
            .map(|map| Frame::Path(map["path"].as_str().unwrap_or_default().to_owned())),
        "startup/storagePrepared" => params
            .as_object()
            .filter(|map| map.is_empty())
            .map(|_| Frame::Prepared),
        "startup/storageState" => is_storage_state(params).then(|| Frame::State(params.clone())),
        _ => None,
    }
}

/// 准备一个目录的会话库；`prepared` 记录本次启动已准备过的真实路径。
pub async fn prepare_directory(
    command: &AgentCommand,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    prepared: &mut HashSet<PathBuf>,
    final_database: bool,
    report: &(dyn Fn(Progress) + Send + Sync),
) -> Result<(), PrepareError> {
    let Some(prep) = preparation_command(command, cwd) else {
        return Err(PrepareError::status("unsupported_runtime", None));
    };
    let mut env = env.clone();
    env.extend(prep.env.clone());
    let spawn_cwd = prep.cwd.clone().unwrap_or_else(|| cwd.to_owned());
    let spawned =
        process::spawn(&prep, &spawn_cwd, &env).map_err(|error| PrepareError::io(&error))?;
    let process::Spawned {
        mut child,
        pid,
        stdin,
        stdout,
        mut stderr,
    } = spawned;
    // stderr 只排空，不上报（可能含本地路径）。
    tokio::spawn(async move {
        let mut sink = Vec::new();
        let _ = stderr.read_to_end(&mut sink).await;
    });
    let mut stdin = Some(stdin);
    let mut reader = BufReader::new(stdout);
    let mut failure: Option<PrepareError> = None;
    let mut path_received = false;
    let mut prepared_flag = false;
    let mut prepared_path: Option<PathBuf> = None;
    let mut first_frame = true;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = async {
            let mut limited = (&mut reader).take(MAX_LINE_BYTES as u64 + 1);
            limited.read_until(b'\n', &mut line).await
        };
        let outcome = if first_frame {
            match tokio::time::timeout(FIRST_FRAME_TIMEOUT, read).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    failure.get_or_insert(PrepareError::status("startup_status_timeout", None));
                    break;
                }
            }
        } else {
            read.await
        };
        match outcome {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() > MAX_LINE_BYTES && !line.ends_with(b"\n") {
            failure.get_or_insert(PrepareError::status("transport_closed", None));
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\n', '\r']);
        if text.trim().is_empty() {
            continue;
        }
        let Some(frame) = parse_frame(text) else {
            failure.get_or_insert(PrepareError::status("transport_closed", None));
            break;
        };
        first_frame = false;
        match frame {
            Frame::Path(path) => {
                if path_received {
                    failure.get_or_insert(PrepareError::status("transport_closed", None));
                    break;
                }
                path_received = true;
                let real = match tokio::fs::canonicalize(&path).await {
                    Ok(real) => real,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        std::path::absolute(&path).unwrap_or_else(|_| PathBuf::from(&path))
                    }
                    Err(error) => {
                        failure.get_or_insert(PrepareError::io(&error));
                        break;
                    }
                };
                let reuse = prepared.contains(&real);
                prepared_path = Some(real);
                let reply = format!(
                    "{}\n",
                    json!({"method": "startup/storagePathReady", "reuse": reuse})
                );
                let written = match stdin.as_mut() {
                    Some(stdin) => stdin
                        .write_all(reply.as_bytes())
                        .await
                        .and(stdin.flush().await),
                    None => Err(std::io::Error::other("stdin closed")),
                };
                if let Err(error) = written {
                    failure.get_or_insert(PrepareError::io(&error));
                    break;
                }
            }
            Frame::Prepared => {
                prepared_flag = path_received;
                if let Some(mut stdin) = stdin.take() {
                    let _ = stdin.shutdown().await;
                }
            }
            Frame::State(state) => {
                let phase = state["phase"].as_str().unwrap_or_default();
                if phase == "failed" {
                    let kind = state["errorCode"].as_str().unwrap_or("sql_failed");
                    failure.get_or_insert(PrepareError::status(kind, Some(&state)));
                    continue;
                }
                report(Progress {
                    database_phase: match phase {
                        "checking" => "checking",
                        "waiting_for_lock" => "waiting_for_lock",
                        "migrating" => "migrating",
                        "committing" => "committing",
                        _ => "ready",
                    },
                    database_id: state["databaseId"].as_str().unwrap_or("session").to_owned(),
                    migration: state.get("migration").cloned(),
                    final_database,
                });
            }
        }
    }
    drop(stdin.take());
    let status = if failure.is_some() {
        process::terminate(&mut child, pid).await;
        child.try_wait().ok().flatten()
    } else {
        match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
            Ok(status) => status.ok(),
            Err(_) => {
                process::terminate(&mut child, pid).await;
                None
            }
        }
    };
    if let Some(failure) = failure {
        return Err(failure);
    }
    if status.is_some_and(|status| status.success()) && prepared_flag {
        if let Some(path) = prepared_path {
            prepared.insert(path);
        }
        return Ok(());
    }
    Err(PrepareError::status("transport_closed", None))
}

/// hostDatabaseStartup 的会话库阶段：每个候选目录先报 checking，再准备。
/// 解析不到 CLI 命令时跳过（见规格第 5 节），返回 Ok。
#[allow(clippy::too_many_arguments)]
pub async fn prepare_session_storage(
    command_vars: &BTreeMap<String, String>,
    search_roots: &[PathBuf],
    child_env: &BTreeMap<String, String>,
    warmup: &[String],
    workspace_path: Option<&str>,
    fallback_cwd: &Path,
    report: &(dyn Fn(Option<Progress>) + Send + Sync),
) -> Result<(), PrepareError> {
    let directories = candidate_directories(warmup, workspace_path, fallback_cwd).await;
    let mut prepared = HashSet::new();
    let count = directories.len();
    for (index, cwd) in directories.iter().enumerate() {
        let command = match command::resolve(command_vars, search_roots, cwd) {
            Ok(Some(command)) => command,
            Ok(None) => {
                log::warn("session storage preparation skipped: no ZCode agent command");
                return Ok(());
            }
            Err(error) => {
                log::warn(format!("session storage preparation: {error:#}"));
                return Err(PrepareError::status("unsupported_runtime", None));
            }
        };
        report(None);
        let progress = |progress: Progress| report(Some(progress));
        prepare_directory(
            &command,
            cwd,
            child_env,
            &mut prepared,
            index + 1 == count,
            &progress,
        )
        .await?;
    }
    Ok(())
}

/// 合并各数据库的迁移事实（DatabaseStartupCoordinator.report）。
#[derive(Debug, Default)]
pub struct MigrationFacts {
    baselines: Vec<(String, Option<Value>)>,
    facts: Vec<(String, Value)>,
}

impl MigrationFacts {
    pub fn record(&mut self, database_id: &str, migration: Option<&Value>) {
        let last = migration.and_then(|facts| facts.get("lastAppliedMigrationId").cloned());
        match self.baselines.iter_mut().find(|(id, _)| id == database_id) {
            // 仅首次可信锁内值可替换 unknown；后续无事实通知不能抹掉起点。
            Some((_, baseline)) if baseline.is_none() => *baseline = last,
            Some(_) => {}
            None => self.baselines.push((database_id.to_owned(), last)),
        }
        let Some(next) = migration else { return };
        let kind = |facts: &Value| facts["kind"].as_str().unwrap_or("none").to_owned();
        let merged_kind = match self.facts.iter().find(|(id, _)| id == database_id) {
            Some((_, previous)) => merge_kind(&kind(previous), &kind(next)),
            None => kind(next),
        };
        let mut next = next.clone();
        next["kind"] = json!(merged_kind);
        match self.facts.iter_mut().find(|(id, _)| id == database_id) {
            Some((_, facts)) => *facts = next,
            None => self.facts.push((database_id.to_owned(), next)),
        }
    }

    /// 聚合（无事实时 None）。
    pub fn aggregate(&self) -> Option<Value> {
        if self.facts.is_empty() {
            return None;
        }
        let mut kind = "none".to_owned();
        let (mut executed, mut committed) = (0u64, 0u64);
        for (_, facts) in &self.facts {
            kind = merge_kind(&kind, facts["kind"].as_str().unwrap_or("none"));
            executed += facts["executedCount"].as_u64().unwrap_or(0);
            committed += facts["committedCount"].as_u64().unwrap_or(0);
        }
        Some(json!({"kind": kind, "executedCount": executed, "committedCount": committed}))
    }

    pub fn baselines(&self) -> Vec<Value> {
        self.baselines
            .iter()
            .map(|(id, last)| {
                let mut entry = Map::new();
                entry.insert("databaseId".into(), json!(id));
                entry.insert(
                    "databaseKind".into(),
                    json!(if id == "tasks-index" {
                        "tasks-index"
                    } else {
                        "session"
                    }),
                );
                if let Some(last) = last {
                    entry.insert("lastAppliedMigrationId".into(), last.clone());
                }
                Value::Object(entry)
            })
            .collect()
    }
}

fn merge_kind(a: &str, b: &str) -> String {
    if a == "upgrade" || b == "upgrade" {
        "upgrade".into()
    } else if a == "initialize" || b == "initialize" {
        "initialize".into()
    } else {
        "none".into()
    }
}

#[cfg(test)]
#[path = "storage_prep_tests.rs"]
mod tests;

/// host-core 启动流程的入口：按 InitLocal 与 Host 环境准备会话库（docs §2.11）。
/// `report(None)` 表示进入一个新目录（checking），`report(Some)` 是该目录的数据库阶段。
pub async fn prepare_for_init(
    environment: &crate::HostEnvironment,
    init: &zcode_host_control::InitLocal,
    report: &(dyn Fn(Option<Progress>) + Send + Sync),
) -> Result<(), PrepareError> {
    let child_env = crate::support::env::ProcessEnv::for_children(
        &environment.vars,
        &init.runtime_process_env_patch,
    );
    let warmup: Vec<String> = init
        .agent_warmup_targets
        .iter()
        .map(|target| target.workspace_path.clone())
        .collect();
    #[expect(clippy::disallowed_methods, reason = "存量，U3 改为显式传入")]
    let fallback = init
        .agent_spawn_fallback_cwd
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    prepare_session_storage(
        &environment.vars,
        &environment.search_roots,
        child_env.vars(),
        &warmup,
        init.workspace_path.as_deref(),
        &fallback,
        report,
    )
    .await
}
