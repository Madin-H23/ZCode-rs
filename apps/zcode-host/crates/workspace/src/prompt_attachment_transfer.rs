//! `prompt-attachment-transfer` channel，本地实现
//! （packages/services/src/prompt-attachment-transfer/promptAttachmentTransferService.ts）。
//!
//! 本地 workspace 保持 localPath 零拷贝：`stage` 直接把本地路径当 ref 返回，不伪造上传进度；
//! 远端 workspace 的跨机暂存随 R6 的远端集合一起做。
use serde::Deserialize;
use serde_json::{Value, json};
use zcode_host_kit::{Reply, RpcError, ServiceBuilder};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StageParams {
    operation_id: String,
    local_path: String,
    size_bytes: Option<f64>,
}

async fn stage(params: StageParams) -> Value {
    let bytes = match params.size_bytes {
        Some(size) if size.is_finite() && size > 0.0 && size.fract() == 0.0 => json!(size as u64),
        Some(size) if size.is_finite() && size > 0.0 => json!(size),
        _ => json!(
            tokio::fs::metadata(&params.local_path)
                .await
                .map(|meta| meta.len())
                .unwrap_or(0)
        ),
    };
    json!({
        "operationId": params.operation_id,
        "ref": params.local_path,
        "bytes": bytes,
        "staged": false,
    })
}

/// `adopt` / `cancel` / `cleanup`：本地没有暂存副本，什么都不用做（参数仍按契约校验）。
async fn no_op(args: zcode_host_kit::Args) -> Result<Reply, RpcError> {
    let _: String = args.get(0, "operationId")?;
    Ok(Reply::Undefined)
}

pub fn service() -> ServiceBuilder {
    ServiceBuilder::new("prompt-attachment-transfer")
        .method("stage", |_, args| async move {
            let params: StageParams = args.get(0, "params")?;
            Ok(Reply::Value(stage(params).await))
        })
        .method("adopt", |_, args| no_op(args))
        .method("cancel", |_, args| no_op(args))
        .method("cleanup", |_, args| no_op(args))
        // 本地 stage 不产生进度；订阅合法，但永远没有事件。
        .event("onDynamicProgress", |_, _| {
            Ok(Box::pin(futures_util::stream::pending()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{call, channel};

    #[tokio::test]
    async fn stage_returns_local_path_without_copy() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"12345").unwrap();
        let transfer = channel(service());
        let path = file.to_str().unwrap();
        let staged = call(
            &transfer,
            "stage",
            json!({"operationId": "op", "sessionId": "s", "workspacePath": "/w",
                   "localPath": path, "fileName": "a.bin", "mime": "application/octet-stream"}),
        )
        .await
        .unwrap();
        assert_eq!(
            staged,
            json!({"operationId": "op", "ref": path, "bytes": 5, "staged": false})
        );
        let sized = call(
            &transfer,
            "stage",
            json!({"operationId": "op", "sessionId": "s", "workspacePath": "/w", "sizeBytes": 9,
                   "localPath": "/missing", "fileName": "x", "mime": "text/plain"}),
        )
        .await
        .unwrap();
        assert_eq!(sized["bytes"], 9);
        let missing = call(
            &transfer,
            "stage",
            json!({"operationId": "op", "sessionId": "s", "workspacePath": "/w",
                   "localPath": "/definitely/missing", "fileName": "x", "mime": "text/plain"}),
        )
        .await
        .unwrap();
        assert_eq!(missing["bytes"], 0);
        assert!(
            transfer
                .call(
                    &zcode_host_kit::CallContext {
                        connection: 1,
                        attachment_id: "test".into(),
                        client_mode: zcode_host_control::ClientMode::DesktopContinuous,
                    },
                    "adopt",
                    vec![zcode_host_kit::RpcValue::String("op".into())]
                )
                .await
                .map(|reply| matches!(reply, Reply::Undefined))
                .unwrap()
        );
    }
}
