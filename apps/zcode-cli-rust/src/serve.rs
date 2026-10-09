//! The App Server entry without its transport (spec rust-u1-runtime-library §3):
//! storage preparation and progress frames, engine composition and protocol
//! routing. The binary feeds it stdin / stdout; an embedder feeds it a pipe.
use crate::runtime::{Context, RuntimeOptions};
use anyhow::Result;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zcode_cli_app_server::{self as app_server, Sink, stdio};
use zcode_cli_core_api::{Input, SessionStore};

pub struct AppServerOptions {
    pub runtime: RuntimeOptions,
    /// `--config`: a static model instead of the provider registry.
    pub config: Option<PathBuf>,
    /// `--surface desktop`.
    pub desktop: bool,
    /// `--prepare-storage`: run the storage handshake and migrations, then stop.
    pub prepare_storage: bool,
}

/// Serves the App protocol until the input ends or `cancel` fires. `output` is
/// dropped on return, so the caller can then wait for its writer to drain.
/// `tasks` is the instance's scope ([`crate::runtime::instance_scope`]).
pub async fn app_server(
    options: AppServerOptions,
    tasks: &zcode_async::Spawner,
    mut input: mpsc::Receiver<Input>,
    mut output: Sink,
    cancel: CancellationToken,
) -> Result<()> {
    let ctx = Context::prepare(options.runtime, tasks.clone()).await?;
    // 会话存储只有 Node 的库（spec rust-m11-node-storage §2.1）。
    let path = ctx.node_database().await?.database;
    let attempt = zcode_cli_host::id();
    let database_id = format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()));
    let progress = |phase: &str, sequence: u64| json!({"method":"startup/storageState","params":{"schemaVersion":1,"attemptId":attempt,"sequence":sequence,"databaseId":database_id,"databaseKind":"session","phase":phase,"elapsedMs":0}});
    if options.prepare_storage {
        stdio::storage_prepare(&path, &mut input, &mut output).await?;
    }
    output.send_values(vec![progress("checking", 1)]).await?;
    let store: Arc<dyn SessionStore> = match ctx.node_store().await {
        Ok(store) => Arc::new(store),
        Err(error) => {
            let mut frame = progress("failed", 2);
            // Node `DatabaseStartupErrorCode`：迁移与打开失败按原因上报，其余为 sql_failed。
            frame["params"]["errorCode"] = error
                .downcast_ref::<zcode_cli_state::node::open::StartupError>()
                .map_or("sql_failed", |e| e.code)
                .into();
            output.send_values(vec![frame]).await?;
            anyhow::bail!("Session storage failed");
        }
    };
    output.send_values(vec![progress("ready", 2)]).await?;
    if options.prepare_storage {
        drop(store);
        return output
            .send_values(vec![
                json!({"method":"startup/storagePrepared","params":{}}),
            ])
            .await;
    }
    // Node 的协议会话在首条输入后生成标题（`-p` 不生成）。
    let engine = ctx
        .engine(store, options.config.as_ref(), options.desktop)
        .await?
        .with_title_generation(true);
    // App Server 独占输出；返回时已排空 runtime 输出并释放 sink。
    app_server::serve(move |rx, tx| engine.serve(rx, tx, cancel), input, output).await
}
