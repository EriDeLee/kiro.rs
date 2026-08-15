//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::http_client::ProxyConfig;
use crate::kiro::auth::idc::{self, BUILDER_ID_START_URL};
use crate::kiro::auth::social;
use crate::kiro::endpoint;
use crate::kiro::error::UpstreamRateLimitError;
use crate::kiro::model::available_models::ListAvailableModelsResponse;
use crate::kiro::model::credentials::{
    KiroCredentials, normalize_import_auth_method, validate_external_idp_endpoint,
};
use crate::kiro::model::events::{Event, strip_tool_use_xml_leaks};
use crate::kiro::model::requests::conversation::{
    ConversationState, CurrentMessage, UserInputMessage,
};
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::provider::KiroProvider;
use crate::kiro::token_manager::{
    IdcReloginCredentials, MultiTokenManager, RefreshTokenInvalidError,
};
use crate::model::config::Config;

use super::error::AdminServiceError;
use super::proxy_pool::{GetUrlResult, ProxyPoolManager};
use super::types::{
    AccountRpmLimitConfigResponse, AccountThrottleConfigResponse, AddCredentialRequest,
    AddCredentialResponse, AssignProxyRequest,
    AssignRoundRobinResponse, AvailableModelItem, AvailableModelsResponse, BalanceResponse,
    BatchAddProxyRequest, BatchImportEvent, CredentialStatusItem,
    CredentialsExportResponse, CredentialsStatusResponse, EnableOverageAllResult, ExportedAccount,
    ExportedCredentials, LoadBalancingModeResponse,
    LogGovernanceConfigResponse, ModelSelectionMode, ModelTestRequest, ModelTestResponse,
    PollIdcLoginResponse, ProxyCheckAllResponse, ProxyCheckResponse, ProxyPoolEntry,
    ProxyPoolResponse, QuotaExceededResult, SelfHealConfigResponse,
    SetAccountRpmLimitConfigRequest, SetAccountThrottleConfigRequest,
    SetLoadBalancingModeRequest, SetLogGovernanceConfigRequest,
    SetSelfHealConfigRequest, StartIdcLoginRequest, StartIdcLoginResponse,
    StartSocialLoginRequest, StartSocialLoginResponse,
    UpdateCredentialRequest, UpdateRefreshTokenRequest,
};

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

fn available_models_response(
    id: u64,
    selection_mode: ModelSelectionMode,
    response: ListAvailableModelsResponse,
) -> AvailableModelsResponse {
    let models = response
        .models
        .into_iter()
        .map(|model| {
            let (max_input_tokens, max_output_tokens) = model
                .token_limits
                .map(|limits| (limits.max_input_tokens, limits.max_output_tokens))
                .unwrap_or((None, None));
            AvailableModelItem {
                model_id: model.model_id,
                model_name: model.model_name,
                description: model.description,
                max_input_tokens,
                max_output_tokens,
            }
        })
        .collect();

    AvailableModelsResponse {
        id,
        selection_mode,
        models,
    }
}

fn validate_model_id(model_id: &str) -> Result<&str, AdminServiceError> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return Err(AdminServiceError::InvalidCredential(
            "模型 ID 不能为空".to_string(),
        ));
    }
    if model_id.len() > 256 || model_id.chars().any(char::is_control) {
        return Err(AdminServiceError::InvalidCredential(
            "模型 ID 格式无效".to_string(),
        ));
    }
    if model_id.eq_ignore_ascii_case("auto") {
        return Err(AdminServiceError::InvalidCredential(
            "自动路由模型不支持直接测试，请选择具体模型".to_string(),
        ));
    }
    Ok(model_id)
}

/// 单条凭据导入结果（服务端内部用，映射为 SSE 事件）
pub(crate) enum ImportStatus {
    Verified,
    /// 直接导入（未验活）成功
    Imported,
    Duplicate,
    Failed,
}

pub(crate) struct ImportItemResult {
    pub status: ImportStatus,
    pub credential_id: Option<u64>,
    pub email: Option<String>,
    pub balance: Option<BalanceResponse>,
    pub error: Option<String>,
    pub rolled_back: bool,
}

impl ImportItemResult {
    /// 转换为 SSE 事件（携带在数组中的下标）
    pub fn into_event(self, index: usize) -> BatchImportEvent {
        let status = match self.status {
            ImportStatus::Verified => "verified",
            ImportStatus::Imported => "imported",
            ImportStatus::Duplicate => "duplicate",
            ImportStatus::Failed => "failed",
        }
        .to_string();
        BatchImportEvent {
            index: Some(index),
            status,
            credential_id: self.credential_id,
            email: self.email,
            usage: self.balance.as_ref().map(|b| {
                format!(
                    "{:.0}/{:.0}",
                    b.current_usage.round(),
                    b.usage_limit.round()
                )
            }),
            subscription: self.balance.and_then(|b| b.subscription_title),
            error: self.error,
            rolled_back: if self.rolled_back { Some(true) } else { None },
            summary: None,
        }
    }
}

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    kiro_provider: Option<Arc<KiroProvider>>,
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    cache_path: Option<PathBuf>,
    /// 已注册的端点名称集合（用于 add_credential 校验）
    known_endpoints: HashSet<String>,
    /// 代理 IP 池管理器
    proxy_pool: ProxyPoolManager,
    /// 进行中的 IdC 设备授权会话
    idc_sessions: Arc<Mutex<HashMap<String, IdcAuthSession>>>,
    /// 进行中的 Social 登录会话
    social_sessions: Arc<Mutex<HashMap<String, SocialAuthSession>>>,
    /// 请求链路追踪存储（用于日志治理：开关 + 保留天数运行时可改）
    trace_store: Option<crate::admin::trace_db::SharedTraceStore>,
}

/// Social 登录会话状态
struct SocialAuthSession {
    auth_endpoint: String,
    /// 发起时生成的 state，用于 CSRF 验证
    state: String,
    code_verifier: String,
    redirect_uri: String,
    expires_at: DateTime<Utc>,
    /// 收到 OAuth 回调时的数据（code + login_option + path）
    callback_rx: tokio::sync::Mutex<tokio::sync::oneshot::Receiver<social::OAuthCallbackData>>,
    cred_template: KiroCredentials,
    proxy: Option<ProxyConfig>,
    /// Drop 时自动关闭回调服务器并释放端口
    _server_handle: social::ServerHandle,
    /// 重新登录时更新此凭据的 Token（非 None 时更新已有凭据而非创建新凭据）
    relogin_target_id: Option<u64>,
}

/// IdC 设备授权会话状态
struct IdcAuthSession {
    region: String,
    start_url: String,
    client_id: String,
    client_secret: String,
    device_code: String,
    expires_at: DateTime<Utc>,
    poll_interval: i64,
    /// 登录成功后写入的凭据配置
    cred_template: KiroCredentials,
    /// 用于发起 token 请求的代理
    proxy: Option<ProxyConfig>,
    /// 重新登录时更新此凭据的 Token（非 None 时更新已有凭据而非创建新凭据）
    relogin_target_id: Option<u64>,
}

/// 身份提供商：默认 Start URL 为 AWS Builder ID，自定义 Start URL 为企业 IAM Identity Center
fn idc_provider_for(start_url: &str) -> &'static str {
    if start_url == BUILDER_ID_START_URL {
        "BuilderId"
    } else {
        "Enterprise"
    }
}

/// 将单个凭据映射为嵌套 `Account` 结构
///
/// API Key 凭据无 refreshToken，导出格式无对应字段，跳过。
/// 空字符串字段会被过滤，保持导出 JSON 整洁。
fn credential_to_export_account(cred: KiroCredentials) -> Option<ExportedAccount> {
    let refresh_token = cred
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)?;

    fn non_empty(value: Option<String>) -> Option<String> {
        value
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    // authMethod 规范化："idc" → "IdC"，其余按 social 处理
    // authMethod 规范化："idc" → "IdC"，"external_idp" 保留，其余按 social 处理
    let auth_method = non_empty(cred.auth_method.clone()).map(|m| {
        if m.eq_ignore_ascii_case("idc")
            || m.eq_ignore_ascii_case("builder-id")
            || m.eq_ignore_ascii_case("iam")
        {
            "IdC".to_string()
        } else if cred.is_external_idp_credential() {
            "external_idp".to_string()
        } else {
            "social".to_string()
        }
    });
    let is_idc = auth_method.as_deref() == Some("IdC");
    let is_external_idp = auth_method.as_deref() == Some("external_idp");

    let provider = non_empty(cred.provider.clone());
    // idp 与 provider 同义；缺失时按认证方式回退到合法的身份提供商
    let idp = provider.clone().unwrap_or_else(|| {
        if is_external_idp {
            "AzureAD"
        } else if is_idc {
            "BuilderId"
        } else {
            "Google"
        }
        .to_string()
    });

    let status = if cred.disabled {
        "unknown".to_string()
    } else {
        "active".to_string()
    };

    // expiresAt → 毫秒时间戳（解析失败或缺失时为 0）
    let expires_at_ms = cred
        .expires_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0);

    // 订阅：最小可用结构（type + 原始 title）
    let subscription = serde_json::json!({
        "type": subscription_type_from_title(cred.subscription_title.as_deref()),
        "title": cred.subscription_title,
    });
    let now_ms = Utc::now().timestamp_millis();
    let usage = serde_json::json!({
        "current": 0,
        "limit": 0,
        "percentUsed": 0,
        "lastUpdated": now_ms,
    });

    // 仅导出真实 profileArn，跳过 BuilderID 占位符
    let profile_arn = cred.effective_profile_arn().map(str::to_string);

    let credentials = ExportedCredentials {
        access_token: non_empty(cred.access_token).unwrap_or_default(),
        csrf_token: String::new(),
        refresh_token: Some(refresh_token),
        client_id: non_empty(cred.client_id),
        client_secret: non_empty(cred.client_secret),
        region: non_empty(cred.region.clone())
            .or_else(|| non_empty(cred.auth_region.clone()))
            .or_else(|| non_empty(cred.api_region.clone())),
        start_url: non_empty(cred.start_url.clone()),
        token_endpoint: non_empty(cred.token_endpoint.clone()),
        issuer_url: non_empty(cred.issuer_url.clone()),
        scopes: non_empty(cred.scopes.clone()),
        expires_at: expires_at_ms,
        auth_method,
        provider: provider.clone(),
    };

    Some(ExportedAccount {
        id: uuid::Uuid::new_v4().to_string(),
        email: non_empty(cred.email).unwrap_or_default(),
        nickname: None,
        idp,
        user_id: None,
        profile_arn,
        machine_id: non_empty(cred.machine_id),
        credentials,
        subscription,
        usage,
        tags: Vec::new(),
        status,
        created_at: now_ms,
        last_used_at: now_ms,
    })
}

/// 由订阅标题推断 `SubscriptionType`（粗粒度，导入方刷新后会自行校正）
fn subscription_type_from_title(title: Option<&str>) -> &'static str {
    let Some(title) = title else { return "Free" };
    let u = title.to_uppercase();
    if u.contains("FREE") {
        "Free"
    } else if u.contains("PRO+") || u.contains("PRO PLUS") || u.contains("PRO_PLUS") {
        "Pro_Plus"
    } else if u.contains("POWER") || u.contains("ENTERPRISE") || u.contains("TEAM") {
        "Enterprise"
    } else if u.contains("PRO") {
        "Pro"
    } else {
        "Free"
    }
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        known_endpoints: impl IntoIterator<Item = String>,
    ) -> Self {
        let cache_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_balance_cache.json"));

        let proxy_pool_path = token_manager.cache_dir().map(|d| d.join("proxy_pool.json"));
        let token_manager_tls_backend = token_manager.config().tls_backend;

        let balance_cache = Self::load_balance_cache_from(&cache_path);

        let svc = Self {
            token_manager,
            kiro_provider: None,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
            proxy_pool: ProxyPoolManager::new(proxy_pool_path, token_manager_tls_backend),
            idc_sessions: Arc::new(Mutex::new(HashMap::new())),
            social_sessions: Arc::new(Mutex::new(HashMap::new())),
            trace_store: None,
        };

        // 后台任务：每 5 分钟清理过期的登录会话，防止内存泄漏
        {
            let idc = Arc::clone(&svc.idc_sessions);
            let social = Arc::clone(&svc.social_sessions);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
                loop {
                    interval.tick().await;
                    let now = Utc::now();
                    idc.lock().retain(|_, s| now < s.expires_at);
                    social.lock().retain(|_, s| now < s.expires_at);
                }
            });
        }

        svc
    }

    pub fn with_kiro_provider(mut self, provider: Arc<KiroProvider>) -> Self {
        self.kiro_provider = Some(provider);
        self
    }

    /// 暴露 TokenManager 给 handlers（分组管理需要 count / rename / remove 凭据 groups 字段）
    pub fn token_manager(&self) -> &Arc<MultiTokenManager> {
        &self.token_manager
    }

    /// 注入 trace 存储句柄，用于运行时改 trace 保留期/开关。
    ///
    /// 不含 usage 记录器 —— usage_log 的 JSONL 一律保留，没有可供运行时调节的量。
    pub fn with_log_governance(
        mut self,
        trace_store: Option<crate::admin::trace_db::SharedTraceStore>,
    ) -> Self {
        self.trace_store = trace_store;
        self
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        let snapshot = self.token_manager.snapshot();
        let exposed_current_id = if self.token_manager.get_load_balancing_mode() == "balanced" {
            0
        } else {
            snapshot.current_id
        };
        let default_endpoint = self.token_manager.config().default_endpoint.clone();

        // 一次性快照余额缓存，避免 N 次加锁
        let balance_snapshot: HashMap<u64, CachedBalance> = {
            let cache = self.balance_cache.lock();
            cache.clone()
        };
        let now_ts = Utc::now().timestamp() as f64;

        let mut credentials: Vec<CredentialStatusItem> = snapshot
            .entries
            .into_iter()
            .map(|entry| {
                let (balance, balance_updated_at) = balance_snapshot
                    .get(&entry.id)
                    .filter(|c| (now_ts - c.cached_at) < BALANCE_CACHE_TTL_SECS as f64)
                    .map(|c| (Some(c.data.clone()), Some(c.cached_at)))
                    .unwrap_or((None, None));

                CredentialStatusItem {
                    id: entry.id,
                    priority: entry.priority,
                    disabled: entry.disabled,
                    failure_count: entry.failure_count,
                    total_failure_count: entry.total_failure_count,
                    is_current: entry.id == exposed_current_id,
                    expires_at: entry.expires_at,
                    auth_method: entry.auth_method,
                    provider: entry.provider,
                    has_profile_arn: entry.has_profile_arn,
                    refresh_token_hash: entry.refresh_token_hash,
                    api_key_hash: entry.api_key_hash,
                    masked_api_key: entry.masked_api_key,
                    email: entry.email,
                    success_count: entry.success_count,
                    last_used_at: entry.last_used_at.clone(),
                    has_proxy: entry.has_proxy,
                    proxy_url: entry.proxy_url,
                    refresh_failure_count: entry.refresh_failure_count,
                    disabled_reason: entry.disabled_reason,
                    throttled_remaining_secs: entry.throttled_remaining_secs,
                    endpoint: entry.endpoint.unwrap_or_else(|| default_endpoint.clone()),
                    groups: entry.groups,
                    source_channel: entry.source_channel,
                    balance,
                    balance_updated_at,
                    created_at: entry.created_at,
                }
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: exposed_current_id,
            credentials,
        }
    }

    /// 导出凭据为兼容 JSON（嵌套 `Account` 格式）
    ///
    /// 返回的结构体含 refreshToken、accessToken、clientSecret 等敏感字段，
    /// 调用方需自行保证传输与存储安全；按 priority 升序排序，与 UI 列表一致。
    /// `id_filter` 为 None 时导出全部凭据；为 Some 时仅导出集合内的 ID。
    pub fn export_credentials(
        &self,
        id_filter: Option<&HashSet<u64>>,
    ) -> CredentialsExportResponse {
        let mut credentials = self.token_manager.clone_all_credentials();
        if let Some(filter) = id_filter {
            credentials.retain(|c| c.id.map(|id| filter.contains(&id)).unwrap_or(false));
        }
        credentials.sort_by_key(|c| c.priority);

        let accounts = credentials
            .into_iter()
            .filter_map(credential_to_export_account)
            .collect();

        CredentialsExportResponse {
            version: "1.8.3".to_string(),
            exported_at: Utc::now().timestamp_millis(),
            accounts,
            groups: Vec::new(),
            tags: Vec::new(),
        }
    }

    /// 一键禁用所有"已超额"的凭据（remaining ≤ 0 或 usage_percentage ≥ 100）
    ///
    /// 数据来源是 `balance_cache`，所以前端在调用前最好先触发一次"查询信息"
    /// 或等待后台调度器完成首次刷新。返回 (禁用数量, 跳过数量, 已超额未禁用名单)。
    pub fn disable_quota_exceeded(&self) -> QuotaExceededResult {
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        let cache_snapshot: HashMap<u64, CachedBalance> = {
            let cache = self.balance_cache.lock();
            cache.clone()
        };
        let now_ts = Utc::now().timestamp() as f64;

        let mut disabled_ids: Vec<u64> = Vec::new();
        let mut skipped_ids: Vec<u64> = Vec::new();
        let mut switched_current = false;

        for entry in snapshot.entries.iter() {
            if entry.disabled {
                continue;
            }
            let cached = match cache_snapshot.get(&entry.id) {
                Some(c) if (now_ts - c.cached_at) < BALANCE_CACHE_TTL_SECS as f64 => c,
                _ => continue,
            };
            let exceeded = cached.data.remaining <= 0.0 || cached.data.usage_percentage >= 100.0;
            if !exceeded {
                continue;
            }
            match self.token_manager.disable_quota_exceeded(entry.id) {
                Ok(()) => {
                    disabled_ids.push(entry.id);
                    if entry.id == current_id {
                        switched_current = true;
                    }
                }
                Err(e) => {
                    tracing::warn!("一键超额：禁用凭据 #{} 失败: {}", entry.id, e);
                    skipped_ids.push(entry.id);
                }
            }
        }

        if switched_current {
            let _ = self.token_manager.switch_to_next();
        }

        QuotaExceededResult {
            disabled_ids,
            skipped_ids,
        }
    }

    /// 设置凭据禁用状态
    pub fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        // 先获取当前凭据 ID，用于判断是否需要切换
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;

        // 只有禁用的是当前凭据时才尝试切换到下一个
        if disabled && id == current_id {
            let _ = self.token_manager.switch_to_next();
        }
        Ok(())
    }

    /// 设置凭据优先级
    pub fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 重置失败计数并重新启用
    pub fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))
    }

    pub fn clear_throttle(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .clear_throttle(id)
            .map_err(|e| self.classify_error(e, id))
    }

    pub fn reset_success_count(&self, id: Option<u64>) -> Result<u32, AdminServiceError> {
        self.token_manager
            .reset_success_count(id)
            .map_err(|e| self.classify_error(e, id.unwrap_or(0)))
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // 先查缓存
        {
            let cache = self.balance_cache.lock();
            if let Some(cached) = cache.get(&id) {
                let now = Utc::now().timestamp() as f64;
                if (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    tracing::debug!("凭据 #{} 余额命中缓存", id);
                    return Ok(cached.data.clone());
                }
            }
        }

        // 缓存未命中或已过期，从上游获取
        let balance = self.fetch_balance(id).await?;

        // 更新缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance.clone(),
                },
            );
        }
        self.save_balance_cache();

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        // 允许 remaining 显示为负值：开启超额后实际使用可能超过限额，
        // 直接保留差值便于在 UI 中体现"已欠多少"。
        let remaining = usage_limit - current_usage;
        // usage_percentage 同理保留真实值，超额时 > 100%。
        let usage_percentage = if usage_limit > 0.0 {
            current_usage / usage_limit * 100.0
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
            overage_enabled: usage.overage_enabled(),
            overage_capable: usage.overage_capable(),
            overage_capability_raw: usage
                .subscription_info
                .as_ref()
                .and_then(|s| s.overage_capability.clone()),
        })
    }

    /// 获取指定凭据当前可用的模型列表（实时查询上游，成功后更新共享模型缓存）
    pub async fn get_available_models(
        &self,
        id: u64,
    ) -> Result<AvailableModelsResponse, AdminServiceError> {
        let resp = self
            .token_manager
            .get_available_models_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        Ok(available_models_response(
            id,
            ModelSelectionMode::Specified,
            resp,
        ))
    }

    /// 使用账号池当前选中的可用凭据实时获取模型列表。
    pub async fn get_current_available_models(
        &self,
    ) -> Result<AvailableModelsResponse, AdminServiceError> {
        let snapshot = self.token_manager.snapshot();
        if snapshot.available == 0 {
            return Err(AdminServiceError::InvalidCredential(
                "没有当前可用凭据，无法查询模型列表".to_string(),
            ));
        }

        let (id, response, is_balanced) = self
            .token_manager
            .get_available_models_for_current()
            .await
            .map_err(|error| self.classify_balance_error(error, snapshot.current_id))?;

        let selection_mode = if is_balanced {
            ModelSelectionMode::Balanced
        } else {
            ModelSelectionMode::Priority
        };
        Ok(available_models_response(id, selection_mode, response))
    }

    /// 对指定模型发送真实、最小化的 Kiro 请求。
    pub async fn test_model(
        &self,
        request: ModelTestRequest,
    ) -> Result<ModelTestResponse, AdminServiceError> {
        let model_id = validate_model_id(&request.model_id)?;

        let provider = self
            .kiro_provider
            .as_ref()
            .ok_or_else(|| AdminServiceError::InternalError("Kiro Provider 未配置".to_string()))?;
        let conversation_state = ConversationState::new(Uuid::new_v4().to_string())
            .with_agent_continuation_id(Uuid::new_v4().to_string())
            .with_agent_task_type("vibe")
            .with_chat_trigger_type("MANUAL")
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("Reply with exactly: OK", model_id).with_origin("AI_EDITOR"),
            ));
        let body = serde_json::to_string(&KiroRequest {
            conversation_state,
            profile_arn: None,
            additional_model_request_fields: None,
        })
        .map_err(|error| AdminServiceError::InternalError(error.to_string()))?;

        let started = std::time::Instant::now();
        let (credential_id, bytes) =
            tokio::time::timeout(std::time::Duration::from_secs(90), async {
                let call = provider.call_api(&body, None, None).await?;
                let credential_id = call.credential_id;
                let bytes = call.response.bytes().await?;
                Ok::<_, anyhow::Error>((credential_id, bytes))
            })
            .await
            .map_err(|_| AdminServiceError::UpstreamError("模型测试请求超时".to_string()))?
            .map_err(|error| AdminServiceError::UpstreamError(error.to_string()))?;

        let mut decoder = EventStreamDecoder::new();
        decoder
            .feed(&bytes)
            .map_err(|error| AdminServiceError::UpstreamError(error.to_string()))?;
        let mut response_text = String::new();
        let mut credit_usage = 0.0_f64;
        let mut credit_unit = None;

        for frame in decoder.decode_iter() {
            let frame = frame.map_err(|error| {
                AdminServiceError::UpstreamError(format!("模型响应解析失败: {error}"))
            })?;
            let event = Event::from_frame(frame).map_err(|error| {
                AdminServiceError::UpstreamError(format!("模型事件解析失败: {error}"))
            })?;
            match event {
                Event::AssistantResponse(response) => response_text.push_str(&response.content),
                Event::Metering(metering) => {
                    credit_usage += metering.usage;
                    if !metering.unit.is_empty() {
                        credit_unit = Some(metering.unit);
                    }
                }
                Event::Error {
                    error_code,
                    error_message,
                } => {
                    return Err(AdminServiceError::UpstreamError(format!(
                        "{error_code}: {error_message}"
                    )));
                }
                Event::Exception {
                    exception_type,
                    message,
                } => {
                    return Err(AdminServiceError::UpstreamError(format!(
                        "{exception_type}: {message}"
                    )));
                }
                _ => {}
            }
        }

        let response_text = strip_tool_use_xml_leaks(&response_text);
        if response_text.trim().is_empty() {
            return Err(AdminServiceError::UpstreamError(
                "模型返回了空响应".to_string(),
            ));
        }

        Ok(ModelTestResponse {
            model_id: model_id.to_string(),
            credential_id,
            latency_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            response_text,
            credit_usage: (credit_usage > 0.0).then_some(credit_usage),
            credit_unit,
        })
    }

    /// 批量刷新所有非禁用凭据的余额（用于后台调度）
    ///
    /// 串行执行以避免对上游产生瞬时高并发，每次成功的查询都会更新内存缓存
    /// 与磁盘缓存。失败的条目不会清空旧缓存，调用方可在下次轮询时重试。
    pub async fn refresh_all_balances(&self) -> (usize, usize) {
        let snapshot = self.token_manager.snapshot();
        let mut success = 0_usize;
        let mut failure = 0_usize;

        for entry in snapshot.entries.into_iter() {
            if entry.disabled {
                continue;
            }
            match self.fetch_balance(entry.id).await {
                Ok(balance) => {
                    {
                        let mut cache = self.balance_cache.lock();
                        cache.insert(
                            entry.id,
                            CachedBalance {
                                cached_at: Utc::now().timestamp() as f64,
                                data: balance,
                            },
                        );
                    }
                    success += 1;
                }
                Err(e) => {
                    tracing::warn!("后台刷新凭据 #{} 余额失败: {}", entry.id, e);
                    failure += 1;
                }
            }
            // 节流，避免上游限流
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }

        if success > 0 {
            self.save_balance_cache();
        }
        (success, failure)
    }

    /// 启动余额后台刷新调度器
    ///
    /// - 启动后立刻执行一次刷新
    /// - 之后按 `interval` 周期循环刷新
    /// - 调用方持有 `Arc<Self>` 即可，任务在后台 tokio runtime 上运行
    pub fn start_balance_refresher(self: &Arc<Self>, interval: std::time::Duration) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            // 启动后稍等片刻，让上游/Token Manager 准备就绪
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            loop {
                let started = std::time::Instant::now();
                let (ok, err) = svc.refresh_all_balances().await;
                tracing::info!(
                    "余额后台刷新完成：成功 {}，失败 {}，耗时 {:.1}s",
                    ok,
                    err,
                    started.elapsed().as_secs_f32()
                );
                tokio::time::sleep(interval).await;
            }
        });
    }

    /// 启动代理池后台健康检查调度器
    ///
    /// - 启动后稍等片刻再执行首次探测
    /// - 之后按 `interval` 周期循环，对所有已启用代理并发探测
    /// - 连续探测失败达阈值的代理由 `check_all` 内部自动禁用
    pub fn start_proxy_health_checker(self: &Arc<Self>, interval: std::time::Duration) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            // 启动后稍等片刻，让网络/代理就绪
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            loop {
                let started = std::time::Instant::now();
                let summary = svc.proxy_pool.check_all().await;
                tracing::info!(
                    "代理池健康检查完成：健康 {}，异常 {}，本轮自动禁用 {}，耗时 {:.1}s",
                    summary.healthy,
                    summary.unhealthy,
                    summary.auto_disabled,
                    started.elapsed().as_secs_f32()
                );
                tokio::time::sleep(interval).await;
            }
        });
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 默认获取余额（保持单条添加 / 登录路径的既有行为：加完即可见订阅等级）
        self.add_credential_inner(req, true).await
    }

    /// 添加凭据的核心实现。
    ///
    /// - `fetch_balance = true`：添加后主动拉取余额（含订阅等级 / 邮箱）并写入缓存，
    ///   既是"加完即可见"，也作为 API Key 的有效性校验（即"验活"）。
    /// - `fetch_balance = false`：跳过余额拉取，仅落库（"直接导入"路径），
    ///   订阅信息留待首次请求时按需获取。
    async fn add_credential_inner(
        &self,
        req: AddCredentialRequest,
        fetch_balance: bool,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 校验端点名：未指定则默认合法，指定则必须已注册
        if let Some(ref name) = req.endpoint {
            if !self.known_endpoints.contains(name) {
                let mut known: Vec<&str> =
                    self.known_endpoints.iter().map(|s| s.as_str()).collect();
                known.sort();
                return Err(AdminServiceError::InvalidCredential(format!(
                    "未知端点 \"{}\"，已注册端点: {:?}",
                    name, known
                )));
            }
        }

        // 规范化 auth_method：识别企业 SSO 别名；带 tokenEndpoint 但未声明时推断为 external_idp。
        let auth_method =
            normalize_import_auth_method(&req.auth_method, req.token_endpoint.as_deref());

        // 企业 SSO 导入校验（安全边界）：
        // - 必须同时具备 clientId 与 tokenEndpoint（refresh_external_idp_token 的前提）；
        // - tokenEndpoint / issuerUrl 必须过 Microsoft allow-list，防止把 refreshToken
        //   外发到内网 / 攻击者控制的主机（SSRF / 凭据外泄）。
        if auth_method == "external_idp" {
            let client_id_ok = req
                .client_id
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty());
            let token_endpoint = req
                .token_endpoint
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let Some(token_endpoint) = token_endpoint else {
                return Err(AdminServiceError::InvalidCredential(
                    "企业 SSO (external_idp) 需要 clientId 和 tokenEndpoint".to_string(),
                ));
            };
            if !client_id_ok {
                return Err(AdminServiceError::InvalidCredential(
                    "企业 SSO (external_idp) 需要 clientId 和 tokenEndpoint".to_string(),
                ));
            }
            validate_external_idp_endpoint(token_endpoint).map_err(|e| {
                AdminServiceError::InvalidCredential(format!("tokenEndpoint 被拒绝: {}", e))
            })?;
            if let Some(issuer) = req
                .issuer_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                validate_external_idp_endpoint(issuer).map_err(|e| {
                    AdminServiceError::InvalidCredential(format!("issuerUrl 被拒绝: {}", e))
                })?;
            }
        }

        // 构建凭据对象
        let email = req.email.clone();
        let new_cred = KiroCredentials {
            id: None,
            access_token: req.access_token,
            refresh_token: req.refresh_token,
            profile_arn: req.profile_arn,
            expires_at: req.expires_at,
            auth_method: Some(auth_method),
            provider: req.provider,
            client_id: req.client_id,
            client_secret: req.client_secret,
            start_url: req.start_url,
            token_endpoint: req.token_endpoint,
            issuer_url: req.issuer_url,
            scopes: req.scopes,
            priority: req.priority,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            proxy_url: req.proxy_url,
            proxy_username: req.proxy_username,
            proxy_password: req.proxy_password,
            disabled: false, // 新添加的凭据默认启用
            disabled_reason: None,
            self_heal_consecutive_rounds: 0,
            self_heal_total_count: 0,
            last_self_heal_at: None,
            self_heal_model: None,
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
            groups: req.groups,
            source_channel: req.source_channel,
            // 创建时间由 token_manager.add_credential 在入库时统一写入
            created_at: None,
        };

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取余额（含订阅等级 / 邮箱）并写入缓存，添加后立即可见，
        // 同时避免首次请求时 Free 账号绕过 Opus 模型过滤。
        // 仅验活路径需要；"直接导入"路径跳过以省掉这次上游往返。
        if fetch_balance {
            if let Err(e) = self.get_balance(credential_id).await {
                tracing::warn!("添加凭据后刷新余额失败（不影响凭据添加）: {}", e);
            }
        }

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    /// 批量导入的单条处理。
    ///
    /// - `verify = true`（验活路径）：add（内部 refresh + 缓存 balance）→ 显式取余额验活
    ///   → 失败回滚删除。镜像前端旧流程的"add → getCredentialBalance → 失败回滚"。
    /// - `verify = false`（直接导入路径）：仅 add 落库，不取余额、不回滚。
    ///
    /// 全部在服务端完成，便于在 `buffer_unordered` 下有界并发。
    pub async fn import_one_credential(
        &self,
        req: AddCredentialRequest,
        verify: bool,
    ) -> ImportItemResult {
        // 1. add：去重 / 未知端点 / token 刷新失败在此暴露，未插入即无需回滚。
        //    verify=false 时跳过内部余额拉取。
        let resp = match self.add_credential_inner(req, verify).await {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                let is_duplicate = msg.contains("凭据已存在") || msg.contains("重复");
                return ImportItemResult {
                    status: if is_duplicate {
                        ImportStatus::Duplicate
                    } else {
                        ImportStatus::Failed
                    },
                    credential_id: None,
                    email: None,
                    balance: None,
                    error: Some(msg),
                    rolled_back: false,
                };
            }
        };

        // 2. 直接导入：add 成功即完成，不做余额验活、不回滚。
        if !verify {
            return ImportItemResult {
                status: ImportStatus::Imported,
                credential_id: Some(resp.credential_id),
                email: resp.email.clone(),
                balance: None,
                error: None,
                rolled_back: false,
            };
        }

        // 3. 验活路径：显式取余额验活（OAuth 正常路径命中 add 内缓存；
        //    API Key 无 token 刷新，余额拉取即真正的验活，失败则回滚）。
        match self.get_balance(resp.credential_id).await {
            Ok(balance) => ImportItemResult {
                status: ImportStatus::Verified,
                credential_id: Some(resp.credential_id),
                email: resp.email.clone(),
                balance: Some(balance),
                error: None,
                rolled_back: false,
            },
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(
                    "批量导入凭据 #{} 验活失败，回滚删除: {}",
                    resp.credential_id,
                    msg
                );
                // 回滚：直接删除（delete_credential 会清理 balance 缓存与 trace）。
                // 不先 disable——delete 是整条移除，无 enabled 守卫，足够原子。
                let rolled_back = self.delete_credential(resp.credential_id).is_ok();
                ImportItemResult {
                    status: ImportStatus::Failed,
                    credential_id: Some(resp.credential_id),
                    email: resp.email,
                    balance: None,
                    error: Some(msg),
                    rolled_back,
                }
            }
        }
    }

    /// 更新凭据的可编辑字段（email、proxy 等）
    pub fn update_credential(
        &self,
        id: u64,
        req: UpdateCredentialRequest,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .update_credential(
                id,
                req.email.map(|v| if v.is_empty() { None } else { Some(v) }),
                req.proxy_url
                    .map(|v| if v.is_empty() { None } else { Some(v) }),
                req.proxy_username
                    .map(|v| if v.is_empty() { None } else { Some(v) }),
                req.proxy_password
                    .map(|v| if v.is_empty() { None } else { Some(v) }),
                req.groups,
                req.source_channel
                    .map(|v| if v.is_empty() { None } else { Some(v) }),
            )
            .map_err(|e| self.classify_error(e, id))
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_credential(id)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存
        self.invalidate_balance_cache(id);

        if let Some(trace_store) = &self.trace_store {
            trace_store.delete_for_credential(id);
        }

        Ok(())
    }

    /// 从磁盘加载最新配置并应用更新，再写回磁盘。
    ///
    /// 每次读最新文件再写，避免多次调用之间字段互相覆盖。
    fn update_config_file(&self, updater: impl FnOnce(&mut Config)) {
        if let Err(error) = self.token_manager.update_config_file(updater) {
            tracing::warn!("保存配置文件失败: {}", error);
        }
    }

    /// 获取全局代理 URL
    pub fn get_global_proxy(&self) -> Option<String> {
        self.token_manager.proxy().map(|p| p.url.clone())
    }

    /// 设置全局代理 URL（None 表示清除）并持久化到配置文件
    pub fn set_global_proxy(&self, url: Option<String>) -> Result<(), AdminServiceError> {
        if let Some(ref u) = url {
            let valid_prefix = u.starts_with("http://")
                || u.starts_with("https://")
                || u.starts_with("socks5://")
                || u.starts_with("socks4://");
            if !valid_prefix {
                return Err(AdminServiceError::InvalidCredential(
                    "代理 URL 格式无效，需以 http://、https://、socks5:// 或 socks4:// 开头"
                        .to_string(),
                ));
            }
        }

        let proxy = url.as_deref().map(ProxyConfig::new);
        self.token_manager.set_global_proxy(proxy);

        // 从磁盘加载最新 config 再写，避免覆盖其他字段的并发修改
        let url_for_save = url;
        self.update_config_file(move |c| c.proxy_url = url_for_save);
        Ok(())
    }

    /// 持久化新的登录API密钥（adminApiKey）到配置文件（内存中的 key 由 handler 层负责更新）
    pub fn persist_admin_key(&self, new_key: &str) {
        let key = new_key.to_string();
        self.update_config_file(move |c| c.admin_api_key = Some(key));
    }

    /// 将系统密钥写回 `config.json`。
    pub fn persist_api_key(&self, new_key: &str) {
        let key = new_key.to_string();
        self.update_config_file(move |c| c.api_key = Some(key));
    }

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {
        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 设置负载均衡模式
    pub fn set_load_balancing_mode(
        &self,
        req: SetLoadBalancingModeRequest,
    ) -> Result<LoadBalancingModeResponse, AdminServiceError> {
        // 验证模式值
        if req.mode != "priority" && req.mode != "balanced" {
            return Err(AdminServiceError::InvalidCredential(
                "mode 必须是 'priority' 或 'balanced'".to_string(),
            ));
        }

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    /// 获取账号级风控故障转移配置
    pub fn get_account_throttle_config(&self) -> AccountThrottleConfigResponse {
        AccountThrottleConfigResponse {
            failover: self.token_manager.get_account_throttle_failover(),
            cooldown_secs: self.token_manager.get_account_throttle_cooldown_secs(),
        }
    }

    /// 更新账号级风控故障转移配置
    pub fn set_account_throttle_config(
        &self,
        req: SetAccountThrottleConfigRequest,
    ) -> Result<AccountThrottleConfigResponse, AdminServiceError> {
        if req.failover.is_none() && req.cooldown_secs.is_none() {
            return Err(AdminServiceError::InvalidCredential(
                "至少提供 failover 或 cooldownSecs 一个字段".to_string(),
            ));
        }

        self.token_manager
            .set_account_throttle_config(req.failover, req.cooldown_secs)
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))?;

        Ok(self.get_account_throttle_config())
    }

    /// 获取单账号 RPM 限流配置
    pub fn get_account_rpm_limit_config(&self) -> AccountRpmLimitConfigResponse {
        let (enabled, limit) = self.token_manager.get_account_rpm_limit_config();
        AccountRpmLimitConfigResponse { enabled, limit }
    }

    /// 更新单账号 RPM 限流配置
    pub fn set_account_rpm_limit_config(
        &self,
        req: SetAccountRpmLimitConfigRequest,
    ) -> Result<AccountRpmLimitConfigResponse, AdminServiceError> {
        if req.enabled.is_none() && req.limit.is_none() {
            return Err(AdminServiceError::InvalidCredential(
                "至少提供 enabled 或 limit 一个字段".to_string(),
            ));
        }

        self.token_manager
            .set_account_rpm_limit_config(req.enabled, req.limit)
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))?;

        Ok(self.get_account_rpm_limit_config())
    }

    /// 获取自愈治理配置
    pub fn get_self_heal_config(&self) -> SelfHealConfigResponse {
        let (
            suspended_detection_enabled,
            enabled,
            min_interval_secs,
            max_consecutive_rounds,
            consecutive_rounds,
            total_count,
        ) = self.token_manager.get_self_heal_config();
        SelfHealConfigResponse {
            suspended_detection_enabled,
            enabled,
            min_interval_secs,
            max_consecutive_rounds,
            consecutive_rounds,
            total_count,
        }
    }

    /// 更新自愈治理配置
    pub fn set_self_heal_config(
        &self,
        req: SetSelfHealConfigRequest,
    ) -> Result<SelfHealConfigResponse, AdminServiceError> {
        if req.suspended_detection_enabled.is_none()
            && req.enabled.is_none()
            && req.min_interval_secs.is_none()
            && req.max_consecutive_rounds.is_none()
        {
            return Err(AdminServiceError::InvalidCredential(
                "至少提供 suspendedDetectionEnabled / enabled / minIntervalSecs / maxConsecutiveRounds 一个字段"
                    .to_string(),
            ));
        }

        self.token_manager
            .set_self_heal_config(
                req.suspended_detection_enabled,
                req.enabled,
                req.min_interval_secs,
                req.max_consecutive_rounds,
            )
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))?;

        Ok(self.get_self_heal_config())
    }

    /// 读取日志治理配置（trace 开关 / trace 保留天数 / usage 保留天数）
    pub fn get_log_governance_config(&self) -> LogGovernanceConfigResponse {
        let cfg = self.token_manager.config();
        LogGovernanceConfigResponse {
            trace_enabled: self
                .trace_store
                .as_ref()
                .map(|s| s.is_enabled())
                .unwrap_or(cfg.trace_enabled),
            trace_retention_days: self
                .trace_store
                .as_ref()
                .map(|s| s.retention_days() as u32)
                .unwrap_or(cfg.trace_retention_days),
        }
    }

    /// 更新日志治理配置：改运行时原子值 + 持久化到 config.json。
    /// 任一字段缺省表示不修改。
    pub fn set_log_governance_config(
        &self,
        req: SetLogGovernanceConfigRequest,
    ) -> Result<LogGovernanceConfigResponse, AdminServiceError> {
        if req.trace_enabled.is_none() && req.trace_retention_days.is_none() {
            return Err(AdminServiceError::InvalidCredential(
                "至少提供 traceEnabled / traceRetentionDays 一个字段".to_string(),
            ));
        }
        // 校验范围：保留天数 1..=365
        if let Some(d) = req.trace_retention_days {
            if !(1..=365).contains(&d) {
                return Err(AdminServiceError::InvalidCredential(format!(
                    "traceRetentionDays 必须在 1..=365 内: {}",
                    d
                )));
            }
        }

        // 先改运行时原子值
        if let Some(enabled) = req.trace_enabled {
            if let Some(s) = &self.trace_store {
                s.set_enabled(enabled);
            }
        }
        if let Some(days) = req.trace_retention_days {
            if let Some(s) = &self.trace_store {
                s.set_retention_days(days);
            }
        }

        // 持久化到 config.json
        if let Err(e) = self.persist_log_governance_config(&req) {
            tracing::warn!("持久化日志治理配置失败（运行时已生效）: {}", e);
        }

        Ok(self.get_log_governance_config())
    }

    fn persist_log_governance_config(
        &self,
        req: &SetLogGovernanceConfigRequest,
    ) -> anyhow::Result<()> {
        self.token_manager.update_config_file(|config| {
            if let Some(value) = req.trace_enabled {
                config.trace_enabled = value;
            }
            if let Some(value) = req.trace_retention_days {
                config.trace_retention_days = value;
            }
        })
    }

    /// 更新指定凭据的 refreshToken（仅限已禁用凭据）
    pub fn update_refresh_token(
        &self,
        id: u64,
        req: UpdateRefreshTokenRequest,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .update_refresh_token(id, req.refresh_token, req.access_token, req.expires_at)
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("不存在") {
                    AdminServiceError::NotFound { id }
                } else if msg.contains("只能为已禁用")
                    || msg.contains("refreshToken 重复")
                    || msg.contains("已被截断")
                    || msg.contains("refreshToken 为空")
                    || msg.contains("缺少 refreshToken")
                {
                    AdminServiceError::InvalidCredential(msg)
                } else {
                    AdminServiceError::InternalError(msg)
                }
            })
    }

    /// 一键开启所有"可开启超额且当前未开启"凭据的超额
    /// 数据来源是 balance_cache（5 分钟有效）；若缓存缺失或 capable 状态未知则乐观尝试，
    /// 由上游 setUserPreference 接口本身决定是否成功（不支持的订阅会返回 4xx 失败）。
    pub async fn enable_overage_for_all_capable(&self) -> EnableOverageAllResult {
        let snapshot = self.token_manager.snapshot();
        let cache_snapshot: HashMap<u64, CachedBalance> = {
            let cache = self.balance_cache.lock();
            cache.clone()
        };
        let now_ts = Utc::now().timestamp() as f64;

        // 选出需要操作的 ID 列表
        let mut targets: Vec<u64> = Vec::new();
        let mut skipped: Vec<u64> = Vec::new();
        for entry in snapshot.entries.iter() {
            if entry.disabled {
                skipped.push(entry.id);
                continue;
            }
            let cached = cache_snapshot
                .get(&entry.id)
                .filter(|c| (now_ts - c.cached_at) < BALANCE_CACHE_TTL_SECS as f64);

            match cached {
                // 缓存命中：明确不可开启，跳过
                Some(c) if c.data.overage_capable == Some(false) => {
                    skipped.push(entry.id);
                    continue;
                }
                // 缓存命中：明确已开启，跳过
                Some(c) if c.data.overage_enabled == Some(true) => {
                    skipped.push(entry.id);
                    continue;
                }
                // 其它（缓存缺失 / 状态未知 / 明确可开启未开启）— 乐观尝试
                _ => targets.push(entry.id),
            }
        }

        let mut enabled_ids: Vec<u64> = Vec::new();
        let mut failed_ids: Vec<u64> = Vec::new();
        let mut failure_messages: Vec<String> = Vec::new();

        for id in targets {
            match self
                .token_manager
                .set_user_preference_for(id, "ENABLED")
                .await
            {
                Ok(()) => {
                    enabled_ids.push(id);
                    // 失效本地缓存
                    let mut cache = self.balance_cache.lock();
                    cache.remove(&id);
                }
                Err(e) => {
                    tracing::warn!("一键开启超额：凭据 #{} 失败: {}", id, e);
                    failed_ids.push(id);
                    failure_messages.push(e.to_string());
                }
            }
            // 节流
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }

        if !enabled_ids.is_empty() {
            self.save_balance_cache();
        }

        EnableOverageAllResult {
            enabled_ids,
            skipped_ids: skipped,
            failed_ids,
            failure_messages,
        }
    }

    /// 强制刷新指定凭据的 Token
    pub async fn force_refresh_token(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .force_refresh_token_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    /// 设置凭据的"超额"开关（ENABLED / DISABLED）
    /// 成功后会主动失效本地余额缓存，让下次列表刷新展示最新 overage 状态
    pub async fn set_overage(&self, id: u64, enabled: bool) -> Result<(), AdminServiceError> {
        let status = if enabled { "ENABLED" } else { "DISABLED" };
        self.token_manager
            .set_user_preference_for(id, status)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        // 让本地缓存的 overage 状态失效（下次刷新时重新拉）
        self.invalidate_balance_cache(id);

        // 异步触发一次新的余额查询（不阻塞响应）
        let svc_handle = self.token_manager.clone();
        tokio::spawn(async move {
            if let Err(e) = svc_handle.get_usage_limits_for(id).await {
                tracing::warn!("超额状态变更后预热余额失败 #{}: {}", id, e);
            }
        });

        Ok(())
    }

    // ============ 余额缓存持久化 ============

    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<u64, CachedBalance> {
        let path = match cache_path {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // 文件中使用字符串 key 以兼容 JSON 格式
        let map: HashMap<String, CachedBalance> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析余额缓存失败，将忽略: {}", e);
                return HashMap::new();
            }
        };

        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                let id = k.parse::<u64>().ok()?;
                // 丢弃超过 TTL 的条目
                if (now - v.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    Some((id, v))
                } else {
                    None
                }
            })
            .collect()
    }

    /// 作废某个凭据的余额缓存并落盘
    fn invalidate_balance_cache(&self, id: u64) {
        self.balance_cache.lock().remove(&id);
        self.save_balance_cache();
    }

    fn save_balance_cache(&self) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // 持有锁期间完成序列化和写入，防止并发损坏
        let cache = self.balance_cache.lock();
        let map: HashMap<String, &CachedBalance> =
            cache.iter().map(|(k, v)| (k.to_string(), v)).collect();

        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("保存余额缓存失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化余额缓存失败: {}", e),
        }
    }

    // ============ 代理池管理 ============

    /// 获取代理池列表（含凭据引用计数）
    pub fn get_proxy_pool(&self) -> ProxyPoolResponse {
        let proxies = self.proxy_pool.list();
        let credentials = {
            let snapshot = self.token_manager.snapshot();
            snapshot.entries
        };

        let pool: Vec<ProxyPoolEntry> = proxies
            .into_iter()
            .map(|p| {
                let count = credentials
                    .iter()
                    .filter(|c| c.proxy_url.as_deref().map(|u| u == p.url).unwrap_or(false))
                    .count() as u32;
                ProxyPoolEntry {
                    id: p.id,
                    url: p.url,
                    label: p.label,
                    enabled: p.enabled,
                    credential_count: count,
                    health: p.health,
                    latency_ms: p.latency_ms,
                    last_checked_at: p.last_checked_at,
                    consecutive_failures: p.consecutive_failures,
                    auto_disabled: p.auto_disabled,
                }
            })
            .collect();

        ProxyPoolResponse {
            total: pool.len(),
            proxies: pool,
        }
    }

    /// 添加代理到池中
    pub fn add_proxy(
        &self,
        url: String,
        label: Option<String>,
    ) -> Result<ProxyPoolEntry, AdminServiceError> {
        let entry = self
            .proxy_pool
            .add(url, label)
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))?;
        Ok(ProxyPoolEntry {
            id: entry.id,
            url: entry.url,
            label: entry.label,
            enabled: entry.enabled,
            credential_count: 0,
            health: entry.health,
            latency_ms: entry.latency_ms,
            last_checked_at: entry.last_checked_at,
            consecutive_failures: entry.consecutive_failures,
            auto_disabled: entry.auto_disabled,
        })
    }

    /// 批量添加代理
    pub fn batch_add_proxies(
        &self,
        req: BatchAddProxyRequest,
    ) -> (Vec<ProxyPoolEntry>, Vec<String>) {
        let (added, errors) = self.proxy_pool.batch_add(req.urls);
        let result = added
            .into_iter()
            .map(|e| ProxyPoolEntry {
                id: e.id,
                url: e.url,
                label: e.label,
                enabled: e.enabled,
                credential_count: 0,
                health: e.health,
                latency_ms: e.latency_ms,
                last_checked_at: e.last_checked_at,
                consecutive_failures: e.consecutive_failures,
                auto_disabled: e.auto_disabled,
            })
            .collect();
        (result, errors)
    }

    /// 删除代理池中的代理
    pub fn delete_proxy(&self, id: u64) -> Result<(), AdminServiceError> {
        self.proxy_pool.delete(id).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("不存在") {
                AdminServiceError::NotFound { id }
            } else {
                AdminServiceError::InternalError(msg)
            }
        })
    }

    /// 设置代理启用/禁用状态
    pub fn set_proxy_enabled(&self, id: u64, enabled: bool) -> Result<(), AdminServiceError> {
        self.proxy_pool
            .set_enabled(id, enabled)
            .map_err(|_| AdminServiceError::NotFound { id })
    }

    /// 将代理池中的代理分配给指定凭据
    pub fn assign_proxy_to_credential(
        &self,
        credential_id: u64,
        req: AssignProxyRequest,
    ) -> Result<(), AdminServiceError> {
        let proxy_url = match req.proxy_id {
            Some(proxy_id) => {
                let url = match self.proxy_pool.get_url(proxy_id) {
                    GetUrlResult::Ok(url) => url,
                    GetUrlResult::NotFound => {
                        return Err(AdminServiceError::NotFound { id: proxy_id });
                    }
                    GetUrlResult::Disabled => {
                        return Err(AdminServiceError::InvalidCredential(format!(
                            "代理 #{} 已被禁用，请先启用后再分配",
                            proxy_id
                        )));
                    }
                };
                Some(url)
            }
            None => None, // 清除代理
        };

        self.token_manager
            .update_credential(
                credential_id,
                None,            // email 不修改
                Some(proxy_url), // 设置或清除 proxy_url（Some(None) = 清除，Some(Some(url)) = 设置）
                None,            // proxy_username 不修改
                None,            // proxy_password 不修改
                None,            // groups 不修改
                None,            // source_channel 不修改
            )
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("不存在") {
                    AdminServiceError::NotFound { id: credential_id }
                } else {
                    AdminServiceError::InternalError(msg)
                }
            })
    }

    /// 即时探测单个代理的连通性（供 UI「测试」按钮调用）
    pub async fn check_proxy(&self, id: u64) -> Result<ProxyCheckResponse, AdminServiceError> {
        let entry = self
            .proxy_pool
            .check_one(id)
            .await
            .map_err(|_| AdminServiceError::NotFound { id })?;
        Ok(ProxyCheckResponse {
            id: entry.id,
            health: entry.health,
            latency_ms: entry.latency_ms,
            last_checked_at: entry.last_checked_at,
            enabled: entry.enabled,
            auto_disabled: entry.auto_disabled,
        })
    }

    /// 触发全部代理的健康检查
    pub async fn check_all_proxies(&self) -> ProxyCheckAllResponse {
        let summary = self.proxy_pool.check_all().await;
        ProxyCheckAllResponse {
            healthy: summary.healthy,
            unhealthy: summary.unhealthy,
            auto_disabled: summary.auto_disabled,
        }
    }

    /// 将可用代理（已启用且非 Unhealthy）按轮询方式批量分配给凭据
    ///
    /// - `credential_ids` 为 None 时对全部凭据分配
    /// - 无可用代理时返回错误
    pub fn assign_proxies_round_robin(
        &self,
        credential_ids: Option<Vec<u64>>,
    ) -> Result<AssignRoundRobinResponse, AdminServiceError> {
        let urls = self.proxy_pool.assignable_urls();
        if urls.is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "没有可用代理（需已启用且健康检查未失败）".to_string(),
            ));
        }

        let target_ids: Vec<u64> = match credential_ids {
            Some(ids) if !ids.is_empty() => ids,
            _ => self
                .token_manager
                .snapshot()
                .entries
                .iter()
                .map(|c| c.id)
                .collect(),
        };

        let mut assigned = 0;
        for (i, cred_id) in target_ids.iter().enumerate() {
            let url = urls[i % urls.len()].clone();
            if self
                .token_manager
                .update_credential(*cred_id, None, Some(Some(url)), None, None, None, None)
                .is_ok()
            {
                assigned += 1;
            }
        }

        Ok(AssignRoundRobinResponse {
            assigned,
            proxy_count: urls.len(),
        })
    }

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        if let Some(error) = classify_rate_limit(&e) {
            return error;
        }
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        if let Some(error) = classify_rate_limit(&e) {
            return error;
        }
        let msg = e.to_string();

        if e.downcast_ref::<RefreshTokenInvalidError>().is_some() {
            return AdminServiceError::InvalidCredential(msg);
        }

        // 1. 凭据不存在
        if msg.contains("不存在") {
            return AdminServiceError::NotFound { id };
        }

        // 2. API Key 凭据不支持刷新：客户端请求错误，映射为 400
        if msg.contains("API Key 凭据不支持刷新") {
            return AdminServiceError::InvalidCredential(msg);
        }

        // 3. 上游明确指出凭据缺少或携带了错误的 Profile ARN，属于导入凭据不完整/无效。
        if msg.contains("Invalid profileArn") {
            return AdminServiceError::InvalidCredential(
                "凭据缺少或包含无效 profileArn，无法查询余额；请重新登录获取 profileArn，或导入包含 profileArn 的完整凭据"
                    .to_string(),
            );
        }

        // 4. 上游已明确说明原因：优先复用数据面那三个判据，避免同一语义两套字符串。
        //    错误串形如 "{前缀}: {status} {body}"（token_manager 的 bail!），
        //    body 原样在内，所以这些判据可直接作用于它。
        //    文案保持极简：面板要的是「下一步做什么」，不是 AWS 的整段报文。
        if endpoint::default_is_account_suspended(&msg) {
            return AdminServiceError::UpstreamRejected {
                public: "账号被封禁",
                detail: msg,
            };
        }
        if endpoint::default_is_monthly_request_limit(&msg) {
            return AdminServiceError::UpstreamRejected {
                public: "额度已用尽",
                detail: msg,
            };
        }
        if endpoint::default_is_bearer_token_invalid(&msg) {
            return AdminServiceError::UpstreamRejected {
                public: "Token 已失效",
                detail: msg,
            };
        }

        // 5. body 没带可识别标记时，退到 token_manager 自己按 HTTP 状态给出的前缀。
        //
        //    这一层是必要的而非锦上添花：REST 端点对**被封账号**只回一句通用的
        //    `User is not authorized to make this call.`，既无 `TEMPORARILY_SUSPENDED`
        //    也无封禁文案（2026-08-13 实测），上面三条判据必然全部落空。只有流式端点
        //    才吐详细原因。所以这里能诚实说出的就是「上游拒绝了」，**不能**推断成
        //    「账号被封禁」—— 403 也可能是权限/区域问题，猜错比不说更糟。
        //    账号是否真被封由数据面写进 disabledReason，卡片徽标另有显示。
        //
        //    两侧都是固定字面量，不带上游报文，故不存在泄露账号 ID / request-id。
        const UPSTREAM_PREFIX_MESSAGES: &[(&str, &str)] = &[
            ("认证失败", "Token 已失效"),
            ("权限不足", "上游拒绝访问"),
            ("服务器错误", "上游服务异常"),
        ];
        if let Some((_, public)) = UPSTREAM_PREFIX_MESSAGES
            .iter()
            .find(|(prefix, _)| msg.starts_with(prefix))
        {
            return AdminServiceError::UpstreamRejected {
                public,
                detail: msg,
            };
        }

        // 6. 其余上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error = msg.contains("获取使用额度失败") ||
            msg.contains("获取可用模型失败") ||
            msg.contains("设置用户偏好失败") ||
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭证已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("Token 刷新失败") ||
            msg.contains("暂时不可用") ||
            // 网络错误（reqwest 错误格式）
            msg.contains("error sending request") ||
            msg.contains("error trying to connect") ||
            msg.contains("connection") ||
            msg.contains("timeout") ||
            msg.contains("timed out") ||
            msg.contains("proxy") ||
            msg.contains("SOCKS") ||
            msg.contains("dns") ||
            msg.contains("DNS");

        if is_upstream_error {
            AdminServiceError::UpstreamError(msg)
        } else {
            // 7. 默认归类为内部错误（本地验证失败、配置错误等）
            // 包括：缺少 refreshToken、refreshToken 已被截断、无法生成 machineId 等
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类添加凭据错误
    fn classify_add_error(&self, e: anyhow::Error) -> AdminServiceError {
        if let Some(error) = classify_rate_limit(&e) {
            return error;
        }
        let msg = e.to_string();

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
            || msg.contains("凭据已存在")
            || msg.contains("refreshToken 重复")
            || msg.contains("kiroApiKey 重复")
            || msg.contains("缺少 kiroApiKey")
            || msg.contains("kiroApiKey 为空")
            || msg.contains("凭证已过期或无效")
            || msg.contains("权限不足")
            || msg.contains("已被限流");

        if is_invalid_credential {
            AdminServiceError::InvalidCredential(msg)
        } else if msg.contains("error trying to connect")
            || msg.contains("connection")
            || msg.contains("timeout")
        {
            AdminServiceError::UpstreamError(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    // ── Social 登录（Portal PKCE OAuth）────────────────────────────────────────

    /// 发起 Social 登录，返回 portal URL 供用户在浏览器打开
    ///
    /// 始终在服务端本机启动临时 TCP 回调服务器，redirect_uri 为 `http://127.0.0.1:{port}`。
    /// 本机浏览器授权后自动完成；远程访问时用户从地址栏复制回调 URL，经 `complete_social_login` 手动完成。
    pub async fn start_social_login(
        &self,
        req: StartSocialLoginRequest,
    ) -> Result<StartSocialLoginResponse, AdminServiceError> {
        let global_proxy = self.token_manager.proxy();
        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(global_proxy);

        let auth_endpoint = req
            .auth_endpoint
            .unwrap_or_else(|| social::KIRO_AUTH_ENDPOINT.to_string());

        let (code_verifier, code_challenge) = social::generate_pkce();
        let state = uuid::Uuid::new_v4().to_string();

        let (tx, rx) = tokio::sync::oneshot::channel::<social::OAuthCallbackData>();

        // 启动本地 TCP 回调服务器（本地模式）
        // 远程访问时用户须从浏览器地址栏复制回调 URL，通过 complete_social_login 接口手动完成
        let (port, server_handle) = social::start_callback_server(tx)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let redirect_uri = format!("http://127.0.0.1:{}", port);
        let portal_url = social::build_portal_url(&state, &code_challenge, &redirect_uri);

        let expires_at = Utc::now() + Duration::minutes(10);
        let session_id = uuid::Uuid::new_v4().to_string();

        let cred_template = KiroCredentials {
            auth_method: Some("social".to_string()),
            priority: req.priority,
            email: req.email,
            proxy_url: req.proxy_url,
            ..Default::default()
        };

        let session = SocialAuthSession {
            auth_endpoint,
            state,
            code_verifier,
            redirect_uri,
            expires_at,
            callback_rx: tokio::sync::Mutex::new(rx),
            cred_template,
            proxy,
            _server_handle: server_handle,
            relogin_target_id: None,
        };

        self.social_sessions
            .lock()
            .insert(session_id.clone(), session);

        Ok(StartSocialLoginResponse {
            session_id,
            portal_url,
            expires_at: expires_at.to_rfc3339(),
        })
    }

    /// 轮询一次 Social 登录状态
    pub async fn poll_social_login(
        &self,
        session_id: &str,
    ) -> Result<PollIdcLoginResponse, AdminServiceError> {
        use tokio::sync::oneshot::error::TryRecvError;

        // 一次加锁同时完成：过期检查 + 非阻塞回调接收，消除 TOCTOU
        enum PollOutcome {
            Expired,
            Closed,
            Pending,
            Received(social::OAuthCallbackData),
        }

        let outcome = {
            let sessions = self.social_sessions.lock();
            let Some(session) = sessions.get(session_id) else {
                return Err(AdminServiceError::NotFound { id: 0 });
            };

            if Utc::now() >= session.expires_at {
                PollOutcome::Expired
            } else {
                match session.callback_rx.try_lock() {
                    Ok(mut rx) => match rx.try_recv() {
                        Ok(data) => PollOutcome::Received(data),
                        Err(TryRecvError::Empty) => PollOutcome::Pending,
                        Err(TryRecvError::Closed) => PollOutcome::Closed,
                    },
                    Err(_) => PollOutcome::Pending,
                }
            }
        };

        match outcome {
            PollOutcome::Pending => return Ok(PollIdcLoginResponse::Pending),
            PollOutcome::Expired => {
                self.social_sessions.lock().remove(session_id);
                return Ok(PollIdcLoginResponse::Expired);
            }
            PollOutcome::Closed => {
                self.social_sessions.lock().remove(session_id);
                return Err(AdminServiceError::InternalError(
                    "Social 登录回调服务器已关闭，请重新发起登录".to_string(),
                ));
            }
            PollOutcome::Received(callback) => {
                self.do_complete_social_login(session_id, callback).await
            }
        }
    }

    /// 内部：完成 Social 登录的 token 兑换和凭据创建（供轮询回调和手动完成共用）
    ///
    /// 调用前须确认 session 存在且未过期。会在内部做 state CSRF 校验。
    async fn do_complete_social_login(
        &self,
        session_id: &str,
        callback: social::OAuthCallbackData,
    ) -> Result<PollIdcLoginResponse, AdminServiceError> {
        // 先做 CSRF 校验（不移除 session，校验失败时保持 session 可继续轮询）
        {
            let sessions = self.social_sessions.lock();
            let s = sessions
                .get(session_id)
                .ok_or(AdminServiceError::NotFound { id: 0 })?;
            if callback.state != s.state {
                tracing::warn!(
                    "Social 登录 state 不匹配（期望 {}, 收到 {}），已拒绝",
                    s.state,
                    callback.state
                );
                return Err(AdminServiceError::InternalError(
                    "OAuth state 不匹配，请重新发起登录".to_string(),
                ));
            }
        }

        // 移除 session（含 code_verifier 等敏感数据）
        let session = self
            .social_sessions
            .lock()
            .remove(session_id)
            .ok_or(AdminServiceError::NotFound { id: 0 })?;

        let config = self.token_manager.config();

        // 构建完整的 redirect_uri（与 IDE 行为一致）
        let full_redirect_uri = if callback.login_option.is_empty() {
            format!("{}{}", session.redirect_uri, callback.path)
        } else {
            format!(
                "{}{}?login_option={}",
                session.redirect_uri,
                callback.path,
                urlencoding::encode(&callback.login_option),
            )
        };

        let token = social::exchange_code_for_token(
            &session.auth_endpoint,
            &callback.code,
            &session.code_verifier,
            &full_redirect_uri,
            config,
            session.proxy.as_ref(),
        )
        .await
        .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        // 重新登录模式：更新已有凭据而非创建新凭据
        if let Some(target_id) = session.relogin_target_id {
            let refresh_token = token.refresh_token.ok_or_else(|| {
                AdminServiceError::InternalError(
                    "Social 登录未返回 refreshToken，无法更新凭据".to_string(),
                )
            })?;
            self.do_relogin_update(target_id, refresh_token)
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
            tracing::info!("Social 重新登录成功，凭据 #{} Token 已更新", target_id);
            return Ok(PollIdcLoginResponse::Success {
                credential_id: target_id,
            });
        }

        let mut new_cred = session.cred_template;
        new_cred.access_token = Some(token.access_token);
        new_cred.refresh_token = token.refresh_token;
        new_cred.expires_at = token.expires_at.or_else(|| {
            token
                .expires_in
                .map(|secs| (Utc::now() + Duration::seconds(secs)).to_rfc3339())
        });
        if let Some(arn) = token.profile_arn {
            new_cred.profile_arn = Some(arn);
        }

        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        // 主动刷新余额（含订阅等级 / 邮箱）并写入缓存，登录后立即可见
        if let Err(e) = self.get_balance(credential_id).await {
            tracing::warn!("Social 登录后刷新余额失败（不影响登录）: {}", e);
        }

        tracing::info!("Social 登录成功，已添加凭据 #{}", credential_id);
        Ok(PollIdcLoginResponse::Success { credential_id })
    }

    /// 手动完成 Social 登录：远程访问时从浏览器地址栏粘贴的回调 URL 中提取参数，直接完成 token 兑换
    pub async fn complete_social_login(
        &self,
        session_id: &str,
        code: String,
        state: String,
        login_option: String,
        path: String,
    ) -> Result<PollIdcLoginResponse, AdminServiceError> {
        // 过期检查
        {
            let sessions = self.social_sessions.lock();
            let s = sessions
                .get(session_id)
                .ok_or(AdminServiceError::NotFound { id: 0 })?;
            if Utc::now() >= s.expires_at {
                return Ok(PollIdcLoginResponse::Expired);
            }
        }

        let callback = social::OAuthCallbackData {
            code,
            login_option,
            path,
            state,
        };
        self.do_complete_social_login(session_id, callback).await
    }

    /// 分类删除凭据错误
    fn classify_delete_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        // `delete_credential` 只会产生「不存在」或持久化失败两类错误 ——
        // 它不要求凭据先禁用，故此处无需匹配「请先禁用凭据」之类的文案
        // （那是 `update_refresh_token` 的前置条件，走不到这条路径）。
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    // ── IdC 设备授权登录 ──────────────────────────────────────────────────────

    /// 发起 IdC 设备授权，返回验证码和 URL
    pub async fn start_idc_login(
        &self,
        req: StartIdcLoginRequest,
    ) -> Result<StartIdcLoginResponse, AdminServiceError> {
        // 代理：优先用请求级，否则回退全局
        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(self.token_manager.proxy());

        let start_url = req
            .start_url
            .unwrap_or_else(|| BUILDER_ID_START_URL.to_string());

        // 新建凭据独有的字段，其余由设备授权流程统一填充
        let cred_template = KiroCredentials {
            priority: req.priority,
            email: req.email,
            proxy_url: req.proxy_url,
            ..Default::default()
        };

        self.begin_idc_device_authorization(req.region, start_url, proxy, cred_template, None)
            .await
    }

    /// 注册 OIDC 客户端、发起设备授权并登记会话。首次登录与重新登录共用。
    ///
    /// `cred_template` 只需带调用方独有的字段（如 priority/email），IdC 相关字段
    /// （authMethod / provider / clientId / clientSecret / startUrl / region）在此统一写入。
    async fn begin_idc_device_authorization(
        &self,
        region: String,
        start_url: String,
        proxy: Option<ProxyConfig>,
        mut cred_template: KiroCredentials,
        relogin_target_id: Option<u64>,
    ) -> Result<StartIdcLoginResponse, AdminServiceError> {
        let config = self.token_manager.config();

        // 1. 注册 OIDC 客户端
        let reg = idc::register_client(&region, &start_url, config, proxy.as_ref())
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        // 2. 发起设备授权
        let device = idc::start_device_authorization(
            &region,
            &start_url,
            &reg.client_id,
            &reg.client_secret,
            config,
            proxy.as_ref(),
        )
        .await
        .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let expires_at = Utc::now() + Duration::seconds(device.expires_in);
        let session_id = Uuid::new_v4().to_string();

        cred_template.auth_method = Some("idc".to_string());
        cred_template.provider = Some(idc_provider_for(&start_url).to_string());
        cred_template.client_id = Some(reg.client_id.clone());
        cred_template.client_secret = Some(reg.client_secret.clone());
        cred_template.start_url = Some(start_url.clone());
        cred_template.region = Some(region.clone());

        let session = IdcAuthSession {
            region,
            start_url,
            client_id: reg.client_id,
            client_secret: reg.client_secret,
            device_code: device.device_code,
            expires_at,
            poll_interval: device.interval.max(5),
            cred_template,
            proxy,
            relogin_target_id,
        };

        let poll_interval = session.poll_interval;
        self.idc_sessions.lock().insert(session_id.clone(), session);

        Ok(StartIdcLoginResponse {
            session_id,
            user_code: device.user_code,
            verification_uri: device.verification_uri,
            verification_uri_complete: device.verification_uri_complete,
            expires_at: expires_at.to_rfc3339(),
            poll_interval,
        })
    }

    /// 轮询一次 IdC 登录状态
    pub async fn poll_idc_login(
        &self,
        session_id: &str,
    ) -> Result<PollIdcLoginResponse, AdminServiceError> {
        let (
            region,
            start_url,
            client_id,
            client_secret,
            device_code,
            _expires_at,
            proxy,
            cred_template,
            relogin_target_id,
        ) = {
            let sessions = self.idc_sessions.lock();
            let s = sessions
                .get(session_id)
                .ok_or_else(|| AdminServiceError::NotFound { id: 0 })?;

            if Utc::now() >= s.expires_at {
                return Ok(PollIdcLoginResponse::Expired);
            }

            (
                s.region.clone(),
                s.start_url.clone(),
                s.client_id.clone(),
                s.client_secret.clone(),
                s.device_code.clone(),
                s.expires_at,
                s.proxy.clone(),
                s.cred_template.clone(),
                s.relogin_target_id,
            )
        };

        let config = self.token_manager.config();

        match idc::poll_token(
            &region,
            &client_id,
            &client_secret,
            &device_code,
            config,
            proxy.as_ref(),
        )
        .await
        {
            idc::PollResult::Pending => Ok(PollIdcLoginResponse::Pending),
            idc::PollResult::Expired => {
                self.idc_sessions.lock().remove(session_id);
                Ok(PollIdcLoginResponse::Expired)
            }
            idc::PollResult::Error(e) => Err(AdminServiceError::InternalError(e.to_string())),
            idc::PollResult::Success(token) => {
                self.idc_sessions.lock().remove(session_id);

                // 重新登录模式：更新已有凭据而非创建新凭据
                if let Some(target_id) = relogin_target_id {
                    let refresh_token = token.refresh_token.ok_or_else(|| {
                        AdminServiceError::InvalidCredential(
                            "IdC 登录未返回 refreshToken，无法更新凭据".to_string(),
                        )
                    })?;
                    let expires_at = token
                        .expires_in
                        .map(|secs| (Utc::now() + Duration::seconds(secs)).to_rfc3339());
                    let provider = idc_provider_for(&start_url).to_string();

                    self.token_manager
                        .replace_idc_relogin_credentials(
                            target_id,
                            IdcReloginCredentials {
                                access_token: token.access_token,
                                refresh_token,
                                expires_at,
                                client_id,
                                client_secret,
                                region,
                                start_url,
                                provider,
                            },
                        )
                        .await
                        .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

                    // 订阅等级/邮箱可能随新身份变化，作废旧缓存待下次查询重建
                    self.invalidate_balance_cache(target_id);
                    tracing::info!("IdC 重新登录成功，凭据 #{} Token 已更新", target_id);
                    return Ok(PollIdcLoginResponse::Success {
                        credential_id: target_id,
                    });
                }

                // 写入凭据
                let mut new_cred = cred_template;
                new_cred.access_token = Some(token.access_token);
                new_cred.refresh_token = token.refresh_token;
                if let Some(secs) = token.expires_in {
                    new_cred.expires_at = Some((Utc::now() + Duration::seconds(secs)).to_rfc3339());
                }

                let credential_id = self
                    .token_manager
                    .add_credential(new_cred)
                    .await
                    .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

                // 主动刷新余额（含订阅等级 / 邮箱）并写入缓存，登录后立即可见
                if let Err(e) = self.get_balance(credential_id).await {
                    tracing::warn!("IdC 登录后刷新余额失败（不影响登录）: {}", e);
                }

                tracing::info!("IdC 设备授权登录成功，已添加凭据 #{}", credential_id);
                Ok(PollIdcLoginResponse::Success { credential_id })
            }
        }
    }

    /// 内部：重新登录完成后更新已有凭据的 Token（禁用→更新→重置→启用）
    fn do_relogin_update(&self, target_id: u64, refresh_token: String) -> anyhow::Result<()> {
        // 先禁用（update_refresh_token 要求凭据处于禁用状态）
        self.token_manager.set_disabled(target_id, true)?;
        // 更新 refreshToken（同时清空 accessToken 和 expiresAt，系统会在下次使用时自动刷新）
        self.token_manager
            .update_refresh_token(target_id, refresh_token, None, None)?;
        // 重置失败计数并重新启用
        self.token_manager.reset_and_enable(target_id)?;
        Ok(())
    }

    /// 发起 Social 重新登录（更新已有凭据的 Token 而非创建新凭据）
    pub async fn start_social_relogin(
        &self,
        target_id: u64,
        req: StartSocialLoginRequest,
    ) -> Result<StartSocialLoginResponse, AdminServiceError> {
        // 验证目标凭据存在
        {
            let snapshot = self.token_manager.snapshot();
            if !snapshot.entries.iter().any(|e| e.id == target_id) {
                return Err(AdminServiceError::NotFound { id: target_id });
            }
        }

        let global_proxy = self.token_manager.proxy();
        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(global_proxy);

        let auth_endpoint = req
            .auth_endpoint
            .unwrap_or_else(|| social::KIRO_AUTH_ENDPOINT.to_string());

        let (code_verifier, code_challenge) = social::generate_pkce();
        let state = uuid::Uuid::new_v4().to_string();

        let (tx, rx) = tokio::sync::oneshot::channel::<social::OAuthCallbackData>();

        let (port, server_handle) = social::start_callback_server(tx)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let redirect_uri = format!("http://127.0.0.1:{}", port);
        let portal_url = social::build_portal_url(&state, &code_challenge, &redirect_uri);

        let expires_at = Utc::now() + Duration::minutes(10);
        let session_id = uuid::Uuid::new_v4().to_string();

        let session = SocialAuthSession {
            auth_endpoint,
            state,
            code_verifier,
            redirect_uri,
            expires_at,
            callback_rx: tokio::sync::Mutex::new(rx),
            cred_template: KiroCredentials::default(),
            proxy,
            _server_handle: server_handle,
            relogin_target_id: Some(target_id),
        };

        self.social_sessions
            .lock()
            .insert(session_id.clone(), session);

        Ok(StartSocialLoginResponse {
            session_id,
            portal_url,
            expires_at: expires_at.to_rfc3339(),
        })
    }

    /// 发起 IdC 重新登录（更新已有凭据的 Token 而非创建新凭据）
    pub async fn start_idc_relogin(
        &self,
        target_id: u64,
        req: StartIdcLoginRequest,
    ) -> Result<StartIdcLoginResponse, AdminServiceError> {
        let existing_credential = self
            .token_manager
            .clone_credential(target_id)
            .ok_or(AdminServiceError::NotFound { id: target_id })?;

        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(self.token_manager.proxy());

        // 未指定时沿用原凭据的接入点，避免重新登录悄悄换到 Builder ID
        let start_url = req
            .start_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .or(existing_credential.start_url.as_deref())
            .unwrap_or(BUILDER_ID_START_URL)
            .to_string();
        let region = if req.region.trim().is_empty() {
            existing_credential
                .auth_region
                .as_deref()
                .or(existing_credential.region.as_deref())
                .unwrap_or(self.token_manager.config().effective_auth_region())
                .to_string()
        } else {
            req.region.trim().to_string()
        };

        self.begin_idc_device_authorization(
            region,
            start_url,
            proxy,
            KiroCredentials::default(),
            Some(target_id),
        )
        .await
    }
}

fn classify_rate_limit(error: &anyhow::Error) -> Option<AdminServiceError> {
    error
        .downcast_ref::<UpstreamRateLimitError>()
        .map(|rate_limit| AdminServiceError::RateLimited {
            retry_after: rate_limit.retry_after().map(str::to_string),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_models_response_preserves_both_token_limits() {
        let upstream: ListAvailableModelsResponse = serde_json::from_value(serde_json::json!({
            "models": [{
                "modelId": "claude-sonnet-4.5",
                "modelName": "Claude Sonnet 4.5",
                "tokenLimits": {
                    "maxInputTokens": 200000,
                    "maxOutputTokens": 64000
                }
            }]
        }))
        .unwrap();

        let response = available_models_response(7, ModelSelectionMode::Specified, upstream);

        assert_eq!(response.id, 7);
        assert_eq!(response.models.len(), 1);
        assert_eq!(response.models[0].max_input_tokens, Some(200000));
        assert_eq!(response.models[0].max_output_tokens, Some(64000));
        assert_eq!(
            serde_json::to_value(&response).unwrap()["selectionMode"],
            "specified"
        );
    }

    #[test]
    fn model_id_validation_trims_and_rejects_invalid_values() {
        assert_eq!(
            validate_model_id("  claude-sonnet-4.5  ").unwrap(),
            "claude-sonnet-4.5"
        );
        assert!(validate_model_id("   ").is_err());
        assert!(validate_model_id("claude\nsonnet").is_err());
        assert!(validate_model_id("auto").is_err());
        assert!(validate_model_id("AUTO").is_err());
        assert!(validate_model_id(&"x".repeat(257)).is_err());
    }

    #[tokio::test]
    async fn balanced_credentials_snapshot_has_no_single_current_credential() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let first = KiroCredentials {
            priority: 0,
            ..KiroCredentials::default()
        };
        let second = KiroCredentials {
            priority: 1,
            ..KiroCredentials::default()
        };
        let manager = Arc::new(
            MultiTokenManager::new(config, vec![first, second], None, None, false).unwrap(),
        );
        let service = AdminService::new(manager, Vec::new());

        let response = service.get_all_credentials();

        assert_eq!(response.current_id, 0);
        assert!(response.credentials.iter().all(|item| !item.is_current));
    }

    #[tokio::test]
    async fn priority_credentials_snapshot_preserves_single_current_credential() {
        let mut config = Config::default();
        config.load_balancing_mode = "priority".to_string();

        let first = KiroCredentials {
            priority: 0,
            ..KiroCredentials::default()
        };
        let second = KiroCredentials {
            priority: 1,
            ..KiroCredentials::default()
        };
        let manager = Arc::new(
            MultiTokenManager::new(config, vec![first, second], None, None, false).unwrap(),
        );
        let service = AdminService::new(manager, Vec::new());

        let response = service.get_all_credentials();

        assert_eq!(response.current_id, 1);
        assert!(response.credentials[0].is_current);
        assert!(!response.credentials[1].is_current);
    }

    #[test]
    fn typed_upstream_rate_limit_is_classified_without_losing_retry_after() {
        let error = anyhow::Error::new(UpstreamRateLimitError::new(Some("120".to_string())));
        let classified = classify_rate_limit(&error).expect("应识别类型化上游 429");

        match classified {
            AdminServiceError::RateLimited { retry_after } => {
                assert_eq!(retry_after.as_deref(), Some("120"));
            }
            other => panic!("预期 RateLimited，实际为 {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_refresh_token_is_classified_as_bad_request() {
        let manager = Arc::new(
            MultiTokenManager::new(Config::default(), Vec::new(), None, None, false).unwrap(),
        );
        let service = AdminService::new(manager, Vec::new());
        let error = anyhow::Error::new(RefreshTokenInvalidError {
            message: "IdC refreshToken 已失效 (invalid_grant)".to_string(),
        });

        let classified = service.classify_balance_error(error, 1);
        assert!(matches!(
            &classified,
            AdminServiceError::InvalidCredential(_)
        ));
        assert_eq!(
            classified.status_code(),
            axum::http::StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn export_uses_nested_account_format() {
        let mut cred = KiroCredentials::default();
        cred.refresh_token = Some("rt-123".to_string());
        cred.client_id = Some("cid".to_string());
        cred.client_secret = Some("csec".to_string());
        cred.auth_method = Some("idc".to_string());
        cred.provider = Some("Enterprise".to_string());
        cred.region = Some("us-east-1".to_string());
        cred.email = Some("e@example.com".to_string());
        cred.expires_at = Some("2026-06-06T00:00:00Z".to_string());
        // 占位符 profileArn 应在导出时被剥离
        cred.profile_arn =
            Some(crate::kiro::model::credentials::BUILDER_ID_PROFILE_ARN.to_string());

        let acc = credential_to_export_account(cred).expect("应生成账号");

        // 嵌套 credentials 结构
        assert_eq!(acc.credentials.refresh_token.as_deref(), Some("rt-123"));
        assert_eq!(acc.credentials.client_id.as_deref(), Some("cid"));
        // authMethod 规范化为 "IdC"
        assert_eq!(acc.credentials.auth_method.as_deref(), Some("IdC"));
        // expiresAt 解析为毫秒时间戳
        assert!(acc.credentials.expires_at > 0);
        // idp 取 provider
        assert_eq!(acc.idp, "Enterprise");
        // 占位符 profileArn 被跳过
        assert_eq!(acc.profile_arn, None);
        // 必填的 csrfToken 输出空串
        assert_eq!(acc.credentials.csrf_token, "");
    }

    #[test]
    fn export_skips_api_key_credentials() {
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_abc".to_string());
        cred.auth_method = Some("api_key".to_string());
        // 无 refreshToken → 跳过
        assert!(credential_to_export_account(cred).is_none());
    }

    #[test]
    fn subscription_type_mapping() {
        assert_eq!(subscription_type_from_title(Some("KIRO FREE")), "Free");
        assert_eq!(subscription_type_from_title(Some("KIRO PRO+")), "Pro_Plus");
        assert_eq!(subscription_type_from_title(Some("KIRO PRO")), "Pro");
        assert_eq!(
            subscription_type_from_title(Some("KIRO POWER")),
            "Enterprise"
        );
        assert_eq!(subscription_type_from_title(None), "Free");
    }
}
