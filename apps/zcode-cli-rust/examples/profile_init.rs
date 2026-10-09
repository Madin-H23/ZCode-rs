//! Isolated stage profiler: pass an empty workspace and HOME, never a user project.
#![expect(clippy::print_stdout, reason = "开发用示例，结果输出到 stdout")]
use std::{path::PathBuf, time::Instant};
use tokio_util::sync::CancellationToken;
use zcode_cli_rust::{
    adapters::context_source::WorkspaceContext,
    contract::{ContextPort, ToolPort},
};
fn stage(name: &str, start: Instant) {
    println!("{name}: {:.3} ms", start.elapsed().as_secs_f64() * 1000.0);
}
#[tokio::main]
#[expect(
    clippy::disallowed_methods,
    reason = "示例从进程环境取 HOME 与探测环境"
)]
async fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::args().nth(1).expect("isolated workspace"));
    anyhow::ensure!(
        PathBuf::from(std::env::var("HOME")?).starts_with(&root),
        "Profile requires an isolated HOME under its root"
    );
    let cancel = CancellationToken::new();
    let start = Instant::now();
    let scope = zcode_cli_rust::runtime::instance_scope();
    let tools =
        zcode_cli_rust::workspace_tools(root.clone(), root.join("artifacts"), &scope.spawner())?;
    stage("tools.new", start);
    let context = WorkspaceContext::new(
        root,
        PathBuf::from(std::env::var("HOME")?),
        false,
        std::env::vars().collect(),
    );
    let start = Instant::now();
    let _ = tools.discover_skills(&cancel).await?;
    stage("skills", start);
    let start = Instant::now();
    let _ = context.snapshot(&cancel).await?;
    stage("snapshot", start);
    let start = Instant::now();
    let _ = tools.scoped_definitions("profile", &cancel).await?;
    stage("tools.definitions", start);
    let start = Instant::now();
    let _ = tools.agent_profiles(&cancel).await?;
    stage("profiles", start);
    let start = Instant::now();
    let _ = context.instructions(&cancel).await?;
    stage("instructions", start);
    let start = Instant::now();
    let _ = zcode_cli_rust::egress()?
        .client(zcode_cli_net::Purpose::Model)
        .await?;
    stage("http.client", start);
    Ok(())
}
