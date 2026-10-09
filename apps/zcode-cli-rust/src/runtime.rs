//! Composition shared by `app-server`, `-p` and embedders: the workspace context
//! read once at startup and the engine wired to the adapters (spec
//! rust-u1-runtime-library). Nothing here reads the process environment or the
//! process cwd except [`RuntimeOptions::from_process`], the binary's entry.
use anyhow::{Context as _, Result};
use std::path::PathBuf;
use std::sync::Arc;
use zcode_cli_core::Engine;
use zcode_cli_core_api::{
    ModelIdentity, ModelPort, ModelRegistry, RuntimePorts, SessionStore, Workspace, WorkspacePorts,
    WorkspaceSource,
};
use zcode_cli_domain::config::ConfigSnapshot;
use zcode_cli_host::{SystemClock, WorkspaceConfig, WorkspaceContext};
use zcode_cli_model::{config::ModelConfig, provider::HttpModel, registry::Registry};
use zcode_cli_net::{Egress, NetworkPolicy, RuntimeEnv};
use zcode_cli_state::NodeStore;
use zcode_cli_tools::WorkspaceTools;

/// Every external fact a runtime starts from. An embedder fills it in; the
/// binary uses [`RuntimeOptions::from_process`].
pub struct RuntimeOptions {
    pub home: PathBuf,
    /// The environment the runtime sees (including variables an embedder injects).
    pub env: Vec<(String, String)>,
    pub argv: Vec<String>,
    /// The default workspace: its path as given (the identity uses it unresolved).
    pub cwd: PathBuf,
    pub workspace_identity: Option<String>,
    /// What relative paths (data dir, `storage.dir`) resolve against.
    pub base_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl RuntimeOptions {
    /// The binary's view: process environment, argv and cwd, read exactly once.
    #[expect(
        clippy::disallowed_methods,
        reason = "二进制入口：进程环境与 cwd 只在这里读取"
    )]
    pub fn from_process(
        home: PathBuf,
        cwd: Option<PathBuf>,
        data_dir: Option<PathBuf>,
    ) -> std::io::Result<Self> {
        let lossy = |value: std::ffi::OsString| value.to_string_lossy().into_owned();
        let base_dir = std::env::current_dir()?;
        let data_dir = data_dir.unwrap_or_else(|| {
            std::env::var_os("ZCODE_CLI_RUST_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".zcode/rust"))
        });
        Ok(Self {
            env: std::env::vars_os()
                .map(|(key, value)| (lossy(key), lossy(value)))
                .collect(),
            argv: std::env::args_os().map(lossy).collect(),
            cwd: cwd.unwrap_or_else(|| base_dir.clone()),
            workspace_identity: std::env::var("ZCODE_WORKSPACE_IDENTITY").ok(),
            base_dir,
            data_dir,
            home,
        })
    }
}

/// `path` made absolute against `base` instead of the process cwd.
fn absolute(base: &std::path::Path, path: &std::path::Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        std::path::absolute(path)
    } else {
        std::path::absolute(base.join(path))
    }
}

/// What every entry resolves before touching storage.
pub struct Context {
    pub home: PathBuf,
    base_dir: PathBuf,
    /// The path as given (the workspace identity uses it unresolved).
    pub requested_cwd: PathBuf,
    pub cwd: PathBuf,
    pub data_dir: PathBuf,
    pub workspace: String,
    pub runtime_env: Arc<RuntimeEnv>,
    pub workspace_config: Arc<WorkspaceConfig>,
    pub startup_config: ConfigSnapshot,
    pub egress: Arc<Egress>,
}

impl Context {
    /// `options.home` is resolved once by the entry point
    /// (`zcode_kit::paths::home_dir`), before logging starts, so every reader
    /// shares the same directory.
    pub async fn prepare(options: RuntimeOptions) -> Result<Self> {
        let RuntimeOptions {
            home,
            env,
            argv,
            cwd: requested_cwd,
            workspace_identity,
            base_dir,
            data_dir: requested_data,
        } = options;
        let cwd = tokio::fs::canonicalize(base_dir.join(&requested_cwd))
            .await
            .context("Workspace unavailable")?;
        let data_dir = base_dir.join(requested_data);
        tokio::fs::create_dir_all(&data_dir).await?;
        let data_dir = tokio::fs::canonicalize(data_dir).await?;
        // 身份使用 Host 提交的路径，不把 macOS /var -> /private/var 的 realpath 改写成新工作区。
        let workspace =
            zcode_cli_host::workspace_identity(workspace_identity.as_deref(), &requested_cwd);
        // 与 Node 启动时净化 process.env 等价：只捕获一次，所有读取方共用这份视图，不改真实进程环境。
        let runtime_env = Arc::new(RuntimeEnv::capture(env, &home, &argv));
        // 配置按 Node 分层规则从 cwd 解析；各入口每次重新加载，与 Node 一致没有文件监听。
        let workspace_config = Arc::new(WorkspaceConfig::new(
            absolute(&base_dir, &requested_cwd)?,
            home.clone(),
            runtime_env.vars().to_vec(),
        ));
        // 网络出口与权限配置与 Node 一样取启动时的配置快照，运行中修改不影响已建立的出口。
        let startup_config = workspace_config.snapshot().await;
        let egress = Arc::new(
            Egress::new(
                runtime_env.clone(),
                &NetworkPolicy::from_config(&startup_config.config["network"]),
                &home,
                &base_dir,
                "electron",
            )
            .map_err(anyhow::Error::msg)
            .context("Invalid network configuration")?,
        );
        Ok(Self {
            home,
            base_dir,
            requested_cwd,
            cwd,
            data_dir,
            workspace,
            runtime_env,
            workspace_config,
            startup_config,
            egress,
        })
    }

    /// The Node session database and artifact root (spec rust-m11-node-storage §2.1).
    pub async fn node_database(&self) -> Result<zcode_cli_host::storage_paths::StoragePaths> {
        let config = self.workspace_config.snapshot().await;
        Ok(zcode_cli_host::storage_paths::resolve(
            &self.base_dir,
            &self.home,
            &config,
        ))
    }

    /// The Node database as the session store.
    pub async fn node_store(&self) -> Result<NodeStore> {
        let source = self.node_database().await?;
        let media_cache = self.data_dir.join("tool-results");
        NodeStore::open(source.database, source.artifacts, media_cache).await
    }

    /// The engine over `store`: a static `--config` model or the provider
    /// registry from the environment.
    pub async fn engine(
        &self,
        store: Arc<dyn SessionStore>,
        config: Option<&PathBuf>,
        desktop: bool,
    ) -> Result<Engine> {
        let question_timing = zcode_cli_host::question_timing(self.runtime_env.vars())?;
        let config = ModelConfig::load(config).await?;
        let registry = if config.is_none() {
            Registry::from_env(self.egress.clone())
                .await?
                .map(|r| r as Arc<dyn ModelRegistry>)
        } else {
            None
        };
        let identity = config.as_ref().map(|c| ModelIdentity {
            provider_id: c.provider_id.clone(),
            model_id: c.model_id.clone(),
            reasoning_level: c.reasoning_level.clone(),
        });
        let model = config
            .map(|c| HttpModel::new(c, self.egress.clone()))
            .map(|m| Arc::new(m) as Arc<dyn ModelPort>);
        // 与 Desktop 共用的项目 hook 信任存储：路径由用户配置文件的 storage.dir 决定。
        // 用户配置不可读时不启用信任（项目 hooks 不运行，保持 fail-closed），不影响启动。
        let user_config = std::path::Path::new(&self.startup_config.user_path);
        let trust_store =
            match zcode_cli_host::trust_store::trust_store_path(&self.home, user_config).await {
                Ok(path) => Some(Arc::new(zcode_cli_host::trust_store::FileTrustStore::new(
                    path,
                ))),
                Err(error) => {
                    tracing::warn!(
                        event = "workspace_hook.trust_store_unavailable",
                        error = %format!("{error:#}"),
                        "Workspace Hook Trust store is unavailable"
                    );
                    None
                }
            };
        let tools = Arc::new(WorkspaceTools::new(
            self.cwd.clone(),
            self.data_dir.join("tool-results"),
            self.workspace_config.clone(),
            self.egress.clone(),
        ));
        // 进程 cwd 只是缺省 workspace；请求可以指向任意 workspace（spec rust-cli-architecture）。
        let workspace = Workspace {
            key: self.workspace.clone(),
            path: self.requested_cwd.to_string_lossy().into_owned(),
        };
        let workspaces = Arc::new(Workspaces {
            home: self.home.clone(),
            base_dir: self.base_dir.clone(),
            desktop,
            runtime_env: self.runtime_env.clone(),
            tools: tools.clone(),
        });
        let engine = Engine::new(
            workspace,
            identity,
            RuntimePorts {
                context: Arc::new(WorkspaceContext::new(
                    self.cwd.clone(),
                    self.home.clone(),
                    desktop,
                    self.runtime_env.vars().into(),
                )),
                store,
                model,
                tools,
                clock: Arc::new(SystemClock),
            },
        )
        .await?
        .with_workspaces(workspaces)
        .with_question_timing(question_timing.0, question_timing.1)
        .with_permission_config(&self.startup_config.config["permission"])
        .with_anomaly_guard(&self.startup_config.config["modelAnomalyGuard"])
        .with_hooks(
            &self.startup_config.config["hooks"],
            &self.startup_config.user_path,
        )
        .with_registry(registry);
        #[cfg(debug_assertions)]
        let engine = engine.with_fault_marker(
            self.runtime_env
                .vars()
                .iter()
                .find(|(name, _)| name == "ZCODE_RUNTIME_FAULT_MARKER")
                .map(|(_, value)| value.clone()),
        );
        Ok(match trust_store {
            Some(store) => engine.with_workspace_trust(
                store,
                self.workspace_config.clone(),
                Some(env!("CARGO_PKG_VERSION").into()),
            ),
            None => engine,
        })
    }
}

/// Opens the workspaces requests name (spec rust-cli-architecture, Workspace
/// 作用域): each reads its own layered configuration and gets a view of the
/// startup tool runtime rooted at its path.
struct Workspaces {
    home: PathBuf,
    base_dir: PathBuf,
    desktop: bool,
    runtime_env: Arc<RuntimeEnv>,
    tools: Arc<WorkspaceTools>,
}

#[async_trait::async_trait]
impl WorkspaceSource for Workspaces {
    async fn open(&self, workspace: &Workspace) -> Result<WorkspacePorts> {
        let requested = PathBuf::from(&workspace.path);
        // 与启动 workspace 相同：配置按给定路径分层解析，文件与命令用解析后的真实路径。
        // 路径不存在时不阻断（列表、索引仍可用），工具使用时按各自规则失败。
        let root = absolute(&self.base_dir, &requested)?;
        let cwd = tokio::fs::canonicalize(&root)
            .await
            .unwrap_or_else(|_| root.clone());
        let config = Arc::new(WorkspaceConfig::new(
            root,
            self.home.clone(),
            self.runtime_env.vars().to_vec(),
        ));
        Ok(WorkspacePorts {
            context: Arc::new(WorkspaceContext::new(
                cwd.clone(),
                self.home.clone(),
                self.desktop,
                self.runtime_env.vars().into(),
            )),
            tools: Arc::new(self.tools.scoped(workspace, cwd, config.clone())),
            config,
        })
    }
}
