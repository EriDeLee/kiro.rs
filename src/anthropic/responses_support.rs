//! `/v1/responses` 所需的最小共享辅助
//!
//! 原先这些符号住在 `openai.rs` 里（788 行，含整套 `/v1/chat/completions`
//! 实现）。本部署不提供 `chat/completions` —— 唯一的 OpenAI 端点是
//! `/v1/responses`（Codex CLI / opencode 走它请求 gpt-5.6-sol），所以只把
//! `responses.rs` 真正用到的几个辅助搬过来，其余整体删除。

use serde_json::{Value, json};

pub(super) struct ParsedResponse {
    pub(super) model: String,
    pub(super) text: String,
    pub(super) tool_calls: Vec<Value>, // OpenAI tool_calls
    pub(super) finish_reason: String,
    pub(super) prompt_tokens: i64,
    pub(super) completion_tokens: i64,
    /// 思考文本（content 里的 thinking 块 + web_search loop 的顶层
    /// `kiro_thinking` 带外字段）。chat/completions 路径不消费，
    /// Responses 路径渲染为 reasoning summary item。
    pub(super) thinking: String,
    /// 内部代答的 web_search 展示（server_tool_use 块）：(id, query)。
    /// Responses 路径渲染为 web_search_call item。
    pub(super) web_searches: Vec<(String, String)>,
    /// 上游 meteringEvent 透传的 credit_usage，未下发时为 None。
    /// 与 kiro-rs /v1/chat/completions 行为对齐：仅在拿到 meteringEvent 时
    /// 才把 credit_usage / credit_unit / credit_unit_plural 写入响应 usage。
    pub(super) credit_usage: Option<f64>,
    pub(super) credit_unit: Option<String>,
    pub(super) credit_unit_plural: Option<String>,
}

/// 仅收集纯文本（system / tool 内容用）
pub(super) fn collect_text_strings(content: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    match content {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                out.push(s.clone());
            }
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    if !t.is_empty() {
                        out.push(t.to_string());
                    }
                }
            }
        }
        _ => {}
    }
    out
}

pub(super) fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

pub(super) fn parse_anthropic_message(anthropic: &Value, model: &str) -> ParsedResponse {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut thinking = String::new();
    let mut web_searches = Vec::new();

    if let Some(blocks) = anthropic.get("content").and_then(|v| v.as_array()) {
        for block in blocks {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                    }
                }
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
                        thinking.push_str(t);
                    }
                }
                Some("server_tool_use") => {
                    // 内部代答的 web_search 展示块（websearch_loop Contract A）
                    if block.get("name").and_then(|v| v.as_str()) == Some("web_search") {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let query = block
                            .pointer("/input/query")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        web_searches.push((id, query));
                    }
                }
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let arguments = block
                        .get("input")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments },
                    }));
                }
                _ => {} // web_search_tool_result / 其它块对 OpenAI 客户端无意义，忽略
            }
        }
    }

    // web_search loop 的带外思考文本（不进 content，避免 Anthropic 客户端回放）
    if let Some(t) = anthropic.get("kiro_thinking").and_then(|v| v.as_str()) {
        if !t.is_empty() {
            if !thinking.is_empty() {
                thinking.push_str("\n\n");
            }
            thinking.push_str(t);
        }
    }

    let stop_reason = anthropic
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn");
    let finish_reason = map_finish_reason(stop_reason, !tool_calls.is_empty()).to_string();

    let usage = anthropic.get("usage");
    let prompt_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let completion_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let credit_usage = usage
        .and_then(|u| u.get("credit_usage"))
        .and_then(|v| v.as_f64());
    let credit_unit = usage
        .and_then(|u| u.get("credit_unit"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let credit_unit_plural = usage
        .and_then(|u| u.get("credit_unit_plural"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    ParsedResponse {
        model: model.to_string(),
        text,
        tool_calls,
        finish_reason,
        prompt_tokens,
        completion_tokens,
        thinking,
        web_searches,
        credit_usage,
        credit_unit,
        credit_unit_plural,
    }
}

/// 追加到 merged，若与上一轮 role 相同则合并 content blocks
pub(super) fn push_merged(merged: &mut Vec<(String, Vec<Value>)>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = merged.last_mut() {
        if last.0 == role {
            last.1.extend(blocks);
            return;
        }
    }
    merged.push((role.to_string(), blocks));
}

fn map_finish_reason(stop_reason: &str, has_tool_calls: bool) -> &'static str {
    match stop_reason {
        "tool_use" => "tool_calls",
        "max_tokens" | "model_context_window_exceeded" => "length",
        _ if has_tool_calls => "tool_calls",
        _ => "stop",
    }
}
