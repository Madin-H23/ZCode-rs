//! 引擎停止时的收口：取消运行中的 run，等工具任务结束，持久化中断态。
use super::{Engine, Event};
use crate::contract::StorageCommitFailure;
use anyhow::Result;

impl Engine {
    /// `result` 是服务循环的结果。循环以存储提交失败或 panic 结束时，内存里的事实可能只改了
    /// 一半：不再提交任何东西，只取消运行并等工具进程收口。
    pub(super) async fn wind_down(&mut self, mut result: Result<()>) -> Result<()> {
        let mut storage_failed = result.as_ref().err().is_some_and(|error| {
            error.is::<StorageCommitFailure>() || error.is::<zcode_async::Panicked>()
        });
        if result.is_err() {
            self.child_updates.clear();
        }
        for active in self.active.values() {
            active.cancel.cancel();
        }
        for job in self.auxiliary.values() {
            job.cancel.cancel();
        }
        self.auxiliary.clear();
        for session in self.sessions.values_mut() {
            if let Some(goal) = &mut session.goal {
                goal.finish_run(self.clock.now(), false, Some("paused"));
            }
            session.auto_drain = false;
            session.queued_now = None;
        }
        self.waiters.clear();
        for id in self.sessions.keys() {
            self.tools.cancel_session(id, None).await?;
        }
        // 必须等待拥有的工具 task 收口后再释放 runtime；只 drop Tokio task 会漏掉 Shell 后代。
        // 清理期限由工具 adapter 的 TERM/KILL 生命周期拥有；不能在 1200ms
        // 提前 drop 仍处于 1500ms 宽限期的前台任务，留下工作进程或丢失终态。
        while !self.active.is_empty()
            || (!storage_failed
                && self
                    .sessions
                    .values()
                    .any(|s| s.background.values().any(|t| t.status == "running")))
        {
            match self.event_rx.recv().await {
                Some(event) => {
                    // 事务失败后的内存事实不可再提交；否则一次失败的 ACK/工具结果会被收口路径复活。
                    if storage_failed {
                        if matches!(event.event, Event::Finished { .. })
                            && self
                                .active
                                .get(&event.session_id)
                                .is_some_and(|active| active.run_id == event.run_id)
                        {
                            self.active.remove(&event.session_id);
                        }
                    } else if let Err(error) = self.apply_event(event).await {
                        storage_failed = true;
                        self.child_updates.clear();
                        result = Err(error);
                    }
                    self.outbox.clear();
                }
                _ => break,
            }
        }
        self.event_rx.close();
        self.tools.shutdown().await?;
        // EOF 后持久化中断态，再结束 actor；不等待挂起权限或再次启动队列。
        for session in self.sessions.values_mut() {
            if (session.running() || session.background.values().any(|t| t.status == "running"))
                && !storage_failed
            {
                session.recover(self.clock.id(), self.clock.now());
                let workspace = session.workspace.clone();
                self.store
                    .commit_receipt(&workspace, Some(session), None)
                    .await?;
            }
        }
        result
    }
}
