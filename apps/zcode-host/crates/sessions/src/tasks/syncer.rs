//! task-index syncer（zcodeTaskIndexSyncer.ts）：把 CLI 的 sessions-index 变化写进 tasks-index，
//! 并广播 `workspace_task_list_changed`。V4 界面从不调用 `zcode-task.createTask`，新任务行只来自这里。
//!
//! ```text
//! SessionsIndexObserver ─► Snapshot(initial) ─► 非 draft 会话静默写基线（只插入缺失行，不广播）
//!                       ├► Snapshot(恢复) / Upserted ─► 与上一份摘要比较：
//!                       │    draft                 → 忽略（草稿不落库）
//!                       │    非终态 → 终态          → 会话信号（带迁移前摘要的 activeInputId）→ patch 状态
//!                       │                             → task_status_changed（+未读提醒）→ 回源
//!                       │    首次可见（新会话/draft 提升）→ 回源并写 grouped 顶层顺序 → task_created
//!                       │    标题变化              → patch 标题 → task_title_changed；无行则回源
//!                       └► Removed / Offline ─► 不写库
//! 回源 = CLI `session/read`（只读现有进程，不拉起 CLI）→ 合并写入 → 提交后再广播
//! ```
//!
//! 事件按到达顺序逐个处理，同一会话的写入不会乱序。
use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{Value, json};
use zcode_async::Spawner;
use zcode_host_kit::log;

use super::meta;
use super::service::TaskService;
use crate::agent::sessions_index::{IndexEvent, SessionsIndexObserver};
use crate::agent::{AgentRuntime, WorkspaceTarget};

const SEED_BATCH: usize = 64;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

pub struct Syncer {
    runtime: AgentRuntime,
    tasks: Arc<TaskService>,
}

/// 同步是 `spawner`（tasks 的作用域）里的常驻任务 `task-index-syncer`，随作用域结束。
pub fn start(
    spawner: &Spawner,
    observer: &SessionsIndexObserver,
    runtime: AgentRuntime,
    tasks: Arc<TaskService>,
) {
    let mut events = Box::pin(observer.events().subscribe());
    let syncer = Syncer { runtime, tasks };
    drop(spawner.spawn_resident("task-index-syncer", async move {
        while let Some(event) = events.next().await {
            syncer.handle(&event).await;
        }
    }));
}

impl Syncer {
    async fn handle(&self, event: &IndexEvent) {
        match event {
            IndexEvent::Snapshot {
                target,
                previous,
                sessions,
                initial: true,
            } if previous.is_empty() => self.seed_baseline(target, sessions).await,
            IndexEvent::Snapshot {
                target,
                previous,
                sessions,
                ..
            } => {
                for session in sessions {
                    let id = session["sessionId"].as_str().unwrap_or_default();
                    self.process(target, previous.get(id), session).await;
                }
            }
            IndexEvent::Upserted {
                target,
                previous,
                session,
            } => self.process(target, previous.as_ref(), session).await,
            IndexEvent::Removed { .. } | IndexEvent::Offline { .. } => {}
        }
    }

    async fn seed_baseline(&self, target: &WorkspaceTarget, sessions: &[Value]) {
        let metas: Vec<Value> = sessions
            .iter()
            .filter(|session| session["phase"] != "draft")
            .map(|session| meta::baseline_meta(target, session))
            .collect();
        for batch in metas.chunks(SEED_BATCH) {
            if let Err(error) = self.tasks.repo().seed_if_missing(batch.to_vec()).await {
                log::warn(format!("seed task index baseline failed: {error:#}"));
                return;
            }
        }
    }

    async fn process(&self, target: &WorkspaceTarget, previous: Option<&Value>, next: &Value) {
        if next["phase"] == "draft" {
            return;
        }
        let Some(session_id) = next["sessionId"].as_str() else {
            return;
        };
        let became_visible = previous.is_none_or(|previous| previous["phase"] == "draft");
        let became_terminal = previous.is_some_and(|previous| {
            !meta::is_terminal_phase(&previous["phase"]) && meta::is_terminal_phase(&next["phase"])
        });
        if became_terminal {
            let input_id = previous
                .and_then(|previous| previous["activeInputId"].as_str())
                .map(str::to_owned);
            self.terminal_transition(target, session_id, next, became_visible, input_id)
                .await;
            return;
        }
        if became_visible {
            self.resync(target, session_id, true, None).await;
            return;
        }
        let title = next["title"].as_str().unwrap_or("").trim();
        if !title.is_empty() && previous.is_some_and(|previous| previous["title"] != next["title"])
        {
            self.title_change(target, session_id, title).await;
        }
    }

    async fn terminal_transition(
        &self,
        target: &WorkspaceTarget,
        session_id: &str,
        summary: &Value,
        move_to_top: bool,
        input_id: Option<String>,
    ) {
        let unread_signal = meta::terminal_unread_signal(summary);
        let failed = summary["phase"] == "error";
        // 顺序同 Node emitTerminalAndReady：先发终态信号（收口当前 input、放行下一条），再写库。
        self.tasks
            .signals()
            .emit(std::sync::Arc::new(super::SessionSignal {
                target: target.clone(),
                session_id: session_id.to_owned(),
                failed,
                input_id,
            }));
        // error 的详情不在摘要里，留给随后回源写权威值；完成时清空旧错误。
        let patch = if failed {
            json!({"status": "error", "updatedAt": now_ms()})
        } else {
            json!({"status": "completed", "lastError": null, "updatedAt": now_ms()})
        };
        let patched = match self
            .tasks
            .repo()
            .apply_agent_patch(target.key(), session_id.to_owned(), patch)
            .await
        {
            Ok(patched) => patched,
            Err(error) => {
                log::warn(format!(
                    "sync terminal phase failed for {session_id}: {error:#}"
                ));
                return;
            }
        };
        if let Some(meta) = &patched {
            self.tasks.emit_list_changed(
                target,
                session_id,
                "task_status_changed",
                Some(meta),
                unread_signal,
            );
        }
        // patch 已广播过提醒时，回源不能再提醒一次；行缺失时把提醒交给回源结果。
        let resync_signal = if patched.is_some() {
            None
        } else {
            unread_signal
        };
        self.resync(target, session_id, move_to_top, resync_signal)
            .await;
    }

    async fn title_change(&self, target: &WorkspaceTarget, session_id: &str, title: &str) {
        let patch = json!({"title": title, "updatedAt": now_ms()});
        match self
            .tasks
            .repo()
            .apply_agent_patch(target.key(), session_id.to_owned(), patch)
            .await
        {
            Ok(Some(meta)) => self.tasks.emit_list_changed(
                target,
                session_id,
                "task_title_changed",
                Some(&meta),
                None,
            ),
            // 草稿不预写占位行：首个标题先于任何快照到达时回源，并写 grouped 顶层顺序。
            Ok(None) => self.resync(target, session_id, true, None).await,
            Err(error) => log::warn(format!("sync title failed for {session_id}: {error:#}")),
        }
    }

    /// resyncTaskIndexRowFromAgent + syncSnapshotAndBroadcast。
    async fn resync(
        &self,
        target: &WorkspaceTarget,
        session_id: &str,
        move_to_top: bool,
        unread_signal: Option<&str>,
    ) {
        if let Err(error) = self
            .try_resync(target, session_id, move_to_top, unread_signal)
            .await
        {
            log::warn(format!(
                "resync task index row {session_id} failed: {error}"
            ));
        }
    }

    async fn try_resync(
        &self,
        target: &WorkspaceTarget,
        session_id: &str,
        move_to_top: bool,
        unread_signal: Option<&str>,
    ) -> Result<(), String> {
        // 被动观察者：只读现有进程，不为回源拉起 CLI。
        let process = self
            .runtime
            .existing(target)
            .await
            .ok_or("runtime is not running")?;
        let snapshot = process
            .observe("session/read", &json!({"sessionId": session_id}))
            .await
            .map_err(|error| error.message)?;
        if snapshot["session"]["sessionKind"] == "subagent_child" {
            return Ok(());
        }
        let meta = meta::meta_from_snapshot(&snapshot);
        let searchable = Some(meta::searchable_text(&snapshot));
        let repo = self.tasks.repo();
        let (persisted, initialized) = if move_to_top {
            let outcome = repo
                .sync_at_grouped_top(meta, searchable)
                .await
                .map_err(|error| format!("{error:#}"))?;
            (outcome.meta, outcome.grouped_order_initialized)
        } else {
            let meta = repo
                .sync(meta, searchable)
                .await
                .map_err(|error| format!("{error:#}"))?;
            (meta, false)
        };
        // 还没有任何用户内容的会话只写库不广播（避免侧栏闪 "New session"），但首次写入
        // grouped 顺序时仍要通知 UI 重拉结构。
        if !meta::has_user_visible_content(&snapshot) {
            if initialized {
                self.tasks
                    .emit_list_changed(target, session_id, "task_created", None, None);
            }
            return Ok(());
        }
        let reason = if initialized {
            "task_created"
        } else {
            "task_status_changed"
        };
        self.tasks
            .emit_list_changed(target, session_id, reason, Some(&persisted), unread_signal);
        Ok(())
    }
}

/// 按 sessionId 索引摘要（测试辅助）。
#[cfg(test)]
fn by_id(sessions: &[Value]) -> std::collections::HashMap<String, Value> {
    sessions
        .iter()
        .filter_map(|session| Some((session["sessionId"].as_str()?.to_owned(), session.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::tasks::repo::memory::MemoryRepo;
    use crate::tasks::repo::{RepoFilter, TaskRepo};
    use crate::tasks::workspace_events;

    fn runtime() -> AgentRuntime {
        AgentRuntime::new(
            &zcode_host_base::testing::spawner(),
            crate::agent::AgentRuntimeConfig {
                base_env: Default::default(),
                command_vars: Default::default(),
                fallback_cwd: None,
            },
            crate::settings::SettingStore::new(
                &zcode_host_base::testing::spawner(),
                std::path::Path::new("/nonexistent"),
            ),
            crate::providers::ProviderRuntime::new(
                &zcode_host_base::testing::spawner(),
                None,
                "/nonexistent/p.json".into(),
            ),
        )
    }

    fn summary(id: &str, phase: &str, title: &str) -> Value {
        json!({"sessionId": id, "phase": phase, "title": title, "createdAt": 1, "lastActivityAt": 2})
    }

    #[tokio::test]
    async fn baseline_seeds_silently_and_terminal_patches_broadcast() {
        let repo = Arc::new(MemoryRepo::default());
        let events = workspace_events();
        let mut seen = Box::pin(events.subscribe());
        let tasks = TaskService::new(repo.clone(), events, runtime());
        let syncer = Syncer {
            runtime: runtime(),
            tasks,
        };
        let target = WorkspaceTarget::local("/w");
        let sessions = vec![summary("s1", "running", "One"), summary("d", "draft", "")];
        syncer
            .handle(&IndexEvent::Snapshot {
                target: target.clone(),
                previous: HashMap::new(),
                sessions: sessions.clone(),
                initial: true,
            })
            .await;
        let rows = repo.list(RepoFilter::default()).await.unwrap();
        assert_eq!(rows.len(), 1, "draft never enters the index");
        assert_eq!(rows[0]["status"], "running");

        // 非终态 → 终态：patch 后广播 task_status_changed 与后台完成提醒。
        syncer
            .handle(&IndexEvent::Upserted {
                target: target.clone(),
                previous: by_id(&sessions).remove("s1"),
                session: summary("s1", "completedSuccess", "One"),
            })
            .await;
        let event = seen.next().await.unwrap();
        assert_eq!(event.event["reason"], "task_status_changed");
        assert_eq!(event.event["unreadSignal"], "background_terminal");
        assert_eq!(event.event["taskMeta"]["status"], "completed");

        // 标题变化：patch 后广播 task_title_changed。
        syncer
            .handle(&IndexEvent::Upserted {
                target: target.clone(),
                previous: Some(summary("s1", "completedSuccess", "One")),
                session: summary("s1", "completedSuccess", "Renamed"),
            })
            .await;
        let event = seen.next().await.unwrap();
        assert_eq!(event.event["reason"], "task_title_changed");
        assert_eq!(event.event["taskMeta"]["title"], "Renamed");
    }
}
