//! `oauth` channel（packages/services/src/oauth/oauthService.ts、oauthUnauthorizedRequest.ts）。
//!
//! 所有改写 OAuth 凭据的路径都在会话变更队列里串行执行：owner 任务 `oauth-session-mutations`
//! （docs/specs/rust-u2-task-scope.md §6.3）按提交顺序逐个执行，提交方放弃等待不打断已经开始的变更。
//! `session_generation` 让迟到的后台结果（profile 迁移、刷新）无法写回已退出或已切换的会话。pending
//! flow 以 `pending_id` 标识身份（对应 TS 的 `this.pendingState === pending`）。后台任务（profile 迁移、
//! pending 超时）在构造时给的作用域里。
use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use url::Url;
use zcode_async::Spawner;
use zcode_host_kit::{Reply, RpcError, Serial, ServiceBuilder, log};

use super::credential::CredentialError;
use super::endpoint::Endpoints;
use super::net::{ApiClient, ApiRequest, HeaderList};
use super::oauth_adapters::{
    Adapter, AdapterError, bigmodel_userinfo_url, has_authorization_code, is_unauthorized,
    migrated_bigmodel_profile, needs_bigmodel_migration, now_ms, parse_attribution, query,
    runtime_config, with_profile_schema, zai_userinfo_url,
};
use super::oauth_repo::{
    ACTIVE_PROVIDER_KEY, BIGMODEL, JwtExpiration, OAuthCredentialRepo, TokenSet, UserProfile, ZAI,
    ZCODE_JWT_TOKEN_KEY, access_token_key, jwt_expiration,
};

const OAUTH_TIMEOUT_MS: i64 = 5 * 60 * 1000;
const COMPLETED_POLLING_STATE_GRACE_MS: i64 = 30 * 1000;

/// 退出后清理派生凭据并刷新账号 provider（`createOAuthProviderLogoutHandler`）。
pub type LogoutHook =
    Arc<dyn Fn(String, Option<String>) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

fn error(message: impl Into<String>) -> RpcError {
    RpcError::new("Error", message)
}

fn credential_error(error: CredentialError) -> RpcError {
    error.to_rpc()
}

fn cred<T>(result: Result<T, CredentialError>) -> Result<T, RpcError> {
    result.map_err(credential_error)
}

fn adapter_error(error: AdapterError) -> RpcError {
    RpcError::new("Error", error.message)
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).expect("system randomness for OAuth state");
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn trimmed(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// `URLSearchParams.set`：替换第一个同名参数并删除其余同名参数；不存在时追加。
fn set_query_param(url: &mut Url, key: &str, value: &str) {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    match pairs.iter().position(|(name, _)| name == key) {
        Some(index) => {
            pairs[index].1 = value.to_owned();
            let mut seen = false;
            pairs.retain(|(name, _)| {
                if name != key {
                    return true;
                }
                let keep = !seen;
                seen = true;
                keep
            });
        }
        None => pairs.push((key.to_owned(), value.to_owned())),
    }
    url.query_pairs_mut().clear().extend_pairs(pairs);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    AwaitingAttributionOrCode,
    AwaitingCodeAfterAttribution,
}

#[derive(Debug, Clone)]
struct Polling {
    expires_at: i64,
    flow_id: String,
    next_poll_at: i64,
    poll_interval_ms: i64,
    poll_token: String,
    poll_url: String,
}

#[derive(Debug, Clone)]
struct Pending {
    id: u64,
    state: String,
    provider: String,
    phase: Phase,
    polling: Option<Polling>,
}

#[derive(Debug, Clone)]
struct Completed {
    generation: u64,
    expires_at: i64,
    provider: String,
    state: String,
}

#[derive(Default)]
struct State {
    pending: Option<Pending>,
    next_pending_id: u64,
    flow_start_generation: u64,
    flow_start_provider: Option<String>,
    session_generation: u64,
    recently_completed: Option<Completed>,
    /// 同一 pending 最近一个兑换候选：(pending id, 序号, 结果)。
    latest_completion: Option<(u64, u64, watch::Receiver<Option<bool>>)>,
    next_completion_seq: u64,
}

/// 退出时被清理的会话：(active provider, 账号身份)。
type LogoutTarget = (Option<String>, Option<String>);

#[derive(Clone)]
pub struct OAuthService {
    inner: Arc<Inner>,
}

struct Inner {
    repo: OAuthCredentialRepo,
    adapters: Vec<Arc<Adapter>>,
    api: ApiClient,
    endpoints: Endpoints,
    /// 会话变更队列的 owner 任务。
    mutations: Serial,
    /// 后台任务（profile 迁移、pending 超时）的作用域。
    tasks: Spawner,
    state: Mutex<State>,
    on_logout: RwLock<Option<LogoutHook>>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

type Completion = (TokenSet, UserProfile);

impl OAuthService {
    pub fn new(spawner: &Spawner, repo: OAuthCredentialRepo, api: ApiClient) -> Self {
        Self::with_clock(spawner, repo, api, Arc::new(now_ms))
    }

    pub fn with_clock(
        spawner: &Spawner,
        repo: OAuthCredentialRepo,
        api: ApiClient,
        clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        let endpoints = api.endpoints().clone();
        let adapters = runtime_config(&endpoints)
            .into_iter()
            .map(|config| Arc::new(Adapter::new(config, api.clone())))
            .collect();
        Self {
            inner: Arc::new(Inner {
                repo,
                adapters,
                api,
                endpoints,
                // 登录、退出、回调与 401 复核：排队超过 16 个时提交方等待。
                mutations: Serial::start(spawner, "oauth-session-mutations", 16),
                tasks: spawner.clone(),
                state: Mutex::new(State::default()),
                on_logout: RwLock::new(None),
                clock,
            }),
        }
    }

    pub fn repo(&self) -> &OAuthCredentialRepo {
        &self.inner.repo
    }

    pub fn set_logout_hook(&self, hook: LogoutHook) {
        *self
            .inner
            .on_logout
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    }

    fn now(&self) -> i64 {
        (self.inner.clock)()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn adapter(&self, provider: &str) -> Result<Arc<Adapter>, RpcError> {
        self.inner
            .adapters
            .iter()
            .find(|adapter| adapter.id() == provider)
            .cloned()
            .ok_or_else(|| error(format!("不支持的 OAuth provider: {provider}")))
    }

    fn enabled_adapter(&self, provider: &str) -> Result<Arc<Adapter>, RpcError> {
        let adapter = self.adapter(provider)?;
        if !adapter.config.enabled {
            return Err(error(format!("OAuth provider 未启用: {provider}")));
        }
        Ok(adapter)
    }

    /// 在会话变更队列里执行（`runSessionMutation`）：交给 owner 任务，按提交顺序逐个执行。原来是跨整个
    /// 变更持有的 tokio 锁，提交方被取消时变更停在半途；现在已经开始的变更总会做完。owner 已结束（作用域
    /// 已关闭）时返回错误。
    async fn mutate<T, F, Fut>(&self, run: F) -> Result<T, RpcError>
    where
        F: FnOnce(OAuthService) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send,
        T: Send + 'static,
    {
        let service = self.clone();
        self.inner
            .mutations
            .run(async move { run(service).await })
            .await
            .map_err(|stopped| error(stopped.to_string()))
    }

    fn clear_pending(state: &mut State) {
        state.pending = None;
    }

    fn pending_is(&self, id: u64) -> bool {
        self.state().pending.as_ref().is_some_and(|p| p.id == id)
    }

    pub fn providers(&self) -> Vec<Value> {
        let mut enabled: Vec<&Arc<Adapter>> = self
            .inner
            .adapters
            .iter()
            .filter(|adapter| adapter.config.enabled)
            .collect();
        enabled.sort_by_key(|adapter| adapter.config.order);
        enabled
            .iter()
            .map(|adapter| adapter.config.meta())
            .collect()
    }

    pub async fn active_provider(&self) -> Result<Option<String>, RpcError> {
        self.inner
            .repo
            .active_provider()
            .await
            .map_err(credential_error)
    }

    pub async fn restore_cached_session(&self) -> Result<Value, RpcError> {
        let result = self.restore_cached_session_state().await?;
        Ok(if result["status"] == "authenticated" {
            result["userInfo"].clone()
        } else {
            Value::Null
        })
    }

    pub async fn restore_cached_session_state(&self) -> Result<Value, RpcError> {
        loop {
            let generation = self.state().session_generation;
            let repo = &self.inner.repo;
            let Some(active) = repo.active_provider().await.map_err(credential_error)? else {
                log::debug("[oauthService] restoreCachedSession skipped: no active provider");
                return Ok(json!({"status": "signed-out"}));
            };
            let adapter = match self.adapter(&active) {
                Ok(adapter) if adapter.config.enabled => adapter,
                _ => {
                    repo.set_active_provider(None)
                        .await
                        .map_err(credential_error)?;
                    return Ok(json!({"status": "signed-out"}));
                }
            };
            let Some(profile) = repo.active_user_profile().await.map_err(credential_error)? else {
                return Ok(json!({"status": "signed-out"}));
            };
            // 只看共享 JWT：不连带读 provider token，避免把后台迁移拖进首屏恢复。
            let jwt = repo
                .credentials()
                .load(ZCODE_JWT_TOKEN_KEY)
                .await
                .map_err(credential_error)?
                .map(|jwt| jwt.trim().to_owned())
                .unwrap_or_default();
            if !jwt.is_empty()
                && matches!(jwt_expiration(&jwt, self.now()), JwtExpiration::Expired(_))
            {
                log::info(format!(
                    "[oauthService] cached session invalidated because zcode JWT expired provider={active}"
                ));
                if !self
                    .invalidate_expired_session(generation, &active, &profile, &jwt)
                    .await?
                {
                    continue;
                }
                return Ok(json!({"status": "reauthentication-required", "reason": "jwt-expired"}));
            }
            if active == BIGMODEL {
                self.spawn_bigmodel_profile_migration(adapter.clone(), generation, profile.clone());
            }
            if active == ZAI && jwt.is_empty() {
                return Ok(json!({"status": "signed-out"}));
            }
            log::debug(format!(
                "[oauthService] restoreCachedSession restored {active} {}",
                profile.id
            ));
            return Ok(json!({"status": "authenticated", "userInfo": profile.user_info()}));
        }
    }

    async fn invalidate_expired_session(
        &self,
        expected_generation: u64,
        provider: &str,
        expected_profile: &UserProfile,
        expected_jwt: &str,
    ) -> Result<bool, RpcError> {
        let repo = self.inner.repo.clone();
        let (provider_owned, expected_profile_owned, expected_jwt_owned) = (
            provider.to_owned(),
            expected_profile.clone(),
            expected_jwt.to_owned(),
        );
        let invalidated = self
            .mutate(move |service| async move {
                let (provider, expected_profile, expected_jwt) = (
                    provider_owned.as_str(),
                    &expected_profile_owned,
                    expected_jwt_owned.as_str(),
                );
                let current_provider = repo.active_provider().await.map_err(credential_error)?;
                let current_profile = repo.user_profile(provider).await.map_err(credential_error)?;
                let current_jwt = repo
                    .credentials()
                    .load(ZCODE_JWT_TOKEN_KEY)
                    .await
                    .map_err(credential_error)?
                    .map(|jwt| jwt.trim().to_owned())
                    .unwrap_or_default();
                let stale = service.state().session_generation != expected_generation
                    || current_provider.as_deref() != Some(provider)
                    || !current_profile.is_some_and(|p| p.same_as(expected_profile))
                    || current_jwt != expected_jwt;
                if stale {
                    log::info(format!(
                        "[oauthService] skipped stale expired JWT session invalidation provider={provider}"
                    ));
                    return Ok(false);
                }
                service.state().session_generation += 1;
                repo.clear_active_session().await.map_err(credential_error)?;
                Ok::<bool, RpcError>(true)
            })
            .await??;
        if !invalidated {
            return Ok(false);
        }
        self.cancel_pending(Some(provider)).await;
        if let Err(message) = self
            .notify_logout(provider, Some(expected_profile.id.clone()))
            .await
        {
            log::warn(format!(
                "[oauthService] expired JWT derived provider cleanup failed provider={provider}: {message}"
            ));
        }
        Ok(true)
    }

    /// 旧版 bigmodel 缓存 profile 的后台迁移（失败不影响恢复结果）。
    fn spawn_bigmodel_profile_migration(
        &self,
        adapter: Arc<Adapter>,
        generation: u64,
        cached: UserProfile,
    ) {
        if !needs_bigmodel_migration(&cached, self.now()) {
            return;
        }
        let service = self.clone();
        drop(self.inner.tasks.spawn("oauth-profile-migration", async move {
            let cached = Arc::new(cached);
            let is_current = move |service: OAuthService, cached: Arc<UserProfile>| async move {
                if service.state().session_generation != generation {
                    return false;
                }
                let repo = &service.inner.repo;
                let active = repo.active_provider().await.ok().flatten();
                let current = repo.user_profile(BIGMODEL).await.ok().flatten();
                active.as_deref() == Some(BIGMODEL) && current.is_some_and(|p| p.same_as(&cached))
            };
            let check = cached.clone();
            let tokens = service
                .mutate(move |service| async move {
                    if !is_current(service.clone(), check).await {
                        return None;
                    }
                    service.inner.repo.token_set(BIGMODEL).await.ok().flatten()
                })
                .await
                .ok()
                .flatten();
            let next = match tokens {
                None => with_profile_schema(BIGMODEL, &cached),
                Some(tokens) => {
                    let refreshed = adapter.fetch_user_info(&tokens, "").await;
                    migrated_bigmodel_profile(&cached, refreshed, service.now())
                }
            };
            let _ = service
                .mutate(move |service| async move {
                    if is_current(service.clone(), cached).await
                        && let Err(error) = service.inner.repo.save_user_profile(BIGMODEL, &next).await
                    {
                        log::debug(format!(
                            "[oauthService] background profile migration failed: {error}"
                        ));
                    }
                })
                .await;
        }));
    }

    /// 保存一次登录：先清互斥的另一个 provider，再写 token、user_info、active；
    /// flow 失效时回滚到写入前的快照（`persistOAuthSession`）。
    async fn persist_session(
        &self,
        provider: &str,
        tokens: &TokenSet,
        profile: &UserProfile,
        pending_id: u64,
    ) -> Result<(), RpcError> {
        let repo = &self.inner.repo;
        let inactive = match provider {
            ZAI => Some(BIGMODEL),
            BIGMODEL => Some(ZAI),
            _ => None,
        };
        let previous_tokens = cred(repo.token_set(provider).await)?;
        let previous_profile = cred(repo.user_profile(provider).await)?;
        let previous_inactive_tokens = match inactive {
            Some(other) => cred(repo.token_set(other).await)?,
            None => None,
        };
        let previous_inactive_profile = match inactive {
            Some(other) => cred(repo.user_profile(other).await)?,
            None => None,
        };
        let previous_active = cred(repo.active_provider().await)?;
        let rollback = async || -> Result<(), RpcError> {
            match &previous_tokens {
                Some(tokens) => cred(repo.save_token_set(provider, tokens).await)?,
                None => cred(repo.clear_provider(provider).await)?,
            }
            match &previous_profile {
                Some(profile) => cred(repo.save_user_profile(provider, profile).await)?,
                None => cred(repo.clear_user_profile(provider).await)?,
            }
            if let Some(other) = inactive {
                match &previous_inactive_tokens {
                    Some(tokens) => cred(repo.save_token_set(other, tokens).await)?,
                    None => cred(repo.clear_provider(other).await)?,
                }
                match &previous_inactive_profile {
                    Some(profile) => cred(repo.save_user_profile(other, profile).await)?,
                    None => cred(repo.clear_user_profile(other).await)?,
                }
            }
            cred(repo.set_active_provider(previous_active.as_deref()).await)
        };
        let cancelled = || error("OAuth flow 已取消");
        self.state().session_generation += 1;
        if let Some(other) = inactive {
            if !self.pending_is(pending_id) {
                rollback().await?;
                return Err(cancelled());
            }
            // 先清旧 provider 再写新 token；反序会让 clearProvider 误删共享 JWT。
            cred(repo.clear_provider(other).await)?;
        }
        if !self.pending_is(pending_id) {
            rollback().await?;
            return Err(cancelled());
        }
        cred(repo.save_token_set(provider, tokens).await)?;
        if !self.pending_is(pending_id) {
            rollback().await?;
            return Err(cancelled());
        }
        cred(
            repo.save_user_profile(provider, &with_profile_schema(provider, profile))
                .await,
        )?;
        if !self.pending_is(pending_id) {
            rollback().await?;
            return Err(cancelled());
        }
        cred(repo.set_active_provider(Some(provider)).await)?;
        if !self.pending_is(pending_id) {
            rollback().await?;
            return Err(cancelled());
        }
        Ok(())
    }

    fn recently_completed_matches(&self, state: &str) -> Option<String> {
        let guard = self.state();
        match (&guard.pending, &guard.recently_completed) {
            (None, Some(completed))
                if completed.state == state
                    && completed.generation == guard.flow_start_generation =>
            {
                Some(completed.provider.clone())
            }
            _ => None,
        }
    }

    /// 一个兑换候选（轮询或回调）在会话队列里完成登录（`runPendingSessionCompletion`）。
    async fn run_pending_completion<F, Fut>(
        &self,
        pending: &Pending,
        complete: F,
        attribution: Option<Map<String, Value>>,
    ) -> Result<Option<Value>, RpcError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<Completion, RpcError>> + Send,
    {
        let (sender, receiver) = watch::channel(None);
        let seq = {
            let mut state = self.state();
            state.next_completion_seq += 1;
            let seq = state.next_completion_seq;
            state.latest_completion = Some((pending.id, seq, receiver));
            seq
        };
        let job_pending = pending.clone();
        let outcome = self
            .mutate(move |service| async move {
                let pending = &job_pending;
                if !service.pending_is(pending.id) {
                    if service.recently_completed_matches(&pending.state).is_some() {
                        // 入队时已通过有效性检查；归因不能因排队超过去重窗口而丢失。
                        if let Some(attribution) = &attribution {
                            service
                                .inner
                                .repo
                                .save_login_attribution(attribution)
                                .await
                                .map_err(credential_error)?;
                        }
                        return Ok(Some(
                            json!({"kind": "duplicate", "provider": pending.provider}),
                        ));
                    }
                    return Ok(None);
                }
                if let Some(attribution) = &attribution {
                    service
                        .inner
                        .repo
                        .save_login_attribution(attribution)
                        .await
                        .map_err(credential_error)?;
                }
                if !service.pending_is(pending.id) {
                    return Ok(None);
                }
                let (tokens, profile) = complete().await?;
                // 兑换期间用户可能取消或换了新 flow：旧结果不能写回。
                if !service.pending_is(pending.id) {
                    return Ok(None);
                }
                service
                    .persist_session(&pending.provider, &tokens, &profile, pending.id)
                    .await?;
                let mut state = service.state();
                if state.pending.as_ref().is_some_and(|p| p.id == pending.id) {
                    Self::clear_pending(&mut state);
                }
                let expires_at = (service.inner.clock)() + COMPLETED_POLLING_STATE_GRACE_MS;
                state.recently_completed = Some(Completed {
                    generation: state.flow_start_generation,
                    expires_at,
                    provider: pending.provider.clone(),
                    state: pending.state.clone(),
                });
                Ok::<Option<Value>, RpcError>(Some(json!({
                    "kind": "session",
                    "provider": pending.provider,
                    "userInfo": profile.user_info(),
                })))
            })
            .await
            .and_then(|outcome| outcome);
        let _ = sender.send(Some(outcome.is_ok()));
        let fallback = {
            let mut state = self.state();
            let fallback = match &state.latest_completion {
                Some((pending_id, latest, receiver))
                    if *pending_id == pending.id && *latest != seq =>
                {
                    Some(receiver.clone())
                }
                _ => None,
            };
            if state
                .latest_completion
                .as_ref()
                .is_some_and(|(_, latest, _)| *latest == seq)
            {
                state.latest_completion = None;
            }
            fallback
        };
        let Err(failure) = outcome else {
            return outcome;
        };
        // 另一条已排队的路径可能拿到了凭据：只有它也失败时才向 UI 报告本条错误。
        if let Some(mut receiver) = fallback {
            let succeeded = loop {
                if let Some(result) = *receiver.borrow() {
                    break result;
                }
                if receiver.changed().await.is_err() {
                    break false;
                }
            };
            if !succeeded {
                return Err(failure);
            }
            if self.recently_completed_matches(&pending.state).is_some() {
                return Ok(Some(
                    json!({"kind": "duplicate", "provider": pending.provider}),
                ));
            }
            if !self.pending_is(pending.id) {
                return Ok(None);
            }
        }
        Err(failure)
    }

    /// 显式校验当前会话（`restoreSession`）：远端 401/403 时按退出处理。
    pub async fn restore_session(&self) -> Result<Value, RpcError> {
        let repo = &self.inner.repo;
        let Some(active) = repo.active_provider().await.map_err(credential_error)? else {
            return Ok(Value::Null);
        };
        let adapter = match self.adapter(&active) {
            Ok(adapter) if adapter.config.enabled => adapter,
            _ => {
                repo.set_active_provider(None)
                    .await
                    .map_err(credential_error)?;
                return Ok(Value::Null);
            }
        };
        let mut tokens = repo.active_token_set().await.map_err(credential_error)?;
        if tokens.is_none() && adapter.has_legacy_tokens() {
            let credentials = repo.credentials();
            if let Some(access) = credentials
                .load("auth_token")
                .await
                .map_err(credential_error)?
                .filter(|token| !token.is_empty())
            {
                let refresh = credentials
                    .load("refresh_token")
                    .await
                    .map_err(credential_error)?
                    .filter(|token| !token.is_empty());
                let legacy = TokenSet {
                    access_token: access,
                    refresh_token: refresh,
                    zcode_jwt_token: None,
                };
                // 回填命名空间 key，之后不再走 legacy 分支。
                repo.save_active_token_set(&legacy)
                    .await
                    .map_err(credential_error)?;
                repo.save_token_set(&active, &legacy)
                    .await
                    .map_err(credential_error)?;
                tokens = Some(legacy);
            }
        }
        let Some(tokens) = tokens else {
            self.logout(None).await?;
            return Ok(Value::Null);
        };
        match adapter.fetch_user_info(&tokens, "").await {
            Ok(profile) => {
                repo.save_active_user_profile(&with_profile_schema(&active, &profile))
                    .await
                    .map_err(credential_error)?;
                repo.set_active_provider(Some(&active))
                    .await
                    .map_err(credential_error)?;
                Ok(profile.user_info())
            }
            Err(failure) => {
                if is_unauthorized(&failure) {
                    log::info(format!(
                        "[oauthService] restoreSession unauthorized, logging out: {active}"
                    ));
                    self.logout(None).await?;
                    return Ok(Value::Null);
                }
                Err(adapter_error(failure))
            }
        }
    }

    pub async fn start_oauth(&self, provider: &str) -> Result<Value, RpcError> {
        let adapter = self.enabled_adapter(provider)?;
        // 同窗口快速切换 provider：先作废旧 state，避免两条流程抢同一回调通道。
        self.mutate(|service| async move {
            let mut state = service.state();
            state.flow_start_generation += 1;
            state.flow_start_provider = None;
            Self::clear_pending(&mut state);
        })
        .await?;
        let flow_state = random_hex(32);
        let id = {
            let mut state = self.state();
            state.next_pending_id += 1;
            let id = state.next_pending_id;
            state.pending = Some(Pending {
                id,
                state: flow_state.clone(),
                provider: adapter.id().to_owned(),
                phase: Phase::AwaitingAttributionOrCode,
                polling: None,
            });
            id
        };
        self.spawn_pending_timeout(id, None, OAUTH_TIMEOUT_MS);
        Ok(json!({
            "provider": adapter.id(),
            "authorizeUrl": adapter.build_authorize_url(&flow_state),
            "state": flow_state,
        }))
    }

    fn spawn_pending_timeout(&self, id: u64, flow_id: Option<String>, delay_ms: i64) {
        let service = self.clone();
        drop(self.inner.tasks.spawn("oauth-pending-timeout", async move {
            tokio::time::sleep(Duration::from_millis(delay_ms.max(0) as u64)).await;
            let mut state = service.state();
            let matches = state.pending.as_ref().is_some_and(|pending| {
                pending.id == id
                    && flow_id.as_deref().is_none_or(|flow| {
                        pending.polling.as_ref().map(|p| p.flow_id.as_str()) == Some(flow)
                    })
            });
            if matches {
                Self::clear_pending(&mut state);
            }
        }));
    }

    pub async fn start_oauth_with_polling(&self, provider: &str) -> Result<Value, RpcError> {
        if provider != ZAI && provider != BIGMODEL {
            return self.start_oauth(provider).await;
        }
        let adapter = self.enabled_adapter(provider)?;
        // 并发 init：最后一次用户操作拥有 flow，旧响应只结束自己的调用方。
        let start_generation = {
            let mut state = self.state();
            state.flow_start_generation += 1;
            state.flow_start_provider = Some(provider.to_owned());
            Self::clear_pending(&mut state);
            state.flow_start_generation
        };
        let poll_token = random_hex(32);
        let init_url = self.inner.endpoints.zcode_api_url("/api/v1/oauth/cli/init");
        let envelope = self
            .inner
            .api
            .read_json(
                ApiRequest::post(init_url)
                    .bearer(&poll_token)
                    .json(&json!({"provider": provider})),
            )
            .await
            .map_err(|failure| error(failure.message))?;
        {
            let mut state = self.state();
            if state.flow_start_generation != start_generation {
                return Err(error("OAuth flow 已被新的登录请求替换"));
            }
            state.flow_start_provider = None;
        }
        let data = &envelope["data"];
        let invalid = || error("OAuth flow 初始化响应无效");
        let finite = |value: &Value| value.as_f64().filter(|v| v.is_finite());
        let (Some(flow_id), Some(authorize), Some(expires), Some(interval)) = (
            trimmed(&data["flow_id"]),
            trimmed(&data["authorize_url"]),
            finite(&data["expires_at"]),
            finite(&data["poll_interval_sec"]),
        ) else {
            return Err(error(
                trimmed(&envelope["msg"]).unwrap_or_else(|| "OAuth flow 初始化响应无效".into()),
            ));
        };
        if envelope["code"].as_f64() != Some(0.0) || !data.is_object() {
            return Err(error(
                trimmed(&envelope["msg"]).unwrap_or_else(|| "OAuth flow 初始化响应无效".into()),
            ));
        }
        let expires_at = (expires * 1000.0) as i64;
        let poll_interval_ms = (interval * 1000.0) as i64;
        let mut authorize_url = Url::parse(&authorize).map_err(|_| invalid())?;
        // Desktop 统一回到官网中转页，再透传到 zcode://oauth/callback。
        let redirect = self.inner.endpoints.desktop_oauth_redirect_uri();
        if provider == BIGMODEL {
            set_query_param(&mut authorize_url, "redirect", &redirect);
        } else {
            set_query_param(&mut authorize_url, "redirect_uri", &redirect);
        }
        let flow_state = query(&authorize_url, "state")
            .map(|state| state.trim().to_owned())
            .filter(|state| !state.is_empty());
        let remaining = expires_at - self.now();
        let Some(flow_state) = flow_state else {
            return Err(invalid());
        };
        if authorize_url.scheme() != "https"
            || remaining <= 0
            || poll_interval_ms < 1_000
            || poll_interval_ms >= remaining
        {
            return Err(invalid());
        }
        let timeout = OAUTH_TIMEOUT_MS.min(remaining);
        let id = {
            let mut state = self.state();
            state.next_pending_id += 1;
            let id = state.next_pending_id;
            state.pending = Some(Pending {
                id,
                state: flow_state.clone(),
                provider: adapter.id().to_owned(),
                phase: Phase::AwaitingAttributionOrCode,
                polling: Some(Polling {
                    expires_at,
                    flow_id: flow_id.clone(),
                    next_poll_at: self.now(),
                    poll_interval_ms,
                    poll_token,
                    poll_url: self.inner.endpoints.zcode_api_url(&format!(
                        "/api/v1/oauth/cli/poll/{}",
                        super::oauth_repo::encode_uri_component(&flow_id)
                    )),
                }),
            });
            id
        };
        self.spawn_pending_timeout(id, Some(flow_id), timeout);
        log::info(format!(
            "[oauthService] OAuth polling flow started provider={provider} expiresInMs={timeout} pollIntervalMs={poll_interval_ms}"
        ));
        Ok(
            json!({"provider": provider, "authorizeUrl": authorize_url.to_string(), "state": flow_state}),
        )
    }

    pub async fn poll_pending_oauth(&self) -> Result<Value, RpcError> {
        let (pending, polling) = {
            let mut state = self.state();
            let now = (self.inner.clock)();
            let Some(pending) = state.pending.as_mut() else {
                return Ok(Value::Null);
            };
            let Some(polling) = pending.polling.as_mut() else {
                return Ok(Value::Null);
            };
            if now >= polling.expires_at {
                Self::clear_pending(&mut state);
                return Err(error("OAuth flow 已过期"));
            }
            if now < polling.next_poll_at {
                return Ok(Value::Null);
            }
            polling.next_poll_at = now + polling.poll_interval_ms;
            let polling = polling.clone();
            (pending.clone(), polling)
        };
        let envelope = match self
            .inner
            .api
            .read_json(ApiRequest::get(&polling.poll_url).bearer(&polling.poll_token))
            .await
        {
            Ok(envelope) => envelope,
            Err(failure) => {
                if !self.pending_is(pending.id) {
                    return Ok(Value::Null);
                }
                let terminal = failure
                    .status
                    .is_some_and(|s| (400..500).contains(&s) && s != 408 && s != 429);
                if terminal {
                    let message = failure.message.clone();
                    let result = self
                        .run_pending_completion(&pending, async move || Err(error(message)), None)
                        .await;
                    return match result {
                        Ok(value) => Ok(value.unwrap_or(Value::Null)),
                        Err(terminal_error) => {
                            self.clear_if_pending(pending.id);
                            Err(terminal_error)
                        }
                    };
                }
                // 单次断网或 5xx 不取消 flow：按服务端间隔继续轮询。
                log::debug(format!(
                    "[oauthService] OAuth polling request will retry provider={}: {}",
                    pending.provider, failure.message
                ));
                return Ok(Value::Null);
            }
        };
        if !self.pending_is(pending.id) {
            return Ok(Value::Null);
        }
        if envelope["code"].as_f64() == Some(0.0)
            && envelope["data"].is_object()
            && trimmed(&envelope["data"]["status"]).as_deref() == Some("pending")
        {
            return Ok(Value::Null);
        }
        let adapter = self.adapter(&pending.provider)?;
        let provider = pending.provider.clone();
        let result = self
            .run_pending_completion(
                &pending,
                async move || {
                    let data = &envelope["data"];
                    if envelope["code"].as_f64() != Some(0.0) || !data.is_object() {
                        return Err(error(
                            trimmed(&envelope["msg"])
                                .unwrap_or_else(|| "OAuth flow 查询响应无效".into()),
                        ));
                    }
                    match trimmed(&data["status"]).as_deref() {
                        Some("failed") => return Err(error("OAuth flow 授权失败")),
                        Some("ready") => {}
                        _ => return Err(error("OAuth flow 查询响应无效")),
                    }
                    let user = if data["user"].is_object() {
                        data["user"].clone()
                    } else {
                        Value::Null
                    };
                    let provider_token = if provider == ZAI {
                        trimmed(&data["zai"]["access_token"])
                    } else {
                        trimmed(&data["bigmodel"]["access_token"])
                            .or_else(|| trimmed(&data["bigmodel"]["accessToken"]))
                    };
                    let (Some(jwt), Some(access), Some(user_id)) = (
                        trimmed(&data["token"]),
                        provider_token,
                        trimmed(&user["user_id"]),
                    ) else {
                        return Err(error("OAuth flow 查询响应无效"));
                    };
                    let username = trimmed(&user["name"])
                        .or_else(|| trimmed(&user["email"]))
                        .unwrap_or_else(|| user_id.clone());
                    let profile = UserProfile {
                        id: user_id,
                        display_name: username.clone(),
                        username,
                        avatar_url: trimmed(&user["avatar"]),
                        raw_profile: Some(user),
                    };
                    let refresh = if provider == BIGMODEL {
                        trimmed(&data["bigmodel"]["refresh_token"])
                            .or_else(|| trimmed(&data["bigmodel"]["refreshToken"]))
                    } else {
                        None
                    };
                    let tokens = adapter
                        .normalize_polled(TokenSet {
                            access_token: access,
                            refresh_token: refresh,
                            zcode_jwt_token: Some(jwt),
                        })
                        .await
                        .map_err(adapter_error)?;
                    Ok((tokens, profile))
                },
                None,
            )
            .await;
        match result {
            Ok(value) => {
                if value.as_ref().is_some_and(|v| v["kind"] == "session") {
                    log::info(format!(
                        "[oauthService] OAuth polling flow completed provider={}",
                        pending.provider
                    ));
                }
                Ok(value.unwrap_or(Value::Null))
            }
            Err(failure) => {
                self.clear_if_pending(pending.id);
                Err(failure)
            }
        }
    }

    fn clear_if_pending(&self, id: u64) {
        let mut state = self.state();
        if state.pending.as_ref().is_some_and(|p| p.id == id) {
            Self::clear_pending(&mut state);
        }
    }

    pub async fn handle_callback(&self, url: &str) -> Result<Value, RpcError> {
        let parsed = Url::parse(url).map_err(|_| RpcError::new("TypeError", "Invalid URL"))?;
        let callback_state = query(&parsed, "state").map(|s| s.trim().to_owned());
        let pending = self.state().pending.clone();
        let Some(pending) = pending else {
            let completed = {
                let state = self.state();
                state.recently_completed.clone().filter(|completed| {
                    callback_state.as_deref() == Some(completed.state.as_str())
                        && !completed.state.is_empty()
                        && completed.generation == state.flow_start_generation
                        && (self.inner.clock)() < completed.expires_at
                })
            };
            if let Some(completed) = completed {
                let attribution = parse_attribution(&parsed);
                let job_completed = completed.clone();
                self.mutate(move |service| async move {
                    let completed = job_completed;
                    // 轮询先成功时仍保存浏览器归因，登录去重不等于丢弃整条回调。
                    let still = {
                        let state = service.state();
                        state.pending.is_none()
                            && state.recently_completed.as_ref().is_some_and(|c| {
                                c.state == completed.state && c.generation == completed.generation
                            })
                            && completed.generation == state.flow_start_generation
                    };
                    if still && let Some(attribution) = &attribution {
                        service
                            .inner
                            .repo
                            .save_login_attribution(attribution)
                            .await
                            .map_err(credential_error)?;
                    }
                    Ok::<(), RpcError>(())
                })
                .await??;
                return Ok(json!({"kind": "duplicate", "provider": completed.provider}));
            }
            return Err(error("OAuth state 不匹配或已过期"));
        };
        if query(&parsed, "state").as_deref() != Some(pending.state.as_str()) {
            return Err(error("OAuth state 不匹配或已过期"));
        }
        if let Some(polling) = &pending.polling
            && self.now() >= polling.expires_at
        {
            self.clear_if_pending(pending.id);
            return Err(error("OAuth flow 已过期"));
        }
        let attribution = parse_attribution(&parsed);
        if let Some(attribution) = &attribution
            && !has_authorization_code(&parsed)
        {
            if pending.phase == Phase::AwaitingCodeAfterAttribution {
                return Err(error("OAuth 归因回调已处理"));
            }
            // 纯归因回调是授权码之前的中转：推进 phase，但保留 state 继续等授权码。
            self.set_phase(pending.id, Phase::AwaitingCodeAfterAttribution);
            if let Err(failure) = self.inner.repo.save_login_attribution(attribution).await {
                self.set_phase(pending.id, Phase::AwaitingAttributionOrCode);
                return Err(credential_error(failure));
            }
            return Ok(
                json!({"kind": "attribution", "provider": pending.provider, "attribution": attribution}),
            );
        }
        let adapter = self.adapter(&pending.provider)?;
        let result = self
            .run_pending_completion(
                &pending,
                async move || {
                    let callback = adapter.parse_callback(&parsed).map_err(adapter_error)?;
                    let tokens = adapter
                        .exchange_token(&callback, &callback.state)
                        .await
                        .map_err(adapter_error)?;
                    // 获取用户信息失败不阻塞登录。
                    let profile = adapter
                        .fetch_user_info(&tokens, &callback.state)
                        .await
                        .unwrap_or_else(|_| UserProfile::fallback());
                    Ok((tokens, profile))
                },
                attribution,
            )
            .await?;
        Ok(result.unwrap_or(Value::Null))
    }

    fn set_phase(&self, id: u64, phase: Phase) {
        let mut state = self.state();
        if let Some(pending) = state.pending.as_mut().filter(|p| p.id == id) {
            pending.phase = phase;
        }
    }

    pub async fn refresh_token(&self, provider: Option<String>) -> Result<(), RpcError> {
        let target = match provider {
            Some(provider) => Some(provider),
            None => self.active_provider().await?,
        };
        let Some(target) = target else {
            return Err(error("当前没有可用的登录 provider"));
        };
        let adapter = self.enabled_adapter(&target)?;
        // 两个 adapter 都没有 refresh token 交换接口（与 TS 相同）。
        Err(error(format!(
            "{} OAuth 暂未提供 refresh token 交换接口，请重新登录",
            adapter.config.display_name
        )))
    }

    /// Host 内 401 的提交入口：在会话队列里复核“请求用的是当前登录凭据”后才退出。
    pub async fn logout_if_current_credential_request(
        &self,
        url: String,
        headers: HeaderList,
    ) -> Result<bool, RpcError> {
        let service = self.clone();
        self.logout_active_session(Some(Box::new(move || {
            Box::pin(async move { service.is_current_credential_request(&url, &headers).await })
        })))
        .await
    }

    async fn logout_active_session(
        &self,
        is_current: Option<Box<dyn FnOnce() -> BoxFuture<'static, bool> + Send>>,
    ) -> Result<bool, RpcError> {
        let conditional = is_current.is_some();
        let repo = self.inner.repo.clone();
        let result = self
            .mutate(move |service| async move {
                if let Some(check) = is_current
                    && !check().await
                {
                    return Ok(None);
                }
                let active = repo.active_provider().await.map_err(credential_error)?;
                let identity = match &active {
                    Some(provider) => repo
                        .user_profile(provider)
                        .await
                        .map_err(credential_error)?
                        .map(|profile| profile.id),
                    None => None,
                };
                service.state().session_generation += 1;
                repo.clear_active_session()
                    .await
                    .map_err(credential_error)?;
                Ok::<Option<LogoutTarget>, RpcError>(Some((active, identity)))
            })
            .await??;
        let Some((active, identity)) = result else {
            return Ok(false);
        };
        if let Some(provider) = active {
            self.cancel_pending(Some(&provider)).await;
            if let Err(message) = self.notify_logout(&provider, identity).await {
                if !conditional {
                    return Err(error(message));
                }
                log::warn(format!(
                    "[oauthService] Unauthorized session provider cleanup failed: {message}"
                ));
            }
        }
        Ok(true)
    }

    pub async fn logout(&self, provider: Option<String>) -> Result<(), RpcError> {
        let Some(provider) = provider else {
            self.logout_active_session(None).await?;
            return Ok(());
        };
        let repo = self.inner.repo.clone();
        let job_provider = provider.clone();
        let logged_out = self
            .mutate(move |service| async move {
                let provider = job_provider;
                let active = repo.active_provider().await.map_err(credential_error)?;
                if active.as_deref() != Some(provider.as_str()) {
                    return Ok(None);
                }
                let identity = repo
                    .user_profile(&provider)
                    .await
                    .map_err(credential_error)?
                    .map(|profile| profile.id);
                service.state().session_generation += 1;
                repo.clear_active_session()
                    .await
                    .map_err(credential_error)?;
                Ok::<Option<Option<String>>, RpcError>(Some(identity))
            })
            .await??;
        if let Some(identity) = logged_out {
            self.notify_logout(&provider, identity)
                .await
                .map_err(error)?;
        }
        self.cancel_pending(Some(&provider)).await;
        Ok(())
    }

    pub async fn logout_all(&self) -> Result<(), RpcError> {
        let providers: Vec<String> = self
            .inner
            .adapters
            .iter()
            .map(|adapter| adapter.id().to_owned())
            .collect();
        let repo = self.inner.repo.clone();
        let job_providers = providers.clone();
        let identities = self
            .mutate(move |service| async move {
                let providers = job_providers;
                let mut identities = Vec::new();
                for provider in &providers {
                    let identity = repo
                        .user_profile(provider)
                        .await
                        .map_err(credential_error)?
                        .map(|profile| profile.id);
                    identities.push((provider.clone(), identity));
                }
                service.state().session_generation += 1;
                repo.clear_all(&providers).await.map_err(credential_error)?;
                Ok::<Vec<(String, Option<String>)>, RpcError>(identities)
            })
            .await??;
        self.cancel_pending(None).await;
        let results = futures_util::future::join_all(identities.into_iter().map(
            |(provider, identity)| async move { self.notify_logout(&provider, identity).await },
        ))
        .await;
        for result in results {
            result.map_err(error)?;
        }
        Ok(())
    }

    pub async fn cancel_pending(&self, provider: Option<&str>) {
        let mut state = self.state();
        match &state.pending {
            None => {
                if let (Some(provider), Some(starting)) = (provider, &state.flow_start_provider)
                    && starting != provider
                {
                    return;
                }
                state.flow_start_generation += 1;
                state.flow_start_provider = None;
            }
            Some(pending) => {
                if provider.is_some_and(|provider| pending.provider != provider) {
                    return;
                }
                state.flow_start_generation += 1;
                state.flow_start_provider = None;
                Self::clear_pending(&mut state);
            }
        }
    }

    async fn notify_logout(&self, provider: &str, identity: Option<String>) -> Result<(), String> {
        let hook = self
            .inner
            .on_logout
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match hook {
            Some(hook) => hook(provider.to_owned(), identity).await,
            None => Ok(()),
        }
    }

    /// `isCurrentOAuthCredentialRequest`：请求带的是当前 JWT，或当前 provider 的 userinfo 接口
    /// 带当前 access token。
    pub async fn is_current_credential_request(&self, url: &str, headers: &HeaderList) -> bool {
        let credentials = self.inner.repo.credentials();
        let authorization = headers.get("authorization").map(str::trim).unwrap_or("");
        if authorization.is_empty() {
            return false;
        }
        let jwt = credentials
            .load(ZCODE_JWT_TOKEN_KEY)
            .await
            .ok()
            .flatten()
            .map(|jwt| jwt.trim().to_owned())
            .unwrap_or_default();
        if !jwt.is_empty() && authorization == format!("Bearer {jwt}") {
            return true;
        }
        let provider = credentials.load(ACTIVE_PROVIDER_KEY).await.ok().flatten();
        let Some(provider) = provider.filter(|p| p == BIGMODEL || p == ZAI) else {
            return false;
        };
        let Some(request) = Url::parse(url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
        else {
            return false;
        };
        let endpoints = &self.inner.endpoints;
        let path = "/api/biz/customer/getCustomerInfo";
        let candidates = if provider == BIGMODEL {
            vec![
                endpoints.bigmodel_api_url(path),
                bigmodel_userinfo_url(endpoints),
            ]
        } else {
            vec![
                endpoints.zai_business_url(path),
                zai_userinfo_url(endpoints),
            ]
        };
        let matches = candidates.iter().any(|candidate| {
            Url::parse(candidate)
                .ok()
                .filter(|u| matches!(u.scheme(), "http" | "https"))
                .is_some_and(|expected| {
                    expected.origin() == request.origin() && expected.path() == request.path()
                })
        });
        if !matches {
            return false;
        }
        let access = credentials
            .load(&access_token_key(&provider))
            .await
            .ok()
            .flatten()
            .map(|token| token.trim().to_owned())
            .unwrap_or_default();
        if access.is_empty()
            || (authorization != access && authorization != format!("Bearer {access}"))
        {
            return false;
        }
        // 读盘期间可能切了平台：不能拿上一平台残留 token 的 401 清理当前登录。
        credentials
            .load(ACTIVE_PROVIDER_KEY)
            .await
            .ok()
            .flatten()
            .as_deref()
            == Some(provider.as_str())
    }
}

fn provider_arg(args: &zcode_host_kit::Args) -> Result<Option<String>, RpcError> {
    args.opt::<String>(0, "provider")
}

pub fn service(oauth: OAuthService) -> ServiceBuilder {
    macro_rules! handler {
        ($oauth:ident, |$service:ident, $args:ident| $body:expr) => {{
            let $oauth = $oauth.clone();
            move |_: zcode_host_kit::CallContext, $args: zcode_host_kit::Args| {
                let $service = $oauth.clone();
                async move { $body }
            }
        }};
    }
    ServiceBuilder::new("oauth")
        .method(
            "getProviders",
            handler!(oauth, |service, _args| Ok(Reply::Value(Value::Array(
                service.providers()
            )))),
        )
        .method(
            "getActiveProvider",
            handler!(oauth, |service, _args| service
                .active_provider()
                .await
                .map(|value| Reply::of(&value))),
        )
        .method(
            "restoreCachedSession",
            handler!(oauth, |service, _args| service
                .restore_cached_session()
                .await
                .map(Reply::Value)),
        )
        .method(
            "restoreCachedSessionState",
            handler!(oauth, |service, _args| service
                .restore_cached_session_state()
                .await
                .map(Reply::Value)),
        )
        .method(
            "restoreSession",
            handler!(oauth, |service, _args| service
                .restore_session()
                .await
                .map(Reply::Value)),
        )
        .method(
            "startOAuth",
            handler!(oauth, |service, args| {
                let provider: String = args.get(0, "provider")?;
                service.start_oauth(&provider).await.map(Reply::Value)
            }),
        )
        .method(
            "startOAuthWithPolling",
            handler!(oauth, |service, args| {
                let provider: String = args.get(0, "provider")?;
                service
                    .start_oauth_with_polling(&provider)
                    .await
                    .map(Reply::Value)
            }),
        )
        .method(
            "pollPendingOAuth",
            handler!(oauth, |service, _args| service
                .poll_pending_oauth()
                .await
                .map(Reply::Value)),
        )
        .method(
            "handleCallback",
            handler!(oauth, |service, args| {
                let url: String = args.get(0, "url")?;
                service.handle_callback(&url).await.map(Reply::Value)
            }),
        )
        .method(
            "refreshToken",
            handler!(oauth, |service, args| {
                service
                    .refresh_token(provider_arg(&args)?)
                    .await
                    .map(|()| Reply::Undefined)
            }),
        )
        .method(
            "logout",
            handler!(oauth, |service, args| {
                service
                    .logout(provider_arg(&args)?)
                    .await
                    .map(|()| Reply::Undefined)
            }),
        )
        .method(
            "logoutAll",
            handler!(oauth, |service, _args| service
                .logout_all()
                .await
                .map(|()| Reply::Undefined)),
        )
        .method(
            "cancelPending",
            handler!(oauth, |service, args| {
                let provider = provider_arg(&args)?;
                service.cancel_pending(provider.as_deref()).await;
                Ok(Reply::Undefined)
            }),
        )
}
