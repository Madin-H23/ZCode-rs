pub use zcode_cli_app_server as app_server;
pub use zcode_cli_core as app;
pub use zcode_cli_core_api as contract;
pub use zcode_cli_domain as domain;
pub mod embed;
pub mod runtime;
pub mod serve;
pub mod adapters {
    pub use zcode_cli_host::{SystemClock, context_source};
    pub use zcode_cli_model::{config, model_protocol, provider, registry};
    pub use zcode_cli_state::{NodeStore, node};
    pub use zcode_cli_tools::tools;
}

/// Run an engine behind the in-process App Server, exchanging parsed wire
/// values instead of stdio. Used by tests and embedders.
pub async fn serve_values(
    engine: app::Engine,
    input: tokio::sync::mpsc::Receiver<contract::Input>,
    output: tokio::sync::mpsc::Sender<Vec<serde_json::Value>>,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    app_server::serve(
        move |rx, tx| engine.serve(rx, tx, cancel),
        input,
        app_server::Sink::Values(output),
    )
    .await
}

/// The process environment as Node sees it after startup sanitization.
/// Non-Unicode entries are converted lossily, like Node's `process.env`.
pub fn runtime_env(home: &std::path::Path) -> zcode_cli_net::RuntimeEnv {
    let vars = std::env::vars_os().map(|(k, v)| {
        (
            k.to_string_lossy().into_owned(),
            v.to_string_lossy().into_owned(),
        )
    });
    let argv: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    zcode_cli_net::RuntimeEnv::capture(vars, home, &argv)
}

/// Egress built from the process environment without network configuration.
/// Used by tests, examples and embedders. Fails when the home directory
/// cannot be resolved (`zcode_kit::paths::home_dir`).
pub fn egress() -> anyhow::Result<std::sync::Arc<zcode_cli_net::Egress>> {
    let home = zcode_kit::paths::home_dir()?;
    let env = std::sync::Arc::new(runtime_env(&home));
    let egress = zcode_cli_net::Egress::new(env, &Default::default(), &home, "electron")
        .map_err(anyhow::Error::msg)?;
    Ok(std::sync::Arc::new(egress))
}

/// Workspace tools wired to the process environment's layered configuration,
/// as the binary does. Used by tests, examples and embedders.
pub fn workspace_tools(
    cwd: std::path::PathBuf,
    artifacts: std::path::PathBuf,
) -> anyhow::Result<zcode_cli_tools::WorkspaceTools> {
    let egress = egress()?;
    let config = zcode_cli_host::WorkspaceConfig::new(
        cwd.clone(),
        zcode_kit::paths::home_dir()?,
        egress.runtime_env().vars().to_vec(),
    );
    Ok(zcode_cli_tools::WorkspaceTools::new(
        cwd,
        artifacts,
        std::sync::Arc::new(config),
        egress,
    ))
}
