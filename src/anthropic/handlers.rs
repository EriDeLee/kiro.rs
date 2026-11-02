//! Anthropic API Handler 函数

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::trace_db::{
    SharedTraceStore, TraceAttempt, TraceKeySource, TraceRecord, TraceSink, outcome,
};
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder, UsageRecord};
use crate::kiro::model::available_models::{TokenLimits, UpstreamModel};
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::token_manager::ModelDiscoveryError;
use crate::model::allowlist::ApiEndpoint;
use crate::token;
use anyhow::Error;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::{Extension, State},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use chrono::Utc;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::converter::{ConversionError, convert_request};
use super::middleware::{AppState, KeyContext};
use super::stream::{SseEvent, StreamContext};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    Thinking,
};
use super::websearch;

/// 请求结束时记录用量的钩子
///
/// 在 handler 入口构造，调用 [`Self::record`] 时把当次请求的 input/output token、
/// 命中的上游凭据 ID、状态写入：
/// - `usage_log.YYYY-MM-DD.jsonl`（持久化历史）
/// - 内存聚合器（仪表盘趋势）
/// - 客户端 Key 计数（按 Key 累计）
#[derive(Clone)]
pub(crate) struct UsageRecordHook {
    pub recorder: Option<SharedRecorder>,
    pub aggregator: Option<SharedAggregator>,
    pub client_keys: Option<SharedClientKeyManager>,
    pub key_id: u64,
    pub model: String,
    pub started_at: Instant,
}

impl UsageRecordHook {
    pub fn from_state(state: &AppState, key_id: u64, model: String) -> Self {
        Self {
            recorder: state.usage_recorder.clone(),
            aggregator: state.usage_aggregator.clone(),
            client_keys: state.client_keys.clone(),
            key_id,
            model,
            started_at: Instant::now(),
        }
    }

    pub fn record(
        &self,
        credential_id: u64,
        input_tokens: i32,
        output_tokens: i32,
        credits: f64,
        status: &str,
    ) {
        let rec = UsageRecord {
            ts: Utc::now().to_rfc3339(),
            key_id: self.key_id,
            credential_id,
            model: self.model.clone(),
            input_tokens: input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            credits: if credits.is_finite() && credits > 0.0 {
                credits
            } else {
                0.0
            },
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            status: status.to_string(),
        };
        if let Some(r) = &self.recorder {
            r.record(&rec);
        }
        if let Some(a) = &self.aggregator {
            a.ingest(&rec);
        }
        if status == "success" && self.key_id != 0 {
            if let Some(m) = &self.client_keys {
                m.record_usage(self.key_id, rec.input_tokens, rec.output_tokens);
            }
        }
    }
}

/// 单次请求的链路追踪器
///
/// 在 handler 入口构造，作为 [`TraceSink`] 传入 provider；provider 在重试循环里
/// 每跳调用 [`on_attempt`](TraceSink::on_attempt) 累积一条 [`TraceAttempt`]。
/// 请求结束时调用 [`Self::finalize`] 组装 [`TraceRecord`] 并写入 SQLite。
///
/// `store` 为 None（未启用 Admin / trace）时所有方法都是空操作，零开销。
pub(crate) struct RequestTracer {
    store: Option<SharedTraceStore>,
    trace_id: String,
    ts: String,
    key_id: u64,
    key_source: TraceKeySource,
    api_endpoint: String,
    model: String,
    is_stream: bool,
    started_at: Instant,
    /// 首个上游 chunk 到达时刻（仅流式标记；取第一次）
    first_token_at: parking_lot::Mutex<Option<Instant>>,
    attempts: parking_lot::Mutex<Vec<TraceAttempt>>,
    /// 是否已落过库。`finalize` 靠它保证只生效一次，`Drop` 兜底才不会和显式收尾双写。
    finalized: AtomicBool,
    /// 上游响应头是否已到达。取消发生在头到达之前还是之后，决定 Drop 兜底记哪种分类。
    upstream_headers_received: AtomicBool,
}

/// 本次请求的用量快照（落入 trace 行，与 usage_log 同源）
#[derive(Clone, Copy, Default)]
pub(crate) struct TraceUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub credits: f64,
}

impl TraceUsage {
    /// 错误早退等无用量场景
    pub fn zero() -> Self {
        Self::default()
    }
}

pub(super) struct RequestTraceOptions {
    pub key_ctx: KeyContext,
    pub api_endpoint: ApiEndpoint,
    pub model: String,
    pub is_stream: bool,
}

impl RequestTracer {
    pub(super) fn new(state: &AppState, options: RequestTraceOptions) -> Self {
        Self {
            store: state.trace_store.clone(),
            trace_id: Uuid::new_v4().to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: options.key_ctx.key_id,
            key_source: options.key_ctx.key_source,
            api_endpoint: options.api_endpoint.as_str().to_string(),
            model: options.model,
            is_stream: options.is_stream,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            attempts: parking_lot::Mutex::new(Vec::new()),
            finalized: AtomicBool::new(false),
            upstream_headers_received: AtomicBool::new(false),
        }
    }

    /// 标记上游响应头已到达（`call_api*` 返回成功之后）。
    pub fn mark_upstream_headers_received(&self) {
        self.upstream_headers_received.store(true, Ordering::SeqCst);
    }

    /// 标记首个上游 chunk 到达（幂等，仅记录第一次）
    pub fn mark_first_token(&self) {
        let mut slot = self.first_token_at.lock();
        if slot.is_none() {
            *slot = Some(Instant::now());
        }
    }

    /// 组装并落库一条完整链路。store 为 None 时不做任何事。
    pub fn finalize(
        &self,
        final_status: &str,
        error_type: Option<&str>,
        error_message: Option<&str>,
        interrupted_after_bytes: Option<u64>,
        usage: TraceUsage,
    ) {
        // 幂等：显式收尾与 Drop 兜底可能先后到达，只有第一次生效。
        if self.finalized.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(store) = &self.store else { return };
        let mut attempts = std::mem::take(&mut *self.attempts.lock());
        // 按时间顺序统一重排 attempt 编号。它是 `trace_attempts` 主键
        // `(trace_id, attempt)` 的一半，而 provider 的编号在**每次** `call_api*`
        // 调用时从 1 重新开始。web_search loop 的一条 trace 会跨多轮调用 provider，
        // 沿用原编号时第二轮的 attempt=1 会被 `INSERT OR REPLACE` 顶掉第一轮的同名
        // 行：attempts 表少行，而 traces.total_attempts 仍按内存里的条数上报，两者
        // 对不上。单轮请求（provider 本就给出 1..n）重排后编号与原来完全一致。
        for (idx, attempt) in attempts.iter_mut().enumerate() {
            attempt.attempt = (idx + 1) as u32;
        }
        // 最终凭据：最后一跳的命中凭据（成功跳即命中凭据，失败跳即最后尝试的凭据）
        let final_credential_id = attempts.last().map(|a| a.credential_id).unwrap_or(0);
        let first_token_ms = self
            .first_token_at
            .lock()
            .map(|t| t.duration_since(self.started_at).as_millis() as u64);
        let rec = TraceRecord {
            trace_id: self.trace_id.clone(),
            ts: self.ts.clone(),
            key_id: self.key_id,
            key_source: self.key_source,
            api_endpoint: self.api_endpoint.clone(),
            model: self.model.clone(),
            is_stream: self.is_stream,
            final_status: final_status.to_string(),
            final_credential_id,
            error_type: error_type.map(|s| s.to_string()),
            error_message: error_message.map(|s| s.to_string()),
            total_attempts: attempts.len() as u32,
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            interrupted_after_bytes,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            credits: usage.credits,
            first_token_ms,
            attempts,
        };
        store.insert(&rec);
    }
}

/// 请求没有任何显式收尾就被丢弃（客户端取消、前置网关等头超时断开）时，补一行
/// `interrupted` 记录。这类请求上游可能已在生成并计费，trace 里却此前毫无痕迹。
///
/// 取消原因在这一层长得完全一样（用户 ESC、客户端读超时、进程退出），分类只区分
/// 「响应头到达之前」与「之后」；已正常收尾的请求 `finalized` 已置位，此处是空操作。
impl Drop for RequestTracer {
    fn drop(&mut self) {
        let (error_type, message) = if self.upstream_headers_received.load(Ordering::SeqCst) {
            (
                outcome::STREAM_INTERRUPTED,
                "request was cancelled after upstream response headers arrived",
            )
        } else {
            (
                outcome::ABANDONED_BEFORE_HEADERS,
                "request was cancelled before upstream response headers arrived",
            )
        };
        self.finalize(
            "interrupted",
            Some(error_type),
            Some(message),
            None,
            TraceUsage::zero(),
        );
    }
}

impl TraceSink for RequestTracer {
    fn on_attempt(&self, attempt: TraceAttempt) {
        self.attempts.lock().push(attempt);
    }
}

/// 取追踪器里最后一跳的 outcome（用于把 provider 的失败分类提升到 record.error_type）。
/// 返回 'static str（outcome 常量），无 attempt 时返回 None。
pub(super) fn last_attempt_outcome(tracer: &RequestTracer) -> Option<&'static str> {
    let last = tracer.attempts.lock().last()?.outcome.clone();
    Some(canonical_attempt_outcome(&last))
}

fn canonical_attempt_outcome(value: &str) -> &'static str {
    match value {
        outcome::QUOTA_EXHAUSTED => outcome::QUOTA_EXHAUSTED,
        outcome::ACCOUNT_THROTTLED => outcome::ACCOUNT_THROTTLED,
        outcome::ACCOUNT_SUSPENDED => outcome::ACCOUNT_SUSPENDED,
        outcome::AUTH_FAILED => outcome::AUTH_FAILED,
        outcome::TRANSIENT => outcome::TRANSIENT,
        outcome::NETWORK_ERROR => outcome::NETWORK_ERROR,
        outcome::BAD_REQUEST => outcome::BAD_REQUEST,
        _ => outcome::UNKNOWN,
    }
}

/// Image-budget warning threshold (in raw base64 chars, not decoded bytes).
/// Emits a warning when the total base64 char count of all image content in one request exceeds this threshold.
/// The threshold does not reject the request (the upstream makes the final call); it only gives operators more precise diagnostics.
const IMAGE_BUDGET_WARN_BYTES: usize = 800 * 1024;

/// Budget statistics for the image content in one inbound request.
struct ImageBudget {
    count: usize,
    total_b64_bytes: usize,
    largest_b64_bytes: usize,
}

/// Counts the total number of images in the payload and their base64 byte size.
/// Looks only at inline base64 (image source.type == "base64"), skipping url-mode images (which do not
/// go directly into a Bedrock single message body). This is a lightweight O(N) scan that does not decode base64.
fn count_image_budget(payload: &super::types::MessagesRequest) -> ImageBudget {
    let mut count = 0usize;
    let mut total = 0usize;
    let mut largest = 0usize;
    for msg in &payload.messages {
        if let serde_json::Value::Array(arr) = &msg.content {
            for item in arr {
                if item.get("type").and_then(|v| v.as_str()) != Some("image") {
                    continue;
                }
                let Some(src) = item.get("source") else {
                    continue;
                };
                if src.get("type").and_then(|v| v.as_str()) != Some("base64") {
                    continue;
                }
                let n = src
                    .get("data")
                    .and_then(|v| v.as_str())
                    .map(|s| s.len())
                    .unwrap_or(0);
                count += 1;
                total += n;
                if n > largest {
                    largest = n;
                }
            }
        }
    }
    ImageBudget {
        count,
        total_b64_bytes: total,
        largest_b64_bytes: largest,
    }
}

/// 将 KiroProvider 错误映射为 HTTP 响应
pub(super) fn map_provider_error(err: Error) -> Response {
    if let Some(rate_limit) = err.downcast_ref::<crate::kiro::error::UpstreamRateLimitError>() {
        tracing::warn!(error = %err, "上游限流（映射为 429）");
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse::new(
                "rate_limit_error",
                "Upstream rate limit exceeded. Retry later.",
            )),
        )
            .into_response();
        if let Some(value) = rate_limit
            .retry_after()
            .and_then(|value| value.parse::<header::HeaderValue>().ok())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }

    let err_str = err.to_string();

    // 上下文窗口满了（对话历史累积超出模型上下文窗口限制）
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Context window is full. Reduce conversation history, system prompt, or tools.",
            )),
        )
            .into_response();
    }

    // 单次输入太长（请求体本身超出上游限制）
    if err_str.contains("Input is too long") {
        tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Input is too long. Reduce the size of your messages.",
            )),
        )
            .into_response();
    }

    // Bedrock client-side validation errors (tool_use <-> tool_result mismatch, invalid message sequence, etc.)
    // The root cause is the client's own messages array, not an upstream failure, so it must not map to 5xx
    // otherwise it triggers an upstream cooldown that amplifies one client error into a 30+ burst of 503s.
    // Detection is centralized in the endpoint layer (single source of truth for the markers); the provider
    // already bails out without retry on these, and this mapping is the client-facing safety net.
    if crate::kiro::endpoint::default_is_client_validation_error(&err_str) {
        tracing::warn!(
            error = %err,
            "client messages array violates the protocol (Bedrock validation; mapped to 400 to avoid a false cooldown)"
        );
        // Return a stable, client-facing message and avoid echoing the raw upstream
        // error string (which can carry request IDs or internal validation details).
        // The full error is already logged above for diagnostics.
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Invalid message sequence: tool_use and tool_result blocks must be correctly paired and ordered.".to_string(),
            )),
        )
            .into_response();
    }

    tracing::error!("Kiro API 调用失败: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            "Upstream API request failed.",
        )),
    )
        .into_response()
}

fn validate_max_tokens(max_tokens: i32) -> Result<(), ErrorResponse> {
    if max_tokens <= 0 {
        Err(ErrorResponse::new(
            "invalid_request_error",
            "max_tokens must be greater than 0",
        ))
    } else {
        Ok(())
    }
}

fn merge_token_limits(target: &mut Option<TokenLimits>, incoming: Option<TokenLimits>) {
    let Some(incoming) = incoming else {
        return;
    };
    match target {
        Some(target) => {
            target.max_input_tokens = target.max_input_tokens.max(incoming.max_input_tokens);
            target.max_output_tokens = target.max_output_tokens.max(incoming.max_output_tokens);
        }
        None => *target = Some(incoming),
    }
}

fn infer_model_owner(model_id: &str) -> &'static str {
    let id = model_id.to_ascii_lowercase();
    if id.starts_with("claude-") {
        "anthropic"
    } else if id.starts_with("gpt-")
        || id.starts_with("chatgpt-")
        || id.starts_with("o1-")
        || id.starts_with("o3-")
        || id.starts_with("o4-")
    {
        "openai"
    } else {
        "kiro"
    }
}

fn model_from_upstream(upstream: UpstreamModel) -> Model {
    let max_tokens = upstream
        .token_limits
        .as_ref()
        .and_then(|limits| limits.max_output_tokens)
        .and_then(|limit| i32::try_from(limit).ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(64_000);
    // 上游 `tokenLimits` 是运行时数据：缺字段或非正数时才回退到按族推算的窗口，
    // 不能反过来用常量覆盖上游返回值。
    let context_window = upstream
        .token_limits
        .as_ref()
        .and_then(|limits| limits.max_input_tokens)
        .and_then(|limit| i32::try_from(limit).ok())
        .filter(|limit| *limit > 0)
        .unwrap_or_else(|| super::converter::get_context_window_size(&upstream.model_id));
    Model {
        display_name: upstream
            .model_name
            .clone()
            .unwrap_or_else(|| upstream.model_id.clone()),
        owned_by: infer_model_owner(&upstream.model_id).to_string(),
        id: upstream.model_id,
        object: "model".to_string(),
        created: 0,
        model_type: "chat".to_string(),
        max_tokens,
        context_window,
    }
}

fn aggregate_available_models_inner(upstream_models: Vec<UpstreamModel>) -> Vec<Model> {
    let mut merged_upstream: BTreeMap<String, UpstreamModel> = BTreeMap::new();
    for incoming in upstream_models {
        match merged_upstream.get_mut(&incoming.model_id) {
            Some(existing) => {
                if existing.model_name.is_none() {
                    existing.model_name = incoming.model_name;
                }
                if existing.description.is_none() {
                    existing.description = incoming.description;
                }
                merge_token_limits(&mut existing.token_limits, incoming.token_limits);
            }
            None => {
                merged_upstream.insert(incoming.model_id.clone(), incoming);
            }
        }
    }

    let mut models: BTreeMap<String, Model> = BTreeMap::new();
    for upstream in merged_upstream.into_values() {
        let model = model_from_upstream(upstream);
        models.insert(model.id.clone(), model);
    }

    // 自定义别名最后写入，同名时其展示元数据优先于动态条目。

    models.into_values().collect()
}

/// 汇总可用模型清单。
///
/// 只保留白名单内的模型：Claude 组（`claude-opus-5.5`、`claude-sonnet-5.5`）与 GPT 组
///（`gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna`）。全部模型可走 `/v1/messages`，
/// GPT 组也可走 `/v1/responses`；其余一律在请求阶段 400。若这里仍列出白名单外
/// 的模型，客户端会看到一个「能选但一用就报错」
/// 的假选项 —— 配置与实际行为不一致，是最容易自己踩的坑。
fn aggregate_available_models(upstream_models: Vec<UpstreamModel>) -> Vec<Model> {
    aggregate_available_models_inner(upstream_models)
        .into_iter()
        .filter(|m| crate::model::allowlist::normalize(&m.id).is_some())
        .collect()
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
) -> Response {
    tracing::info!("Received GET /v1/models request");

    let Some(provider) = &state.kiro_provider else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "service_unavailable",
                "Kiro API provider not configured",
            )),
        )
            .into_response();
    };

    let upstream = match provider
        .token_manager()
        .discover_models_for_group(key_ctx.group.as_deref())
        .await
    {
        Ok(models) => models,
        Err(ModelDiscoveryError::NoAvailableCredentials) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "No available credentials for this API key",
                )),
            )
                .into_response();
        }
        Err(error @ ModelDiscoveryError::ColdStartFailed { .. }) => {
            tracing::warn!("动态模型列表加载失败: {}", error);
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    "Unable to load available models from upstream",
                )),
            )
                .into_response();
        }
    };

    let models = aggregate_available_models(upstream);

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
    .into_response()
}

/// 共享执行上下文。外部 `/v1/messages` 请求没有该 Extension，使用 Messages 默认值；
/// `/v1/responses` 翻译后直接调用本函数并显式保留原始入口和外部 stream 标记。
///
/// 客户端无法伪造它：axum 的 Extension 只来自路由层显式 `.layer()` 或调用方手动
/// 传入，HTTP 头与请求体都进不了 request extensions。这一点现在比以前更吃重 ——
/// `endpoint` 直接决定放行哪些模型（见下面的 `allowlist::resolve` 调用）。
/// 退一步说，即便能伪造也只会收窄白名单（Responses 的允许集是 Messages 的子集），
/// 不构成放宽。
#[derive(Clone, Copy)]
pub(super) struct ForwardContext {
    pub endpoint: ApiEndpoint,
    pub external_stream: bool,
}

/// POST /v1/messages，以及 `/v1/responses` 翻译后的共享执行链路。
pub async fn post_messages(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    forward_context: Option<Extension<ForwardContext>>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    let context = forward_context.map(|v| v.0).unwrap_or(ForwardContext {
        endpoint: ApiEndpoint::Messages,
        external_stream: payload.stream,
    });
    // 模型名首尾空白不改变语义（白名单匹配本就 trim 后比较），但用量统计、trace 与
    // 日志按原始字符串分组时，同一模型会裂成「claude-sonnet-5.5」与「claude-sonnet-5.5 」
    // 两行。入站统一去掉首尾空白；-thinking / -latest 等有语义的后缀原样保留。
    payload.model = payload.model.trim().to_string();
    // Count the image budget on inbound to provide precise diagnostics for later context-window-full errors
    let img_stats = count_image_budget(&payload);
    tracing::info!(
        api_endpoint = %context.endpoint.as_str(),
        api_path = %context.endpoint.path(),
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %context.external_stream,
        pipeline_stream = %payload.stream,
        message_count = %payload.messages.len(),
        image_count = %img_stats.count,
        image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
        image_largest_b64_kb = %(img_stats.largest_b64_bytes / 1024),
        "entering shared model execution pipeline"
    );
    if let Err(error) = validate_max_tokens(payload.max_tokens) {
        return (StatusCode::BAD_REQUEST, Json(error)).into_response();
    }
    // Messages 接受全部白名单模型；Responses 只接受 GPT。上游字段路径由 converter
    // 按模型族决定，不由入口决定。
    if let Err(rejected) = crate::model::allowlist::resolve(&payload.model, context.endpoint) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                rejected.message(&payload.model),
            )),
        )
            .into_response();
    }
    if img_stats.total_b64_bytes > IMAGE_BUDGET_WARN_BYTES {
        tracing::warn!(
            image_count = %img_stats.count,
            image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
            "incoming image payload is large; if upstream rejects with CONTENT_LENGTH_EXCEEDS_THRESHOLD, reduce image count or use lower-resolution screenshots"
        );
    }
    let hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0.0, "error");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 注意：这里曾有一条「纯 WebSearch 快速路径」——当 tools 恰好只有一个原生
    // web_search 时，直接由 `handle_websearch_request` 代答。它已删除，原因：
    //
    // 1. 它用 `extract_search_query` **从消息文本里猜搜索词**，模型完全不参与
    //    决策 —— 替客户端做了它没要求的决定；
    // 2. 「按工具数量分流」逼得 `/v1/responses` 侧为了逃离这条路径而向上游注入
    //    一个名为 `noop` 的假工具（只为把 len() 顶到 2）。判断条件修正后，
    //    那个假工具也随之删除。
    //
    // 现在只要 tools 里含原生 web_search 就统一走下面的 agentic loop：由模型
    // 自己决定何时搜、搜什么，代理只负责执行搜索并把结果回喂。

    let payload_stream = payload.stream;
    // 链路追踪器在**所有**分支之前构造：websearch loop 同样要落 trace 行。
    // 它此前只在下面两条 stream/non-stream 分支里各建一次，于是走 loop 的请求
    // 一条 trace 都没有 —— 而 `/v1/responses` 恒注入原生 web_search，等于整个
    // GPT 组在 Admin 日志页彻底隐身（usage 统计有、链路日志无）。
    let tracer = std::sync::Arc::new(RequestTracer::new(
        &state,
        RequestTraceOptions {
            key_ctx: key_ctx.clone(),
            api_endpoint: context.endpoint,
            model: payload.model.clone(),
            is_stream: context.external_stream,
        },
    ));

    // Mixed-tools (web_search + exec...) case: web_search coexists with other tools and falls onto the normal chat path,
    // where the upstream may return a tool_use with name=web_search. Take the internal agentic loop: search internally and feed the results back.
    if websearch::has_web_search_among_tools(&payload) {
        tracing::info!(
            "detected mixed tools containing web_search, entering the web_search agentic loop"
        );
        return super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            payload_stream,
            key_ctx.group.clone(),
            tracer,
        )
        .await;
    }

    // 转换请求
    let conversion_result = match convert_request(&payload) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::InvalidModel(reason) => {
                    ("invalid_request_error", format!("无效模型 ID: {}", reason))
                }
                // 请求违反严格约束（effort 档位 / prefill）：原文即为可操作说明。
                ConversionError::UnsupportedRequest(reason) => {
                    ("invalid_request_error", reason.clone())
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            hook.record(0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                Some(outcome::BAD_REQUEST),
                Some(&message),
                None,
                TraceUsage::zero(),
            );
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // Build the Kiro request. profile_arn is injected by the provider layer from the actual
    // credentials; additional_model_request_fields is already filtered by converter model support.
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion_result.additional_model_request_fields,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            hook.record(0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                Some(outcome::UNKNOWN),
                Some(&format!("序列化请求失败: {}", e)),
                None,
                TraceUsage::zero(),
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens
    let total_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let declared_tools = super::stream::declared_tool_requirements(payload.tools.as_ref());

    if payload.stream {
        // 流式响应
        handle_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            thinking_enabled,
            tool_name_map,
            declared_tools,
            hook,
            tracer,
            key_ctx.group.clone(),
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            extract_thinking,
            tool_name_map,
            declared_tools,
            hook,
            tracer,
            key_ctx.group.clone(),
        )
        .await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    declared_tools: std::collections::HashMap<String, bool>,
    hook: UsageRecordHook,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let call_result = match provider
        .call_api_stream(request_body, Some(tracer.as_ref()), group.as_deref())
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, input_tokens, 0, 0.0, "error");
            // 重试链路全部失败、未开始返回内容：error_type 取最后一跳分类
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(e);
        }
    };
    let response = call_result.response;
    let credential_id = call_result.credential_id;
    tracer.mark_upstream_headers_received();

    // 创建流处理上下文
    let mut ctx =
        StreamContext::new_with_thinking(model, input_tokens, thinking_enabled, tool_name_map)
            .with_declared_tools(declared_tools);

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流
    let stream = create_sse_stream(response, ctx, initial_events, hook, credential_id, tracer);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();
    // 结算对象随状态一起移动：正常结束、上游中断时显式结算，客户端取消时靠它的 Drop 兜底。
    let settlement = StreamSettlement::new(hook, credential_id, tracer, &ctx);

    let processing_stream = stream::unfold(
        (body_stream, ctx, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS)), settlement),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, mut settlement)| async move {
            if finished {
                return None;
            }

            // 使用 select! 同时等待数据和 ping 定时器
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            settlement.tracer.mark_first_token();
                            settlement.sent_bytes += chunk.len() as u64;
                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            let mut events = Vec::new();
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => match Event::from_frame(frame) {
                                        Ok(event) => {
                                            let sse_events = ctx.process_kiro_event(&event);
                                            events.extend(sse_events);
                                        }
                                        // 事件解析失败：整段上游内容会凭空消失，
                                        // 至少要留下痕迹，不能静默吞掉。
                                        Err(e) => {
                                            tracing::error!(
                                                "上游事件解析失败，该事件内容已丢失: {}",
                                                e
                                            );
                                        }
                                    },
                                    Err(e) => {
                                        tracing::warn!("解码事件失败: {}", e);
                                    }
                                }
                            }

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            settlement.update(&ctx);

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, settlement)))
                        }
                        Some(Err(e)) => {
                            // 传输层读流失败（网络、超时、对端重置）：流已经返回 200，只能补 error 事件。
                            // 绝不走正常收尾：那会发 end_turn + message_stop，客户端把半截回答当成答完。
                            let bytes = settlement.interrupt(&mut ctx, &e.to_string());
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement)))
                        }
                        None => {
                            // 上游关了连接但一个字节都没给，或最后一帧只到了一半：
                            // 都是没说完就断了，按中断处理，不当成功。
                            if settlement.sent_bytes == 0 {
                                let bytes = settlement.interrupt(&mut ctx, "Upstream returned an empty event stream");
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement)));
                            }
                            if decoder.has_pending_data() {
                                let bytes = settlement.interrupt(&mut ctx, "Upstream ended with an incomplete event frame");
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement)));
                            }
                            // 流结束，发送最终事件（generate_final_events 内部会 finish()
                            // 累积器，据此判定是否有半截 / 非法工具调用 JSON）。
                            let final_events = ctx.generate_final_events();
                            settlement.update(&ctx);
                            if let Some(message) = ctx
                                .upstream_failure_message()
                                .or_else(|| ctx.tool_json_error_message())
                            {
                                // 工具调用 JSON 半截 / 非法：实时流已回 200，无法改状态码，
                                // 只能记 error 并让 generate_final_events 补发的 `error` 事件透传给客户端。
                                settlement.finish("error", "error", Some(outcome::BAD_REQUEST), Some(&message), None);
                            } else {
                                settlement.finish("success", "success", None, None, None);
                            }
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, settlement)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
}

/// 流式响应的「恰好记一次」结算。
///
/// 正常结束与上游中断走显式的 [`Self::finish`] / [`Self::interrupt`]；客户端中途取消时
/// axum 丢弃响应体，连带丢弃手里的 unfold 状态，`Drop` 用最近一次快照补记用量并落 trace，
/// 上游已经计费的 credits 不至于在统计里凭空消失。
struct StreamSettlement {
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
    input_tokens: i32,
    output_tokens: i32,
    credits: f64,
    sent_bytes: u64,
    settled: bool,
}

impl StreamSettlement {
    fn new(
        hook: UsageRecordHook,
        credential_id: u64,
        tracer: std::sync::Arc<RequestTracer>,
        ctx: &StreamContext,
    ) -> Self {
        let mut settlement = Self {
            hook,
            credential_id,
            tracer,
            input_tokens: 0,
            output_tokens: 0,
            credits: 0.0,
            sent_bytes: 0,
            settled: false,
        };
        settlement.update(ctx);
        settlement
    }

    /// 记下最新用量快照；客户端取消时 `Drop` 只能用它。
    fn update(&mut self, ctx: &StreamContext) {
        self.input_tokens = ctx.resolved_usage();
        self.output_tokens = ctx.output_tokens;
        self.credits = ctx.credits;
    }

    fn trace_usage(&self) -> TraceUsage {
        TraceUsage {
            input_tokens: self.input_tokens.max(0) as u64,
            output_tokens: self.output_tokens.max(0) as u64,
            credits: if self.credits.is_finite() && self.credits > 0.0 {
                self.credits
            } else {
                0.0
            },
        }
    }

    /// 结算一次：用量日志与 trace 各写一条，之后再调用（含 `Drop`）都是空操作。
    fn finish(
        &mut self,
        usage_status: &str,
        trace_status: &str,
        error_type: Option<&str>,
        error_message: Option<&str>,
        interrupted_after_bytes: Option<u64>,
    ) {
        if self.settled {
            return;
        }
        self.settled = true;
        self.hook.record(
            self.credential_id,
            self.input_tokens,
            self.output_tokens,
            self.credits,
            usage_status,
        );
        self.tracer.finalize(
            trace_status,
            error_type,
            error_message,
            interrupted_after_bytes,
            self.trace_usage(),
        );
    }

    /// 上游中途断流：补 `error` 事件（不发 message_delta / message_stop），
    /// 用量记 error，trace 记 interrupted 并带上已发送字节数。
    ///
    /// 给客户端的类型用 `overloaded_error`：Anthropic 约定的「可重试」类型。客户端
    /// （opencode 按报错文字里的 overloaded 判断）据此自动重试，一次断流不会停掉整个
    /// agent 循环。代价：重试会让模型重新生成整段，断流前那一段已被上游计费。
    fn interrupt(
        &mut self,
        ctx: &mut StreamContext,
        cause: &str,
    ) -> Vec<Result<Bytes, Infallible>> {
        tracing::error!("上游响应流中断: {}", cause);
        self.update(ctx);
        let elapsed = self.hook.started_at.elapsed().as_secs();
        let events = ctx.generate_error_events(
            "overloaded_error",
            &format!(
                "Upstream connection was interrupted before the response finished (after {elapsed}s). Please retry."
            ),
        );
        self.finish(
            "error",
            "interrupted",
            Some(outcome::STREAM_INTERRUPTED),
            Some(cause),
            Some(self.sent_bytes),
        );
        events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string())))
            .collect()
    }
}

impl Drop for StreamSettlement {
    fn drop(&mut self) {
        self.finish(
            "error",
            "interrupted",
            Some(outcome::STREAM_INTERRUPTED),
            Some("response stream was cancelled before completion"),
            Some(self.sent_bytes),
        );
    }
}

use super::converter::get_context_window_size;

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    declared_tools: std::collections::HashMap<String, bool>,
    hook: UsageRecordHook,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let call_result = match provider
        .call_api(request_body, Some(tracer.as_ref()), group.as_deref())
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, input_tokens, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(e);
        }
    };
    let response = call_result.response;
    let credential_id = call_result.credential_id;
    tracer.mark_upstream_headers_received();

    // 读取响应体
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("读取响应体失败: {}", e);
            hook.record(credential_id, input_tokens, 0, 0.0, "error");
            tracer.finalize(
                "interrupted",
                Some(outcome::STREAM_INTERRUPTED),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("读取响应失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    let mut native_thinking = String::new();
    let mut native_thinking_signature: Option<String> = None;
    let mut native_redacted_thinking: Vec<String> = Vec::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;
    // meteringEvent 上报的 credit 计费量（上游真实下发）；
    let mut credits: f64 = 0.0;
    // 最近一次 meteringEvent 的完整 payload，用于在响应体 usage 中透传
    // credit_usage / credit_unit / credit_unit_plural 字段，与 /v1/messages
    // 流式（message_delta）行为一致；如果上游多次下发则取最后一次。
    let mut metering: Option<crate::kiro::model::events::MeteringEvent> = None;

    // 工具调用参数 JSON 累积器：按 tool_use_id 缓冲分片，stop 时整体解析。
    // 半截 / 非法 JSON 显式暴露为错误（返回 502），不再静默回退 {} 或丢弃。
    let mut tool_accumulator = super::stream::ToolJsonAccumulator::with_declared_tools(declared_tools);
    let mut tool_json_error: Option<super::stream::ToolJsonAccumulatorError> = None;
    // 上游下发的错误 / 异常事件。只记第一条（后续多为同一故障的连带事件），
    // 收尾时返回 502 而不是 200 —— 宁可失败，也不把上游的失败伪装成完整答案。
    let mut upstream_failure: Option<String> = None;
    // 上游内容过滤拒答：本轮只有 reasoning、没有正文。必须显式标成 refusal，
    // 否则会被当成正常 end_turn 的空响应返回，客户端会反复重发同一段历史。
    let mut refused = false;

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                let event = match Event::from_frame(frame) {
                    Ok(ev) => ev,
                    Err(e) => {
                        tracing::error!("上游事件解析失败，该事件内容已丢失: {}", e);
                        continue;
                    }
                };
                {
                    match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ReasoningContent(reasoning) => {
                            if let Some(text) = reasoning.text
                                && !text.is_empty()
                            {
                                native_thinking.push_str(&text);
                            }
                            if let Some(signature) = reasoning.signature
                                && !signature.is_empty()
                            {
                                native_thinking_signature = Some(signature);
                            }
                            if let Some(redacted) = reasoning.redacted_content
                                && !redacted.is_empty()
                            {
                                native_redacted_thinking.push(redacted);
                            }
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;
                            match tool_accumulator.push(&tool_use, &tool_name_map) {
                                Ok(Some(completed)) => {
                                    tool_uses.push(completed.to_anthropic_block());
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    tracing::error!("{}", e);
                                    tool_json_error = Some(e);
                                }
                            }
                        }
                        Event::Metadata(metadata) => {
                            if metadata.is_content_filtered() {
                                tracing::warn!(
                                    refusal = %metadata.refusal_summary().unwrap_or_default(),
                                    "上游内容过滤拒答，本轮以 refusal 结束"
                                );
                                refused = true;
                                stop_reason = "refusal".to_string();
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            // 从上下文使用百分比计算实际的 input_tokens
                            let window_size = get_context_window_size(model);
                            let actual_input_tokens =
                                (context_usage.context_usage_percentage * (window_size as f64)
                                    / 100.0) as i32;
                            context_input_tokens = Some(actual_input_tokens);
                            // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded。
                            // 拒答轮次后续也会收到 100%，不得覆盖 refusal。
                            if !refused && context_usage.context_usage_percentage >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                            tracing::debug!(
                                "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                context_usage.context_usage_percentage,
                                actual_input_tokens
                            );
                        }
                        Event::Metering(event_metering) => {
                            // 上游只下发 credit；token / cache 字段不存在
                            credits += event_metering.usage;
                            tracing::debug!(
                                usage = event_metering.usage,
                                unit = %event_metering.unit,
                                unit_plural = %event_metering.unit_plural,
                                "metering credits +{:.6}", event_metering.usage
                            );
                            metering = Some(event_metering);
                        }
                        // 上游明确报错，必须让客户端知道。
                        //
                        // 这个 match 曾经没有 `Event::Error` 分支，它会落进下面的
                        // `_ => {}`：代理照旧返回 200 + 已累计的正文 + `end_turn`，
                        // 客户端把上游的失败当成了模型的完整答案。
                        Event::Error {
                            error_code,
                            error_message,
                        } => {
                            tracing::error!("收到错误事件: {} - {}", error_code, error_message);
                            if upstream_failure.is_none() {
                                upstream_failure = Some(format!("{error_code}: {error_message}"));
                            }
                        }
                        Event::Exception {
                            exception_type,
                            message,
                        } => {
                            tracing::warn!("收到异常事件: {} - {}", exception_type, message);
                            // `ContentLengthExceededException` 是上游对「输出被长度截断」
                            // 的正常告知，用 stop_reason 表达即可，不是失败。
                            // 拒答轮次：refusal 已定，后到的截断/异常都不得改写终态。
                            if refused {
                                // 终态已定，忽略
                            } else if exception_type == "ContentLengthExceededException" {
                                stop_reason = "max_tokens".to_string();
                            } else if upstream_failure.is_none() {
                                upstream_failure = Some(format!("{exception_type}: {message}"));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    // 空流、被截断的最后一帧：上游没说完就断了。非流式路径尚未发送任何字节，
    // 直接回 502，不把半截内容当成功返回。
    let truncated = if body_bytes.is_empty() {
        Some("Upstream returned an empty event stream")
    } else if decoder.has_pending_data() {
        Some("Upstream ended with an incomplete event frame")
    } else {
        None
    };
    if let Some(message) = truncated {
        tracing::error!("{}", message);
        hook.record(credential_id, input_tokens, 0, 0.0, "error");
        tracer.finalize(
            "interrupted",
            Some(outcome::STREAM_INTERRUPTED),
            Some(message),
            None,
            TraceUsage::zero(),
        );
        return (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "api_error",
                format!("upstream {message}"),
            )),
        )
            .into_response();
    }

    // 收尾：若仍有未收到 stop=true 的工具调用缓冲（上游在参数写到一半时截断），
    // finish() 返回 IncompleteJson。已有错误则保持不变。
    // 拒答轮次：拦截可能切断半截工具参数，不捞回、不报错；已完成的工具调用也丢弃，
    // 避免客户端执行被拦截轮次里的半截动作。
    if refused {
        tool_json_error = None;
        tool_uses.clear();
    } else if tool_json_error.is_none() {
        // 无参工具：0 字节且无 stop 的缓冲，在「干净结束 + 该工具没有必填参数」时按 `{}` 收口。
        // 空流与残帧已在上面返回 502，走到这里说明响应体是完整读完的。
        match tool_accumulator.finish(&tool_name_map, upstream_failure.is_none()) {
            Ok(completed) => {
                for tool in completed {
                    has_tool_use = true;
                    tool_uses.push(tool.to_anthropic_block());
                }
            }
            Err(e) => {
                tracing::error!("{}", e);
                tool_json_error = Some(e);
            }
        }
    }

    // 上游报了错 / 异常：非流式路径尚未发送任何字节，直接回 502，把上游的失败如实
    // 暴露出去，而不是返回 200 + 半截正文 + `end_turn`（那等于把失败伪装成完整答案）。
    if let Some(message) = upstream_failure {
        hook.record(credential_id, input_tokens, 0, 0.0, "error");
        tracer.finalize(
            "error",
            Some(outcome::UNKNOWN),
            Some(&message),
            None,
            TraceUsage::zero(),
        );
        return (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "api_error",
                format!("upstream {message}"),
            )),
        )
            .into_response();
    }

    // 工具调用 JSON 半截 / 非法：非流式路径尚未发送任何字节，直接回 502，
    // 明确暴露上游问题，而不是把无法解析的参数当成完整调用返回。
    if let Some(err) = tool_json_error {
        let message = err.message();
        hook.record(credential_id, input_tokens, 0, 0.0, "error");
        tracer.finalize(
            "error",
            Some(outcome::BAD_REQUEST),
            Some(&message),
            None,
            TraceUsage::zero(),
        );
        return (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new("upstream_tool_json_error", message)),
        )
            .into_response();
    }

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 字面 <tool_use 标签只告警、不删除（与流式同口径）。删除会连带吞掉标签之后的
    // 正文（未闭合时直接丢到末尾），客户端拿到一条被截断的回复且毫无提示 —— 那比让
    // 它看到一段多余的 XML 更糟。助手正常提到这个标签名（例如解释协议）就会触发。
    if crate::kiro::model::events::contains_tool_use_xml_leak(&text_content) {
        tracing::warn!(
            text_len = text_content.len(),
            "上游正文里出现字面 <tool_use 标签，已原样透传（不再删除）"
        );
    }

    // 构建响应内容
    let mut content = build_non_stream_content(
        thinking_enabled,
        text_content,
        native_thinking,
        native_thinking_signature,
        native_redacted_thinking,
    );
    content.extend(tool_uses);

    // 估算输出 tokens（上游不下发 token，全部走估算）
    let output_tokens = token::estimate_output_tokens(&content);

    // 输入 tokens：contextUsage 真实值优先，否则用客户端估算。
    // contextUsage 覆盖生成结束后的整个上下文（含本轮输出），先扣掉输出再上报，
    // 否则同一段回答会在 input_tokens 与 output_tokens 里各算一次。
    let total_input_tokens =
        token::input_tokens_excluding_output(context_input_tokens, input_tokens, output_tokens);
    let final_input_tokens = total_input_tokens;

    // 构建 Anthropic 响应
    let mut usage_json = json!({
        "input_tokens": final_input_tokens,
        "output_tokens": output_tokens
    });
    // 透传上游 meteringEvent 的 credit_* 字段，让客户端拿到与 Kiro
    // 后端口径一致的计费元数据；只在收到过 meteringEvent 时才追加。
    if let Some(m) = &metering {
        usage_json["credit_usage"] = json!(m.usage);
        usage_json["credit_unit"] = json!(m.unit);
        usage_json["credit_unit_plural"] = json!(m.unit_plural);
    }
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage_json
    });

    hook.record(
        credential_id,
        final_input_tokens,
        output_tokens,
        credits,
        "success",
    );
    tracer.finalize(
        "success",
        None,
        None,
        None,
        TraceUsage {
            input_tokens: final_input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            credits: if credits.is_finite() && credits > 0.0 {
                credits
            } else {
                0.0
            },
        },
    );
    (StatusCode::OK, Json(response_body)).into_response()
}

fn build_non_stream_content(
    thinking_enabled: bool,
    text_content: String,
    native_thinking: String,
    native_thinking_signature: Option<String>,
    native_redacted_thinking: Vec<String>,
) -> Vec<serde_json::Value> {
    let mut content = Vec::new();
    // 上游给了思考内容就原样下发，不看内容是什么。
    //
    // 这里曾经把「纯空白的思考」当作没有思考丢掉（客户端会把它渲染成一条内容为空的
    // 「思考」条目）。那仍然是代理替上游决定用户能看到什么。这条路径也是
    // `/v1/responses` 的来源（Responses 内部强制非流式），两个入口共用此处判断。
    let has_native_thinking = !native_thinking.is_empty();

    if thinking_enabled {
        if has_native_thinking {
            let mut block = json!({
                "type": "thinking",
                "thinking": native_thinking.clone(),
            });
            // 只有上游真给了签名才带 signature 字段。这里曾经在缺签名时填一个自造的占位
            // 串（好让客户端 SDK 不要因为缺 signature 而丢弃整块），那是发上游从未发过的
            // 字段，且必须在下一轮入站时再剔除，否则回传即触发上游验签失败。
            if let Some(signature) = native_thinking_signature {
                block["signature"] = json!(signature);
            }
            content.push(block);
        } else if !text_content.is_empty() {
            // 只认原生 `reasoningContentEvent`（带真 signature）。上游若把思考写进
            // 正文的 `<thinking>` 标签，那是 legacy 形态：此处不再解析提取，正文
            // 原样输出。2026-07 实测 claude-opus-5.5 在长推理 / 工具调用 / 流式与
            // 非流式全部走原生模式（406 个 reasoning 事件，XML 模式 0 次）。
            content.push(json!({
                "type": "text",
                "text": text_content
            }));
        }

        for redacted in native_redacted_thinking {
            content.push(json!({
                "type": "redacted_thinking",
                "data": redacted
            }));
        }

        if has_native_thinking && !text_content.is_empty() {
            content.push(json!({
                "type": "text",
                "text": text_content
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    } else if has_native_thinking {
        // 正文为空 + 客户端没要思考：曾经把思考文本当作正文块下发。那是伪造上游从未
        // 发过的块（思考与正文是两个通道），客户端无从分辨；实测后果是 OpenCode 的
        // 压缩摘要被写成了模型的内心独白。客户端没要就不发 —— 此时 content 为空数组，
        // 不补空正文、不伪造截断（与流式侧同一条回归锁）。
        tracing::warn!(
            len = native_thinking.len(),
            "客户端未启用 thinking 且正文为空，丢弃上游 reasoning 文本（不再当正文下发）"
        );
    }
    content
}

/// 模型名带 `-thinking` 后缀时开启思考。
///
/// 白名单接受 `claude-opus-5.5-thinking` 作为 `claude-opus-5.5` 的别名（`allowlist`
/// 只校验不改写 `payload.model`，所以后缀在此仍可见）。
///
/// 上游 Claude 族只接受 `thinking.type = adaptive`，发 `enabled` 会 400；
/// 档位交给 `converter` 的取值链决定（顶层 effort > output_config > 默认 high），
/// 此处不写死 `output_config`，否则会覆盖客户端显式指定的档位。
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    if !payload.model.to_lowercase().contains("thinking") {
        return;
    }
    if payload.thinking.is_some() {
        // 客户端已显式给了 thinking 配置，别名后缀不覆盖它。
        return;
    }

    tracing::info!(
        model = %payload.model,
        "模型名含 thinking 后缀，开启 adaptive thinking"
    );
    payload.thinking = Some(Thinking {
        thinking_type: "adaptive".to_string(),
        budget_tokens: 20000,
        display: None,
    });
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    Extension(_key_ctx): Extension<KeyContext>,
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

/// 创建缓冲 SSE 事件流
///

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::trace_db::{TraceQuery, TraceStore};

    fn test_tracer(store: &SharedTraceStore, model: &str) -> RequestTracer {
        let state = AppState::new(false).with_trace_store(Some(store.clone()));
        RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: KeyContext {
                    key_id: 1,
                    group: None,
                    key_source: TraceKeySource::ClientKey,
                },
                api_endpoint: ApiEndpoint::Messages,
                model: model.to_string(),
                is_stream: false,
            },
        )
    }

    fn test_attempt(attempt: u32, credential_id: u64) -> TraceAttempt {
        TraceAttempt {
            attempt,
            credential_id,
            endpoint: "ide".to_string(),
            http_status: Some(200),
            outcome: outcome::SUCCESS.to_string(),
            error_snippet: None,
            duration_ms: 12,
        }
    }

    /// 回归锁：跨多次 provider 调用的 attempt 必须重排编号后落库。
    ///
    /// `trace_attempts` 主键是 `(trace_id, attempt)`，而 provider 每次
    /// `call_api*` 都从 1 开始编号。web_search loop 一条 trace 跨多轮调用
    /// provider，不重排的话第二轮的 attempt=1 会被 `INSERT OR REPLACE` 顶掉
    /// 第一轮同名行：attempts 表少行，traces.total_attempts 仍是内存里的条数。
    #[test]
    fn attempts_from_multiple_provider_calls_are_renumbered() {
        let store: SharedTraceStore =
            std::sync::Arc::new(TraceStore::open_in_memory().expect("内存 trace store"));
        let tracer = test_tracer(&store, "gpt-5.6-sol");
        // 两轮 web_search，各一跳，provider 都把它编号为 1
        tracer.on_attempt(test_attempt(1, 11));
        tracer.on_attempt(test_attempt(1, 22));
        tracer.finalize("success", None, None, None, TraceUsage::zero());

        let (rows, _) = store.query_paged(&TraceQuery {
            model: Some("gpt-5.6-sol".to_string()),
            ..Default::default()
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total_attempts, 2);
        // 落库条数必须与 total_attempts 一致，且编号按时间顺序单调
        let numbers: Vec<u32> = rows[0].attempts.iter().map(|a| a.attempt).collect();
        assert_eq!(numbers, vec![1, 2], "跨轮 attempt 必须重排为 1..N");
        let credentials: Vec<u64> = rows[0].attempts.iter().map(|a| a.credential_id).collect();
        assert_eq!(credentials, vec![11, 22], "重排不得改变时间顺序");
    }

    /// 回归锁：每个 SSE 响应都带 `X-Accel-Buffering: no`。
    ///
    /// 前面有 nginx 时，默认会先攒着流式分片再转发，文字成块到达。这个头只作用于当前响应，
    /// 不用改反代全局配置。靠源码扫描防止以后新增的 SSE 端点漏带。
    #[test]
    fn every_sse_response_disables_proxy_buffering() {
        let sources = [
            ("anthropic/handlers.rs", include_str!("handlers.rs")),
            ("anthropic/responses.rs", include_str!("responses.rs")),
            ("anthropic/websearch_loop.rs", include_str!("websearch_loop.rs")),
            ("admin/handlers.rs", include_str!("../admin/handlers.rs")),
        ];
        for (name, source) in sources {
            let production = source.split("#[cfg(test)]").next().unwrap();
            let sse = production.matches("\"text/event-stream\"").count();
            let guarded = production.matches("\"x-accel-buffering\"").count();
            assert!(sse >= 1, "{name} 应当至少有一个 SSE 响应，扫描目标可能已变");
            assert_eq!(sse, guarded, "{name}: 每个 SSE 响应都要带 x-accel-buffering: no");
        }
    }

    fn rows_for(store: &SharedTraceStore, model: &str) -> Vec<crate::admin::trace_db::TraceRecord> {
        store
            .query_paged(&TraceQuery {
                model: Some(model.to_string()),
                ..Default::default()
            })
            .0
    }

    /// 回归锁：请求没有任何显式收尾就被丢弃（客户端取消）时，trace 里必须留下一行。
    ///
    /// 此前这类请求在日志里毫无痕迹，而上游可能已经在生成并计费。
    #[test]
    fn cancelled_request_leaves_an_interrupted_trace_row() {
        let store: SharedTraceStore =
            std::sync::Arc::new(TraceStore::open_in_memory().expect("内存 trace store"));

        drop(test_tracer(&store, "cancel-before-headers"));

        let after = test_tracer(&store, "cancel-after-headers");
        after.mark_upstream_headers_received();
        drop(after);

        let before_rows = rows_for(&store, "cancel-before-headers");
        assert_eq!(before_rows.len(), 1);
        assert_eq!(before_rows[0].final_status, "interrupted");
        assert_eq!(
            before_rows[0].error_type.as_deref(),
            Some(outcome::ABANDONED_BEFORE_HEADERS)
        );

        let after_rows = rows_for(&store, "cancel-after-headers");
        assert_eq!(after_rows.len(), 1);
        assert_eq!(
            after_rows[0].error_type.as_deref(),
            Some(outcome::STREAM_INTERRUPTED)
        );
    }

    /// 显式收尾必须压过 Drop 兜底：已落库的请求不能被再写一行覆盖成「中断」。
    #[test]
    fn explicit_finalize_is_not_overwritten_by_drop() {
        let store: SharedTraceStore =
            std::sync::Arc::new(TraceStore::open_in_memory().expect("内存 trace store"));
        let tracer = test_tracer(&store, "finished-normally");
        tracer.finalize("success", None, None, None, TraceUsage::zero());
        tracer.finalize("error", Some(outcome::UNKNOWN), None, None, TraceUsage::zero());
        drop(tracer);

        let rows = rows_for(&store, "finished-normally");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].final_status, "success", "只有第一次收尾生效");
    }

    fn settlement_fixture(
        model: &str,
    ) -> (
        StreamSettlement,
        StreamContext,
        SharedAggregator,
        SharedTraceStore,
    ) {
        use crate::admin::usage_stats::UsageAggregator;
        let store: SharedTraceStore =
            std::sync::Arc::new(TraceStore::open_in_memory().expect("内存 trace store"));
        let aggregator: SharedAggregator = std::sync::Arc::new(UsageAggregator::new());
        let hook = UsageRecordHook {
            recorder: None,
            aggregator: Some(aggregator.clone()),
            client_keys: None,
            key_id: 1,
            model: model.to_string(),
            started_at: Instant::now(),
        };
        let mut ctx =
            StreamContext::new_with_thinking(model, 100, false, std::collections::HashMap::new());
        ctx.output_tokens = 40;
        ctx.credits = 0.5;
        let tracer = std::sync::Arc::new(test_tracer(&store, model));
        (
            StreamSettlement::new(hook, 7, tracer, &ctx),
            ctx,
            aggregator,
            store,
        )
    }

    /// 回归锁：客户端中途取消，流被丢弃时，用量与 trace 各补记一次。
    ///
    /// 上游此时已在生成并计费；此前这条请求在用量统计和请求日志里都不存在。
    #[test]
    fn cancelled_stream_is_settled_once_from_the_last_snapshot() {
        let (mut settlement, ctx, aggregator, store) = settlement_fixture("cancel-stream");
        settlement.sent_bytes = 1234;
        settlement.update(&ctx);
        drop(settlement);

        assert_eq!(aggregator.overview().today_calls, 1, "用量只记一次");
        assert_eq!(aggregator.overview().today_input_tokens, 100);
        let rows = rows_for(&store, "cancel-stream");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].final_status, "interrupted");
        assert_eq!(rows[0].error_type.as_deref(), Some(outcome::STREAM_INTERRUPTED));
        assert_eq!(rows[0].interrupted_after_bytes, Some(1234));
        assert_eq!(rows[0].output_tokens, 40);
    }

    #[test]
    fn finished_stream_is_not_settled_again_on_drop() {
        let (mut settlement, _ctx, aggregator, store) = settlement_fixture("finished-stream");
        settlement.finish("success", "success", None, None, None);
        drop(settlement);

        assert_eq!(aggregator.overview().today_calls, 1);
        let rows = rows_for(&store, "finished-stream");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].final_status, "success", "Drop 不得把已完成的请求改成中断");
    }

    /// 上游断流：客户端收到 `overloaded_error`（可重试），没有 message_stop；用量记 error，
    /// trace 记 interrupted；之后的 Drop 不再重复结算。
    #[test]
    fn interrupted_stream_tells_the_client_to_retry_and_settles_once() {
        let (mut settlement, mut ctx, aggregator, store) = settlement_fixture("cut-stream");
        settlement.sent_bytes = 77;

        let frames = settlement.interrupt(&mut ctx, "error decoding response body");
        let wire: String = frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame.as_ref().unwrap()).to_string())
            .collect();
        assert!(wire.contains("event: error"), "{wire}");
        assert!(wire.contains("overloaded_error"), "{wire}");
        assert!(!wire.contains("message_stop"), "不得声称正常结束: {wire}");
        assert!(!wire.contains("message_delta"), "不得声称正常结束: {wire}");
        assert!(
            !wire.contains("error decoding response body"),
            "底层错误细节只进日志，不发给客户端"
        );
        drop(settlement);

        assert_eq!(aggregator.overview().today_calls, 1);
        let rows = rows_for(&store, "cut-stream");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].final_status, "interrupted");
        assert_eq!(rows[0].interrupted_after_bytes, Some(77));
        assert_eq!(
            rows[0].error_message.as_deref(),
            Some("error decoding response body")
        );
    }

    #[test]
    fn account_suspended_attempt_is_preserved_as_request_error_type() {
        assert_eq!(
            canonical_attempt_outcome(outcome::ACCOUNT_SUSPENDED),
            outcome::ACCOUNT_SUSPENDED
        );
    }

    #[test]
    fn bedrock_client_validation_errors_map_to_400() {
        // 客户端校验错误必须映射为 400（而非 5xx），否则会被 provider 当作上游
        // 瞬态错误触发冷却，放大成 503 风暴。识别逻辑集中在 endpoint 层。
        for needle in [
            // 精确 reason（provider 错误串里嵌着上游 body）
            "非流式 API 请求失败: 500 {\"reason\":\"TOOL_USE_RESULT_MISMATCH\"}",
            // message 级特异短语（纯文本报文）
            "Expected toolResult blocks but found none",
        ] {
            let resp = map_provider_error(anyhow::anyhow!(needle.to_string()));
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "错误串 `{needle}` 应映射为 400"
            );
        }
    }

    #[test]
    fn generic_upstream_error_still_maps_to_502() {
        // 回归：普通上游错误不应被新分支误伤，仍应是 502 BAD_GATEWAY。
        let resp = map_provider_error(anyhow::anyhow!("connection reset by peer"));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        // 回归：宽泛的 ValidationException 不再被当作客户端校验错误而误判为 400，
        // 仍按上游错误走 502（避免把可重试故障误杀）。
        let resp = map_provider_error(anyhow::anyhow!(
            "ValidationException: transient backend issue".to_string()
        ));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn upstream_rate_limit_maps_to_429_with_retry_after() {
        let err = crate::kiro::error::UpstreamRateLimitError::new(Some("1800".to_string()));
        let resp = map_provider_error(err.into());

        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "1800");
    }

    #[test]
    fn upstream_rate_limit_drops_invalid_retry_after() {
        let err =
            crate::kiro::error::UpstreamRateLimitError::new(Some("not-a-retry-delay".to_string()));
        let resp = map_provider_error(err.into());

        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get(header::RETRY_AFTER).is_none());
    }

    #[tokio::test]
    async fn generic_upstream_error_does_not_expose_raw_body() {
        let secret = "aws-account=123456789012 request-id=private-request";
        let resp = map_provider_error(anyhow::anyhow!(secret));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(!body.contains(secret));
        assert!(body.contains("Upstream API request failed"));
    }

    #[test]
    fn non_stream_native_thinking_precedes_redacted_and_text() {
        let content = build_non_stream_content(
            true,
            "final answer".to_string(),
            "native thinking".to_string(),
            Some("real-signature".to_string()),
            vec!["encrypted-thinking".to_string()],
        );

        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "native thinking");
        assert_eq!(content[0]["signature"], "real-signature");
        assert_eq!(content[1]["type"], "redacted_thinking");
        assert_eq!(content[1]["data"], "encrypted-thinking");
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "final answer");
    }

    /// 回归锁：legacy `<thinking>` XML 提取已移除，只认原生 reasoningContentEvent。
    ///
    /// 上游若把思考写进正文标签（2026-07 实测 claude-opus-5.5 从不这样：406 个
    /// reasoning 事件全部走原生模式），这里不再解析成 thinking 块，正文原样输出。
    /// 伪造出的 thinking 块带不了真 signature，回传上游必然验签失败。
    #[test]
    fn non_stream_does_not_extract_legacy_xml_thinking() {
        let content = build_non_stream_content(
            true,
            "<thinking>legacy thinking</thinking>\n\nfinal answer".to_string(),
            String::new(),
            None,
            Vec::new(),
        );

        assert_eq!(content.len(), 1, "不应再拆出 thinking 块");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(
            content[0]["text"], "<thinking>legacy thinking</thinking>\n\nfinal answer",
            "正文应原样保留，不做 XML 提取"
        );
    }

    /// 回归锁：上游给的思考内容原样下发，只含空白也照发。
    ///
    /// 这里曾经把「只含换行的 reasoning 内容」当作没有思考丢掉（客户端会把它渲染成一条
    /// 内容为空的「思考」）。那仍然是代理替上游决定用户能看到什么。这条路径也是
    /// `/v1/responses` 的来源（Responses 内部强制非流式），两个入口共用此处判断。
    #[test]
    fn non_stream_whitespace_only_thinking_is_still_emitted() {
        for whitespace in ["\n\n", " ", "\t\n  \n"] {
            let content = build_non_stream_content(
                true,
                "final answer".to_string(),
                whitespace.to_string(),
                Some("real-signature".to_string()),
                Vec::new(),
            );

            assert_eq!(
                content.len(),
                2,
                "{whitespace:?} 应产出 thinking + text 两块"
            );
            assert_eq!(content[0]["type"], "thinking");
            assert_eq!(
                content[0]["thinking"], whitespace,
                "思考内容必须逐字保真，不许 trim"
            );
            assert_eq!(content[1]["type"], "text");
            assert_eq!(content[1]["text"], "final answer");
        }
    }

    #[test]
    /// 回归锁：非流式下客户端没启用 thinking 且正文为空时，content 就是空数组 ——
    /// 不许把思考文本当正文下发（旧行为如此，OpenCode 的压缩摘要因此变成模型独白），
    /// 也不许补一个空正文块。
    fn non_stream_native_thinking_is_dropped_when_thinking_disabled() {
        let content = build_non_stream_content(
            false,
            String::new(),
            "native thinking fallback".to_string(),
            Some("ignored-signature".to_string()),
            vec!["ignored-redacted".to_string()],
        );

        assert!(
            content.is_empty(),
            "思考文本不许当正文下发，实际: {content:?}"
        );
    }

    #[test]
    fn dynamic_models_do_not_synthesize_claude_thinking_alias() {
        let models = aggregate_available_models(vec![UpstreamModel {
            model_id: "claude-opus-5.5".to_string(),
            model_name: Some("Claude Opus 5".to_string()),
            description: None,
            token_limits: None,
        }]);
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();

        assert!(ids.contains(&"claude-opus-5.5"));
        assert!(!ids.contains(&"claude-opus-5.5-thinking"));
    }

    #[test]
    fn count_image_budget_handles_empty() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": []
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.total_b64_bytes, 0);
        assert_eq!(stats.largest_b64_bytes, 0);
    }

    #[test]
    fn count_image_budget_counts_inline_base64() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA1111"}},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBBBBBBBB"}},
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#).unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_b64_bytes, 18);
        assert_eq!(stats.largest_b64_bytes, 10);
    }

    #[test]
    fn count_image_budget_skips_url_only_images() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
    }

    #[test]
    fn dynamic_models_merge_metadata_and_do_not_use_input_limit_as_output_limit() {
        let models = aggregate_available_models(vec![
            UpstreamModel {
                model_id: "claude-opus-5.5".to_string(),
                model_name: None,
                description: Some("first".to_string()),
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(200_000),
                    max_output_tokens: None,
                }),
            },
            UpstreamModel {
                model_id: "claude-opus-5.5".to_string(),
                model_name: Some("Claude Opus 5".to_string()),
                description: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(1_000_000),
                    max_output_tokens: Some(32_000),
                }),
            },
        ]);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "Claude Opus 5");
        // owned_by 按模型名前缀推断：claude-* → anthropic
        assert_eq!(models[0].owned_by, "anthropic");
        assert_eq!(models[0].max_tokens, 32_000);
        // 合并时输入窗口取两个来源的较大值。
        assert_eq!(models[0].context_window, 1_000_000);
    }

    /// 回归锁：`/v1/models` 只能列出白名单内的模型。
    ///
    /// 上游动态发现会返回账号可用的全部模型，但本部署只服务白名单里的 5 个
    /// （Claude 组 2 个 + GPT 组 3 个）。若把白名单外的模型也列出来，客户端会看到
    /// 「能选但一用就 400」的假选项。
    #[test]
    fn available_models_only_lists_allowlisted() {
        let upstream = vec![
            UpstreamModel {
                model_id: "claude-opus-5.5".to_string(),
                model_name: Some("Claude Opus 5".to_string()),
                description: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(1_000_000),
                    max_output_tokens: Some(128_000),
                }),
            },
            UpstreamModel {
                model_id: "claude-sonnet-5.5".to_string(),
                model_name: Some("Claude Sonnet 5".to_string()),
                description: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(1_000_000),
                    max_output_tokens: Some(64_000),
                }),
            },
            UpstreamModel {
                model_id: "gpt-5.6-sol".to_string(),
                model_name: Some("GPT 5.6 Sol".to_string()),
                description: None,
                token_limits: None,
            },
            UpstreamModel {
                model_id: "gpt-5.6-terra".to_string(),
                model_name: Some("GPT 5.6 Terra".to_string()),
                description: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(986_000),
                    max_output_tokens: Some(128_000),
                }),
            },
            UpstreamModel {
                model_id: "gpt-5.6-luna".to_string(),
                model_name: Some("GPT 5.6 Luna".to_string()),
                description: None,
                token_limits: None,
            },
            // 以下都在白名单外，必须被过滤掉
            UpstreamModel {
                model_id: "claude-opus-4.8".to_string(),
                model_name: None,
                description: None,
                token_limits: None,
            },
            // 相邻版本号：绝不能被误判成 claude-sonnet-5.5
            UpstreamModel {
                model_id: "claude-sonnet-4.6".to_string(),
                model_name: None,
                description: None,
                token_limits: None,
            },
            UpstreamModel {
                model_id: "gpt-5.6".to_string(),
                model_name: None,
                description: None,
                token_limits: None,
            },
            UpstreamModel {
                model_id: "deepseek-3.2".to_string(),
                model_name: None,
                description: None,
                token_limits: None,
            },
        ];

        let models = aggregate_available_models(upstream);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        // BTreeMap 排序：claude-* 在 gpt-* 之前
        assert_eq!(
            ids,
            vec![
                "claude-opus-5.5",
                "claude-sonnet-5.5",
                "gpt-5.6-luna",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
            ]
        );
        // sonnet-5 的输出上限是 64k，与 opus-5 的 128k 不同 —— 上游值必须原样透出，
        // 不能被同族的另一个模型的值污染。
        let sonnet = models.iter().find(|m| m.id == "claude-sonnet-5.5").unwrap();
        assert_eq!(sonnet.max_tokens, 64_000);
        // 输入窗口同样来自上游 tokenLimits，而不是按族推断的常量。
        assert_eq!(sonnet.context_window, 1_000_000);
        let opus = models.iter().find(|m| m.id == "claude-opus-5.5").unwrap();
        assert_eq!(opus.max_tokens, 128_000);
        // 上游没给 tokenLimits 的模型回退到按族计算的窗口。
        let luna = models.iter().find(|m| m.id == "gpt-5.6-luna").unwrap();
        assert_eq!(luna.context_window, 986_000);
        let terra = models.iter().find(|m| m.id == "gpt-5.6-terra").unwrap();
        assert_eq!(terra.context_window, 986_000);
    }

    #[test]
    fn max_tokens_must_be_positive() {
        assert!(validate_max_tokens(1).is_ok());
        assert!(validate_max_tokens(0).is_err());
        assert!(validate_max_tokens(-1).is_err());
    }
}
