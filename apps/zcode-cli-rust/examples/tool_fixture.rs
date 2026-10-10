//! Test-only direct adapter driver; the shipped Agent still only accepts app-server --stdio.
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use zcode_cli_rust::contract::ToolPort;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cwd = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let scope = zcode_cli_rust::runtime::instance_scope();
    let tools =
        zcode_cli_rust::workspace_tools(cwd.clone(), cwd.join(".artifacts"), &scope.spawner())?;
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let input: Value = serde_json::from_str(&line)?;
        let result = if input["definitions"] == true {
            let mut definitions = tools.definitions();
            if let Some(input_format) = input.get("inputFormat") {
                tools.model_definitions(&mut definitions, input_format);
            }
            if let Some(profiles) = input.get("profiles") {
                let profiles: Vec<zcode_cli_rust::domain::subagent::Profile> =
                    serde_json::from_value(profiles.clone())?;
                if let Some(agent) = definitions
                    .iter_mut()
                    .find(|tool| tool["function"]["name"] == "Agent")
                {
                    let base = agent["function"]["description"]
                        .as_str()
                        .unwrap_or_default();
                    agent["function"]["description"] =
                        zcode_cli_rust::domain::agent_description::description(base, &profiles)
                            .into();
                }
            }
            json!({"definitions":definitions})
        } else {
            match tools
                .call(
                    input["session"].as_str().unwrap_or("fixture"),
                    input["name"].as_str().unwrap(),
                    &input["args"],
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
            {
                Ok(result) => {
                    json!({"data":result.data,"content":result.content,"display":result.display})
                }
                Err(e) => json!({"error":e.to_string()}),
            }
        };
        out.write_all(format!("{result}\n").as_bytes()).await?;
        out.flush().await?;
    }
    tools.shutdown().await
}
