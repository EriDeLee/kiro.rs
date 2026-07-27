//! Token 计算模块
//!
//! 提供文本 token 数量计算功能。
//!
//! # 计算规则
//! - 非西文字符：每个计 4.5 个字符单位
//! - 西文字符：每个计 1 个字符单位
//! - 4 个字符单位 = 1 token（四舍五入）

use crate::anthropic::types::{
    CountTokensRequest, CountTokensResponse, Message, SystemMessage, Tool,
};
use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use std::sync::OnceLock;

/// Count Tokens API 配置
#[derive(Clone, Default)]
pub struct CountTokensConfig {
    /// 外部 count_tokens API 地址
    pub api_url: Option<String>,
    /// count_tokens API 密钥
    pub api_key: Option<String>,
    /// count_tokens API 认证类型（"x-api-key" 或 "bearer"）
    pub auth_type: String,
    /// 代理配置
    pub proxy: Option<ProxyConfig>,

    pub tls_backend: TlsBackend,
}

/// 全局配置存储
static COUNT_TOKENS_CONFIG: OnceLock<CountTokensConfig> = OnceLock::new();

/// 初始化 count_tokens 配置
///
/// 应在应用启动时调用一次
pub fn init_config(config: CountTokensConfig) {
    let _ = COUNT_TOKENS_CONFIG.set(config);
}

/// 获取配置
fn get_config() -> Option<&'static CountTokensConfig> {
    COUNT_TOKENS_CONFIG.get()
}

/// 判断字符是否为非西文字符
///
/// 西文字符包括：
/// - ASCII 字符 (U+0000..U+007F)
/// - 拉丁字母扩展 (U+0080..U+024F)
/// - 拉丁字母扩展附加 (U+1E00..U+1EFF)
///
/// 返回 true 表示该字符是非西文字符（如中文、日文、韩文、阿拉伯文等）
fn is_non_western_char(c: char) -> bool {
    !matches!(c,
        // 基本 ASCII
        '\u{0000}'..='\u{007F}' |
        // 拉丁字母扩展-A (Latin Extended-A)
        '\u{0080}'..='\u{00FF}' |
        // 拉丁字母扩展-B (Latin Extended-B)
        '\u{0100}'..='\u{024F}' |
        // 拉丁字母扩展附加 (Latin Extended Additional)
        '\u{1E00}'..='\u{1EFF}' |
        // 拉丁字母扩展-C/D/E
        '\u{2C60}'..='\u{2C7F}' |
        '\u{A720}'..='\u{A7FF}' |
        '\u{AB30}'..='\u{AB6F}'
    )
}

/// 计算文本的 token 数量
///
/// # 计算规则
/// - 非西文字符：每个计 4.5 个字符单位
/// - 西文字符：每个计 1 个字符单位
/// - 4 个字符单位 = 1 token（四舍五入）
/// ```
pub fn count_tokens(text: &str) -> u64 {
    // println!("text: {}", text);

    let char_units: f64 = text
        .chars()
        .map(|c| if is_non_western_char(c) { 4.0 } else { 1.0 })
        .sum();

    let tokens = char_units / 4.0;

    let acc_token = if tokens < 100.0 {
        tokens * 1.5
    } else if tokens < 200.0 {
        tokens * 1.3
    } else if tokens < 300.0 {
        tokens * 1.25
    } else if tokens < 800.0 {
        tokens * 1.2
    } else {
        tokens * 1.0
    } as u64;

    // println!("tokens: {}, acc_tokens: {}", tokens, acc_token);
    acc_token
}

/// 估算请求的输入 tokens
///
/// 优先调用远程 API，失败时回退到本地计算
pub(crate) fn count_all_tokens(
    model: String,
    system: Option<Vec<SystemMessage>>,
    messages: Vec<Message>,
    tools: Option<Vec<Tool>>,
) -> u64 {
    // 检查是否配置了远程 API
    if let Some(config) = get_config() {
        if let Some(api_url) = &config.api_url {
            // 尝试调用远程 API
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(call_remote_count_tokens(
                    api_url, config, model, &system, &messages, &tools,
                ))
            });

            match result {
                Ok(tokens) => {
                    tracing::debug!("远程 count_tokens API 返回: {}", tokens);
                    return tokens;
                }
                Err(e) => {
                    tracing::warn!("远程 count_tokens API 调用失败，回退到本地计算: {}", e);
                }
            }
        }
    }

    // 本地计算
    count_all_tokens_local(system, messages, tools)
}

/// 调用远程 count_tokens API
async fn call_remote_count_tokens(
    api_url: &str,
    config: &CountTokensConfig,
    model: String,
    system: &Option<Vec<SystemMessage>>,
    messages: &Vec<Message>,
    tools: &Option<Vec<Tool>>,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let client = build_client(config.proxy.as_ref(), 300, config.tls_backend)?;

    // 构建请求体
    let request = CountTokensRequest {
        model: model, // 模型名称用于 token 计算
        messages: messages.clone(),
        system: system.clone(),
        tools: tools.clone(),
    };

    // 构建请求
    let mut req_builder = client.post(api_url);

    // 设置认证头
    if let Some(api_key) = &config.api_key {
        if config.auth_type == "bearer" {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        } else {
            req_builder = req_builder.header("x-api-key", api_key);
        }
    }

    // 发送请求
    let response = req_builder
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("API 返回错误状态: {}", response.status()).into());
    }

    let result: CountTokensResponse = response.json().await?;
    Ok(result.input_tokens as u64)
}

/// 本地计算请求的输入 tokens
fn count_all_tokens_local(
    system: Option<Vec<SystemMessage>>,
    messages: Vec<Message>,
    tools: Option<Vec<Tool>>,
) -> u64 {
    let mut total = 0;

    // 系统消息
    if let Some(ref system) = system {
        for msg in system {
            total += count_tokens(&msg.text);
        }
    }

    // 用户 / assistant 消息
    for msg in &messages {
        if let serde_json::Value::String(s) = &msg.content {
            total += count_tokens(s);
        } else if let serde_json::Value::Array(arr) = &msg.content {
            for item in arr {
                total += count_content_block_tokens(item);
            }
        }
    }

    // 工具定义
    if let Some(ref tools) = tools {
        for tool in tools {
            total += count_tokens(&tool.name);
            total += count_tokens(&tool.description);
            let input_schema_json = serde_json::to_string(&tool.input_schema).unwrap_or_default();
            total += count_tokens(&input_schema_json);
        }
    }

    total.max(1)
}

/// 估算单个 Anthropic content block 的 token 数。
///
/// 此前只统计 `text` 字段，`tool_use.input` / `tool_result.content` / `image` /
/// `thinking` 全部漏算。opencode 依赖 `/v1/messages/count_tokens` 决定何时压缩
/// 上下文，低估会让它压缩得太晚、直接撞上游窗口上限。
///
/// `tool_result.content` 本身可以是字符串或嵌套 block 数组，故递归处理。
fn count_content_block_tokens(block: &serde_json::Value) -> u64 {
    let mut total = 0;

    match block.get("type").and_then(|v| v.as_str()) {
        // 图片按 Anthropic 的 (w*h)/750 公式估算，复用 image_resize 的实现。
        Some("image") => {
            if let Some(src) = block.get("source") {
                let media = src
                    .get("media_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("image/png");
                if let Some(data) = src.get("data").and_then(|v| v.as_str()) {
                    total += u64::from(crate::image_resize::estimate_image_tokens(media, data));
                }
            }
            return total;
        }
        // 工具入参是 JSON 对象，按序列化后的字面量计。
        Some("tool_use") => {
            total += count_tokens(block.get("name").and_then(|v| v.as_str()).unwrap_or(""));
            if let Some(input) = block.get("input") {
                total += count_tokens(&input.to_string());
            }
            return total;
        }
        // 工具结果：content 可能是字符串，也可能是嵌套 block 数组（含图片）。
        Some("tool_result") => {
            match block.get("content") {
                Some(serde_json::Value::String(s)) => total += count_tokens(s),
                Some(serde_json::Value::Array(items)) => {
                    for item in items {
                        total += count_content_block_tokens(item);
                    }
                }
                _ => {}
            }
            return total;
        }
        // thinking / redacted_thinking 也占上下文。
        Some("thinking") => {
            total += count_tokens(block.get("thinking").and_then(|v| v.as_str()).unwrap_or(""));
            return total;
        }
        Some("redacted_thinking") => {
            total += count_tokens(block.get("data").and_then(|v| v.as_str()).unwrap_or(""));
            return total;
        }
        _ => {}
    }

    // text 块与未知类型：退化为只数 text 字段（与旧行为一致）。
    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
        total += count_tokens(text);
    }
    total
}

/// 估算输出 tokens
pub(crate) fn estimate_output_tokens(content: &[serde_json::Value]) -> i32 {
    let mut total = 0;

    for block in content {
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            total += count_tokens(text) as i32;
        }
        if let Some(thinking) = block.get("thinking").and_then(|v| v.as_str()) {
            total += count_tokens(thinking) as i32;
        }
        if block.get("type").and_then(|v| v.as_str()) == Some("redacted_thinking") {
            total += 8;
        }
        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
            // 工具调用开销
            if let Some(input) = block.get("input") {
                let input_str = serde_json::to_string(input).unwrap_or_default();
                total += count_tokens(&input_str) as i32;
            }
        }
    }

    total.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 回归锁：入站统计必须覆盖 tool_use / tool_result / thinking / image，
    /// 只数 text 会让 opencode 严重低估上下文、压缩过晚。
    #[test]
    fn count_content_block_tokens_covers_non_text_blocks() {
        let long = "x".repeat(400);

        let tool_use = serde_json::json!({
            "type": "tool_use", "id": "t1", "name": "Edit",
            "input": {"file_path": "/a.rs", "old_string": long.clone()}
        });
        assert!(
            count_content_block_tokens(&tool_use) > 50,
            "tool_use.input 必须计入"
        );

        let tool_result = serde_json::json!({
            "type": "tool_result", "tool_use_id": "t1", "content": long.clone()
        });
        assert!(
            count_content_block_tokens(&tool_result) > 50,
            "tool_result.content 字符串必须计入"
        );

        // 嵌套 block 数组形态
        let nested = serde_json::json!({
            "type": "tool_result", "tool_use_id": "t1",
            "content": [{"type": "text", "text": long.clone()}]
        });
        assert!(
            count_content_block_tokens(&nested) > 50,
            "tool_result 的嵌套 block 必须递归计入"
        );

        let thinking = serde_json::json!({"type": "thinking", "thinking": long.clone()});
        assert!(
            count_content_block_tokens(&thinking) > 50,
            "thinking 必须计入"
        );

        let redacted = serde_json::json!({"type": "redacted_thinking", "data": long});
        assert!(
            count_content_block_tokens(&redacted) > 50,
            "redacted_thinking 必须计入"
        );

        // text 块保持原行为
        let text = serde_json::json!({"type": "text", "text": "hello"});
        assert!(count_content_block_tokens(&text) > 0);
    }

    #[test]
    fn estimate_output_tokens_counts_thinking_blocks() {
        let with_thinking = estimate_output_tokens(&[json!({
            "type": "thinking",
            "thinking": "需要计入输出 token"
        })]);
        let text_only = estimate_output_tokens(&[json!({
            "type": "text",
            "text": ""
        })]);

        assert!(with_thinking > text_only);
    }

    #[test]
    fn estimate_output_tokens_counts_redacted_thinking() {
        let tokens = estimate_output_tokens(&[json!({
            "type": "redacted_thinking",
            "data": "encrypted"
        })]);

        assert!(tokens >= 8);
    }
}
