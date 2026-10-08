//! `media-preview` channel（packages/services/src/media-preview/mediaPreview.ts）。
//!
//! 本地 desktop：路径经 Main 授权（`local-media-preview-path-authorize-*` 控制消息）后返回
//! `zcode-media://` URL，Renderer 直接流式播放；没有授权器时退回 8 MiB 内联预览。
//! `refreshPlaybackUrl` / `release` 只属于远端 range 代理，本地与 Node 一样没有实现。
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use zcode_host_control::ControlRouter;
use zcode_host_kit::{Reply, RpcError, ServiceBuilder};

use crate::agent::extensions::MediaPathAuthorizer;
use crate::file::errors::{node_error, plain_error};
use crate::main_bridge::{MainBridge, random_id};

const INLINE_MAX_BYTES: u64 = 8 * 1024 * 1024;
const LOCAL_MEDIA_PREVIEW_SCHEME: &str = "zcode-media";
pub const AUTHORIZE_REQUEST: &str = "local-media-preview-path-authorize-request";
pub const AUTHORIZE_RESULT: &str = "local-media-preview-path-authorize-result";
/// Node 不设超时；Main 不应答时 Host 不能无限挂起调用，30 s 后失败（见规格第 5 节）。
const AUTHORIZE_TIMEOUT: Duration = Duration::from_secs(30);

/// `MEDIA_PREVIEW_FORMATS`（packages/shared/src/media-preview.ts）。
const FORMATS: &[(&str, &str, &str)] = &[
    (".mp4", "video", "video/mp4"),
    (".mov", "video", "video/quicktime"),
    (".webm", "video", "video/webm"),
    (".m4v", "video", "video/x-m4v"),
    (".mp3", "audio", "audio/mpeg"),
    (".wav", "audio", "audio/wav"),
    (".m4a", "audio", "audio/mp4"),
    (".ogg", "audio", "audio/ogg"),
    (".opus", "audio", "audio/opus"),
    (".flac", "audio", "audio/flac"),
    (".weba", "audio", "audio/webm"),
];

/// `getMediaPreviewFormat`：返回 (kind, mediaType)。
pub fn media_format(path: &str) -> Option<(&'static str, &'static str)> {
    let normalized = path.replace('\\', "/").to_lowercase();
    FORMATS
        .iter()
        .find(|(extension, _, _)| normalized.ends_with(extension))
        .map(|(_, kind, media)| (*kind, *media))
}

/// `buildLocalMediaPreviewUrl`：WHATWG `URLSearchParams` 编码（空格为 `+`）。
pub fn local_media_preview_url(path: &str) -> String {
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("path", path)
        .finish();
    format!("{LOCAL_MEDIA_PREVIEW_SCHEME}://local/preview?{query}")
}

/// 经 Main 授权本地媒体路径；Main 规范化并登记后返回规范路径。
pub fn main_authorizer(bridge: MainBridge, router: &mut ControlRouter) -> MediaPathAuthorizer {
    let resolver = bridge.clone();
    router.on(AUTHORIZE_RESULT, move |result| {
        if let Some(request_id) = result["requestId"].as_str() {
            resolver.resolve(request_id, result.clone());
        }
    });
    std::sync::Arc::new(move |path: String| {
        let bridge = bridge.clone();
        Box::pin(async move {
            let request_id = random_id();
            let message = json!({"type": AUTHORIZE_REQUEST, "requestId": request_id, "path": path});
            let result = bridge
                .request(request_id, message, AUTHORIZE_TIMEOUT, |_| {})
                .await
                .ok_or_else(|| {
                    RpcError::new("Error", "Local media preview path authorization timed out.")
                })?;
            match result["path"].as_str() {
                Some(canonical) if result["ok"] == true && !canonical.is_empty() => {
                    Ok(canonical.to_owned())
                }
                _ => Err(RpcError::new(
                    "Error",
                    result["error"]
                        .as_str()
                        .unwrap_or("本地视频预览路径授权失败"),
                )),
            }
        })
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrepareParams {
    path: String,
    expected_kind: String,
}

async fn prepare(
    authorizer: Option<MediaPathAuthorizer>,
    params: PrepareParams,
) -> Result<Value, RpcError> {
    let path = params.path;
    let Some((_, media_type)) =
        media_format(&path).filter(|(kind, _)| *kind == params.expected_kind)
    else {
        return Err(plain_error(format!(
            "Unsupported media preview format: {path}"
        )));
    };
    let file = PathBuf::from(&path);
    let meta = tokio::fs::metadata(&file)
        .await
        .map_err(|error| node_error(&error, "stat", &file))?;
    if !meta.is_file() {
        return Err(plain_error(format!("Path is not a media file: {path}")));
    }
    let size = meta.len();
    if let Some(authorize) = authorizer {
        let canonical = authorize(path).await?;
        return Ok(json!({
            "kind": "local-url",
            "mediaType": media_type,
            "url": local_media_preview_url(&canonical),
            "path": canonical,
            "size": size,
        }));
    }
    if size > INLINE_MAX_BYTES {
        return Err(plain_error(format!(
            "Media file is too large for inline preview: {path}"
        )));
    }
    let bytes = tokio::fs::read(&file)
        .await
        .map_err(|error| node_error(&error, "open", &file))?;
    Ok(json!({
        "kind": "inline",
        "dataBase64": zcode_host_kit::base64_encode(&bytes),
        "mediaType": media_type,
        "path": path,
        "size": bytes.len(),
    }))
}

pub fn service(authorizer: Option<MediaPathAuthorizer>) -> ServiceBuilder {
    // 与 Node 本地服务一样不提供这两个可选成员：@zcode/rpc 的 ProxyChannel 对缺失方法
    // 报 `Method not found: <name>`；Renderer 只在 host-range-url 时调用，release 的失败被忽略。
    let missing = |name: &'static str| {
        move |_, _| async move {
            Err::<Reply, _>(RpcError::new("Error", format!("Method not found: {name}")))
        }
    };
    ServiceBuilder::new("media-preview")
        .method("prepare", move |_, args| {
            let authorizer = authorizer.clone();
            async move {
                let params: PrepareParams = args.get(0, "params")?;
                Ok(Reply::Value(prepare(authorizer, params).await?))
            }
        })
        .method("refreshPlaybackUrl", missing("refreshPlaybackUrl"))
        .method("release", missing("release"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{call, channel};

    #[test]
    fn url_encoding_matches_url_search_params() {
        // node: new URL("zcode-media://local/preview").searchParams.set("path", …)
        assert_eq!(
            local_media_preview_url("/tmp/a b/视频+1&x=~*.mp4"),
            "zcode-media://local/preview?path=%2Ftmp%2Fa+b%2F%E8%A7%86%E9%A2%91%2B1%26x%3D%7E*.mp4"
        );
        assert_eq!(
            media_format(r"C:\V\CLIP.MOV"),
            Some(("video", "video/quicktime"))
        );
        assert_eq!(media_format("a.txt"), None);
    }

    #[tokio::test]
    async fn prepare_authorizes_local_paths_and_falls_back_inline() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("clip.mp4");
        std::fs::write(&video, b"0123").unwrap();
        let path = video.to_str().unwrap().to_owned();
        let authorizer: MediaPathAuthorizer = std::sync::Arc::new(|path: String| {
            Box::pin(async move { Ok(format!("{path}.canonical")) })
        });
        let preview = channel(service(Some(authorizer)));
        let prepared = call(
            &preview,
            "prepare",
            json!({"path": path, "expectedKind": "video"}),
        )
        .await
        .unwrap();
        assert_eq!(prepared["kind"], "local-url");
        assert_eq!(prepared["path"], format!("{path}.canonical"));
        assert_eq!(prepared["size"], 4);
        assert_eq!(
            prepared["url"],
            local_media_preview_url(&format!("{path}.canonical"))
        );

        let wrong_kind = call(
            &preview,
            "prepare",
            json!({"path": path, "expectedKind": "audio"}),
        )
        .await
        .unwrap_err();
        assert_eq!(
            wrong_kind.message,
            format!("Unsupported media preview format: {path}")
        );
        let directory = dir.path().join("dir.mp4");
        std::fs::create_dir(&directory).unwrap();
        let not_file = call(
            &preview,
            "prepare",
            json!({"path": directory.to_str().unwrap(), "expectedKind": "video"}),
        )
        .await
        .unwrap_err();
        assert!(not_file.message.starts_with("Path is not a media file: "));

        let inline = channel(service(None));
        let prepared = call(
            &inline,
            "prepare",
            json!({"path": path, "expectedKind": "video"}),
        )
        .await
        .unwrap();
        assert_eq!(
            prepared,
            json!({"kind": "inline", "dataBase64": "MDEyMw==", "mediaType": "video/mp4", "path": path, "size": 4})
        );
        let missing = call(&inline, "release", json!({"previewId": "x"}))
            .await
            .unwrap_err();
        assert_eq!(missing.message, "Method not found: release");
    }

    #[tokio::test]
    async fn main_authorizer_round_trips_through_control_messages() {
        struct Sink(std::sync::Mutex<Vec<Value>>);
        impl zcode_host_control::ControlSink for Sink {
            fn send(&self, message: zcode_host_control::HostToMain) {
                if let zcode_host_control::HostToMain::Raw(value) = message {
                    self.0.lock().unwrap().push(value);
                }
            }
        }
        let sink = std::sync::Arc::new(Sink(Default::default()));
        let mut router = ControlRouter::default();
        let authorize = main_authorizer(MainBridge::new(sink.clone()), &mut router);
        let pending = tokio::spawn(authorize("/w/a.mp4".into()));
        let request = loop {
            if let Some(request) = sink.0.lock().unwrap().first().cloned() {
                break request;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(request["type"], AUTHORIZE_REQUEST);
        assert_eq!(request["path"], "/w/a.mp4");
        router.dispatch(
            AUTHORIZE_RESULT,
            json!({"type": AUTHORIZE_RESULT, "requestId": request["requestId"], "ok": true, "path": "/real/a.mp4"}),
        );
        assert_eq!(pending.await.unwrap().unwrap(), "/real/a.mp4");

        let denied = tokio::spawn(authorize("/w/b.mp4".into()));
        let request = loop {
            if let Some(request) = sink.0.lock().unwrap().get(1).cloned() {
                break request;
            }
            tokio::task::yield_now().await;
        };
        router.dispatch(
            AUTHORIZE_RESULT,
            json!({"type": AUTHORIZE_RESULT, "requestId": request["requestId"], "ok": false, "error": "denied"}),
        );
        assert_eq!(denied.await.unwrap().unwrap_err().message, "denied");
    }
}
