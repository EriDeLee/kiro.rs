//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求
//! 支持多凭据故障转移和重试
//! 支持按凭据级 endpoint 切换不同 Kiro API 端点

use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::admin::trace_db::{TraceAttempt, TraceSink, outcome, truncate_snippet};
use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::error::UpstreamRateLimitError;
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::TlsBackend;
use parking_lot::Mutex;

/// 每个凭据的最大重试次数
const MAX_RETRIES_PER_CREDENTIAL: usize = 3;

/// 总重试次数硬上限（避免无限重试）
///
/// 注：上游 429 多为账号级速率配额（SERVICE_REQUEST_RATE_EXCEEDED），高峰期
/// 多账号同时触顶时，过多重试会在账号间连环撞墙、放大限流。故上限取较小值，
/// 配合 429 专用长退避（见 retry_delay_throttle），被限时尽早返回而非耗尽配额。
const MAX_TOTAL_RETRIES: usize = 4;

/// HTTP Client 缓存容量上限（不含常驻的全局代理 client）。
/// 代理池条目较多时，避免每个不同代理都常驻一个 reqwest::Client 导致内存无界增长。
const CLIENT_CACHE_CAP: usize = 64;

/// 带容量上限的 HTTP Client 缓存。
///
/// - key 为 effective proxy 配置（None = 直连/全局回退）
/// - 受保护 key（全局代理对应的 effective 配置）永不被淘汰
/// - 超出容量时按插入顺序淘汰最旧的「非受保护」条目
struct ClientCache {
    map: HashMap<Option<ProxyConfig>, Client>,
    /// 插入顺序（仅记录可淘汰的非受保护 key）
    order: std::collections::VecDeque<Option<ProxyConfig>>,
    /// 受保护、不参与淘汰的 key（全局代理）
    protected: Option<ProxyConfig>,
    cap: usize,
}

impl ClientCache {
    fn new(protected: Option<ProxyConfig>, initial: Client, cap: usize) -> Self {
        let mut map = HashMap::new();
        map.insert(protected.clone(), initial);
        Self {
            map,
            order: std::collections::VecDeque::new(),
            protected,
            cap,
        }
    }

    fn get(&self, key: &Option<ProxyConfig>) -> Option<Client> {
        self.map.get(key).cloned()
    }

    /// 插入新条目，必要时淘汰最旧的非受保护条目
    fn insert(&mut self, key: Option<ProxyConfig>, client: Client) {
        if key == self.protected || self.map.contains_key(&key) {
            self.map.insert(key, client);
            return;
        }
        while self.order.len() >= self.cap {
            if let Some(evict) = self.order.pop_front() {
                self.map.remove(&evict);
            } else {
                break;
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, client);
    }
}

/// API 调用结果，附带本次实际命中的上游凭据 ID（用于用量统计）
pub struct KiroCallResult {
    pub response: reqwest::Response,
    pub credential_id: u64,
}

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 支持多凭据故障转移和重试机制
/// 按凭据 `endpoint` 字段选择 [`KiroEndpoint`] 实现
pub struct KiroProvider {
    token_manager: Arc<MultiTokenManager>,
    /// Client 缓存：key = effective proxy config, value = reqwest::Client
    /// 不同代理配置的凭据使用不同的 Client，共享相同代理的凭据复用 Client。
    /// 带容量上限淘汰（全局代理 client 常驻），避免代理数量增长导致内存无界增长。
    client_cache: Mutex<ClientCache>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
    /// 端点实现注册表（key: endpoint 名称）
    endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
    /// 默认端点名称（凭据未指定 endpoint 时使用）
    default_endpoint: String,
    /// 已尝试过 profileArn 解析的凭据 ID（进程内）。
    ///
    /// 避免对「无 Enterprise profile」的账号（如纯 BuilderID）在每次请求都重复调用
    /// `ListAvailableProfiles`。命中真实 ARN 的账号会把 ARN 持久化进凭据，之后
    /// 通过 `streaming_profile_arn()` 直接命中，不再进入解析路径。
    profile_resolution_attempted: Mutex<HashSet<u64>>,
}

impl KiroProvider {
    /// 返回共享凭据管理器，供模型发现等只读控制面逻辑复用。
    pub fn token_manager(&self) -> &Arc<MultiTokenManager> {
        &self.token_manager
    }

    /// 创建带代理配置和端点注册表的 KiroProvider 实例
    ///
    /// # Arguments
    /// * `token_manager` - 多凭据 Token 管理器
    /// * `proxy` - 全局代理配置
    /// * `endpoints` - 端点名 → 实现的注册表（至少包含 `default_endpoint` 对应条目）
    /// * `default_endpoint` - 凭据未显式指定 endpoint 时使用的名称
    pub fn with_proxy(
        token_manager: Arc<MultiTokenManager>,
        proxy: Option<ProxyConfig>,
        endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
        default_endpoint: String,
    ) -> Self {
        assert!(
            endpoints.contains_key(&default_endpoint),
            "默认端点 {} 未在 endpoints 注册表中",
            default_endpoint
        );
        let tls_backend = token_manager.config().tls_backend;
        // 预热：构建全局代理对应的 Client（作为受保护的常驻条目）
        let initial_client = build_client(proxy.as_ref(), 720, tls_backend)
            .expect("创建 HTTP 客户端失败");
        let client_cache = ClientCache::new(proxy.clone(), initial_client, CLIENT_CACHE_CAP);

        Self {
            token_manager,
            client_cache: Mutex::new(client_cache),
            tls_backend,
            endpoints,
            default_endpoint,
            profile_resolution_attempted: Mutex::new(HashSet::new()),
        }
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client
    ///
    /// 全局代理**每次现取** `token_manager.proxy()`，不能用构造时的快照 ——
    /// Admin 的「设置全局代理」只更新 token_manager 里那一份，若这里读快照，
    /// 改代理后控制面（token 刷新 / getUsageLimits / ListAvailableModels）走新代理，
    /// 而数据面（`/v1/messages`、`/v1/responses`、MCP）仍走旧代理直到重启，
    /// 界面却提示「已设置」—— 属于静默失效。
    ///
    /// `client_cache` 以 effective proxy 为 key，代理变更后 key 自然不同、
    /// 会新建 client，无需额外失效。
    fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let global_proxy = self.token_manager.proxy();
        let effective = credentials.effective_proxy(global_proxy.as_ref());
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&effective) {
            return Ok(client);
        }
        let client = build_client(effective.as_ref(), 720, self.tls_backend)?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// 根据凭据选择 endpoint 实现
    fn endpoint_for(
        &self,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
        let name = credentials
            .endpoint
            .as_deref()
            .unwrap_or(&self.default_endpoint);
        self.endpoints
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("未知端点: {}", name))
    }

    /// 在发起请求前，确保 Enterprise / IdC 账号的真实 profileArn 已解析并写入 `ctx`。
    ///
    /// 流式端点强制要求 profileArn；Enterprise / IdC 账号必须先把 BuilderID
    /// 占位符解析为真实 ARN，纯 BuilderID 账号则回退占位符。
    /// 仅对「OAuth 凭据 + profileArn 缺失或为占位符」的账号触发一次上游
    /// `ListAvailableProfiles` 查询（进程内去重）：
    /// - 命中真实 ARN → 写回 `ctx.credentials.profile_arn` 并由 token_manager 持久化；
    ///   之后该凭据的 `streaming_profile_arn()` 直接命中，不再进入此路径。
    /// - 无 Enterprise profile（纯 BuilderID 等）→ 保持占位符回退逻辑，并标记已尝试，
    ///   避免每次请求重复查询。
    async fn ensure_profile_arn(
        &self,
        ctx: &mut crate::kiro::token_manager::CallContext,
    ) -> anyhow::Result<()> {
        use crate::kiro::model::credentials::is_placeholder_profile_arn;

        if ctx.credentials.is_api_key_credential() {
            return Ok(());
        }
        let needs = match ctx.credentials.profile_arn.as_deref() {
            None => true,
            Some(arn) => is_placeholder_profile_arn(arn),
        };
        if !needs {
            return Ok(());
        }
        // 进程内去重：仅在「拿到上游确定结果」后才标记已尝试，避免一次网络抖动
        // 把账号永久卡在占位符上（重启前不再重试）。
        if self.profile_resolution_attempted.lock().contains(&ctx.id) {
            return Ok(());
        }
        match self
            .token_manager
            .resolve_profile_arn_for(ctx.id, &ctx.token)
            .await
        {
            Ok(Some(arn)) => {
                ctx.credentials.profile_arn = Some(arn);
                self.profile_resolution_attempted.lock().insert(ctx.id);
            }
            Ok(None) => {
                // 上游确认该账号无 Enterprise profile（纯 BuilderID 等）：标记已尝试，
                // 后续请求回退到占位符逻辑，不再重复查询。
                self.profile_resolution_attempted.lock().insert(ctx.id);
            }
            Err(e) => {
                if is_rate_limit_error(&e) {
                    return Err(e);
                }
                // 网络/瞬态错误：不标记，下次请求再试；本次按原 profileArn 继续
                tracing::warn!("凭据 #{} 解析真实 profileArn 失败（按原 profileArn 继续）: {}", ctx.id, e);
            }
        }
        Ok(())
    }

    /// 发送非流式 API 请求
    ///
    /// 支持多凭据故障转移（见 [`Self::call_api_with_retry`]）。
    /// `sink` 可选，用于逐跳上报链路追踪。
    pub async fn call_api(
        &self,
        request_body: &str,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        self.call_api_with_retry(request_body, false, sink, group).await
    }

    /// 发送流式 API 请求
    pub async fn call_api_stream(
        &self,
        request_body: &str,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        self.call_api_with_retry(request_body, true, sink, group).await
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    pub async fn call_mcp(
        &self,
        request_body: &str,
        group: Option<&str>,
    ) -> anyhow::Result<reqwest::Response> {
        self.call_mcp_with_retry(request_body, group).await
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    async fn call_mcp_with_retry(
        &self,
        request_body: &str,
        group: Option<&str>,
    ) -> anyhow::Result<reqwest::Response> {
        let total_credentials = self.token_manager.total_count_in_group(group).max(1);
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        // 刷新**成功**过的凭据。必须与 `force_refreshed` 分开：后者也包含刷新
        // 失败的凭据（它承担"每凭据只试一次刷新"的职责），用它判终态会把一次
        // OIDC 网络抖动误判成永久失效，而终态是不参与自愈的。
        let mut refreshed_ok: HashSet<u64> = HashSet::new();

        for attempt in 0..max_retries {
            // MCP 调用不涉及模型选择，但必须遵守客户端 Key 的凭据分组隔离。
            let mut ctx = match self.token_manager.acquire_context(None, group).await {
                Ok(c) => c,
                Err(e) => {
                    if is_rate_limit_error(&e) {
                        return Err(e);
                    }
                    if let Some(rate_limit) = take_rate_limit_error(&mut last_error) {
                        return Err(rate_limit);
                    }
                    last_error = Some(e);
                    continue;
                }
            };

            // Pure MCP routes (including Web Search) require the same Enterprise / IdC
            // profileArn resolution as regular model calls.
            self.ensure_profile_arn(&mut ctx).await?;

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    // endpoint 解析失败：记为失败，换下一张凭据
                    self.token_manager
                        .report_failure_for_request(ctx.id, None, group);
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config,
            };

            let url = endpoint.mcp_url(&rctx);
            let body = endpoint.transform_mcp_body(request_body, &rctx);

            let base = self
                .client_for(&ctx.credentials)?
                .post(&url)
                .body(body)
                .header("content-type", endpoint.content_type())
                .header("Connection", "close");
            let request = endpoint.decorate_mcp(base, &rctx);

            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();
            let rate_limit_error = (status.as_u16() == 429)
                .then(|| UpstreamRateLimitError::from_headers(response.headers()));

            // 成功响应
            if status.is_success() {
                self.token_manager.report_success_for_request(ctx.id, None);
                return Ok(response);
            }

            // 失败响应
            let body = response.text().await.unwrap_or_default();

            // 402 额度用尽
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                let has_available = self
                    .token_manager
                    .report_quota_exhausted_for_request(ctx.id, None, group);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 400 Bad Request
            if status.as_u16() == 400 {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 401/403 凭据问题
            if matches!(status.as_u16(), 401 | 403) {
                // 403 + 明确封禁文案：账号被封禁，立即禁用且不参与自愈（受配置开关控制）
                if status.as_u16() == 403
                    && self.token_manager.get_suspended_detection_enabled()
                    && endpoint.is_account_suspended(&body)
                {
                    let has_available = self
                        .token_manager
                        .report_suspended_for_request(ctx.id, None, group);
                    if !has_available {
                        anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                    }
                    last_error = Some(anyhow::anyhow!("MCP 请求失败（账号封禁）: {} {}", status, body));
                    continue;
                }

                // 已成功换过新 token 仍被拒绝：不可自动恢复的终态，理由同流式路径
                if endpoint.is_bearer_token_invalid(&body) && refreshed_ok.contains(&ctx.id) {
                    let has_available = self
                        .token_manager
                        .report_token_rejected_for_request(ctx.id, None, group);
                    if !has_available {
                        anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                    }
                    last_error = Some(anyhow::anyhow!(
                        "MCP 请求失败（token 刷新后仍被拒绝）: {} {}",
                        status,
                        body
                    ));
                    continue;
                }

                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if Self::handle_force_refresh_result(
                        self.token_manager.force_refresh_token_for(ctx.id).await,
                    )? {
                        refreshed_ok.insert(ctx.id);
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self
                    .token_manager
                    .report_failure_for_request(ctx.id, None, group);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 瞬态错误
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_error = if let Some(rate_limit) = rate_limit_error {
                    if !rate_limit.should_retry_locally() {
                        return Err(rate_limit.into());
                    }
                    Some(rate_limit.into())
                } else {
                    Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body))
                };
                if attempt + 1 < max_retries {
                    // 429 限流用更长退避；408/5xx 仍用通用快速退避
                    let delay = if status.as_u16() == 429 {
                        Self::retry_delay_throttle(attempt)
                    } else {
                        Self::retry_delay(attempt)
                    };
                    sleep(delay).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
        }))
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个凭据最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试次数 = min(凭据数量 × 每凭据重试次数, MAX_TOTAL_RETRIES)
    /// - 硬上限 9 次，避免无限重试
    async fn call_api_with_retry(
        &self,
        request_body: &str,
        is_stream: bool,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        // 重试预算按当前请求所属分组的账号数计算，避免小分组按全局账号数获得过多无效重试
        let total_credentials = self.token_manager.total_count_in_group(group).max(1);
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        // 见 MCP 路径同名变量的说明：刷新失败不等于凭据永久失效。
        let mut refreshed_ok: HashSet<u64> = HashSet::new();
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 图片数量上限由上游模型决定，只在上游明确拒绝后才逐张删除最旧图片。
        // 图片重试有请求内图片数这一自然上限，不占用网络/限流的重试预算。
        let mut retryable_request = serde_json::from_str::<KiroRequest>(request_body).ok();
        let image_retry_budget = retryable_request
            .as_ref()
            .map_or(0, |request| request.conversation_state.image_count());
        let mut image_retries = 0usize;
        let mut effective_request_body = request_body.to_string();

        // 尝试从请求体中提取模型信息
        let model = Self::extract_model_from_request(request_body);

        for network_attempt in 0..max_retries.saturating_add(image_retry_budget) {
            let attempt = network_attempt - image_retries;
            if attempt >= max_retries {
                break;
            }
            let attempt_start = Instant::now();
            // 获取调用上下文（绑定 index、credentials、token）
            let mut ctx = match self.token_manager.acquire_context(model.as_deref(), group).await {
                Ok(c) => c,
                Err(e) => {
                    Self::emit_attempt(
                        sink, network_attempt, 0, "", None, outcome::UNKNOWN,
                        Some(&e.to_string()), attempt_start,
                    );
                    if is_rate_limit_error(&e) {
                        return Err(e);
                    }
                    if let Some(rate_limit) = take_rate_limit_error(&mut last_error) {
                        return Err(rate_limit);
                    }
                    last_error = Some(e);
                    continue;
                }
            };

            // 确保 Enterprise / IdC 账号的真实 profileArn 已解析（流式端点强制要求）
            self.ensure_profile_arn(&mut ctx).await?;

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    Self::emit_attempt(
                        sink, network_attempt, ctx.id, "", None, outcome::UNKNOWN,
                        Some(&e.to_string()), attempt_start,
                    );
                    last_error = Some(e);
                    self.token_manager
                        .report_failure_for_request(ctx.id, model.as_deref(), group);
                    continue;
                }
            };
            let endpoint_name = endpoint.name();

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config,
            };

            let url = endpoint.api_url(&rctx);
            let body = endpoint.transform_api_body(&effective_request_body, &rctx);

            tracing::debug!("使用端点 [{}] POST {}", endpoint.name(), url);
            tracing::debug!(
                "实际发送请求体: {}",
                crate::security::body_log_summary(&body)
            );
            // 诊断：历史 reasoningContent 是否真的下发（只报有无与长度，不含内容）。
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                let hist = v.pointer("/conversationState/history").and_then(|h| h.as_array());
                if let Some(hist) = hist {
                    let with_rc = hist
                        .iter()
                        .filter_map(|m| m.pointer("/assistantResponseMessage/reasoningContent"))
                        .map(|rc| {
                            let t = rc.pointer("/reasoningText/text").and_then(|v| v.as_str()).map_or(0, str::len);
                            let sg = rc.pointer("/reasoningText/signature").and_then(|v| v.as_str()).map_or(0, str::len);
                            let rd = rc.get("redactedContent").and_then(|v| v.as_str()).map_or(0, str::len);
                            format!("text={t},sig={sg},redacted={rd}")
                        })
                        .collect::<Vec<_>>();
                    if !with_rc.is_empty() {
                        tracing::debug!(
                            "history reasoningContent x{}: [{}]",
                            with_rc.len(),
                            with_rc.join(" | ")
                        );
                    }
                }
            }

            // 诊断：只打 additionalModelRequestFields（推理档位），不含任何对话内容。
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                tracing::debug!(
                    "additionalModelRequestFields = {}",
                    v.get("additionalModelRequestFields")
                        .map(|f| f.to_string())
                        .unwrap_or_else(|| "<absent>".to_string())
                );
            }

            let base = self
                .client_for(&ctx.credentials)?
                .post(&url)
                .body(body)
                .header("content-type", endpoint.content_type())
                .header("Connection", "close");
            let request = endpoint.decorate_api(base, &rctx);

            // 打印实际发送的请求头（RUST_LOG=debug 时输出，便于排查问题）
            let request = request.build().map_err(|e| anyhow::anyhow!("构建请求失败: {}", e))?;
            if tracing::enabled!(tracing::Level::DEBUG) {
                for (k, v) in request.headers() {
                    // 逐条过脱敏：Authorization / tokentype 等敏感头只留占位，
                    // 否则 RUST_LOG=debug 会把 `Bearer <access_token>` 明文写进日志。
                    tracing::debug!(
                        "  header {}: {}",
                        k,
                        crate::security::redact_header_value(
                            k.as_str(),
                            v.to_str().unwrap_or("<binary>")
                        )
                    );
                }
            }
            let response = match self.client_for(&ctx.credentials)?.execute(request).await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "API 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    Self::emit_attempt(
                        sink, network_attempt, ctx.id, endpoint_name, None,
                        outcome::NETWORK_ERROR, Some(&e.to_string()), attempt_start,
                    );
                    // 网络错误通常是上游/链路瞬态问题，不应导致"禁用凭据"或"切换凭据"
                    // （否则一段时间网络抖动会把所有凭据都误禁用，需要重启才能恢复）
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();
            let rate_limit_error = (status.as_u16() == 429)
                .then(|| UpstreamRateLimitError::from_headers(response.headers()));

            // 成功响应
            if status.is_success() {
                Self::emit_attempt(
                    sink, network_attempt, ctx.id, endpoint_name, Some(status.as_u16()),
                    outcome::SUCCESS, None, attempt_start,
                );
                self.token_manager
                    .report_success_for_request(ctx.id, model.as_deref());
                return Ok(KiroCallResult {
                    response,
                    credential_id: ctx.id,
                });
            }

            // 失败响应：读取 body 用于日志/错误信息
            let body = response.text().await.unwrap_or_default();

            if endpoint.is_image_count_exceeded(&body)
                && let Some(request) = retryable_request.as_mut()
                && request.conversation_state.remove_oldest_image()
            {
                effective_request_body = serde_json::to_string(request)
                    .map_err(|e| anyhow::anyhow!("重建图片超限重试请求失败: {}", e))?;
                image_retries += 1;
                let remaining = request.conversation_state.image_count();
                tracing::warn!(
                    "上游报告图片数超限，已丢弃最早图片并重试（剩余 {} 张）",
                    remaining
                );
                Self::emit_attempt(
                    sink,
                    network_attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::BAD_REQUEST,
                    Some(&body),
                    attempt_start,
                );
                continue;
            }

            // 402 Payment Required 且额度用尽：禁用凭据并故障转移
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                tracing::warn!(
                    "API 请求失败（额度已用尽，禁用凭据并切换，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                Self::emit_attempt(
                    sink, network_attempt, ctx.id, endpoint_name, Some(status.as_u16()),
                    outcome::QUOTA_EXHAUSTED, Some(&body), attempt_start,
                );

                let has_available = self.token_manager.report_quota_exhausted_for_request(
                    ctx.id,
                    model.as_deref(),
                    group,
                );
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 400 + INVALID_MODEL_ID：唯一一种"换个凭据就可能成功"的 400。
            //
            // 上游对「该凭据此刻取不到这个模型」返回 400，与请求格式错误同码不同因。
            // 这是**临时**状态：已观察到订阅确实含该模型的账号也会收到它，数小时后恢复。
            // 所以只做短时冷却 + 换号，绝不把凭据对该模型永久标记为不支持。
            if status.as_u16() == 400 && endpoint.is_model_unavailable(&body) {
                let cooldown_secs = self
                    .token_manager
                    .get_account_throttle_cooldown_secs()
                    .max(1);
                let remaining = self.token_manager.report_model_unavailable_for_request(
                    ctx.id,
                    std::time::Duration::from_secs(cooldown_secs),
                    model.as_deref(),
                    group,
                );
                Self::emit_attempt(
                    sink, network_attempt, ctx.id, endpoint_name, Some(400),
                    outcome::BAD_REQUEST, Some(&body), attempt_start,
                );
                if remaining == 0 {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据当前均取不到该模型）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败（该凭据当前取不到该模型）: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 其余 400 Bad Request - 请求问题，重试/切换凭据无意义
            if status.as_u16() == 400 {
                Self::emit_attempt(
                    sink, network_attempt, ctx.id, endpoint_name, Some(400),
                    outcome::BAD_REQUEST, Some(&body), attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 401/403 - 更可能是凭据/权限问题：计入失败并允许故障转移
            if matches!(status.as_u16(), 401 | 403) {
                // 403 + 明确封禁文案：账号被封禁，立即禁用且不参与自愈（受配置开关控制）
                if status.as_u16() == 403
                    && self.token_manager.get_suspended_detection_enabled()
                    && endpoint.is_account_suspended(&body)
                {
                    tracing::error!(
                        "API 请求失败（账号被封禁，禁用凭据 #{} 并切换，尝试 {}/{}）: {} {}",
                        ctx.id,
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    Self::emit_attempt(
                        sink, network_attempt, ctx.id, endpoint_name, Some(403),
                        outcome::ACCOUNT_SUSPENDED, Some(&body), attempt_start,
                    );

                    let has_available = self.token_manager.report_suspended_for_request(
                        ctx.id,
                        model.as_deref(),
                        group,
                    );
                    if !has_available {
                        anyhow::bail!(
                            "{} API 请求失败（所有凭据已用尽）: {} {}",
                            api_type,
                            status,
                            body
                        );
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（账号封禁）: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    continue;
                }

                tracing::warn!(
                    "API 请求失败（可能为凭据错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                Self::emit_attempt(
                    sink, network_attempt, ctx.id, endpoint_name, Some(status.as_u16()),
                    outcome::AUTH_FAILED, Some(&body), attempt_start,
                );

                // 本次请求已为该凭据强制刷新过一次，拿到新 token 后上游依然拒绝：
                // refresh token 还能换票、但业务端不认这个身份，属不可自动恢复的终态。
                // 若继续走 report_failure，它会以 TooManyFailures 被禁用，而自愈恰好
                // 只复活 TooManyFailures —— 每 5 分钟复活一轮、每轮再白烧一次 OIDC
                // 刷新（实测同一凭据连吃 17 次 403）。故直接打入终态。
                if endpoint.is_bearer_token_invalid(&body) && refreshed_ok.contains(&ctx.id) {
                    let has_available = self.token_manager.report_token_rejected_for_request(
                        ctx.id,
                        model.as_deref(),
                        group,
                    );
                    if !has_available {
                        anyhow::bail!(
                            "{} API 请求失败（所有凭据已用尽）: {} {}",
                            api_type,
                            status,
                            body
                        );
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（token 刷新后仍被拒绝）: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    continue;
                }

                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if Self::handle_force_refresh_result(
                        self.token_manager.force_refresh_token_for(ctx.id).await,
                    )? {
                        refreshed_ok.insert(ctx.id);
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available =
                    self.token_manager
                        .report_failure_for_request(ctx.id, model.as_deref(), group);
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 429 + suspicious activity = 账号级临时风控
            // 仅当前凭据被针对，故障转移到其它凭据可立即恢复（受配置开关控制）。
            if status.as_u16() == 429
                && self.token_manager.get_account_throttle_failover()
                && endpoint.is_account_throttled(&body)
            {
                let cooldown_secs = self
                    .token_manager
                    .get_account_throttle_cooldown_secs()
                    .max(1);
                let cooldown = std::time::Duration::from_secs(cooldown_secs);
                tracing::warn!(
                    "API 请求失败（账号级风控，凭据 #{} 冷却 {}s 并切换，尝试 {}/{}）: {}",
                    ctx.id,
                    cooldown_secs,
                    attempt + 1,
                    max_retries,
                    body
                );

                let remaining = self
                    .token_manager
                    .report_account_throttled_for_request(
                        ctx.id,
                        cooldown,
                        model.as_deref(),
                        group,
                    );
                Self::emit_attempt(
                    sink, network_attempt, ctx.id, endpoint_name, Some(429),
                    outcome::ACCOUNT_THROTTLED, Some(&body), attempt_start,
                );
                // 账号级风控通常不返回 Retry-After；此时使用本地实际冷却时间，
                // 让下游网关在同一时段内也停止调度该虚拟账号。
                let (rate_limit_error, must_wait_for_upstream) =
                    account_rate_limit_with_fallback(rate_limit_error, cooldown_secs);

                // 上游给出明确等待时间时必须立即交给客户端遵守，不能在同一请求中
                // 提前换号重试。无有效 Retry-After 时仍允许按既有策略故障转移。
                if must_wait_for_upstream {
                    return Err(rate_limit_error.into());
                }

                if remaining == 0 {
                    return Err(rate_limit_error.into());
                }
                last_error = Some(rate_limit_error.into());
                continue;
            }

            // 客户端请求格式错误（messages 数组违反协议）：根因在调用方，重试无意义
            // 上游常以 5xx 返回，必须在下方"瞬态错误重试"分支之前拦截，否则会被当作
            // 上游故障重试 max_retries 次，把一个坏请求放大成多次 503（503 风暴）。
            // 直接终止：不重试、不切换凭据、不计入凭据失败。
            if endpoint.is_client_validation_error(&body) {
                tracing::warn!(
                    "API 请求失败（客户端请求格式错误，不重试）: {} {}",
                    status,
                    body
                );
                Self::emit_attempt(
                    sink, network_attempt, ctx.id, endpoint_name, Some(status.as_u16()),
                    outcome::BAD_REQUEST, Some(&body), attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 524 / gateway timeout：上游边缘层超时，继续在本次请求内重试通常只会
            // 放大客户端等待时间和 Claude 端 Retrying 轮数；快速返回，让客户端下一次调用
            // 重新建连。
            if status.as_u16() == 524 || endpoint.is_gateway_timeout(&body) {
                tracing::warn!(
                    "API 请求失败（上游网关超时，不重试）: {} {}",
                    status,
                    body
                );
                Self::emit_attempt(
                    sink,
                    network_attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::TRANSIENT,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 429/408/5xx - 瞬态上游错误：重试但不禁用或切换凭据
            // （避免 429 high traffic / 502 high load 等瞬态错误把所有凭据锁死）
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "API 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                Self::emit_attempt(
                    sink, network_attempt, ctx.id, endpoint_name, Some(status.as_u16()),
                    outcome::TRANSIENT, Some(&body), attempt_start,
                );
                last_error = if let Some(rate_limit) = rate_limit_error {
                    if !rate_limit.should_retry_locally() {
                        return Err(rate_limit.into());
                    }
                    Some(rate_limit.into())
                } else {
                    Some(anyhow::anyhow!(
                        "{} API 请求失败: {} {}",
                        api_type,
                        status,
                        body
                    ))
                };
                if attempt + 1 < max_retries {
                    // 429 限流用更长退避给账号配额恢复时间；408/5xx 仍用通用快速退避
                    let delay = if status.as_u16() == 429 {
                        Self::retry_delay_throttle(attempt)
                    } else {
                        Self::retry_delay(attempt)
                    };
                    sleep(delay).await;
                }
                continue;
            }

            // 其他 4xx - 通常为请求/配置问题：直接返回，不计入凭据失败
            if status.is_client_error() {
                Self::emit_attempt(
                    sink, network_attempt, ctx.id, endpoint_name, Some(status.as_u16()),
                    outcome::BAD_REQUEST, Some(&body), attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 兜底：当作可重试的瞬态错误处理（不切换凭据）
            tracing::warn!(
                "API 请求失败（未知错误，尝试 {}/{}）: {} {}",
                attempt + 1,
                max_retries,
                status,
                body
            );
            Self::emit_attempt(
                sink, network_attempt, ctx.id, endpoint_name, Some(status.as_u16()),
                outcome::UNKNOWN, Some(&body), attempt_start,
            );
            last_error = Some(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            ));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        // 所有重试都失败
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                max_retries
            )
        }))
    }

    /// 向 trace sink 上报一跳结果（sink 为 None 时无开销）
    #[allow(clippy::too_many_arguments)]
    fn emit_attempt(
        sink: Option<&dyn TraceSink>,
        attempt: usize,
        credential_id: u64,
        endpoint: &str,
        http_status: Option<u16>,
        outcome: &str,
        error_body: Option<&str>,
        started: Instant,
    ) {
        let Some(sink) = sink else { return };
        sink.on_attempt(TraceAttempt {
            attempt: attempt as u32,
            credential_id,
            endpoint: endpoint.to_string(),
            http_status,
            outcome: outcome.to_string(),
            error_snippet: error_body.and_then(truncate_snippet),
            duration_ms: started.elapsed().as_millis() as u64,
        });
    }

    /// 从请求体中提取模型信息
    ///
    /// 尝试解析 JSON 请求体，提取 conversationState.currentMessage.userInputMessage.modelId
    fn extract_model_from_request(request_body: &str) -> Option<String> {
        use serde_json::Value;

        let json: Value = serde_json::from_str(request_body).ok()?;

        json.get("conversationState")?
            .get("currentMessage")?
            .get("userInputMessage")?
            .get("modelId")?
            .as_str()
            .map(|s| s.to_string())
    }

    fn retry_delay(attempt: usize) -> Duration {
        // 指数退避 + 少量抖动，避免上游抖动时放大故障
        const BASE_MS: u64 = 200;
        const MAX_MS: u64 = 2_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    /// 429 限流专用退避：比通用退避更长。
    ///
    /// 上游 429（SERVICE_REQUEST_RATE_EXCEEDED）是账号级速率配额耗尽，需要更长
    /// 时间恢复；用通用的 ≤2s 快速退避只会让请求在配额恢复前反复撞墙、持续触顶。
    /// 这里 base 1s、封顶 8s，给账号配额留出恢复窗口。
    fn retry_delay_throttle(attempt: usize) -> Duration {
        const BASE_MS: u64 = 1_000;
        const MAX_MS: u64 = 8_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    /// 返回是否刷新成功；类型化刷新 429 原样传播，其他刷新失败交回调用方按认证失败处理。
    fn handle_force_refresh_result(result: anyhow::Result<()>) -> anyhow::Result<bool> {
        match result {
            Ok(()) => Ok(true),
            Err(error) if is_rate_limit_error(&error) => Err(error),
            Err(_) => Ok(false),
        }
    }
}

fn is_rate_limit_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<UpstreamRateLimitError>().is_some()
}

fn take_rate_limit_error(last_error: &mut Option<anyhow::Error>) -> Option<anyhow::Error> {
    if last_error
        .as_ref()
        .is_some_and(|error| error.downcast_ref::<UpstreamRateLimitError>().is_some())
    {
        last_error.take()
    } else {
        None
    }
}

/// 为账号风控 429 补齐本地冷却时间，并区分上游是否明确要求等待。
fn account_rate_limit_with_fallback(
    rate_limit: Option<UpstreamRateLimitError>,
    cooldown_secs: u64,
) -> (UpstreamRateLimitError, bool) {
    let must_wait_for_upstream = rate_limit
        .as_ref()
        .is_some_and(|error| !error.should_retry_locally());
    let error = match rate_limit {
        Some(error) if error.retry_after().is_some() => error,
        _ => UpstreamRateLimitError::new(Some(cooldown_secs.to_string())),
    };
    (error, must_wait_for_upstream)
}

#[cfg(test)]
mod rate_limit_tests {
    use super::*;

    #[test]
    fn preserves_typed_rate_limit_when_later_credential_selection_fails() {
        let mut last_error = Some(anyhow::Error::new(UpstreamRateLimitError::new(Some(
            "45".to_string(),
        ))));

        // A concurrent request may cool the final credential before the next acquisition.
        // The earlier upstream 429 must win over the later generic selection failure.
        let returned = take_rate_limit_error(&mut last_error)
            .unwrap_or_else(|| anyhow::anyhow!("所有凭据均已禁用"));

        let rate_limit = returned
            .downcast_ref::<UpstreamRateLimitError>()
            .expect("应保留最初的类型化 429");
        assert_eq!(rate_limit.retry_after(), Some("45"));
        assert!(last_error.is_none());
    }

    #[test]
    fn does_not_relabel_generic_error_as_rate_limit() {
        let mut last_error = Some(anyhow::anyhow!("所有凭据均已禁用"));
        assert!(take_rate_limit_error(&mut last_error).is_none());
        assert!(last_error.is_some());
    }

    #[test]
    fn account_rate_limit_uses_cooldown_when_retry_after_is_missing() {
        let (error, must_wait) = account_rate_limit_with_fallback(
            Some(UpstreamRateLimitError::new(None)),
            300,
        );

        assert_eq!(error.retry_after(), Some("300"));
        assert!(!must_wait, "无上游等待值时仍可按账号冷却策略故障转移");
    }

    #[test]
    fn account_rate_limit_honors_explicit_upstream_retry_after() {
        let (error, must_wait) = account_rate_limit_with_fallback(
            Some(UpstreamRateLimitError::new(Some("90".to_string()))),
            300,
        );

        assert_eq!(error.retry_after(), Some("90"));
        assert!(must_wait, "上游明确要求等待时不得在内部提前重试");
    }

    #[test]
    fn force_refresh_rate_limit_is_propagated_instead_of_counted_as_auth_failure() {
        let error = anyhow::Error::new(UpstreamRateLimitError::new(Some("60".to_string())));
        let returned = KiroProvider::handle_force_refresh_result(Err(error))
            .expect_err("强制刷新 429 应立即传播");

        let rate_limit = returned
            .downcast_ref::<UpstreamRateLimitError>()
            .expect("应保留类型化 429");
        assert_eq!(rate_limit.retry_after(), Some("60"));
    }

    #[test]
    fn generic_force_refresh_failure_remains_an_auth_failure() {
        let outcome = KiroProvider::handle_force_refresh_result(Err(anyhow::anyhow!(
            "invalid refresh token",
        )))
        .unwrap();
        assert!(!outcome);
    }

    #[test]
    fn current_acquire_rate_limit_is_detected_before_outer_retry() {
        let error = anyhow::Error::new(UpstreamRateLimitError::new(Some("30".to_string())));
        assert!(is_rate_limit_error(&error));
    }
}

#[cfg(test)]
mod image_retry_tests {
    use super::*;
    use axum::{
        Json, Router,
        body::Bytes,
        extract::State,
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::post,
    };
    use serde_json::{Value, json};

    use crate::kiro::model::requests::{
        conversation::{
            ConversationState, CurrentMessage, HistoryUserMessage, KiroImage, Message,
            UserInputMessage, UserMessage,
        },
        kiro::KiroRequest,
    };

    struct MockEndpoint {
        url: String,
    }

    impl KiroEndpoint for MockEndpoint {
        fn name(&self) -> &'static str {
            "image-retry-test"
        }

        fn api_url(&self, _ctx: &RequestContext<'_>) -> String {
            self.url.clone()
        }

        fn mcp_url(&self, _ctx: &RequestContext<'_>) -> String {
            self.url.clone()
        }

        fn decorate_api(
            &self,
            request: reqwest::RequestBuilder,
            _ctx: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            request
        }

        fn decorate_mcp(
            &self,
            request: reqwest::RequestBuilder,
            _ctx: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            request
        }

        fn transform_api_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
            body.to_string()
        }
    }

    async fn image_limit_once(
        State(requests): State<Arc<Mutex<Vec<Value>>>>,
        body: Bytes,
    ) -> Response {
        let request = serde_json::from_slice(&body).expect("测试请求应为 JSON");
        let mut requests = requests.lock();
        requests.push(request);
        if requests.len() == 1 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "message": "too many inline media segments: 2 exceeds limit 1",
                    "reason": "IMAGE_COUNT_EXCEEDED"
                })),
            )
                .into_response();
        }
        StatusCode::OK.into_response()
    }

    #[tokio::test]
    async fn upstream_image_limit_drops_oldest_image_and_retries() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("kiro_rs=warn")
            .with_test_writer()
            .try_init();

        let encoded = "aW1hZ2U=".to_string();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/generateAssistantResponse", post(image_limit_once))
            .with_state(requests.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut config = crate::model::config::Config::default();
        config.default_endpoint = "image-retry-test".to_string();
        let credential = KiroCredentials {
            id: Some(1),
            auth_method: Some("api_key".to_string()),
            kiro_api_key: Some("test-key".to_string()),
            machine_id: Some("00000000-0000-4000-8000-000000000000".to_string()),
            endpoint: Some("image-retry-test".to_string()),
            ..KiroCredentials::default()
        };
        let token_manager = Arc::new(
            MultiTokenManager::new(config, vec![credential], None, None, true).unwrap(),
        );
        let endpoint: Arc<dyn KiroEndpoint> = Arc::new(MockEndpoint {
            url: format!("http://{address}/generateAssistantResponse"),
        });
        let provider = KiroProvider::with_proxy(
            token_manager,
            None,
            HashMap::from([("image-retry-test".to_string(), endpoint)]),
            "image-retry-test".to_string(),
        );

        let history = vec![Message::User(HistoryUserMessage {
            user_input_message: UserMessage::new("old", "claude-opus-5")
                .with_images(vec![KiroImage::from_base64("jpeg", encoded.clone())]),
        })];
        let request = KiroRequest {
            conversation_state: ConversationState::new("image-retry-test")
                .with_history(history)
                .with_current_message(CurrentMessage::new(
                    UserInputMessage::new("current", "claude-opus-5")
                        .with_images(vec![KiroImage::from_base64("png", encoded)]),
                )),
            profile_arn: None,
            additional_model_request_fields: None,
        };
        let body = serde_json::to_string(&request).unwrap();

        let result = provider.call_api(&body, None, None).await.unwrap();
        assert_eq!(result.response.status(), StatusCode::OK);
        server.abort();

        let requests = requests.lock();
        assert_eq!(requests.len(), 2, "应在图片超限后仅重试一次");
        assert_eq!(
            requests[0]["conversationState"]["history"][0]["userInputMessage"]["images"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            requests[0]["conversationState"]["currentMessage"]["userInputMessage"]["images"]
                [0]["format"],
            "png"
        );
        assert!(
            requests[1]["conversationState"]["history"][0]["userInputMessage"]
                .get("images")
                .is_none(),
            "第二次请求应丢弃历史中的最早图片"
        );
        assert_eq!(
            requests[1]["conversationState"]["currentMessage"]["userInputMessage"]["images"]
                [0]["format"],
            "png",
            "当前消息中的较新图片应保留"
        );
    }
}
