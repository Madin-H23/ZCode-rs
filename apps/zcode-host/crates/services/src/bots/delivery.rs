//! 回复投递与终态回推（botsService.ts `sendOutbound`、`watchTaskStream` 的摘要模式）。
//!
//! 飞书 / Lark 的普通回复用交互卡片的 markdown 元素承载，超过 1900 个 UTF-16 码元分卡发送。
//! 会话终态时回推最后一段助手文本（`summary_changes` 的 latestPart）；失败时回推 `taskFailed`。
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use zcode_host_kit::log;

use super::FinalReply;
use super::feishu::{Actor, interactive_card, split_text};
use super::inbound::outbound;
use super::model::{is_feishu, message as msg, workspace_key};
use super::service::{Bots, guard, text};
use crate::agent::WorkspaceTarget;

/// 回推等待上限：超过后放弃订阅（长任务仍会正常完成，只是不再回推）。
const WATCH_LIMIT: Duration = Duration::from_secs(24 * 60 * 60);

/// 从会话行里取最后一轮的结果：最后一个 userInput 之后的最后一段非空助手文本。
pub fn final_reply_from_rows(rows: &[Value]) -> FinalReply {
    let start = rows
        .iter()
        .rposition(|row| row["kind"] == "userInput")
        .map_or(0, |index| index + 1);
    let turn = &rows[start.min(rows.len())..];
    let text = turn
        .iter()
        .rev()
        .filter(|row| row["kind"] == "assistantText")
        .find_map(|row| {
            row["text"]
                .as_str()
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned)
        });
    let error = turn.iter().rev().find_map(|row| {
        let error = &row["error"];
        error["message"]
            .as_str()
            .or_else(|| error.as_str())
            .or_else(|| row["errorMessage"].as_str())
            .filter(|message| !message.trim().is_empty())
            .map(str::to_owned)
    });
    FinalReply { text, error }
}

impl Bots {
    /// `sendOutbound`：只有飞书 / Lark 有传输层；token 缺失时静默跳过（同 Node）。
    pub(crate) async fn send_outbound(&self, bot: &Value, message: &Value) -> anyhow::Result<()> {
        let provider = text(bot, "provider").unwrap_or_default();
        if !is_feishu(provider) {
            return Ok(());
        }
        let Some(token) = self.tenant_token(bot).await? else {
            return Ok(());
        };
        let receive_id = text(message, "providerUserId").unwrap_or_default();
        for chunk in split_text(text(message, "text").unwrap_or_default()) {
            let result = self
                .api
                .send_card(provider, &token, receive_id, &interactive_card(&chunk))
                .await;
            // 收消息正常不代表回复已投递：投递错误单独记录。
            self.on_delivery_result(bot, result.as_ref().err().map(ToString::to_string));
            result?;
        }
        Ok(())
    }

    /// 订阅会话终态并回推摘要。订阅在返回前完成，调用方随后才能发送输入。
    /// `input_id` 为 None 时（automation 回推）匹配该会话的任意终态。
    pub(crate) fn watch_task(
        self: &Arc<Self>,
        bot: Value,
        actor: Actor,
        workspace: WorkspaceTarget,
        task_id: String,
        input_id: Option<String>,
    ) {
        let mut outcomes = self.tasks.terminal_outcomes();
        let bots = self.clone();
        let key = workspace_key(
            &workspace.workspace_path,
            workspace.workspace_identity.as_deref(),
        );
        tokio::spawn(async move {
            let wait = async {
                while let Some(outcome) = outcomes.next().await {
                    if outcome.workspace_key == key
                        && outcome.session_id == task_id
                        && input_id
                            .as_ref()
                            .is_none_or(|id| outcome.input_id.as_ref() == Some(id))
                    {
                        return Some(outcome);
                    }
                }
                None
            };
            let Ok(Some(outcome)) = tokio::time::timeout(WATCH_LIMIT, wait).await else {
                guard(&bots.running).remove(&task_id);
                return;
            };
            guard(&bots.running).remove(&task_id);
            let reply = bots.tasks.final_reply(workspace, task_id.clone()).await;
            let locale = bots.locale().await;
            let locale = locale.as_deref();
            let text = if outcome.outcome == "failed" {
                let reason = reply
                    .error
                    .unwrap_or_else(|| msg(locale, "streamingStatusFailed", &[]));
                Some(msg(locale, "taskFailed", &[("message", &reason)]))
            } else {
                reply.text
            };
            let Some(text) = text else {
                return;
            };
            if let Err(error) = bots
                .send_outbound(&bot, &outbound(&actor, text, None))
                .await
            {
                log::warn(format!(
                    "bots terminal reply failed bot={} task={task_id}: {error:#}",
                    actor.bot_id
                ));
            }
        });
    }
}
