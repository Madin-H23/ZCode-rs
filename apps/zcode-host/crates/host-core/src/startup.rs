//! 启动状态的唯一所有者（docs/specs/rust-host-resilience.md §2.2）。
//!
//! 与 Node `DatabaseStartupCoordinator` 语义相同：同一 `startupId` 内 sequence 跨尝试严格递增；
//! 失败后只有显式 `retry{attemptId}` 才会开始新的尝试，刷新和重复请求不会重新执行迁移。
use std::sync::{Arc, Mutex};

use zcode_host_control::{ControlSink, DatabaseStartupState, HostToMain};
use zcode_host_services::agent::storage_prep::{MigrationFacts, PrepareError, Progress};

use crate::{now_ms, random_hex};

/// `canRetryDatabaseStartup` 中不可重试的错误码（packages/shared/src/database-startup.ts）。
const NON_RETRYABLE_CODES: [&str; 5] = [
    "corrupt",
    "checksum_mismatch",
    "transport_closed",
    "startup_status_timeout",
    "unsupported_runtime",
];

struct Cell {
    attempt_id: String,
    started_at: u64,
    next_sequence: u64,
    running: bool,
    latest: Option<DatabaseStartupState>,
    /// 会话库准备上报的迁移事实（docs/specs/rust-host-agent.md §2.11）；本次尝试之后的状态一直带着，
    /// retry 开始新尝试时清空。
    migrations: MigrationFacts,
}

pub(crate) struct StartupCoordinator {
    control: Arc<dyn ControlSink>,
    startup_id: String,
    cell: Mutex<Cell>,
}

impl StartupCoordinator {
    /// 第一次尝试从这里开始，状态为运行中。
    pub(crate) fn begin(control: Arc<dyn ControlSink>, startup_id: String) -> Self {
        Self {
            control,
            startup_id,
            cell: Mutex::new(Cell {
                attempt_id: random_hex(16),
                started_at: now_ms(),
                next_sequence: 0,
                running: true,
                latest: None,
                migrations: MigrationFacts::default(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Cell> {
        self.cell
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 生成下一条状态并记为最新；在锁内发出，保证发给 Main 的顺序与 sequence 一致。
    fn publish(&self, build: impl FnOnce(&mut DatabaseStartupState)) {
        let mut cell = self.lock();
        let mut state = DatabaseStartupState::new(
            self.startup_id.clone(),
            cell.attempt_id.clone(),
            cell.next_sequence,
            cell.started_at,
            now_ms(),
            "starting",
        );
        cell.next_sequence += 1;
        state.migration = cell.migrations.aggregate();
        let baselines = cell.migrations.baselines();
        if !baselines.is_empty() {
            state.migration_baselines = Some(baselines);
        }
        build(&mut state);
        if matches!(state.phase, "ready" | "failed") {
            cell.running = false;
        }
        cell.latest = Some(state.clone());
        self.control
            .send(HostToMain::DatabaseStartupState { state });
    }

    pub(crate) fn send(&self, phase: &'static str, database_phase: Option<&'static str>) {
        self.publish(|state| {
            state.phase = phase;
            state.database_phase = database_phase;
        });
    }

    pub(crate) fn fail(&self, failed_phase: &'static str, error_code: &'static str) {
        self.publish(|state| {
            state.phase = "failed";
            state.failed_phase = Some(failed_phase);
            state.error_code = Some(error_code);
        });
    }

    /// `preparing_session_storage`：None 是进入新目录（checking），Some 是该目录的数据库阶段。
    pub(crate) fn send_session(&self, progress: Option<Progress>) {
        let Some(progress) = progress else {
            self.send("preparing_session_storage", Some("checking"));
            return;
        };
        self.lock()
            .migrations
            .record(&progress.database_id, progress.migration.as_ref());
        self.publish(|state| {
            state.phase = "preparing_session_storage";
            state.database_phase = Some(progress.database_phase);
            state.current_migration = progress.migration;
            state.final_database = Some(progress.final_database);
        });
    }

    pub(crate) fn fail_session(&self, error: PrepareError) {
        if let Some((database_id, facts)) = &error.migration {
            self.lock().migrations.record(database_id, Some(facts));
        }
        self.publish(|state| {
            state.phase = "failed";
            state.failed_phase = Some("preparing_session_storage");
            state.error_code = Some(error.error_code);
            state.sqlite_code = error.sqlite_code;
            state.system_code = error.system_code;
            state.migration_id = error.migration_id;
            state.current_migration = error.migration.map(|(_, facts)| facts);
        });
    }

    /// `snapshot`：重发最近一次状态（sequence 不变，Main 的 relay 会按 sequence 去重）。
    pub(crate) fn republish(&self) {
        let latest = self.lock().latest.clone();
        if let Some(state) = latest {
            self.control
                .send(HostToMain::DatabaseStartupState { state });
        }
    }

    /// `retry{attemptId}`：接受时切换到新尝试并返回 true，由调用方重新执行启动。
    pub(crate) fn try_retry(&self, attempt_id: &str) -> bool {
        let mut cell = self.lock();
        let retryable = cell.latest.as_ref().is_some_and(can_retry);
        if cell.running || !retryable || cell.attempt_id != attempt_id {
            return false;
        }
        cell.attempt_id = random_hex(16);
        cell.started_at = now_ms();
        cell.running = true;
        cell.migrations = MigrationFacts::default();
        true
    }
}

/// `canRetryDatabaseStartup`：服务装配阶段失败、或数据损坏/失联类错误只能退出重开。
fn can_retry(state: &DatabaseStartupState) -> bool {
    state.phase == "failed"
        && state.failed_phase != Some("starting_services")
        && !NON_RETRYABLE_CODES.contains(&state.error_code.unwrap_or("sql_failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Sink(Mutex<Vec<DatabaseStartupState>>);

    impl ControlSink for Sink {
        fn send(&self, message: HostToMain) {
            if let HostToMain::DatabaseStartupState { state } = message {
                self.0.lock().unwrap().push(state);
            }
        }
    }

    fn states(sink: &Sink) -> Vec<DatabaseStartupState> {
        sink.0.lock().unwrap().clone()
    }

    #[test]
    fn retry_starts_a_new_attempt_with_increasing_sequence() {
        let sink = Arc::new(Sink::default());
        let coordinator = StartupCoordinator::begin(sink.clone(), "s1".into());
        coordinator.send("starting", None);
        // 运行中不接受重试。
        let first = states(&sink)[0].attempt_id.clone();
        assert!(!coordinator.try_retry(&first));
        coordinator.fail("preparing_host_storage", "open_failed");
        assert!(!coordinator.try_retry("other-attempt"));
        assert!(coordinator.try_retry(&first));
        // 新尝试开始后，旧 attemptId 的重复点击被忽略。
        assert!(!coordinator.try_retry(&first));
        coordinator.send("starting", None);
        coordinator.send("ready", None);
        let all = states(&sink);
        let sequences: Vec<u64> = all.iter().map(|state| state.sequence).collect();
        assert_eq!(sequences, [0, 1, 2, 3]);
        assert!(all.iter().all(|state| state.startup_id == "s1"));
        assert_ne!(all[2].attempt_id, first);
        assert_eq!(all[2].attempt_id, all[3].attempt_id);
        // ready 之后不再可重试。
        assert!(!coordinator.try_retry(&all[3].attempt_id));
    }

    #[test]
    fn non_retryable_failures_need_a_restart() {
        for (failed_phase, code) in [
            ("preparing_host_storage", "checksum_mismatch"),
            ("preparing_host_storage", "corrupt"),
            ("starting_services", "sql_failed"),
        ] {
            let sink = Arc::new(Sink::default());
            let coordinator = StartupCoordinator::begin(sink.clone(), "s1".into());
            coordinator.fail(failed_phase, code);
            let attempt = states(&sink)[0].attempt_id.clone();
            assert!(!coordinator.try_retry(&attempt), "{failed_phase}/{code}");
        }
    }

    #[test]
    fn snapshot_republishes_the_latest_state() {
        let sink = Arc::new(Sink::default());
        let coordinator = StartupCoordinator::begin(sink.clone(), "s1".into());
        coordinator.republish();
        assert!(states(&sink).is_empty());
        coordinator.fail("preparing_host_storage", "io_error");
        coordinator.republish();
        let all = states(&sink);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], all[1]);
    }
}
