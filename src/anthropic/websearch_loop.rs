//! web_search local agentic loop
//!
//! Handles the case "after mixed tools (web_search + exec...) fall onto the normal chat path, the upstream returns a tool_use with name=web_search":
//! kiro-rs internally calls /mcp to search -> feeds the results back as a tool_result -> reconverts and resends -> loops until the upstream stops asking to search;
//! tool_use calls other than web_search (exec, etc.) are returned to the client as usual: they do not enter the loop and are not swallowed.
//!
//! Reuses: converter::convert_request (feedback), provider.call_api_stream, EventStreamDecoder,
//! websearch::{create_mcp_request, call_mcp_api, parse_search_results, generate_search_summary}。

use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::kiro::model::events::{Event, MeteringEvent};
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::provider::KiroProvider;
use crate::token;

// 别名 `trace_outcome`：本文件里 `outcome` 已被 run_round 的局部变量占用
//（`let mut outcome = decode_round(...)`），同名会让读者分不清是模块还是变量。
use crate::admin::trace_db::outcome as trace_outcome;

use super::converter::{ConversionError, convert_request, get_context_window_size};
use super::handlers::{
    RequestTracer, TraceUsage, UsageRecordHook, last_attempt_outcome, map_provider_error,
};
use super::stream::{CompletedToolUse, SseEvent, ToolJsonAccumulator, ToolJsonAccumulatorError};
use super::types::{ErrorResponse, Message, MessagesRequest};
use super::websearch::{self, WebSearchResults};

/// Maximum number of search rounds, to prevent an infinite loop if the upstream keeps asking to search
const MAX_WEB_SEARCH_ROUNDS: usize = 5;

/// A valid assistant turn after a tool result must contain either visible text or
/// another client tool call. Kiro occasionally closes a successful upstream stream
/// without either, which used to be serialized as `end_turn` and made the client mark an
/// unfinished task complete. Retry once before surfacing an upstream error.
const MAX_EMPTY_TOOL_RESULT_RETRIES: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptyToolResultDisposition {
    Accept,
    Retry,
    Fail,
}

/// 与 `hook.record(..., "error")` 成对：把本轮失败落成一条 trace 行。
///
/// 每条失败路径都必须调用它一次且仅一次 —— `RequestTracer::finalize` 会
/// `mem::take` 掉 attempts，调两次会多出一条 0 跳的假记录。`store` 为 None
/// （未启用 trace）时整体是空操作。
fn trace_error(
    tracer: &RequestTracer,
    error_type: &str,
    message: &str,
    input_tokens: i32,
    credits: f64,
) {
    tracer.finalize(
        "error",
        Some(error_type),
        Some(message),
        None,
        TraceUsage {
            input_tokens: input_tokens.max(0) as u64,
            output_tokens: 0,
            credits: if credits.is_finite() && credits > 0.0 {
                credits
            } else {
                0.0
            },
        },
    );
}

/// Result of buffer-decoding one round of the upstream response
struct RoundOutcome {
    /// Accumulated assistant text
    text: String,
    /// Accumulated thinking / reasoning text (Kiro reasoningContentEvent).
    /// Surfaced out-of-band via render_json's `kiro_thinking` so Anthropic
    /// clients never see (and never replay) an unsigned thinking block.
    thinking: String,
    /// The complete tool_use for this round (name already restored via tool_name_map)
    tool_uses: Vec<CompletedToolUse>,
    /// Actual input tokens computed from contextUsageEvent
    context_input_tokens: Option<i32>,
    /// Cumulative credits from meteringEvent (sum of usage across rounds)
    credits: f64,
    /// 最近一次 meteringEvent 完整 payload（含 unit / unit_plural / usage）。
    /// 在 run_web_search_loop 出口处透传到响应 usage 字段；如果上游多次下发
    /// 则取最后一次（与 /v1/messages 非流 / 流式路径一致）。
    last_metering: Option<MeteringEvent>,
    /// stop_reason override (max_tokens / model_context_window_exceeded)
    stop_reason_override: Option<String>,
    /// True if the upstream stream ended due to a read error, so the decoded
    /// content for this round is partial and must not be treated as a success.
    stream_error: bool,
    /// 上游 tool JSON 非法或半截。与 `stream_error` 同性质：本轮内容不可信，
    /// 不能当成功处理，更不能把降级后的空参数交给客户端执行。
    tool_json_error: Option<ToolJsonAccumulatorError>,
    /// Tool names declared to the upstream this round (original + shortened),
    /// taken from `ConversionResult::known_tool_names`. Used by the shared
    /// `<invoke>` text-leak fault tolerance so a leaked `<invoke name=...>` is only
    /// reclaimed when its name is a real declared tool.
    known_tool_names: std::collections::HashSet<String>,
    /// Short-name -> original-name map for this round, taken from
    /// `ConversionResult::tool_name_map`. Used to restore the original tool name when a
    /// leaked `<invoke>` carries a shortened (>63 char) tool name.
    tool_name_map: std::collections::HashMap<String, String>,
}

/// Normalize model-produced Web Search input into one non-empty query.
///
/// Codex-compatible providers can emit `query`, `search_query`, `q`, a
/// `queries` array, or wrap the text in `text`/`value`. Kiro's MCP
/// endpoint accepts only one string in `arguments.query`.
fn normalized_query_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let query = s.trim();
            (!query.is_empty()).then(|| query.to_string())
        }
        Value::Array(values) => values.iter().find_map(normalized_query_value),
        Value::Object(object) => ["query", "search_query", "q", "text", "value"]
            .iter()
            .find_map(|key| object.get(*key).and_then(normalized_query_value)),
        _ => None,
    }
}

/// Extract a usable Web Search query from a model tool-use input.
fn tool_query(tu: &CompletedToolUse) -> Option<String> {
    ["query", "search_query", "q", "queries"]
        .iter()
        .find_map(|key| tu.input.get(*key).and_then(normalized_query_value))
        .or_else(|| normalized_query_value(&tu.input))
}

/// 模型给的 web_search 入参里找不到可用 query 时，构造上抛的错误。
///
/// **不降级成空结果。** 上游此处是 `warn` + `searched.push(None)`，那等于告诉模型
/// 「搜过了，没找到」——而真实情况是「我们没搜」。模型会据此得出「这事没有相关信息」
/// 的结论并写进回答，这正是 AGENTS.md §2.5 所禁的：替客户端制造一个不存在的结果。
/// 入参畸形是模型输出的问题，让它显式失败比让它以为搜过更有用。
fn invalid_web_search_input_error(tu: &CompletedToolUse) -> anyhow::Error {
    let (input_kind, input_details) = match &tu.input {
        Value::Object(object) => {
            let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
            keys.sort_unstable();
            ("object", keys.join(","))
        }
        Value::Array(values) => ("array", format!("len={}", values.len())),
        Value::String(value) => ("string", format!("len={}", value.chars().count())),
        Value::Number(_) => ("number", String::new()),
        Value::Bool(_) => ("bool", String::new()),
        Value::Null => ("null", String::new()),
    };
    tracing::warn!(
        tool_use_id = %tu.id,
        input_kind,
        input_details = %input_details,
        "web_search tool input has no usable non-empty query; failing the request instead of faking an empty result"
    );
    anyhow::anyhow!(
        "web_search tool_use {} carries no usable non-empty query (input kind: {}{}{})",
        tu.id,
        input_kind,
        if input_details.is_empty() { "" } else { ", keys/len: " },
        input_details
    )
}

fn is_no_results_mcp_error(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .contains("MCP error: -32602 - Tool returned no results")
}

fn log_normalized_web_search_query(tu: &CompletedToolUse, query: &str) {
    tracing::info!(
        tool_use_id = %tu.id,
        query_chars = query.chars().count(),
        "web_search normalized a non-empty query before calling Kiro MCP"
    );
}

/// Decides whether this round should keep searching (enter the next loop round)
///
/// Continue condition: every tool_use this round is web_search (at least one) and the round limit has not been reached.
/// As soon as a client tool such as exec is mixed in, there is no tool_use at all, or the limit is reached, it stops and flushes (exec is never swallowed).
fn should_search_round(round_idx: usize, tool_uses: &[CompletedToolUse]) -> bool {
    let only_web_search =
        !tool_uses.is_empty() && tool_uses.iter().all(|t| t.name == "web_search");
    only_web_search && round_idx < MAX_WEB_SEARCH_ROUNDS
}

/// Whether the request is the continuation immediately following a tool result.
fn last_message_has_tool_result(payload: &MessagesRequest) -> bool {
    let Some(last) = payload.messages.last() else {
        return false;
    };
    if last.role != "user" {
        return false;
    }
    last.content.as_array().is_some_and(|blocks| {
        blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
    })
}

/// Decide how to handle a successful upstream round after tool output. Reasoning
/// by itself is intentionally not enough: the client needs either assistant text (a
/// real final answer) or a client tool call to keep the task lifecycle sound.
fn empty_tool_result_disposition(
    payload: &MessagesRequest,
    round: &RoundOutcome,
    retries: usize,
) -> EmptyToolResultDisposition {
    let is_invalid_empty_continuation = last_message_has_tool_result(payload)
        && round.text.trim().is_empty()
        && round.tool_uses.is_empty()
        && round.stop_reason_override.is_none();
    if !is_invalid_empty_continuation {
        EmptyToolResultDisposition::Accept
    } else if retries < MAX_EMPTY_TOOL_RESULT_RETRIES {
        EmptyToolResultDisposition::Retry
    } else {
        EmptyToolResultDisposition::Fail
    }
}

/// Buffer-decode one round of the upstream streaming response
///
/// `tracer` 只用来标记首个上游 chunk 的到达时刻（`first_token_ms`）。它必须在
/// 这里打点而不是在 `call_api_stream` 返回处：后者只代表响应头到达，写进
/// first_token 会让 Admin 面板上的首字延迟系统性偏小。
async fn decode_round(
    response: reqwest::Response,
    model: &str,
    tool_name_map: &std::collections::HashMap<String, String>,
    tracer: &RequestTracer,
) -> RoundOutcome {
    let mut body_stream = response.bytes_stream();
    let mut decoder = EventStreamDecoder::new();

    let mut text = String::new();
    let mut thinking = String::new();
    // 与 /v1/messages 主路径同款的 tool JSON 累积器：只有整段 JSON 完整可解析才
    // 产出 tool_use。此前这里是「解析失败就降级成 {}」，会让客户端拿着空参数去
    // 执行工具（混合工具集场景会命中），属于静默数据损坏。
    let mut tool_accumulator = ToolJsonAccumulator::new();
    let mut tool_uses: Vec<CompletedToolUse> = Vec::new();
    let mut tool_json_error: Option<ToolJsonAccumulatorError> = None;
    let mut context_input_tokens: Option<i32> = None;
    let mut credits = 0.0;
    let mut last_metering: Option<MeteringEvent> = None;
    let mut stop_reason_override: Option<String> = None;
    let mut stream_error = false;

    while let Some(chunk) = body_stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("web_search loop failed to read the response stream: {}", e);
                stream_error = true;
                break;
            }
        };
        // 多轮时只有第一轮的第一个 chunk 生效（mark_first_token 幂等）。
        tracer.mark_first_token();
        if let Err(e) = decoder.feed(&chunk) {
            tracing::warn!("buffer overflow: {}", e);
        }
        for result in decoder.decode_iter() {
            let frame = match result {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("failed to decode event: {}", e);
                    continue;
                }
            };
            let event = match Event::from_frame(frame) {
                Ok(ev) => ev,
                Err(_) => continue,
            };
            match event {
                Event::AssistantResponse(resp) => text.push_str(&resp.content),
                Event::ReasoningContent(r) => {
                    if let Some(t) = &r.text {
                        thinking.push_str(t);
                    }
                }
                Event::ToolUse(tu) => match tool_accumulator.push(&tu, tool_name_map) {
                    // 未收到 stop：继续缓冲。
                    Ok(None) => {}
                    Ok(Some(completed)) => tool_uses.push(completed),
                    // 上游给了非法 JSON：记录并终止本轮，绝不降级成空参数。
                    Err(e) => {
                        tracing::error!("{}", e);
                        tool_json_error = Some(e);
                    }
                },
                Event::ContextUsage(cu) => {
                    let window = get_context_window_size(model);
                    let actual = (cu.context_usage_percentage * (window as f64) / 100.0) as i32;
                    context_input_tokens = Some(actual);
                    if cu.context_usage_percentage >= 100.0 {
                        stop_reason_override = Some("model_context_window_exceeded".to_string());
                    }
                }
                Event::Metering(m) => {
                    credits += m.usage;
                    last_metering = Some(m.clone());
                }
                Event::Exception { exception_type, .. } => {
                    if exception_type == "ContentLengthExceededException" {
                        stop_reason_override = Some("max_tokens".to_string());
                    }
                }
                _ => {}
            }
        }
    }

    // 收尾：仍在缓冲、始终没收到 stop 的半截 tool JSON 视为错误，不静默丢弃。
    if tool_json_error.is_none()
        && let Err(e) = tool_accumulator.finish()
    {
        tracing::error!("{}", e);
        tool_json_error = Some(e);
    }

    // 剥离混入文本的字面 <tool_use> XML 泄漏（与非流式同口径）。
    let text = crate::kiro::model::events::strip_tool_use_xml_leaks(&text);

    RoundOutcome {
        text,
        thinking,
        tool_uses,
        context_input_tokens,
        credits,
        last_metering,
        stop_reason_override,
        stream_error,
        tool_json_error,
        // Populated by the caller (run_round), which holds ConversionResult::known_tool_names.
        known_tool_names: std::collections::HashSet::new(),
        // Populated by the caller (run_round), which holds ConversionResult::tool_name_map.
        tool_name_map: std::collections::HashMap::new(),
    }
}

/// Run one upstream round (convert + streaming request + buffer decode)
///
/// On upstream/conversion failure, returns Err(an already-constructed pass-through error Response)
async fn run_round(
    provider: &Arc<KiroProvider>,
    payload: &MessagesRequest,
    hook: &UsageRecordHook,
    fallback_input_tokens: i32,
    group: Option<&str>,
    tracer: &RequestTracer,
) -> Result<(RoundOutcome, u64), Response> {
    let conversion = match convert_request(payload) {
        Ok(c) => c,
        Err(e) => {
            let (et, msg) = match &e {
                ConversionError::InvalidModel(reason) => {
                    ("invalid_request_error", format!("invalid model id: {}", reason))
                }
                ConversionError::UnsupportedRequest(reason) => {
                    ("invalid_request_error", reason.clone())
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "message list is empty".to_string())
                }
            };
            hook.record(0, 0, 0, 0.0, "error");
            trace_error(tracer, trace_outcome::BAD_REQUEST, &msg, 0, 0.0);
            return Err((StatusCode::BAD_REQUEST, Json(ErrorResponse::new(et, msg))).into_response());
        }
    };

    let kiro_request = KiroRequest {
        conversation_state: conversion.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion.additional_model_request_fields,
    };
    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(b) => b,
        Err(e) => {
            let message = format!("failed to serialize request: {}", e);
            hook.record(0, 0, 0, 0.0, "error");
            trace_error(tracer, trace_outcome::UNKNOWN, &message, 0, 0.0);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("internal_error", message)),
            )
                .into_response());
        }
    };

    // TraceSink 必须传进去：provider 在重试循环里每跳登记一条 attempt，
    // 传 None 等于把整条链路（换了几个凭据、每跳为什么失败）全部丢弃。
    let call_result = match provider
        .call_api_stream(&request_body, Some(tracer), group)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let message = e.to_string();
            hook.record(0, fallback_input_tokens, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(tracer),
                Some(&message),
                None,
                TraceUsage {
                    input_tokens: fallback_input_tokens.max(0) as u64,
                    ..TraceUsage::zero()
                },
            );
            return Err(map_provider_error(e));
        }
    };
    let credential_id = call_result.credential_id;
    let mut outcome = decode_round(
        call_result.response,
        &payload.model,
        &conversion.tool_name_map,
        tracer,
    )
    .await;
    // Carry the declared tool names (original + shortened) so the flush step can run the
    // shared `<invoke>` text-leak fault tolerance with a correct tool-table guard.
    outcome.known_tool_names = conversion.known_tool_names;
    // Carry the short->original tool name map so reclaimed <invoke> names get restored.
    outcome.tool_name_map = conversion.tool_name_map;
    if outcome.stream_error {
        // The upstream stream was cut off mid-round; the decoded content is partial,
        // so fail the round instead of feeding truncated text/tool_use back into the loop.
        let message =
            "Upstream response stream ended unexpectedly during the web_search loop.".to_string();
        hook.record(0, fallback_input_tokens, 0, 0.0, "error");
        // interrupted_after_bytes 传 None：本路径一个字节都没发给客户端
        //（整轮缓冲解码后才渲染），填数字会谎报「已发出 N 字节后断开」。
        trace_error(
            tracer,
            trace_outcome::STREAM_INTERRUPTED,
            &message,
            fallback_input_tokens,
            0.0,
        );
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new("upstream_error", message)),
        )
            .into_response());
    }
    // 上游 tool JSON 非法 / 半截：与断流同等对待。降级成空参数再交给客户端执行，
    // 等于让它拿错误的参数真的去动文件或跑命令。
    if let Some(e) = &outcome.tool_json_error {
        let message = e.message();
        hook.record(0, fallback_input_tokens, 0, 0.0, "error");
        trace_error(
            tracer,
            trace_outcome::BAD_REQUEST,
            &message,
            fallback_input_tokens,
            0.0,
        );
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(e.error_type(), message)),
        )
            .into_response());
    }
    Ok((outcome, credential_id))
}

/// Feeds one round of assistant(text + web_search tool_use) + user(tool_result) back into payload.messages,
/// and appends server_tool_use + web_search_tool_result blocks (Contract A fields) to the presentation.
///
/// `searched` corresponds one-to-one (same order) to `round.tool_uses`; the search has already been completed.
fn append_search_round(
    payload: &mut MessagesRequest,
    round: &RoundOutcome,
    searched: &[Option<WebSearchResults>],
    presentation: &mut Vec<Value>,
) {
    // assistant: text + this round's web_search tool_use (Kiro history requires tool_use<->tool_result pairing)
    let mut assistant_content: Vec<Value> = Vec::new();
    if !round.text.is_empty() {
        assistant_content.push(json!({"type": "text", "text": round.text}));
    }
    for tu in &round.tool_uses {
        assistant_content.push(tu.to_anthropic_block());
    }
    payload.messages.push(Message {
        role: "assistant".to_string(),
        content: Value::Array(assistant_content),
    });

    // user: each web_search tool_use is paired with a tool_result (content = search summary, shown to the upstream)
    let mut user_content: Vec<Value> = Vec::new();
    for (tu, results) in round.tool_uses.iter().zip(searched.iter()) {
        let query = tool_query(tu).unwrap_or_default();
        let summary = websearch::generate_search_summary(&query, results);
        user_content.push(json!({
            "type": "tool_result", "tool_use_id": tu.id, "content": summary
        }));

        // Client presentation: server_tool_use + web_search_tool_result。
        //
        // `tool_use_id` **必填**，且必须等于同组 `server_tool_use` 的 `id` ——
        // Anthropic 官方 schema 如此规定，`@ai-sdk/anthropic` 的 zod schema 里是
        // `tool_use_id: z.string()`（无 `.nullish()`）。此前这里不带该字段，注释
        // 自称 "Contract A"，依据是已删除的 `generate_websearch_events`（单工具
        // 快速路径的产物）。实测：SDK 直接以 `Invalid JSON response` 拒绝整个响应，
        // 客户端拿不到任何内容。
        let (srv_id, _mcp) = websearch::create_mcp_request(&query);
        presentation.push(json!({
            "type": "server_tool_use", "id": srv_id, "name": "web_search",
            "input": {"query": query}
        }));
        presentation.push(json!({
            "type": "web_search_tool_result",
            "tool_use_id": srv_id,
            "content": build_result_block(results)
        }));
    }
    payload.messages.push(Message {
        role: "user".to_string(),
        content: Value::Array(user_content),
    });
}

/// Converts search results into an array of web_search_result blocks (Contract A fields)
fn build_result_block(results: &Option<WebSearchResults>) -> Vec<Value> {
    match results {
        Some(r) => r
            .results
            .iter()
            .map(|item| {
                let page_age = item.published_date.and_then(|ms| {
                    chrono::DateTime::from_timestamp_millis(ms)
                        .map(|dt| dt.format("%B %-d, %Y").to_string())
                });
                json!({
                    "type": "web_search_result",
                    "title": item.title,
                    "url": item.url,
                    "encrypted_content": item.snippet.clone().unwrap_or_default(),
                    "page_age": page_age
                })
            })
            .collect(),
        None => vec![],
    }
}

/// Splits a round's tool_uses into (web_search calls, client tool calls),
/// preserving order within each group. This is the structural core of the
/// invariant "web_search is always handled internally and never leaves kiro-rs
/// as a raw tool_use": every flush path partitions first, then handles each
/// group differently (web_search -> presentation blocks, client tools -> raw).
fn partition_tool_uses(
    tool_uses: &[CompletedToolUse],
) -> (Vec<&CompletedToolUse>, Vec<&CompletedToolUse>) {
    let mut web = Vec::new();
    let mut client = Vec::new();
    for tu in tool_uses {
        if tu.name == "web_search" {
            web.push(tu);
        } else {
            client.push(tu);
        }
    }
    (web, client)
}

/// Resolves the final `stop_reason` for a flushed web_search-loop response.
///
/// Inputs:
/// - `override_reason`: an upstream-forced terminal reason (max_tokens /
///   model_context_window_exceeded). When present it always wins.
/// - `client_uses_empty`: whether the round had NO structured client tool_use.
/// - `content`: the FINAL flushed content (after the `<invoke>` fault tolerance may have
///   reclaimed a structured tool_use out of the assistant text).
///
/// Rules:
/// 1. An upstream override always wins (verbatim).
/// 2. Otherwise, if the final content contains a real (non-web_search) `tool_use` block,
///    the reason MUST be `tool_use` — this covers BOTH the structured case and the
///    reclaimed-from-text case (the common leak: model emits the call as text, so
///    `client_uses_empty` is true but a tool_use was reclaimed into `content`).
/// 3. Otherwise fall back to the structured signal: `tool_use` if the round had a client
///    tool_use, else `end_turn` (web_search-only rounds end as end_turn).
fn resolve_flush_stop_reason(
    override_reason: Option<&str>,
    client_uses_empty: bool,
    content: &[Value],
) -> String {
    if let Some(r) = override_reason {
        return r.to_string();
    }
    let has_client_tool_use = content
        .iter()
        .any(|c| c["type"] == "tool_use" && c["name"] != "web_search");
    if has_client_tool_use || !client_uses_empty {
        "tool_use".to_string()
    } else {
        "end_turn".to_string()
    }
}

/// Canonical, order-independent key for a tool_use `input` JSON value, used to
/// detect that a reclaimed-from-text tool_use is identical to a structured one.
/// `serde_json::Value`'s `Map` is a BTreeMap (or preserves order when the
/// `preserve_order` feature is on); to be robust we serialize via a BTreeMap so
/// key order never affects equality.
fn canonical_input_key(input: &Value) -> String {
    match input {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<&String, &Value> = map.iter().collect();
            serde_json::to_string(&sorted).unwrap_or_else(|_| input.to_string())
        }
        _ => input.to_string(),
    }
}

/// Builds the final flush content with the web_search invariant baked in:
/// - any web_search tool_use becomes a `server_tool_use` + `web_search_tool_result`
///   presentation pair (NEVER a raw `tool_use`, which the client host rejects);
/// - client tools (exec, get_time, ...) are returned verbatim as raw `tool_use`.
///
/// `searched` corresponds one-to-one (same order) to `tool_uses`; entries for
/// web_search carry the already-completed search results, client-tool entries
/// are ignored (typically None).
///
/// `known_tool_names` is the set of tool names declared by the current request
/// (client short/long names). It is used to run the SAME `<invoke>` text-leak fault
/// tolerance as the streaming path (`stream.rs`): when the upstream model degrades
/// and emits a literal `<invoke name="...">...</invoke>` inside its assistant TEXT,
/// we reclaim it into a structured `tool_use` instead of passing the raw XML through.
/// The web_search loop builds its own SSE/content and historically bypassed that
/// fault tolerance entirely — this is the fix.
fn build_flush_content(
    presentation: Vec<Value>,
    text: &str,
    tool_uses: &[CompletedToolUse],
    searched: &[Option<WebSearchResults>],
    known_tool_names: &std::collections::HashSet<String>,
    tool_name_map: &std::collections::HashMap<String, String>,
) -> Result<Vec<Value>, ToolJsonAccumulatorError> {
    let mut content: Vec<Value> = presentation;
    if !text.is_empty() {
        // Run the shared one-shot `<invoke>` sniffer: splits `text` into a sequence of
        // text blocks + reclaimed structured tool_use blocks (same safety gates as the
        // streaming fault tolerance). For clean text with no leaked `<invoke>`, this
        // returns a single text block identical to the old behavior.
        //
        // INVARIANT GUARD: `web_search` must NEVER be reclaimed as a raw client `tool_use`
        // — the client host has no web_search executor and rejects it with
        // "unsupported call: web_search". `known_tool_names` is copied verbatim from
        // req.tools and (since we are in the web_search loop) always contains "web_search",
        // so we strip it from the reclamation tool-table here. A leaked
        // `<invoke name="web_search">` then fails the tool-table gate and stays as plain
        // text (ugly but protocol-safe), instead of being upgraded into a raw tool_use that
        // breaks the loop's core invariant.
        let reclaim_tools: std::collections::HashSet<String> = known_tool_names
            .iter()
            .filter(|n| n.as_str() != "web_search")
            .cloned()
            .collect();
        // DEDUP GUARD: a degraded model can emit BOTH a leaked literal `<invoke>` in the
        // text AND the matching structured tool_use in `tool_uses`. Emitting both would
        // make the host execute the same command twice. Suppress any reclaimed-from-text
        // tool_use whose (name + canonical input) already appears in the structured
        // `tool_uses` for this round. Text blocks (and distinct tool_uses) are kept as-is.
        let structured_keys: std::collections::HashSet<(String, String)> = tool_uses
            .iter()
            .filter(|t| t.name != "web_search")
            .map(|t| (t.name.clone(), canonical_input_key(&t.input)))
            .collect();
        // 坏 JSON 上抛：与主路径 ToolJsonAccumulator 同口径，绝不降级成空参数。
        let reclaimed = match super::stream::extract_invoke_content_blocks(
            text,
            &reclaim_tools,
            tool_name_map,
        ) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("{}", e);
                return Err(e);
            }
        };
        for block in reclaimed {
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let key = (
                    name.to_string(),
                    block
                        .get("input")
                        .map(canonical_input_key)
                        .unwrap_or_default(),
                );
                if structured_keys.contains(&key) {
                    // identical to a structured tool_use already emitted below -> drop the
                    // reclaimed duplicate (avoid double execution).
                    continue;
                }
            }
            content.push(block);
        }
    }
    for (idx, tu) in tool_uses.iter().enumerate() {
        if tu.name == "web_search" {
            // INVARIANT: present as server_tool_use + web_search_tool_result,
            // never as a raw tool_use.
            let query = tool_query(tu).unwrap_or_default();
            let (srv_id, _mcp) = websearch::create_mcp_request(&query);
            content.push(json!({
                "type": "server_tool_use", "id": srv_id, "name": "web_search",
                "input": {"query": query}
            }));
            let results: &Option<WebSearchResults> = searched.get(idx).unwrap_or(&None);
            // tool_use_id 必填且须与上面的 server_tool_use.id 一致（官方 schema）。
            content.push(json!({
                "type": "web_search_tool_result",
                "tool_use_id": srv_id,
                "content": build_result_block(results)
            }));
        } else {
            // Client tool (exec, get_time, ...): returned to the client verbatim.
            content.push(tu.to_anthropic_block());
        }
    }
    Ok(content)
}

/// web_search loop entry point
///
/// `stream_client`: whether the client wants SSE (true) or a single JSON response (false).
/// `tracer`: 由 `post_messages` 在分支前构造，本函数负责在**每条**出口调用一次
/// `finalize`（含 `run_round` 内部提前返回的失败路径）。少调一次，Admin 日志页
/// 就会整类请求隐身；多调一次，则会多出一条 0 跳的假记录。
pub(super) async fn run_web_search_loop(
    provider: Arc<KiroProvider>,
    mut payload: MessagesRequest,
    hook: UsageRecordHook,
    stream_client: bool,
    group: Option<String>,
    tracer: Arc<RequestTracer>,
) -> Response {
    let fallback_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    let mut presentation: Vec<Value> = Vec::new();
    let mut last_credential_id: u64 = 0;
    let mut last_context_input: Option<i32> = None;
    let mut total_credits = 0.0;
    let mut latest_metering: Option<MeteringEvent> = None;
    let mut all_thinking = String::new();

    for round_idx in 0..=MAX_WEB_SEARCH_ROUNDS {
        let mut empty_retries = 0usize;
        let round = loop {
            let (round, credential_id) =
                match run_round(
                    &provider,
                    &payload,
                    &hook,
                    fallback_input_tokens,
                    group.as_deref(),
                    tracer.as_ref(),
                )
                .await
                {
                    // run_round 的失败路径已自行 finalize，这里不能再补一次。
                    Ok(v) => v,
                    Err(resp) => return resp,
                };
            last_credential_id = credential_id;
            last_context_input = round.context_input_tokens.or(last_context_input);
            total_credits += round.credits;
            // 跨 round 保留最近一次 meteringEvent，多 round 时取最后一次
            // (clone 以避免与 empty_tool_result_disposition 后续对 round 的借用冲突)。
            if let Some(ref m) = round.last_metering {
                latest_metering = Some(m.clone());
            }

            match empty_tool_result_disposition(&payload, &round, empty_retries) {
                EmptyToolResultDisposition::Accept => {}
                EmptyToolResultDisposition::Retry => {
                    empty_retries += 1;
                    tracing::warn!(
                        round = round_idx,
                        retry = empty_retries,
                        "upstream returned an empty assistant turn after tool_result; retrying"
                    );
                    continue;
                }
                EmptyToolResultDisposition::Fail => {
                    let final_input = last_context_input.unwrap_or(fallback_input_tokens);
                    let message =
                        "Upstream returned no assistant text or tool call after a tool result."
                            .to_string();
                    hook.record(
                        last_credential_id,
                        final_input,
                        0,
                        total_credits,
                        "error",
                    );
                    trace_error(
                        tracer.as_ref(),
                        trace_outcome::UNKNOWN,
                        &message,
                        final_input,
                        total_credits,
                    );
                    tracing::error!(
                        round = round_idx,
                        "upstream repeated an empty assistant turn after tool_result"
                    );
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(ErrorResponse::new("upstream_error", message)),
                    )
                        .into_response();
                }
            }

            // Only surface reasoning from the accepted attempt. An empty attempt is
            // discarded and retried, so replaying its hidden reasoning would duplicate
            // or contradict the successful attempt's summary.
            if !round.thinking.is_empty() {
                if !all_thinking.is_empty() {
                    all_thinking.push_str("\n\n");
                }
                all_thinking.push_str(&round.thinking);
            }

            break round;
        };

        if should_search_round(round_idx, &round.tool_uses) {
            // Real search: if any one fails -> propagate the error, never silently turn it into "No results found"
            let mut searched: Vec<Option<WebSearchResults>> = Vec::with_capacity(round.tool_uses.len());
            for tu in &round.tool_uses {
                let Some(query) = tool_query(tu) else {
                    let err = invalid_web_search_input_error(tu);
                    hook.record(
                        last_credential_id,
                        fallback_input_tokens,
                        0,
                        total_credits,
                        "error",
                    );
                    trace_error(
                        tracer.as_ref(),
                        trace_outcome::BAD_REQUEST,
                        &err.to_string(),
                        fallback_input_tokens,
                        total_credits,
                    );
                    return map_provider_error(err);
                };
                log_normalized_web_search_query(tu, &query);
                let (_id, mcp_request) = websearch::create_mcp_request(&query);
                match websearch::call_mcp_api(&provider, &mcp_request, group.as_deref()).await {
                    Ok(resp) => searched.push(websearch::parse_search_results(&resp)),
                    Err(e) if is_no_results_mcp_error(&e) => {
                        tracing::warn!("web_search MCP returned no results; continuing with an empty result");
                        searched.push(None);
                    }
                    Err(e) => {
                        tracing::warn!("web_search MCP call failed: {}", e);
                        hook.record(
                            last_credential_id,
                            fallback_input_tokens,
                            0,
                            total_credits,
                            "error",
                        );
                        // call_mcp 不接 TraceSink（MCP 走独立端点，不参与凭据重试
                        // 链路的 attempt 登记），故这里没有可提升的分类，记 UNKNOWN
                        // 并靠 error_message 带出真实原因。
                        trace_error(
                            tracer.as_ref(),
                            trace_outcome::UNKNOWN,
                            &e.to_string(),
                            fallback_input_tokens,
                            total_credits,
                        );
                        return map_provider_error(e);
                    }
                }
            }
            append_search_round(&mut payload, &round, &searched, &mut presentation);
            continue;
        }

        // Terminate: this round is not "pure web_search", or the limit has been reached -> flush to the client.
        // stop_reason must reflect CLIENT tools only: web_search is handled internally
        // (presented as server_tool_use, not a pending tool_use), so a round with only
        // web_search must end as "end_turn", not "tool_use" (otherwise the host would
        // wait for a client tool call that is never emitted).
        let (_web_uses, client_uses) = partition_tool_uses(&round.tool_uses);
        let final_input = last_context_input.unwrap_or(fallback_input_tokens);
        // INVARIANT: web_search is ALWAYS executed internally and is NEVER flushed
        // as a raw tool_use (the client host has no executor for it and rejects it
        // with "unsupported call: web_search"). This covers the mixed-round case
        // (web_search + exec) and the round-limit case: search every web_search call
        // in this final round here, then build the flushed content with web_search
        // presented as server_tool_use + web_search_tool_result while client tools
        // (exec, etc.) are returned verbatim.
        let mut searched: Vec<Option<WebSearchResults>> = Vec::with_capacity(round.tool_uses.len());
        for tu in &round.tool_uses {
            if tu.name == "web_search" {
                let Some(query) = tool_query(tu) else {
                    let err = invalid_web_search_input_error(tu);
                    hook.record(
                        last_credential_id,
                        fallback_input_tokens,
                        0,
                        total_credits,
                        "error",
                    );
                    trace_error(
                        tracer.as_ref(),
                        trace_outcome::BAD_REQUEST,
                        &err.to_string(),
                        fallback_input_tokens,
                        total_credits,
                    );
                    return map_provider_error(err);
                };
                log_normalized_web_search_query(tu, &query);
                let (_id, mcp_request) = websearch::create_mcp_request(&query);
                match websearch::call_mcp_api(&provider, &mcp_request, group.as_deref()).await {
                    Ok(resp) => searched.push(websearch::parse_search_results(&resp)),
                    Err(e) if is_no_results_mcp_error(&e) => {
                        tracing::warn!("web_search MCP returned no results in final round; continuing with an empty result");
                        searched.push(None);
                    }
                    Err(e) => {
                        tracing::warn!("web_search MCP call (final round) failed: {}", e);
                        hook.record(
                            last_credential_id,
                            fallback_input_tokens,
                            0,
                            total_credits,
                            "error",
                        );
                        trace_error(
                            tracer.as_ref(),
                            trace_outcome::UNKNOWN,
                            &e.to_string(),
                            fallback_input_tokens,
                            total_credits,
                        );
                        return map_provider_error(e);
                    }
                }
            } else {
                searched.push(None);
            }
        }
        let content = match build_flush_content(
            presentation.clone(),
            &round.text,
            &round.tool_uses,
            &searched,
            &round.known_tool_names,
            &round.tool_name_map,
        ) {
            Ok(c) => c,
            // 上游 `<invoke>` 泄漏里的 JSON 非法：与断流同等对待，不降级成空参数。
            Err(e) => {
                tracing::error!("{}", e);
                let message = e.message();
                hook.record(0, fallback_input_tokens, 0, 0.0, "error");
                trace_error(
                    tracer.as_ref(),
                    trace_outcome::BAD_REQUEST,
                    &message,
                    fallback_input_tokens,
                    total_credits,
                );
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(ErrorResponse::new(e.error_type(), message)),
                )
                    .into_response();
            }
        };
        // stop_reason must be computed from the FINAL flushed content, not just
        // round.tool_uses: the <invoke> fault tolerance can reclaim a structured tool_use
        // out of the assistant text (the common leak case where the model emits the call as
        // text and round.tool_uses is empty). See resolve_flush_stop_reason for the rules.
        let stop_reason = resolve_flush_stop_reason(
            round.stop_reason_override.as_deref(),
            client_uses.is_empty(),
            &content,
        );

        let output_tokens = token::estimate_output_tokens(&content);
        hook.record(
            last_credential_id,
            final_input,
            output_tokens,
            total_credits,
            "success",
        );
        // 与 usage_log 同源的用量快照落进 trace 行；attempts 已由 provider 在
        // 每一轮的重试循环里累积（跨 round 累加，一条 trace = 一次客户端请求）。
        tracer.finalize(
            "success",
            None,
            None,
            None,
            TraceUsage {
                input_tokens: final_input.max(0) as u64,
                output_tokens: output_tokens.max(0) as u64,
                credits: if total_credits.is_finite() && total_credits > 0.0 {
                    total_credits
                } else {
                    0.0
                },
            },
        );

        return if stream_client {
            render_sse(
                &payload.model,
                content,
                &stop_reason,
                final_input,
                output_tokens,
                latest_metering.as_ref(),
            )
        } else {
            render_json(
                &payload.model,
                content,
                &stop_reason,
                final_input,
                output_tokens,
                &all_thinking,
                latest_metering.as_ref(),
            )
        };
    }

    // Theoretically unreachable (the loop always returns)
    hook.record(last_credential_id, fallback_input_tokens, 0, total_credits, "error");
    trace_error(
        tracer.as_ref(),
        trace_outcome::UNKNOWN,
        "web_search loop exited unexpectedly",
        fallback_input_tokens,
        total_credits,
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new("internal_error", "web_search loop exited unexpectedly")),
    )
        .into_response()
}

/// Single JSON response (non-streaming)
///
/// `thinking`: optional out-of-band reasoning text. Emitted as a TOP-LEVEL
/// `kiro_thinking` field (NOT a content block): Anthropic clients ignore
/// unknown top-level fields and thus never replay an unsigned thinking block
/// upstream, while the Responses translator picks it up for codex's
/// reasoning-summary display.
pub(crate) fn render_json(
    model: &str,
    content: Vec<Value>,
    stop_reason: &str,
    input_tokens: i32,
    output_tokens: i32,
    thinking: &str,
    metering: Option<&MeteringEvent>,
) -> Response {
    let mut usage = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens
    });
    // 透传上游 meteringEvent 的 credit_* 字段，让客户端拿到与 Kiro 后端口径
    // 一致的计费元数据；只在收到过 meteringEvent 时才追加。
    if let Some(m) = metering {
        usage["credit_usage"] = json!(m.usage);
        usage["credit_unit"] = json!(m.unit);
        usage["credit_unit_plural"] = json!(m.unit_plural);
    }
    let mut body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage
    });
    if !thinking.is_empty() {
        body["kiro_thinking"] = json!(thinking);
    }
    (StatusCode::OK, Json(body)).into_response()
}

/// SSE response (streaming): splits the final content into a sequence of Anthropic content_block events
pub(crate) fn render_sse(
    model: &str,
    content: Vec<Value>,
    stop_reason: &str,
    input_tokens: i32,
    output_tokens: i32,
    metering: Option<&MeteringEvent>,
) -> Response {
    let events = build_sse_events(model, content, stop_reason, input_tokens, output_tokens, metering);
    let stream = stream::iter(
        events
            .into_iter()
            .map(|e| Ok::<Bytes, Infallible>(Bytes::from(e.to_sse_string()))),
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Renders the final content array into a sequence of SSE events
fn build_sse_events(
    model: &str,
    content: Vec<Value>,
    stop_reason: &str,
    input_tokens: i32,
    output_tokens: i32,
    metering: Option<&MeteringEvent>,
) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let message_id = format!(
        "msg_{}",
        &Uuid::new_v4().to_string().replace('-', "")[..24]
    );

    events.push(SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": 0
                }
            }
        }),
    ));

    for (index, block) in content.iter().enumerate() {
        let index = index as i32;
        let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match btype {
            "text" => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                events.push(SseEvent::new("content_block_start", json!({
                    "type": "content_block_start", "index": index,
                    "content_block": {"type": "text", "text": ""}
                })));
                events.push(SseEvent::new("content_block_delta", json!({
                    "type": "content_block_delta", "index": index,
                    "delta": {"type": "text_delta", "text": text}
                })));
                events.push(SseEvent::new("content_block_stop", json!({
                    "type": "content_block_stop", "index": index
                })));
            }
            "tool_use" => {
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let partial = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                events.push(SseEvent::new("content_block_start", json!({
                    "type": "content_block_start", "index": index,
                    "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
                })));
                events.push(SseEvent::new("content_block_delta", json!({
                    "type": "content_block_delta", "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": partial}
                })));
                events.push(SseEvent::new("content_block_stop", json!({
                    "type": "content_block_stop", "index": index
                })));
            }
            "server_tool_use" | "web_search_tool_result" => {
                events.push(SseEvent::new("content_block_start", json!({
                    "type": "content_block_start", "index": index,
                    "content_block": block
                })));
                events.push(SseEvent::new("content_block_stop", json!({
                    "type": "content_block_stop", "index": index
                })));
            }
            _ => {}
        }
    }

    let mut message_delta_usage = json!({ "output_tokens": output_tokens });
    // 透传上游 meteringEvent 的 credit_* 字段（仅在拿到 meteringEvent 时）。
    if let Some(m) = metering {
        message_delta_usage["credit_usage"] = json!(m.usage);
        message_delta_usage["credit_unit"] = json!(m.unit);
        message_delta_usage["credit_unit_plural"] = json!(m.unit_plural);
    }
    events.push(SseEvent::new("message_delta", json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason},
        "usage": message_delta_usage
    })));
    events.push(SseEvent::new("message_stop", json!({"type": "message_stop"})));

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::trace_db::{TraceKeySource, TraceQuery, TraceStore};
    use crate::anthropic::handlers::RequestTraceOptions;
    use crate::anthropic::middleware::{AppState, KeyContext};
    use crate::anthropic::websearch::{WebSearchResult, WebSearchResults};

    /// 回归锁：web_search loop 的失败路径必须落一条 trace 行，且带上模型名与
    /// 已知用量。
    ///
    /// 这条锁的由来：loop 此前完全不构造 `RequestTracer`（`call_api_stream` 传
    /// None），于是凡是 tools 里含 web_search 的请求在 Admin 日志页一条不显示 ——
    /// 而 `/v1/responses` 恒注入原生 web_search，等于整个 GPT 组隐身。usage 统计
    /// 有数、链路日志没数，排查时会误判成"请求没到"。
    #[test]
    fn websearch_loop_failure_lands_one_trace_row() {
        let store: crate::admin::trace_db::SharedTraceStore =
            Arc::new(TraceStore::open_in_memory().expect("内存 trace store"));
        let state = AppState::new(false).with_trace_store(Some(store.clone()));
        let tracer = RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: KeyContext {
                    key_id: 7,
                    group: None,
                    key_source: TraceKeySource::ClientKey,
                },
                model: "gpt-5.6-sol".to_string(),
                is_stream: false,
            },
        );

        trace_error(&tracer, trace_outcome::UNKNOWN, "mcp call failed", 1234, 0.5);

        let (rows, total) = store.query_paged(&TraceQuery {
            model: Some("gpt-5.6-sol".to_string()),
            ..Default::default()
        });
        assert_eq!(total, 1, "失败路径必须恰好落一条 trace 行");
        assert_eq!(rows[0].final_status, "error");
        assert_eq!(rows[0].error_type.as_deref(), Some(trace_outcome::UNKNOWN));
        assert_eq!(rows[0].error_message.as_deref(), Some("mcp call failed"));
        // 用量不能一律记 0：input_tokens / credits 是失败请求的唯一成本线索。
        assert_eq!(rows[0].input_tokens, 1234);
        assert_eq!(rows[0].output_tokens, 0);
        assert_eq!(rows[0].credits, 0.5);
    }

    fn tu(name: &str) -> CompletedToolUse {
        CompletedToolUse {
            id: format!("toolu_{}", name),
            name: name.to_string(),
            input: json!({"query": "rust 2026"}),
        }
    }

    fn tu_with_input(input: Value) -> CompletedToolUse {
        CompletedToolUse {
            id: "toolu_web_search".to_string(),
            name: "web_search".to_string(),
            input,
        }
    }

    #[test]
    fn tool_query_normalizes_supported_input_shapes() {
        assert_eq!(tool_query(&tu_with_input(json!({"query": "  rust 2026  "}))), Some("rust 2026".to_string()));
        assert_eq!(tool_query(&tu_with_input(json!({"search_query": "南京演唱会"}))), Some("南京演唱会".to_string()));
        assert_eq!(tool_query(&tu_with_input(json!({"queries": ["", "上海天气"]}))), Some("上海天气".to_string()));
        assert_eq!(tool_query(&tu_with_input(json!({"query": {"text": "Paris weather"}}))), Some("Paris weather".to_string()));
    }

    #[test]
    fn tool_query_rejects_missing_or_non_string_input() {
        assert_eq!(tool_query(&tu_with_input(json!({"query": "   "}))), None);
        assert_eq!(tool_query(&tu_with_input(json!({"query": 42}))), None);
        assert_eq!(tool_query(&tu_with_input(json!({"other": true}))), None);
    }

    /// 回归锁：模型给的 web_search 入参无可用 query 时，**必须失败**，
    /// 不得降级成空搜索结果。
    ///
    /// 上游此处是 `warn` + `searched.push(None)`，那等于对模型说「搜过了，没找到」，
    /// 而真实情况是「我们没搜」——模型会据此断言「查无此事」。这是 AGENTS.md §2.5
    /// 禁止的「替客户端制造不存在的结果」。留 warn 不够，必须让请求显式失败。
    #[test]
    fn invalid_web_search_input_is_an_error_not_an_empty_result() {
        for input in [
            json!({"query": "   "}),
            json!({"query": 42}),
            json!({"other": true}),
            json!({}),
            json!(null),
        ] {
            let tu = tu_with_input(input.clone());
            assert!(
                tool_query(&tu).is_none(),
                "{input} 不该被解析出 query"
            );
            // 该形态下必须走 map_provider_error，落到非 2xx。
            let resp = map_provider_error(invalid_web_search_input_error(&tu));
            assert!(
                !resp.status().is_success(),
                "{input}: 畸形入参必须失败，不得伪造空结果"
            );
        }
    }

    /// 与上一条互补：MCP 明确回「无结果」是**正常业务态**，不是错误 ——
    /// 上游确实搜了，只是没命中。这种情况继续用空结果是对的。
    #[test]
    fn no_results_mcp_error_is_nonfatal() {
        assert!(is_no_results_mcp_error(&anyhow::anyhow!("MCP error: -32602 - Tool returned no results")));
        assert!(!is_no_results_mcp_error(&anyhow::anyhow!("MCP error: -32602 - Invalid tool parameters provided")));
    }

    /// Build a known-tool-names set for build_flush_content tests.
    fn names(ns: &[&str]) -> std::collections::HashSet<String> {
        ns.iter().map(|s| s.to_string()).collect()
    }

    /// Empty short->original tool name map for build_flush_content tests.
    fn nomap() -> std::collections::HashMap<String, String> {
        std::collections::HashMap::new()
    }

    // ---- should_search_round: hit / skip / limit reached ----

    #[test]
    fn round_with_only_web_search_continues() {
        // Hit: this round is all web_search and the limit is not reached -> keep searching
        let tools = vec![tu("web_search"), tu("web_search")];
        assert!(should_search_round(0, &tools));
        assert!(should_search_round(MAX_WEB_SEARCH_ROUNDS - 1, &tools));
    }

    #[test]
    fn round_with_exec_does_not_enter_loop() {
        // Skip: exec mixed in (not web_search) -> terminate, exec returned to the client as-is
        let mixed = vec![tu("web_search"), tu("exec")];
        assert!(!should_search_round(0, &mixed));
        // Same for exec-only
        let exec_only = vec![tu("exec")];
        assert!(!should_search_round(0, &exec_only));
    }

    #[test]
    fn round_with_no_tool_use_does_not_enter_loop() {
        // Skip: no tool_use at all (plain-text answer) -> terminate
        let empty: Vec<CompletedToolUse> = vec![];
        assert!(!should_search_round(0, &empty));
    }

    fn round_outcome(text: &str, tool_uses: Vec<CompletedToolUse>) -> RoundOutcome {
        RoundOutcome {
            text: text.to_string(),
            thinking: String::new(),
            tool_uses,
            context_input_tokens: None,
            credits: 0.0,
            last_metering: None,
            stop_reason_override: None,
            stream_error: false,
            tool_json_error: None,
            known_tool_names: std::collections::HashSet::new(),
            tool_name_map: std::collections::HashMap::new(),
        }
    }

    fn payload_with_last_block(block: Value) -> MessagesRequest {
        MessagesRequest {
            model: "claude-opus-5".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!([block]),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            effort: None,
            metadata: None,
        }
    }

    #[test]
    fn empty_round_after_tool_result_retries_once_then_fails() {
        let payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("", vec![]), 0),
            EmptyToolResultDisposition::Retry
        );
        assert_eq!(
            empty_tool_result_disposition(
                &payload,
                &round_outcome("", vec![]),
                MAX_EMPTY_TOOL_RESULT_RETRIES,
            ),
            EmptyToolResultDisposition::Fail
        );
    }

    #[test]
    fn text_or_tool_call_after_tool_result_is_not_retried() {
        let payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("finished", vec![]), 0),
            EmptyToolResultDisposition::Accept
        );
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("", vec![tu("exec")]), 0),
            EmptyToolResultDisposition::Accept
        );
    }

    #[test]
    fn empty_initial_round_is_not_misclassified_as_tool_continuation() {
        let payload = payload_with_last_block(json!({"type": "text", "text": "hello"}));
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("", vec![]), 0),
            EmptyToolResultDisposition::Accept
        );
    }

    #[test]
    fn whitespace_and_reasoning_only_after_tool_result_is_retried() {
        let payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        let mut round = round_outcome(" \n\t", vec![]);
        round.thinking = "hidden reasoning without a client-visible continuation".to_string();
        assert_eq!(
            empty_tool_result_disposition(&payload, &round, 0),
            EmptyToolResultDisposition::Retry
        );
    }

    #[test]
    fn terminal_limit_reason_after_tool_result_is_not_retried() {
        let payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        let mut round = round_outcome("", vec![]);
        round.stop_reason_override = Some("max_tokens".to_string());
        assert_eq!(
            empty_tool_result_disposition(&payload, &round, 0),
            EmptyToolResultDisposition::Accept
        );
    }

    #[test]
    fn only_the_last_message_determines_tool_continuation() {
        let mut payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        payload.messages.push(Message {
            role: "user".to_string(),
            content: json!([{"type": "text", "text": "new user turn"}]),
        });
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("", vec![]), 0),
            EmptyToolResultDisposition::Accept
        );
    }

    #[test]
    fn round_at_limit_stops_even_if_web_search() {
        // Limit reached: even if this round is all web_search, hitting the limit must stop (prevents an infinite loop)
        let tools = vec![tu("web_search")];
        assert!(!should_search_round(MAX_WEB_SEARCH_ROUNDS, &tools));
        assert!(!should_search_round(MAX_WEB_SEARCH_ROUNDS + 1, &tools));
    }

    // ---- build_result_block: search results -> Contract A web_search_result fields ----

    #[test]
    fn result_block_maps_contract_a_fields() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Rust 1.99".to_string(),
                url: "https://example.com/rust".to_string(),
                snippet: Some("Rust 1.99 released".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("rust".to_string()),
            error: None,
        };
        let block = build_result_block(&Some(results));
        assert_eq!(block.len(), 1);
        assert_eq!(block[0]["type"], "web_search_result");
        assert_eq!(block[0]["title"], "Rust 1.99");
        assert_eq!(block[0]["url"], "https://example.com/rust");
        assert_eq!(block[0]["encrypted_content"], "Rust 1.99 released");
    }

    #[test]
    fn result_block_none_is_empty() {
        // No results -> empty block (does not fabricate content)
        assert!(build_result_block(&None).is_empty());
    }

    // ---- search-failure pass-through: an Err from the MCP call must map to an error response, never silently become a 200 "No results found" ----

    #[test]
    fn mcp_failure_maps_to_error_response_not_silent_success() {
        // When the loop gets Err from call_mcp_api it directly `return map_provider_error(e)`,
        // before any generate_search_summary, so a search failure can never turn into a successful summary response.
        // This verifies that map_provider_error returns a non-2xx (BAD_GATEWAY) for a generic MCP error,
        // rather than 200, proving the pass-through path cannot produce a false green.
        let err = anyhow::anyhow!("MCP error: -1 - upstream unavailable");
        let resp = map_provider_error(err);
        assert!(
            !resp.status().is_success(),
            "a failed MCP search must return an error status and must not silently succeed"
        );
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    /// 回归锁：MCP 搜索被上游限流时，必须原样传出 429 + `Retry-After`，
    /// **不能降级成「搜到 0 条」的 200**。
    ///
    /// 该断言原先挂在 `websearch::finish_mcp_call` 上（单工具快速路径的收尾函数）。
    /// 快速路径已删除，限流现在经 agentic loop 的 `map_provider_error` 传出，
    /// 语义不变，故断言随之搬来这里 —— 不是删测试，是让它跟着被测路径走。
    #[test]
    fn mcp_rate_limit_passes_through_429_with_retry_after() {
        let err = crate::kiro::error::UpstreamRateLimitError::new(Some("60".to_string()));
        let resp = map_provider_error(err.into());
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers().get(axum::http::header::RETRY_AFTER).unwrap(),
            "60"
        );
    }

    // ---- build_sse_events: present server_tool_use + result, and the exec tool_use is not swallowed ----

    #[test]
    fn sse_events_render_search_presentation_and_keep_exec() {
        let content = vec![
            json!({"type": "server_tool_use", "id": "srvtoolu_x", "name": "web_search", "input": {"query": "q"}}),
            json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_x", "content": []}),
            json!({"type": "text", "text": "done"}),
            json!({"type": "tool_use", "id": "toolu_exec", "name": "exec", "input": {"cmd": "ls"}}),
        ];
        let events = build_sse_events("claude-sonnet-4-8", content, "tool_use", 10, 5, None);

        // Must contain message_start / message_delta(stop_reason) / message_stop
        assert_eq!(events.first().unwrap().event, "message_start");
        assert_eq!(events.last().unwrap().event, "message_stop");
        let delta = events.iter().find(|e| e.event == "message_delta").unwrap();
        assert_eq!(delta.data["delta"]["stop_reason"], "tool_use");

        // the server_tool_use block is placed into content_block_start as-is
        let has_server_tool = events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "server_tool_use"
        });
        assert!(has_server_tool, "the server_tool_use block should be presented");

        // the web_search_tool_result block is presented
        let has_result = events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "web_search_tool_result"
        });
        assert!(has_result, "the web_search_tool_result block should be presented");

        // exec tool_use is not swallowed: name=exec appears in start
        let has_exec = events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "tool_use"
                && e.data["content_block"]["name"] == "exec"
        });
        assert!(has_exec, "the exec tool_use must be returned to the client as-is and not swallowed");
    }
    // ---- INVARIANT: web_search must NEVER leave kiro-rs as a raw tool_use ----
    // Regression for the "mixed-round leak": when the final round mixes web_search
    // with a client tool (exec/get_time), the flush content must present web_search
    // as server_tool_use + web_search_tool_result (never raw tool_use), while the
    // client tool is returned verbatim. Previously the flush loop emitted
    // {"type":"tool_use","name":"web_search"} which the client host rejected with
    // "unsupported call: web_search".

    fn fake_results(q: &str) -> Option<WebSearchResults> {
        Some(WebSearchResults {
            results: vec![WebSearchResult {
                title: "T".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some("snip".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some(q.to_string()),
            error: None,
        })
    }

    #[test]
    fn flush_content_mixed_round_never_emits_raw_web_search() {
        let tool_uses = vec![tu("web_search"), tu("exec")];
        let searched = vec![fake_results("rust 2026"), None];
        let content =
            build_flush_content(Vec::new(), "answer", &tool_uses, &searched, &names(&["exec"]), &nomap()).unwrap();

        let raw_web_search = content
            .iter()
            .any(|c| c["type"] == "tool_use" && c["name"] == "web_search");
        assert!(
            !raw_web_search,
            "web_search must never be flushed as a raw tool_use (host rejects it). content={:?}",
            content
        );

        assert!(
            content
                .iter()
                .any(|c| c["type"] == "server_tool_use" && c["name"] == "web_search"),
            "web_search must be presented as server_tool_use"
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "web_search_tool_result"),
            "web_search must carry a web_search_tool_result block"
        );

        // 回归锁：`web_search_tool_result.tool_use_id` 必填，且必须等于同组
        // `server_tool_use.id`。Anthropic 官方 schema 要求它；`@ai-sdk/anthropic`
        // 的 zod 是 `tool_use_id: z.string()`（无 nullish），缺了它 SDK 会以
        // `Invalid JSON response` 拒绝**整个响应**，客户端一个字都拿不到。
        let srv = content
            .iter()
            .find(|c| c["type"] == "server_tool_use")
            .expect("server_tool_use 块必须存在");
        let res = content
            .iter()
            .find(|c| c["type"] == "web_search_tool_result")
            .expect("web_search_tool_result 块必须存在");
        assert!(
            res["tool_use_id"].is_string(),
            "web_search_tool_result 必须带 tool_use_id"
        );
        assert_eq!(
            res["tool_use_id"], srv["id"],
            "tool_use_id 必须与同组 server_tool_use.id 一致"
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "exec"),
            "the exec client tool must be returned to the client as-is"
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "text" && c["text"] == "answer"),
            "assistant text must be preserved"
        );
    }

    #[test]
    fn flush_content_client_tools_only_passthrough() {
        let tool_uses = vec![tu("exec")];
        let searched: Vec<Option<WebSearchResults>> = vec![None];
        let content = build_flush_content(Vec::new(), "", &tool_uses, &searched, &names(&["exec"]), &nomap()).unwrap();
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "exec")
        );
        assert!(!content.iter().any(|c| c["type"] == "server_tool_use"));
    }

    // ---- FIX: web_search loop must run the same <invoke> text-leak fault tolerance ----
    // Root cause: the web_search agentic loop builds its own SSE/content and historically
    // never ran the `<invoke>` fault tolerance that lives in stream.rs. When the upstream
    // model (Kiro Opus, long-context degradation) emits a literal
    // `<invoke name="exec_command">...</invoke>` as assistant TEXT, build_flush_content used
    // to pass it through verbatim as a {"type":"text"} block (the leak). Now it reclaims it.
    fn leaks_literal_invoke(content: &[Value]) -> bool {
        content.iter().any(|c| {
            c["type"] == "text"
                && c["text"]
                    .as_str()
                    .map(|t| t.contains("<invoke name="))
                    .unwrap_or(false)
        })
    }

    #[test]
    fn flush_content_reclaims_leaked_invoke_into_tool_use() {
        // A clean, line-start, closed <invoke> with a known tool name MUST be reclaimed
        // into a structured tool_use and NOT leaked as literal text.
        let leaked = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">echo hi</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        ).unwrap();
        assert!(
            !leaks_literal_invoke(&content),
            "literal <invoke> must not leak as text. content={:?}",
            content
        );
        let reclaimed = content.iter().find(|c| c["type"] == "tool_use");
        assert!(reclaimed.is_some(), "must reclaim a structured tool_use. content={:?}", content);
        let tu = reclaimed.unwrap();
        assert_eq!(tu["name"], "exec_command");
        assert_eq!(tu["input"]["cmd"], "echo hi", "parameter must be parsed into input");
        // the stray `call` line in front of the invoke must be stripped, not leaked
        assert!(
            !content
                .iter()
                .any(|c| c["type"] == "text" && c["text"].as_str() == Some("call\n")),
            "stray token line must be stripped"
        );
    }

    #[test]
    fn flush_content_keeps_real_text_before_leaked_invoke() {
        // Narrative text before the leaked invoke must be preserved as a text block,
        // and the invoke still reclaimed.
        let leaked = "Here is the result.\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">ls</parameter>\n</invoke>";
        let content = build_flush_content(Vec::new(), leaked, &[], &[], &names(&["exec_command"]), &nomap()).unwrap();
        assert!(!leaks_literal_invoke(&content));
        assert!(
            content.iter().any(|c| c["type"] == "text"
                && c["text"].as_str().unwrap_or("").contains("Here is the result.")),
            "narrative text must be preserved. content={:?}",
            content
        );
        assert!(content.iter().any(|c| c["type"] == "tool_use" && c["name"] == "exec_command"));
    }

    // ---- SAFETY GATES: must NOT reclaim (would risk executing discussed commands) ----

    #[test]
    fn flush_content_does_not_reclaim_invoke_inside_code_fence() {
        // An <invoke> shown inside a ``` code fence is a DISPLAY/discussion, not a real call.
        // It must stay as text, never become a tool_use.
        let text = "Look at this example:\n```\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">rm -rf /</parameter>\n</invoke>\n```";
        let content = build_flush_content(Vec::new(), text, &[], &[], &names(&["exec_command"]), &nomap()).unwrap();
        assert!(
            !content.iter().any(|c| c["type"] == "tool_use"),
            "fenced <invoke> must NOT be reclaimed (it's a display). content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_does_not_reclaim_invoke_mid_sentence() {
        // <invoke> embedded mid-sentence (not at line start) is discussion text, not a call.
        let text = "the tag <invoke name=\"exec_command\"><parameter name=\"cmd\">x</parameter></invoke> means a call";
        let content = build_flush_content(Vec::new(), text, &[], &[], &names(&["exec_command"]), &nomap()).unwrap();
        assert!(
            !content.iter().any(|c| c["type"] == "tool_use"),
            "mid-sentence <invoke> must NOT be reclaimed. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_does_not_reclaim_unknown_tool_name() {
        // Tool-table guard: a clean line-start <invoke> whose name is NOT a declared tool
        // must NOT be reclaimed (never synthesize a call for an unknown tool).
        let leaked = "call\n<invoke name=\"definitely_not_a_tool\">\n<parameter name=\"x\">y</parameter>\n</invoke>";
        let content = build_flush_content(Vec::new(), leaked, &[], &[], &names(&["exec_command"]), &nomap()).unwrap();
        assert!(
            !content.iter().any(|c| c["type"] == "tool_use"),
            "unknown tool name must NOT be reclaimed. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_never_reclaims_web_search_as_raw_tool_use() {
        // Reviewer (v2) #3 — the loop's core invariant: a leaked `<invoke name="web_search">`
        // in the assistant TEXT must NEVER be reclaimed into a raw tool_use, even though
        // known_tool_names contains "web_search" (it's always declared on the request that
        // enters this loop). The host has no web_search executor and rejects raw
        // web_search tool_use with "unsupported call: web_search". It must stay as text.
        let leaked = "let me search\n<invoke name=\"web_search\">\n<parameter name=\"query\">latest news</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            // known_tool_names DELIBERATELY contains web_search (mirrors the real request).
            &names(&["web_search", "exec_command"]),
            &nomap(),
        ).unwrap();
        assert!(
            !content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "web_search"),
            "leaked <invoke name=web_search> must NEVER become a raw tool_use. content={:?}",
            content
        );
        // It also must not be mis-presented as a server_tool_use from the text path
        // (only real structured web_search tool_uses become server_tool_use). Staying as
        // text is the protocol-safe outcome here.
        assert!(
            !content.iter().any(|c| c["type"] == "server_tool_use"),
            "text-leaked web_search must not be upgraded to server_tool_use either. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_web_search_guard_does_not_block_other_tools() {
        // Reviewer (v3) #2: stripping web_search from the reclamation table must NOT hurt
        // other tools. A text with BOTH a leaked exec_command and a leaked web_search:
        // exec_command MUST be reclaimed; web_search MUST stay text (never raw tool_use).
        let leaked = "<invoke name=\"exec_command\">\n<parameter name=\"cmd\">ls</parameter>\n</invoke>\n<invoke name=\"web_search\">\n<parameter name=\"query\">news</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["web_search", "exec_command"]),
            &nomap(),
        ).unwrap();
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "exec_command"),
            "exec_command must still be reclaimed. content={:?}",
            content
        );
        assert!(
            !content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "web_search"),
            "web_search must NOT be reclaimed as raw tool_use. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_clean_text_is_single_text_block() {
        // No <invoke> at all -> behavior identical to before: one text block, unchanged.
        let content = build_flush_content(Vec::new(), "just a normal answer", &[], &[], &names(&["exec_command"]), &nomap()).unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "just a normal answer");
    }

    #[test]
    fn flush_content_reclaims_two_burst_invokes() {
        // Two consecutive leaked invokes must both be reclaimed and not bleed into each other.
        let leaked = "<invoke name=\"exec_command\">\n<parameter name=\"cmd\">a</parameter>\n</invoke>\n<invoke name=\"get_time\">\n<parameter name=\"tz\">utc</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["exec_command", "get_time"]),
            &nomap(),
        ).unwrap();
        assert!(!leaks_literal_invoke(&content));
        let tus: Vec<&Value> = content.iter().filter(|c| c["type"] == "tool_use").collect();
        assert_eq!(tus.len(), 2, "both invokes reclaimed. content={:?}", content);
        assert_eq!(tus[0]["name"], "exec_command");
        assert_eq!(tus[0]["input"]["cmd"], "a");
        assert_eq!(tus[1]["name"], "get_time");
        assert_eq!(tus[1]["input"]["tz"], "utc");
    }

    #[test]
    fn flush_content_unclosed_invoke_stays_text() {
        // An <invoke> with no closing tag in the complete text is not a clean call -> keep as text.
        let text = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">echo hi";
        let content = build_flush_content(Vec::new(), text, &[], &[], &names(&["exec_command"]), &nomap()).unwrap();
        assert!(
            !content.iter().any(|c| c["type"] == "tool_use"),
            "unclosed <invoke> must NOT be reclaimed. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_restores_shortened_tool_name() {
        // Reviewer #2: long tool names (>63) are shortened before being sent upstream, so the
        // model leaks the SHORT name. known_tool_names contains the short name (so it's reclaimed),
        // but the reclaimed tool_use MUST carry the ORIGINAL name (host matches on original).
        let short = "mcp__codex_apps__x___list_projects_a1b2c3d4";
        let original = "mcp__codex_apps__sites___list_projects_with_a_very_long_suffix";
        let leaked = format!(
            "call\n<invoke name=\"{}\">\n<parameter name=\"q\">x</parameter>\n</invoke>",
            short
        );
        let mut map = std::collections::HashMap::new();
        map.insert(short.to_string(), original.to_string());
        let content = build_flush_content(
            Vec::new(),
            &leaked,
            &[],
            &[],
            &names(&[short]),
            &map,
        ).unwrap();
        let tu = content
            .iter()
            .find(|c| c["type"] == "tool_use")
            .expect("must reclaim a tool_use");
        assert_eq!(
            tu["name"], original,
            "reclaimed tool name must be restored to the original (not the shortened) name"
        );
    }

    #[test]
    fn flush_content_yields_tool_use_so_caller_sets_tool_use_stop_reason() {
        // Reviewer #1: the common leak case is the model emitting the call as TEXT with NO
        // structured tool_use, so round.tool_uses is empty and the caller's pre-flush
        // stop_reason would be "end_turn". The fix relies on build_flush_content surfacing a
        // reclaimed (non-web_search) tool_use block, which the caller then keys off to force
        // stop_reason="tool_use". This test pins that contract: a leaked invoke with an empty
        // tool_uses list still yields a client tool_use block in the content.
        let leaked = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">echo hi</parameter>\n</invoke>";
        let content = build_flush_content(Vec::new(), leaked, &[], &[], &names(&["exec_command"]), &nomap()).unwrap();
        let has_client_tool_use = content
            .iter()
            .any(|c| c["type"] == "tool_use" && c["name"] != "web_search");
        assert!(
            has_client_tool_use,
            "a reclaimed leak must surface a client tool_use so the caller sets stop_reason=tool_use. content={:?}",
            content
        );
    }

    // ---- resolve_flush_stop_reason: the protocol-consistency core of the fix ----

    #[test]
    fn stop_reason_reclaimed_text_invoke_is_tool_use_not_end_turn() {
        // Reviewer #1 main scenario: model degrades, emits the call as TEXT, so the round had
        // NO structured client tool_use (client_uses_empty = true). After the fault tolerance
        // reclaims a tool_use into content, the reason MUST be tool_use (not end_turn).
        let content = vec![json!({"type":"tool_use","id":"t","name":"exec_command","input":{}})];
        assert_eq!(
            resolve_flush_stop_reason(None, true, &content),
            "tool_use",
            "a reclaimed tool_use must flip stop_reason to tool_use"
        );
    }

    #[test]
    fn stop_reason_web_search_only_stays_end_turn() {
        // A web_search-only flush (presented as server_tool_use) has no client tool_use ->
        // must stay end_turn so the host doesn't wait for a client call that never comes.
        let content = vec![
            json!({"type":"text","text":"answer"}),
            json!({"type":"server_tool_use","id":"s","name":"web_search","input":{"query":"q"}}),
            json!({"type":"web_search_tool_result","tool_use_id":"srvtoolu_x","content":[]}),
        ];
        assert_eq!(resolve_flush_stop_reason(None, true, &content), "end_turn");
    }

    #[test]
    fn stop_reason_structured_client_tool_use_is_tool_use() {
        // Classic structured case: round had a client tool_use -> tool_use.
        let content = vec![json!({"type":"tool_use","id":"t","name":"exec","input":{}})];
        assert_eq!(resolve_flush_stop_reason(None, false, &content), "tool_use");
    }

    #[test]
    fn stop_reason_upstream_override_always_wins() {
        // max_tokens / context_window_exceeded override must win verbatim even if a tool_use
        // was reclaimed.
        let content = vec![json!({"type":"tool_use","id":"t","name":"exec_command","input":{}})];
        assert_eq!(
            resolve_flush_stop_reason(Some("max_tokens"), true, &content),
            "max_tokens"
        );
    }

    #[test]
    fn partition_separates_web_search_from_client_tools() {
        let tool_uses = vec![tu("web_search"), tu("exec"), tu("web_search")];
        let (web, client) = partition_tool_uses(&tool_uses);
        assert_eq!(web.len(), 2, "two web_search calls");
        assert_eq!(client.len(), 1, "one client tool");
        assert_eq!(client[0].name, "exec");
    }

    #[test]
    fn flush_content_only_web_search_has_no_client_tool() {
        // A final round that is only web_search (e.g. round limit hit) must present
        // the search and emit NO raw tool_use at all -> the caller derives end_turn.
        let tool_uses = vec![tu("web_search")];
        let searched = vec![fake_results("q")];
        let content = build_flush_content(Vec::new(), "", &tool_uses, &searched, &names(&[]), &nomap()).unwrap();
        assert!(!content.iter().any(|c| c["type"] == "tool_use"));
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "server_tool_use" && c["name"] == "web_search")
        );
        // client-tool partition is empty -> caller will choose end_turn
        let (_web, client) = partition_tool_uses(&tool_uses);
        assert!(client.is_empty());
    }

    #[test]
    fn flush_content_dedups_reclaimed_against_structured_tool_use() {
        // Degraded models can emit BOTH a leaked literal `<invoke>` in the assistant
        // text AND a structured tool_use for the SAME action. Without dedup the host
        // would receive two identical tool_use blocks and execute the command twice.
        // The reclaimed-from-text tool_use must be suppressed when an identical
        // (name + canonical input) structured tool_use already exists in this round.
        let leaked = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">rm -rf build</parameter>\n</invoke>";
        let structured = vec![CompletedToolUse {
            id: "toolu_dup".to_string(),
            name: "exec_command".to_string(),
            input: json!({"cmd": "rm -rf build"}),
        }];
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &structured,
            &[],
            &names(&["exec_command"]),
            &nomap(),
        ).unwrap();
        let exec_calls = content
            .iter()
            .filter(|c| c["type"] == "tool_use" && c["name"] == "exec_command")
            .count();
        assert_eq!(
            exec_calls, 1,
            "duplicate tool_use (reclaimed + structured) must be de-duped to one. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_keeps_distinct_reclaimed_and_structured() {
        // Dedup must only collapse TRUE duplicates: a reclaimed tool_use with a
        // different input than the structured one is a distinct action and must be kept.
        let leaked = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">ls</parameter>\n</invoke>";
        let structured = vec![CompletedToolUse {
            id: "toolu_other".to_string(),
            name: "exec_command".to_string(),
            input: json!({"cmd": "pwd"}),
        }];
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &structured,
            &[],
            &names(&["exec_command"]),
            &nomap(),
        ).unwrap();
        let exec_calls = content
            .iter()
            .filter(|c| c["type"] == "tool_use" && c["name"] == "exec_command")
            .count();
        assert_eq!(
            exec_calls, 2,
            "distinct inputs must both be kept. content={:?}",
            content
        );
    }

    // ---- credit_usage 透传：run_web_search_loop 路径 ----

    fn metering_event(usage: f64) -> MeteringEvent {
        MeteringEvent {
            unit: "credit".to_string(),
            unit_plural: "credits".to_string(),
            usage,
        }
    }

    #[test]
    fn render_json_carries_credit_fields_when_metering_present() {
        let content = vec![json!({"type": "text", "text": "ok"})];
        let metering = metering_event(0.42);
        let resp = render_json(
            "claude-opus-4-7",
            content,
            "end_turn",
            10,
            5,
            "",
            Some(&metering),
        );
        // 把 Response 的 body 序列化为 JSON 再断言。
        let body = resp.into_body();
        let bytes = futures::executor::block_on(async {
            axum::body::to_bytes(body, 64 * 1024).await.unwrap()
        });
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let usage = &v["usage"];
        assert_eq!(usage["credit_usage"], json!(0.42));
        assert_eq!(usage["credit_unit"], json!("credit"));
        assert_eq!(usage["credit_unit_plural"], json!("credits"));
        // 原有字段保持原样
        assert_eq!(usage["input_tokens"], json!(10));
        assert_eq!(usage["output_tokens"], json!(5));
    }

    #[test]
    fn render_json_omits_credit_fields_without_metering() {
        let content = vec![json!({"type": "text", "text": "ok"})];
        let resp = render_json(
            "claude-opus-4-7",
            content,
            "end_turn",
            10,
            5,
            "",
            None,
        );
        let body = resp.into_body();
        let bytes = futures::executor::block_on(async {
            axum::body::to_bytes(body, 64 * 1024).await.unwrap()
        });
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let usage = &v["usage"];
        assert!(usage.get("credit_usage").is_none());
        assert!(usage.get("credit_unit").is_none());
        assert!(usage.get("credit_unit_plural").is_none());
    }

    #[test]
    fn build_sse_events_carries_credit_fields_in_message_delta() {
        let content = vec![json!({"type": "text", "text": "ok"})];
        let metering = metering_event(0.99);
        let events = build_sse_events(
            "claude-opus-4-7",
            content,
            "end_turn",
            10,
            5,
            Some(&metering),
        );
        let delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert_eq!(usage["credit_usage"], json!(0.99));
        assert_eq!(usage["credit_unit"], json!("credit"));
        assert_eq!(usage["credit_unit_plural"], json!("credits"));
        // 原有字段保持原样
        assert_eq!(usage["output_tokens"], json!(5));
    }

    #[test]
    fn build_sse_events_omits_credit_fields_without_metering() {
        let content = vec![json!({"type": "text", "text": "ok"})];
        let events = build_sse_events(
            "claude-opus-4-7",
            content,
            "end_turn",
            10,
            5,
            None,
        );
        let delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert!(usage.get("credit_usage").is_none());
        assert!(usage.get("credit_unit").is_none());
        assert!(usage.get("credit_unit_plural").is_none());
    }
}
