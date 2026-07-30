//! WebSearch 的 MCP 调用与结果解析
//!
//! 只负责「把一次搜索请求发给 Kiro MCP 端点并解析结果」。请求路由与多轮编排在
//! [`super::websearch_loop`]：含原生 web_search 的请求统一走那条 agentic loop，
//! 由模型自己决定何时搜、搜什么。
//!
//! 此前本模块还有一条「单工具快速路径」：当 tools 恰好只有一个 web_search 时，
//! 由代理 `extract_search_query` 从消息文本里猜搜索词并直接代答，模型完全不参与
//! 决策。该路径已删除（连带 9 个专属函数），因为它替客户端做了它没要求的决定，
//! 且「按工具数量分流」逼得 `/v1/responses` 侧注入假工具才能绕开它。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::MessagesRequest;

/// MCP 请求
#[derive(Debug, Serialize)]
pub struct McpRequest {
    pub id: String,
    pub jsonrpc: String,
    pub method: String,
    pub params: McpParams,
}

/// MCP 请求参数
#[derive(Debug, Serialize)]
pub struct McpParams {
    pub name: String,
    pub arguments: McpArguments,
}

/// MCP 参数
#[derive(Debug, Serialize)]
pub struct McpArguments {
    pub query: String,
}

/// MCP 响应
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpResponse {
    pub error: Option<McpError>,
    pub id: String,
    pub jsonrpc: String,
    pub result: Option<McpResult>,
}

/// MCP 错误
#[derive(Debug, Deserialize)]
pub struct McpError {
    pub code: Option<i32>,
    pub message: Option<String>,
}

/// MCP 结果
#[derive(Debug, Deserialize)]
pub struct McpResult {
    pub content: Vec<McpContent>,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

/// MCP 内容
#[derive(Debug, Deserialize)]
pub struct McpContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

/// WebSearch 搜索结果
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WebSearchResults {
    pub results: Vec<WebSearchResult>,
    #[serde(rename = "totalResults")]
    pub total_results: Option<i32>,
    pub query: Option<String>,
    pub error: Option<String>,
}

/// 单个搜索结果
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
    #[serde(rename = "publishedDate")]
    pub published_date: Option<i64>,
    pub id: Option<String>,
    pub domain: Option<String>,
    #[serde(rename = "maxVerbatimWordLimit")]
    pub max_verbatim_word_limit: Option<i32>,
    #[serde(rename = "publicDomain")]
    pub public_domain: Option<bool>,
}

fn is_native_web_search_tool(t: &crate::anthropic::types::Tool) -> bool {
    t.name == "web_search"
        && t.tool_type
            .as_deref()
            .is_some_and(|typ| typ.starts_with("web_search_"))
}

/// 请求的 tools 里是否含 Anthropic 原生 web_search（`type` 为 `web_search_*`）。
///
/// 命中即走内部 agentic loop：由模型自己决定何时搜、搜什么，代理只执行搜索并
/// 把结果作为 tool_result 回喂。
///
/// 不看工具数量。此前要求 `len() > 1`，与另一条「单工具快速路径」互斥分流，
/// 结果逼得 `/v1/responses` 侧注入名为 `noop` 的假工具把数量顶到 2 才能走到
/// 这里。快速路径（代理自己猜搜索词代答）已删除，假工具随之删除。
pub(crate) fn has_web_search_among_tools(req: &MessagesRequest) -> bool {
    req.tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(is_native_web_search_tool))
}

/// 生成22位大小写字母和数字的随机字符串
fn generate_random_id_22() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..22)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// 生成8位小写字母和数字的随机字符串
fn generate_random_id_8() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..8)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// 创建 MCP 请求
///
/// ID 格式: web_search_tooluse_{22位随机}_{毫秒时间戳}_{8位随机}
pub fn create_mcp_request(query: &str) -> (String, McpRequest) {
    let random_22 = generate_random_id_22();
    let timestamp = chrono::Utc::now().timestamp_millis();
    let random_8 = generate_random_id_8();

    let request_id = format!(
        "web_search_tooluse_{}_{}_{}",
        random_22, timestamp, random_8
    );

    // tool_use_id 使用相同格式
    let tool_use_id = format!(
        "srvtoolu_{}",
        Uuid::new_v4().to_string().replace('-', "")[..32].to_string()
    );

    let request = McpRequest {
        id: request_id,
        jsonrpc: "2.0".to_string(),
        method: "tools/call".to_string(),
        params: McpParams {
            name: "web_search".to_string(),
            arguments: McpArguments {
                query: query.to_string(),
            },
        },
    };

    (tool_use_id, request)
}

/// 解析 MCP 响应中的搜索结果
pub fn parse_search_results(mcp_response: &McpResponse) -> Option<WebSearchResults> {
    let result = mcp_response.result.as_ref()?;

    // MCP 协议用 `isError` 标记工具执行失败，此时 content 装的是错误描述而不是
    // 搜索结果。此前这个字段解析出来却从不检查，上游报错会被当成正常结果继续
    // 往下走 —— 属于 AGENTS.md §2.3 禁止的静默降级。
    if result.is_error {
        let detail = result
            .content
            .first()
            .map(|c| c.text.as_str())
            .unwrap_or("<no content>");
        tracing::warn!("web_search MCP 返回 isError=true，放弃解析结果: {}", detail);
        return None;
    }

    let content = result.content.first()?;
    if content.content_type != "text" {
        tracing::warn!(
            content_type = %content.content_type,
            "web_search MCP 返回了非 text 内容，无法解析"
        );
        return None;
    }

    let results: WebSearchResults = match serde_json::from_str(&content.text) {
        Ok(results) => results,
        Err(e) => {
            // 解析失败不静默返回 None：上游给了 text 却不是预期结构，说明协议变了。
            tracing::warn!("web_search 结果 JSON 解析失败（上游结构可能已变更）: {}", e);
            return None;
        }
    };

    // 载荷里的业务级错误同样要看：此前该字段解析出来从不检查，搜索服务报错会被
    // 当成「搜到 0 条」，调用方无从区分「确实没结果」与「服务出错」。
    if let Some(err) = results.error.as_deref().filter(|e| !e.trim().is_empty()) {
        tracing::warn!("web_search 服务返回错误: {}", err);
        return None;
    }

    Some(results)
}

/// 生成搜索结果摘要
pub(crate) fn generate_search_summary(query: &str, results: &Option<WebSearchResults>) -> String {
    let mut summary = format!("Here are the search results for \"{}\":\n\n", query);

    if let Some(results) = results {
        for (i, result) in results.results.iter().enumerate() {
            summary.push_str(&format!("{}. **{}**\n", i + 1, result.title));
            if let Some(ref snippet) = result.snippet {
                // 截断过长的摘要（安全处理 UTF-8 多字节字符）
                let truncated = match snippet.char_indices().nth(200) {
                    Some((idx, _)) => format!("{}...", &snippet[..idx]),
                    None => snippet.clone(),
                };
                summary.push_str(&format!("   {}\n", truncated));
            }
            summary.push_str(&format!("   Source: {}\n\n", result.url));
        }
    } else {
        summary.push_str("No results found.\n");
    }

    summary
}

/// 调用 Kiro MCP API
pub(crate) async fn call_mcp_api(
    provider: &crate::kiro::provider::KiroProvider,
    request: &McpRequest,
    group: Option<&str>,
) -> anyhow::Result<McpResponse> {
    let request_body = serde_json::to_string(request)?;

    tracing::debug!("MCP request: {}", request_body);

    let response = provider.call_mcp(&request_body, group).await?;

    let body = response.text().await?;
    tracing::debug!("MCP response: {}", body);

    let mcp_response: McpResponse = serde_json::from_str(&body)?;

    if let Some(ref error) = mcp_response.error {
        anyhow::bail!(
            "MCP error: {} - {}",
            error.code.unwrap_or(-1),
            error.message.as_deref().unwrap_or("Unknown error")
        );
    }

    Ok(mcp_response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regular_tool_named_web_search_does_not_trigger_native_websearch() {
        use crate::anthropic::types::{Message, Tool};

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![
                Tool {
                    tool_type: None,
                    name: "web_search".to_string(),
                    description: "Regular client-side search tool".to_string(),
                    input_schema: Default::default(),
                    max_uses: None,
                    cache_control: None,
                },
                Tool {
                    tool_type: None,
                    name: "other_tool".to_string(),
                    description: "Other tool".to_string(),
                    input_schema: Default::default(),
                    max_uses: None,
                    cache_control: None,
                },
            ]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            effort: None,
            metadata: None,
        };

        // 客户端可以给自己的工具起名叫 web_search —— 识别原生 server tool 只看
        // `type` 是否为 `web_search_*`，绝不按名字猜。否则客户端的同名工具会被
        // 代理劫持成内部搜索，而它本该原样透传给模型。
        assert!(!has_web_search_among_tools(&req));
    }

    #[test]
    fn test_create_mcp_request() {
        let (tool_use_id, request) = create_mcp_request("test query");

        assert!(tool_use_id.starts_with("srvtoolu_"));
        assert_eq!(request.jsonrpc, "2.0");
        assert_eq!(request.method, "tools/call");
        assert_eq!(request.params.name, "web_search");
        assert_eq!(request.params.arguments.query, "test query");

        // 验证 ID 格式: web_search_tooluse_{22位}_{时间戳}_{8位}
        assert!(request.id.starts_with("web_search_tooluse_"));
    }

    #[test]
    fn test_mcp_request_id_format() {
        let (_, request) = create_mcp_request("test");

        // 格式: web_search_tooluse_{22位}_{毫秒时间戳}_{8位}
        let id = &request.id;
        assert!(id.starts_with("web_search_tooluse_"));

        let suffix = &id["web_search_tooluse_".len()..];
        let parts: Vec<&str> = suffix.split('_').collect();
        assert_eq!(parts.len(), 3, "应该有3个部分: 22位随机_时间戳_8位随机");

        // 第一部分: 22位大小写字母和数字
        assert_eq!(parts[0].len(), 22);
        assert!(parts[0].chars().all(|c| c.is_ascii_alphanumeric()));

        // 第二部分: 毫秒时间戳
        assert!(parts[1].parse::<i64>().is_ok());

        // 第三部分: 8位小写字母和数字
        assert_eq!(parts[2].len(), 8);
        assert!(
            parts[2]
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn test_parse_search_results() {
        let response = McpResponse {
            error: None,
            id: "test_id".to_string(),
            jsonrpc: "2.0".to_string(),
            result: Some(McpResult {
                content: vec![McpContent {
                    content_type: "text".to_string(),
                    text: r#"{"results":[{"title":"Test","url":"https://example.com","snippet":"Test snippet"}],"totalResults":1}"#.to_string(),
                }],
                is_error: false,
            }),
        };

        let results = parse_search_results(&response);
        assert!(results.is_some());
        let results = results.unwrap();
        assert_eq!(results.results.len(), 1);
        assert_eq!(results.results[0].title, "Test");
    }

    #[test]
    fn test_generate_search_summary() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Test Result".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some("This is a test snippet".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("test".to_string()),
            error: None,
        };

        let summary = generate_search_summary("test", &Some(results));

        assert!(summary.contains("Test Result"));
        assert!(summary.contains("https://example.com"));
        assert!(summary.contains("This is a test snippet"));
    }
}
