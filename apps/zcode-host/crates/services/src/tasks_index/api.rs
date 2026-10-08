//! 分组、grouped 视图、搜索与归档的异步入口：都在 tasks-index 线程上执行（`TasksIndex::call`）。
use serde_json::Value;

use super::{TasksIndex, archive, grouped_view, groups, task_list};

impl TasksIndex {
    /// `createTaskGroup`；id 为 `task-group-<uuid v4>`。
    pub async fn create_task_group(
        &self,
        title: Option<String>,
        color: Option<String>,
    ) -> anyhow::Result<Value> {
        let group_id = format!("task-group-{}", crate::agent::service::uuid_v4());
        self.call(move |conn, now| {
            groups::create_task_group(conn, now, &group_id, title.as_deref(), color.as_deref())
        })
        .await
    }

    pub async fn rename_task_group(
        &self,
        group_id: String,
        title: String,
    ) -> anyhow::Result<Value> {
        self.call(move |conn, now| groups::rename_task_group(conn, now, &group_id, &title))
            .await
    }

    pub async fn update_task_group_color(
        &self,
        group_id: String,
        color: Option<String>,
    ) -> anyhow::Result<Value> {
        self.call(move |conn, now| {
            groups::update_task_group_color(conn, now, &group_id, color.as_deref())
        })
        .await
    }

    pub async fn delete_task_group(&self, group_id: String) -> anyhow::Result<()> {
        self.call(move |conn, _| groups::delete_task_group(conn, &group_id))
            .await
    }

    /// `queryGroupedTaskViewStructure({workspaceScopes})`。
    pub async fn query_grouped_task_view_structure(
        &self,
        workspace_scopes: Value,
    ) -> anyhow::Result<Value> {
        self.call(move |conn, _| groups::query_grouped_task_view_structure(conn, &workspace_scopes))
            .await
    }

    /// `applyGroupedTaskViewOrder(input & {provider})`，返回 legacy `{nodes}`。
    pub async fn apply_grouped_task_view_order(
        &self,
        input: Value,
        provider: Option<String>,
    ) -> anyhow::Result<Value> {
        self.call(move |conn, now| {
            grouped_view::apply_grouped_task_view_order(conn, now, &input, provider.as_deref())
        })
        .await
    }

    /// `queryTaskList(query & {provider})`，返回 `{items, total, hasMore}`。
    pub async fn query_task_list(
        &self,
        query: Value,
        provider: Option<String>,
    ) -> anyhow::Result<Value> {
        self.call(move |conn, _| task_list::query_task_list(conn, &query, provider.as_deref()))
            .await
    }

    pub async fn archive_stale_tasks(
        &self,
        workspace_key: String,
        older_than_days: f64,
        provider: Option<String>,
    ) -> anyhow::Result<Vec<Value>> {
        self.call(move |conn, now| {
            archive::archive_stale_tasks(
                conn,
                now,
                &workspace_key,
                older_than_days,
                provider.as_deref(),
            )
        })
        .await
    }

    pub async fn delete_archived_task(
        &self,
        workspace_key: String,
        task_id: String,
    ) -> anyhow::Result<Option<Value>> {
        self.call(move |conn, _| archive::delete_archived_task(conn, &workspace_key, &task_id))
            .await
    }

    /// `initializeGroupedTaskAtTop`：新任务第一次出现时放到 grouped 顶层最前；已有顺序或已分组时不动。
    pub async fn initialize_grouped_task_at_top(
        &self,
        workspace_key: String,
        task_id: String,
    ) -> anyhow::Result<bool> {
        self.call(move |conn, now| {
            super::repo::initialize_grouped_task_at_top(conn, &workspace_key, &task_id, now)
        })
        .await
    }
}

impl TasksIndex {
    /// 按裸 taskId 找一行未删除的 meta（多个 workspace 同名时取最近更新的）；task target 登记的兜底。
    pub async fn find_task_meta_by_id(&self, task_id: String) -> anyhow::Result<Option<Value>> {
        self.call(move |conn, _| {
            use rusqlite::OptionalExtension as _;
            let row = conn
                .query_row(
                    &format!(
                        "{}\n        WHERE task_id = ? AND deleted = 0\n        ORDER BY updated_at DESC\n        LIMIT 1",
                        task_list::SELECT_LIST_ROW
                    ),
                    [&task_id],
                    super::repo::read_task_row,
                )
                .optional()?;
            Ok(row.map(|row| super::meta::row_to_meta(&row).to_value()))
        })
        .await
    }
}

impl TasksIndex {
    /// 让一行重新可见（`archived = 0, deleted = 0`）；行不存在时为 None。Claude 重新导入用。
    pub async fn restore_task_visibility(
        &self,
        workspace_key: String,
        task_id: String,
    ) -> anyhow::Result<Option<Value>> {
        self.call(move |conn, _| {
            let changed = conn.execute(
                "UPDATE tasks SET archived = 0, deleted = 0 WHERE workspace_key = ? AND task_id = ?",
                (&workspace_key, &task_id),
            )?;
            if changed == 0 {
                return Ok(None);
            }
            Ok(super::repo::get_task_meta(conn, &workspace_key, &task_id)?.map(|meta| meta.to_value()))
        })
        .await
    }
}
