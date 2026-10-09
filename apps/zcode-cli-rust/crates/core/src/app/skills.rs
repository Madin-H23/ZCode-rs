use super::{Engine, context::RunContext};
use crate::{
    contract::{ContextPort, Event, EventSink, ToolOutput, ToolPort},
    domain::skills::SkillCatalog,
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

impl Engine {
    pub(super) async fn skill_catalog(&mut self, p: &Value) -> Result<Value> {
        let workspace = self.request_workspace(p)?;
        let Some(id) = p["sessionId"].as_str() else {
            // 无会话的目录按请求的 workspace 发现（项目技能来自该 workspace）。
            self.open_scope(&workspace).await?;
            return Ok(self
                .scope(&workspace.key)?
                .tools
                .clone()
                .discover_skills(&CancellationToken::new())
                .await?
                .response("workspace"));
        };
        ensure!(!self.closed.contains(id), "Session unavailable");
        let named = self.named_workspace(p)?;
        self.ensure_session_in(id, named.as_ref()).await?;
        if self.sessions[id].skills.is_none() {
            let catalog = self
                .session_tools(id)?
                .discover_skills(&CancellationToken::new())
                .await?;
            self.sessions.get_mut(id).unwrap().skills = Some(catalog);
            self.persist(id, None).await?;
        }
        Ok(self.sessions[id]
            .skills
            .as_ref()
            .unwrap()
            .response("session"))
    }
}
pub(super) async fn initialize(
    tools: &dyn ToolPort,
    context: &dyn ContextPort,
    history: &mut RunContext,
    sink: &EventSink,
    cancel: &CancellationToken,
) -> Result<()> {
    if history.skills.is_some() && history.prompt_snapshot.is_some() {
        return Ok(());
    }
    let catalog = if let Some(catalog) = &history.skills {
        catalog.clone()
    } else {
        tools.discover_skills(cancel).await?
    };
    let (reply, receipt) = tokio::sync::oneshot::channel();
    if history.prompt_snapshot.is_none() {
        let mut snapshot = context.snapshot(cancel).await?;
        // Node 恢复时以存储的 envInfo 作 config.envInfo（不重新探测 Git），日期按本次重建。
        if let Some(env) = &history.stored_env {
            snapshot =
                crate::domain::prompt::PromptSnapshot::from_env_info(env, snapshot.current_date);
        }
        sink.send(Event::PromptInitialized {
            snapshot: Box::new(snapshot.clone()),
            skills: catalog,
            committed: reply,
        })
        .await?;
        history.prompt_snapshot = Some(snapshot);
    } else {
        sink.send(Event::SkillsInitialized { catalog, reply })
            .await?;
    }
    history.skills = Some(
        tokio::select! {biased; _=cancel.cancelled()=>anyhow::bail!("Cancelled"), result=receipt=>result.context("Skill catalog commit failed")?},
    );
    Ok(())
}
pub(super) async fn execute(
    tools: &dyn ToolPort,
    catalog: &SkillCatalog,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<ToolOutput> {
    ensure!(catalog.enabled, "Skills are disabled");
    let name = args["skill"]
        .as_str()
        .or_else(|| args["name"].as_str())
        .filter(|n| !n.trim().is_empty())
        .context("Skill name required")?;
    ensure!(
        args.get("args").is_none_or(Value::is_string),
        "Skill args must be a string"
    );
    let skill = catalog
        .skills
        .iter()
        .find(|s| s.name == name || s.qualified_name() == name)
        .context("Skill not in this session's catalog")?;
    tools.load_skill(skill, name, cancel).await
}
