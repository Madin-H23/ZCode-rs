//! `queryTaskList`（taskIndexRepo.ts）：zcode-task `listTaskList` 与 window-controller 搜索的数据源。
//!
//! SQL、条件顺序、排序与 TS 逐条相同；搜索是整串子串匹配（不分词、不转义 `%`/`_`），
//! 命中正文时按 UTF-16 下标截取摘要（与 JS `indexOf` / `slice` 相同）。
use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value, json};
use zcode_cli_domain::js_string;

use super::meta::{TaskRow, resolve_workspace_key, row_to_meta};
use super::repo::read_task_row;
use crate::support::collate::locale_compare;

const SNIPPET_PREFIX_RADIUS: usize = 20;
const SNIPPET_SUFFIX_RADIUS: usize = 72;
const SNIPPET_MAX_CHARS: usize = 140;
const SNIPPET_LIMIT: usize = 4;

/// 调用参数里的一个 workspace scope（`{workspacePath, workspaceIdentity?, workspacePurpose?}`）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Scope {
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
    pub workspace_purpose: Option<String>,
}

impl Scope {
    pub fn key(&self) -> String {
        resolve_workspace_key(&self.workspace_path, self.workspace_identity.as_deref())
    }
}

/// 读取 `workspaceScopes`；缺少 workspacePath 的项按空路径处理（随后被 key 去空过滤）。
pub(crate) fn scopes(value: &Value) -> Vec<Scope> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|scope| Scope {
            workspace_path: scope["workspacePath"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            workspace_identity: scope["workspaceIdentity"].as_str().map(str::to_owned),
            workspace_purpose: scope["workspacePurpose"].as_str().map(str::to_owned),
        })
        .collect()
}

/// `normalizeWorkspaceKeys`：解析 key、去空、去重、按 `localeCompare` 排序。
pub(crate) fn normalize_workspace_keys(scopes: &[Scope]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for key in scopes.iter().map(Scope::key) {
        if !key.trim().is_empty() && !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys.sort_by(|left, right| locale_compare(left, right));
    keys
}

pub(crate) fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

/// `normalizeLimit`：有限正数取整，否则不限。
fn normalize_limit(value: &Value) -> Option<i64> {
    value
        .as_f64()
        .filter(|limit| limit.is_finite() && *limit > 0.0)
        .map(|limit| limit.floor() as i64)
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn index_of(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return (from <= haystack.len()).then_some(from);
    }
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| position + from)
}

/// `normalizeSearchSnippetText`：空白折叠为一个空格、trim、截到 140 个 UTF-16 单元。
fn normalize_snippet(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
        if js_string::is_space(c) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    js_string::utf16_prefix(js_string::trim(&collapsed), SNIPPET_MAX_CHARS).to_owned()
}

/// `buildSearchSnippets`：下标来自小写化后的正文，截取用原文（与 TS 相同，即便两者长度不同）。
pub(crate) fn build_search_snippets(searchable_text: &str, search: Option<&str>) -> Vec<String> {
    let Some(search) = search else {
        return Vec::new();
    };
    if js_string::trim(searchable_text).is_empty() {
        return Vec::new();
    }
    let needle = utf16(&search.to_lowercase());
    let lowered = utf16(&searchable_text.to_lowercase());
    let original = utf16(searchable_text);
    let mut snippets = Vec::new();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut from = 0;
    while snippets.len() < SNIPPET_LIMIT && from < lowered.len() {
        let Some(index) = index_of(&lowered, &needle, from) else {
            break;
        };
        let start = index.saturating_sub(SNIPPET_PREFIX_RADIUS);
        let end = original
            .len()
            .min(index + needle.len() + SNIPPET_SUFFIX_RADIUS);
        let slice = String::from_utf16_lossy(original.get(start..end.max(start)).unwrap_or(&[]));
        let prefix = if start > 0 { "..." } else { "" };
        let suffix = if end < original.len() { "..." } else { "" };
        let snippet = normalize_snippet(&format!("{prefix}{slice}{suffix}"));
        let overlaps = ranges
            .iter()
            .any(|&(s, e)| (e.min(end) as i64) - (s.max(start) as i64) > 0);
        if !snippet.is_empty() && !overlaps {
            snippets.push(snippet);
            ranges.push((start, end));
        }
        from = index + needle.len();
    }
    if snippets.is_empty() {
        let fallback = normalize_snippet(searchable_text);
        return if fallback.is_empty() {
            Vec::new()
        } else {
            vec![fallback]
        };
    }
    snippets
}

/// `rowToTaskListItem`：meta，命中时追加 `searchSnippet` 与 `searchSnippets`。
pub(crate) fn row_to_list_item(row: &TaskRow, search: Option<&str>) -> Map<String, Value> {
    let meta = row_to_meta(row).to_value();
    let mut item = match meta {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let snippets = build_search_snippets(&row.searchable_text, search);
    if let Some(first) = snippets.first() {
        item.insert("searchSnippet".into(), json!(first));
        item.insert("searchSnippets".into(), json!(snippets));
    }
    item
}

const SELECT_LIST_ROW: &str = "SELECT
          workspace_key,
          workspace_path,
          workspace_identity,
          task_id,
          title,
          task_status,
          provider,
          mode,
          model,
          migration_source,
          forked_from_task_id,
          cron_automation_id,
          off_peak_task_id,
          created_at,
          updated_at,
          unread_at,
          pinned,
          archived,
          deleted,
          title_overridden,
          searchable_text,
          meta_json
        FROM tasks";

/// `queryTaskList(params & {provider})`，返回 `{items, total, hasMore}`。
pub(crate) fn query_task_list(
    conn: &Connection,
    query: &Value,
    provider: Option<&str>,
) -> anyhow::Result<Value> {
    let scopes = scopes(&query["workspaceScopes"]);
    let keys = normalize_workspace_keys(&scopes);
    if keys.is_empty() {
        return Ok(json!({"items": [], "total": 0, "hasMore": false}));
    }
    let search = query["search"]
        .as_str()
        .map(js_string::trim)
        .filter(|search| !search.is_empty());
    let mut clauses = vec![
        "deleted = 0".to_owned(),
        format!("workspace_key IN ({})", placeholders(keys.len())),
    ];
    let mut args: Vec<SqlValue> = keys.iter().cloned().map(SqlValue::Text).collect();
    if let Some(provider) = provider {
        clauses.push("provider = ?".into());
        args.push(SqlValue::Text(provider.to_owned()));
    }
    match query["kind"].as_str() {
        Some("pinned") => clauses.extend(["pinned = 1".into(), "archived = 0".into()]),
        Some("archived") => clauses.push("archived = 1".into()),
        _ => clauses.extend(["pinned = 0".into(), "archived = 0".into()]),
    }
    if let Some(search) = search {
        clauses.push("(LOWER(title) LIKE ? OR LOWER(searchable_text) LIKE ?)".into());
        let pattern = format!("%{}%", search.to_lowercase());
        args.push(SqlValue::Text(pattern.clone()));
        args.push(SqlValue::Text(pattern));
    }
    let where_clause = clauses.join(" AND ");
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(1) AS total FROM tasks WHERE {where_clause}"),
        rusqlite::params_from_iter(args.iter()),
        |row| row.get(0),
    )?;
    let limit = normalize_limit(&query["limit"]);
    let order_by = if query["sortBy"] == "created" {
        "created_at DESC, updated_at DESC, task_id DESC"
    } else {
        "updated_at DESC, created_at DESC, task_id DESC"
    };
    if let Some(limit) = limit {
        args.push(SqlValue::Integer(limit));
    }
    let sql = format!(
        "{SELECT_LIST_ROW}
        WHERE {where_clause}
        ORDER BY {order_by}{}",
        if limit.is_some() { " LIMIT ?" } else { "" }
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(args.iter()), read_task_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Map 构造：同一 key 后写覆盖先写。
    let mut purposes: Vec<(String, String)> = Vec::new();
    for scope in &scopes {
        if let Some(purpose) = scope.workspace_purpose.as_ref().filter(|p| !p.is_empty()) {
            let key = scope.key();
            purposes.retain(|(existing, _)| existing != &key);
            purposes.push((key, purpose.clone()));
        }
    }
    let count = rows.len();
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut item = row_to_list_item(row, search);
            if let Some((_, purpose)) = purposes.iter().find(|(key, _)| key == &row.workspace_key) {
                item.insert("workspacePurpose".into(), json!(purpose));
            }
            Value::Object(item)
        })
        .collect();
    Ok(json!({"items": items, "total": total, "hasMore": total > count as i64}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_follow_typescript_windows() {
        let text = format!("{}needle{}", "a".repeat(30), "b".repeat(100));
        let snippets = build_search_snippets(&text, Some("NEEDLE"));
        assert_eq!(snippets.len(), 1);
        assert!(snippets[0].starts_with("...aaaa"));
        assert!(snippets[0].ends_with("..."));
        // 未命中正文（标题命中）时整段兜底。
        assert_eq!(
            build_search_snippets("  hello \n world ", Some("zzz")),
            vec!["hello world".to_owned()]
        );
        assert!(build_search_snippets("", Some("x")).is_empty());
        assert!(build_search_snippets("abc", None).is_empty());
    }

    #[test]
    fn overlapping_hits_are_merged() {
        let snippets = build_search_snippets("ab ab ab", Some("ab"));
        assert_eq!(snippets, vec!["ab ab ab".to_owned()]);
    }
}
