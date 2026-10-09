//! 会话帧订阅不可用时的兜底回推：从会话行里取最后一轮的结果（R6 首版的行为）。
//! 正常路径见 [`super::watch`]。
use serde_json::Value;

use super::FinalReply;

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
