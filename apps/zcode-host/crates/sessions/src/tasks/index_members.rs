//! zcode-task 的分组、grouped 视图、搜索与归档成员（zcodeTaskServiceAdapter.ts）。
//! 写入一律先提交，再广播 `workspace_task_list_changed`。
use std::sync::Arc;

use serde_json::{Value, json};
use zcode_host_kit::{Args, Reply, RpcError, ServiceBuilder, log};

use super::context::{GLM_PROVIDER, TaskContext, invalid, repo_error, target_of};
use super::repo::RepoFilter;
use super::service::CHANNEL;
use crate::agent::WorkspaceTarget;

fn first(args: &Args) -> Value {
    args.raw(0).cloned().unwrap_or(Value::Null)
}

fn scope_targets(value: &Value) -> Vec<WorkspaceTarget> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(target_of)
        .collect()
}

fn required_str<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    params[name].as_str().ok_or_else(|| invalid(name))
}

pub async fn list_task_list(context: &TaskContext, query: Value) -> Result<Value, RpcError> {
    let result = context
        .index
        .query_task_list(query, Some(GLM_PROVIDER.to_owned()))
        .await
        .map_err(repo_error)?;
    for item in result["items"].as_array().into_iter().flatten() {
        context.remember(item);
    }
    Ok(result)
}

/// `runWorkspaceTaskAutoArchive`：设置开启时，每个 workspace 归档一次过期任务（不过滤 provider）。
async fn auto_archive(context: &TaskContext, scopes: &[WorkspaceTarget]) -> Result<(), RpcError> {
    if scopes.is_empty() {
        return Ok(());
    }
    let settings = context.settings.get().await;
    if !settings["taskAutoArchiveEnabled"]
        .as_bool()
        .unwrap_or(false)
    {
        return Ok(());
    }
    let days = settings["taskAutoArchiveOlderThanDays"]
        .as_f64()
        .unwrap_or(7.0);
    let mut seen = Vec::new();
    let mut archived_count = 0;
    for scope in scopes {
        let key = scope.key();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key.clone());
        let archived = context
            .index
            .archive_stale_tasks(key, days, None)
            .await
            .map_err(repo_error)?;
        archived_count += archived.len();
        emit_archived(context, &archived);
    }
    if archived_count > 0 {
        log::info(format!(
            "按设置自动归档旧 task 数量={archived_count} olderThanDays={days}"
        ));
    }
    Ok(())
}

/// 自动归档与 `archiveStaleTasks`：逐个归档的任务各广播一次带 meta 的 `task_meta_changed`。
fn emit_archived(context: &TaskContext, archived: &[Value]) {
    for task in archived {
        context.remember(task);
        context.emit_list_changed(
            &target_of(task),
            task["taskId"].as_str(),
            "task_meta_changed",
            Some(task),
        );
    }
}

fn emit_scopes(context: &TaskContext, scopes: &Value) {
    for scope in scope_targets(scopes) {
        // grouped 结构变更没有单任务 meta，沿用 task_meta_changed 驱动 grouped 视图重拉。
        context.emit_list_changed(&scope, None, "task_meta_changed", None);
    }
}

pub async fn delete_archived_task(
    context: &TaskContext,
    target: &WorkspaceTarget,
    task_id: &str,
) -> Result<bool, RpcError> {
    let Some(meta) = context
        .index
        .delete_archived_task(target.key(), task_id.to_owned())
        .await
        .map_err(repo_error)?
    else {
        return Ok(false);
    };
    context.emit_list_changed(target, Some(task_id), "task_deleted", Some(&meta));
    Ok(true)
}

/// 逐项独立事务：一项失败不回滚其他项；整批结束后只广播一次。
pub async fn delete_archived_tasks(
    context: &TaskContext,
    target: &WorkspaceTarget,
    task_ids: Vec<String>,
) -> Result<Value, RpcError> {
    let mut unique: Vec<String> = Vec::new();
    for task_id in task_ids {
        if !unique.contains(&task_id) {
            unique.push(task_id);
        }
    }
    let (mut deleted, mut skipped, mut failed) = (Vec::new(), Vec::new(), Vec::new());
    if unique.is_empty() {
        return Ok(
            json!({"deletedTaskIds": deleted, "skippedTaskIds": skipped, "failedTaskIds": failed}),
        );
    }
    let started = std::time::Instant::now();
    for task_id in unique.iter() {
        match context
            .index
            .delete_archived_task(target.key(), task_id.clone())
            .await
        {
            Ok(Some(_)) => deleted.push(task_id.clone()),
            Ok(None) => skipped.push(task_id.clone()),
            Err(error) => {
                log::warn(format!(
                    "[ArchivedTaskDeletion] 批次目标删除失败 task={task_id}: {error:#}"
                ));
                failed.push(task_id.clone());
            }
        }
    }
    if !deleted.is_empty() {
        context.emit_list_changed(target, None, "task_deleted", None);
    }
    log::info(format!(
        "[ArchivedTaskDeletion] batch completed workspaceKey={} requested={} deleted={} skipped={} failed={} durationMs={}",
        target.key(),
        unique.len(),
        deleted.len(),
        skipped.len(),
        failed.len(),
        started.elapsed().as_millis()
    ));
    Ok(json!({"deletedTaskIds": deleted, "skippedTaskIds": skipped, "failedTaskIds": failed}))
}

async fn archive_workspace_tasks(
    context: &TaskContext,
    target: &WorkspaceTarget,
) -> Result<Value, RpcError> {
    let repo = context.tasks.repo();
    let list = |archived| {
        repo.list(RepoFilter {
            workspace_key: Some(target.key()),
            pinned: None,
            archived: Some(archived),
        })
    };
    let active = list(false).await.map_err(repo_error)?;
    for task in &active {
        let task_id = task["taskId"].as_str().unwrap_or_default().to_owned();
        let task_target = target_of(task);
        repo.set_archived(task_target.key(), task_id, true)
            .await
            .map_err(repo_error)?;
    }
    // 批量归档无逐任务 meta，沿用 task_meta_changed 走 membership 重拉收敛。
    context.emit_list_changed(target, None, "task_meta_changed", None);
    let archived = list(true).await.map_err(repo_error)?;
    for task in &archived {
        context.remember(task);
    }
    Ok(Value::Array(archived))
}

fn remember_nodes(context: &TaskContext, view: &Value) {
    for node in view["nodes"].as_array().into_iter().flatten() {
        if node["type"] == "task" {
            context.remember(&node["task"]);
        } else {
            for task in node["tasks"].as_array().into_iter().flatten() {
                context.remember(task);
            }
        }
    }
}

macro_rules! member {
    ($builder:expr, $name:literal, $context:expr, |$ctx:ident, $params:ident| $body:expr) => {{
        let context = $context.clone();
        $builder.method($name, move |_, args| {
            let $ctx = context.clone();
            let $params = first(&args);
            async move { $body }
        })
    }};
}

pub fn service(context: Arc<TaskContext>) -> ServiceBuilder {
    let builder = ServiceBuilder::new(CHANNEL);
    let builder = member!(builder, "listTaskList", context, |ctx, params| {
        list_task_list(&ctx, params).await.map(Reply::Value)
    });
    let builder = member!(builder, "createTaskGroup", context, |ctx, params| {
        ctx.index
            .create_task_group(
                params["title"].as_str().map(str::to_owned),
                params["color"].as_str().map(str::to_owned),
            )
            .await
            .map(Reply::Value)
            .map_err(repo_error)
    });
    let builder = member!(builder, "renameTaskGroup", context, |ctx, params| {
        let group = ctx
            .index
            .rename_task_group(
                required_str(&params, "groupId")?.to_owned(),
                params["title"].as_str().unwrap_or_default().to_owned(),
            )
            .await
            .map_err(repo_error)?;
        emit_scopes(&ctx, &params["workspaceScopes"]);
        Ok(Reply::Value(group))
    });
    let builder = member!(builder, "updateTaskGroupColor", context, |ctx, params| {
        let group = ctx
            .index
            .update_task_group_color(
                required_str(&params, "groupId")?.to_owned(),
                params["color"].as_str().map(str::to_owned),
            )
            .await
            .map_err(repo_error)?;
        emit_scopes(&ctx, &params["workspaceScopes"]);
        Ok(Reply::Value(group))
    });
    let builder = member!(builder, "deleteTaskGroup", context, |ctx, params| {
        ctx.index
            .delete_task_group(required_str(&params, "groupId")?.to_owned())
            .await
            .map_err(repo_error)?;
        emit_scopes(&ctx, &params["workspaceScopes"]);
        Ok(Reply::Undefined)
    });
    let builder = member!(
        builder,
        "listGroupedTaskViewStructure",
        context,
        |ctx, params| {
            // 进入 grouped 视图时按设置清理超期任务（同 Node），再读原始结构。
            auto_archive(&ctx, &scope_targets(&params["workspaceScopes"])).await?;
            ctx.index
                .query_grouped_task_view_structure(params["workspaceScopes"].clone())
                .await
                .map(Reply::Value)
                .map_err(repo_error)
        }
    );
    let builder = member!(
        builder,
        "applyGroupedTaskViewOrder",
        context,
        |ctx, params| {
            let view = ctx
                .index
                .apply_grouped_task_view_order(params.clone(), Some(GLM_PROVIDER.to_owned()))
                .await
                .map_err(repo_error)?;
            emit_scopes(&ctx, &params["workspaceScopes"]);
            remember_nodes(&ctx, &view);
            Ok(Reply::Value(view))
        }
    );
    let builder = member!(builder, "archiveStaleTasks", context, |ctx, params| {
        let target = target_of(&params);
        let archived = ctx
            .index
            .archive_stale_tasks(
                target.key(),
                params["olderThanDays"]
                    .as_f64()
                    .ok_or_else(|| invalid("olderThanDays"))?,
                None,
            )
            .await
            .map_err(repo_error)?;
        emit_archived(&ctx, &archived);
        Ok(Reply::Value(Value::Array(archived)))
    });
    let builder = member!(builder, "archiveWorkspaceTasks", context, |ctx, params| {
        archive_workspace_tasks(&ctx, &target_of(&params))
            .await
            .map(Reply::Value)
    });
    let builder = member!(builder, "deleteArchivedTask", context, |ctx, params| {
        let task_id = required_str(&params, "taskId")?.to_owned();
        delete_archived_task(&ctx, &target_of(&params), &task_id)
            .await
            .map(|deleted| Reply::Value(json!(deleted)))
    });
    member!(builder, "deleteArchivedTasks", context, |ctx, params| {
        let task_ids: Vec<String> = params["taskIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect();
        delete_archived_tasks(&ctx, &target_of(&params), task_ids)
            .await
            .map(Reply::Value)
    })
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;
    use crate::tasks::context::testing::{fixture, meta};
    use crate::tasks::controller::Controller;

    #[tokio::test]
    async fn search_delete_archived_and_group_events() {
        let dir = tempfile::tempdir().unwrap();
        let fx = fixture(dir.path()).await;
        let index = &fx.context.index;
        for (id, text) in [("a", "has the needle inside"), ("b", "old"), ("c", "older")] {
            index
                .sync_task_meta(meta(id, "/w", json!({})), Some(text.to_owned()))
                .await
                .unwrap();
        }
        let repo = fx.context.tasks.repo();
        repo.set_archived("/w".into(), "b".into(), true)
            .await
            .unwrap();
        let mut events = Box::pin(
            fx.events
                .subscribe_filtered(|event| event.workspace_key == "/w"),
        );
        let controller = Controller::start(
            &zcode_host_base::testing::spawner(),
            Arc::new(fx.context.clone()),
            fx.observer.clone(),
            fx.events.clone(),
        );

        let found = controller
            .list_task_list(&json!({"kind": "timeline", "workspaceScopes": [{"workspacePath": "/w"}], "search": "NEEDLE"}))
            .await
            .unwrap();
        assert_eq!(found["total"], 1);
        assert_eq!(found["items"][0]["taskId"], "a");
        assert_eq!(found["items"][0]["liveStatus"], "completed");
        assert_eq!(
            found["items"][0]["searchSnippets"][0],
            "has the needle inside"
        );
        // 搜索登记了 target，后续 sendPrompt 等可以直接解析。
        assert_eq!(
            fx.context.task_target("a").await.unwrap().workspace_path,
            "/w"
        );

        let result = controller
            .delete_archived_tasks(&json!({
                "address": {"workspacePath": "/w", "taskId": "b"},
                "taskIds": ["b", "b", "a", "zz"],
            }))
            .await
            .unwrap();
        assert_eq!(
            result,
            json!({"deletedTaskIds": ["b"], "skippedTaskIds": ["a", "zz"], "failedTaskIds": []})
        );
        let event = events.next().await.unwrap();
        assert_eq!(event.event["reason"], "task_deleted");
        assert!(event.event.get("taskId").is_none());
        assert_eq!(
            repo.deleted_ids("/w".into()).await.unwrap(),
            vec!["b".to_owned()]
        );

        let group = index
            .create_task_group(Some(" G ".into()), None)
            .await
            .unwrap();
        assert_eq!(
            (group["title"].as_str(), group["color"].as_str()),
            (Some("G"), Some("gray"))
        );
        let missing = fx.context.task_target("nope").await.unwrap_err();
        assert_eq!(missing.code_str(), Some("ZCODE_SESSION_TARGET_NOT_FOUND"));
    }

    #[tokio::test]
    async fn grouped_structure_auto_archives_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let fx = fixture(dir.path()).await;
        let settings_file = fx.home.join(".zcode").join("v2").join("setting.json");
        std::fs::create_dir_all(settings_file.parent().unwrap()).unwrap();
        std::fs::write(
            &settings_file,
            r#"{"taskAutoArchiveEnabled":true,"taskAutoArchiveOlderThanDays":1}"#,
        )
        .unwrap();
        fx.context
            .index
            .sync_task_meta(meta("old", "/w", json!({"updatedAt": 1})), None)
            .await
            .unwrap();
        let mut events = Box::pin(
            fx.events
                .subscribe_filtered(|event| event.workspace_key == "/w"),
        );
        auto_archive(&fx.context, &[WorkspaceTarget::local("/w")])
            .await
            .unwrap();
        let event = events.next().await.unwrap();
        assert_eq!(event.event["reason"], "task_meta_changed");
        assert_eq!(event.event["taskId"], "old");
        let archived = fx
            .context
            .tasks
            .repo()
            .list(RepoFilter {
                workspace_key: Some("/w".into()),
                pinned: None,
                archived: Some(true),
            })
            .await
            .unwrap();
        assert_eq!(archived.len(), 1);
    }
}
