//! Anthropic → Kiro 协议转换器
//!
//! 负责将 Anthropic API 请求格式转换为 Kiro API 请求格式

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::kiro::model::requests::conversation::{
    AssistantMessage, ConversationState, CurrentMessage, HistoryAssistantMessage,
    ReasoningContent,
    HistoryUserMessage, KiroImage, Message, UserInputMessage, UserInputMessageContext, UserMessage,
};
use crate::kiro::model::requests::kiro::{
    AdditionalModelRequestFields, KiroOutputConfig, KiroReasoningConfig, KiroThinkingConfig,
};
use crate::kiro::model::requests::tool::{
    InputSchema, Tool, ToolResult, ToolSpecification, ToolUseEntry,
};
use crate::model::config::ToolCompatibilityMode;

use super::types::{ContentBlock, ImageSource, MessagesRequest};

use crate::image_resize::{ResizeConfig, maybe_shrink_image};

/// 规范化 JSON Schema，修复 MCP 工具定义中常见的类型问题
/// 规范化 JSON Schema，修复工具定义中常见的类型问题
///
/// 问题根源：Claude Code / MCP 工具定义使用 JSON Schema Draft 2020-12 语法（`$schema`、
/// `exclusiveMinimum` 为数字等），kiro CLI endpoint 仅接受 Draft 07 格式，
/// 不合规字段会导致 ValidationException "Improperly formed request."。
fn normalize_json_schema(schema: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut obj) = schema else {
        return serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": true
        });
    };

    // 移除 $schema（kiro API 不接受此字段，且 Draft 2020-12 声明会触发校验失败）
    obj.remove("$schema");

    // 剥离顶层 oneOf/allOf/anyOf：Bedrock/Kiro 的 ToolInputSchema 不支持顶层组合关键字，
    // 否则返回 TOOL_SCHEMA_INVALID 400（常见于 Claude Code workflow 并行子代理携带的 MCP tools）。
    strip_top_level_combinators(&mut obj);

    // type 顶层必须是 "object"（Bedrock ToolInputSchema 硬约束）；非 object 一律强制修正。
    let current_type = obj
        .get("type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if current_type.as_deref() != Some("object") {
        if let Some(ref original) = current_type {
            tracing::warn!(
                original_type = %original,
                "tool inputSchema 顶层 type 不是 object，已强制修正（Bedrock 硬约束）"
            );
        }
        obj.insert(
            "type".to_string(),
            serde_json::Value::String("object".to_string()),
        );
    }

    // properties（必须是 object）；递归规范化每个 property 的子 schema
    match obj.remove("properties") {
        Some(serde_json::Value::Object(props)) => {
            let normalized: serde_json::Map<String, serde_json::Value> = props
                .into_iter()
                .map(|(k, v)| (k, normalize_property_schema(v)))
                .collect();
            obj.insert("properties".to_string(), serde_json::Value::Object(normalized));
        }
        _ => { obj.insert("properties".to_string(), serde_json::Value::Object(serde_json::Map::new())); }
    }

    // required（必须是 string 数组）
    let required = match obj.remove("required") {
        Some(serde_json::Value::Array(arr)) => serde_json::Value::Array(
            arr.into_iter()
                .filter_map(|v| v.as_str().map(|s| serde_json::Value::String(s.to_string())))
                .collect(),
        ),
        _ => serde_json::Value::Array(Vec::new()),
    };
    obj.insert("required".to_string(), required);

    // additionalProperties（允许 bool 或 object，其他按 true 处理）
    match obj.get("additionalProperties") {
        Some(serde_json::Value::Bool(_)) | Some(serde_json::Value::Object(_)) => {}
        _ => { obj.insert("additionalProperties".to_string(), serde_json::Value::Bool(true)); }
    }

    serde_json::Value::Object(obj)
}

/// 剥离顶层 `oneOf`/`anyOf`/`allOf`，并尽量从 variant 中恢复语义字段。
///
/// Bedrock/Kiro 的 ToolInputSchema 不支持顶层组合关键字（AWS 文档 + Anthropic API 均如此）。
/// 若原 schema 没有 `properties`，则遍历各 combinator，取第一个 `type=object` 的 variant，
/// 提取其 `properties`/`required`/`additionalProperties`/`description`（避免退化成空对象丢掉全部入参）。
fn strip_top_level_combinators(obj: &mut serde_json::Map<String, serde_json::Value>) {
    let has_properties_initially = obj.contains_key("properties");

    for combinator in &["oneOf", "anyOf", "allOf"] {
        let Some(serde_json::Value::Array(variants)) = obj.remove(*combinator) else {
            continue;
        };

        // 原始 schema 已有 properties，或前一个 combinator 已提取过 → 纯剥离，不再恢复。
        if has_properties_initially || obj.contains_key("properties") {
            continue;
        }

        // 从 variants 中找第一个 type=object 的变体，提取关键字段。
        for variant in variants {
            let serde_json::Value::Object(m) = variant else {
                continue;
            };
            if m.get("type").and_then(|v| v.as_str()) != Some("object") {
                continue;
            }
            for key in &["properties", "required", "additionalProperties", "description"] {
                if let Some(val) = m.get(*key) {
                    obj.entry(key.to_string()).or_insert_with(|| val.clone());
                }
            }
            break;
        }
    }
}

/// 规范化 property 级别的子 schema（非顶层 inputSchema）
///
/// 处理 Draft 2020-12 特有字段，使其兼容 Draft 07：
/// - 移除 `$schema`
/// - `exclusiveMinimum`/`exclusiveMaximum` 为数字时（Draft 2019-09+）移除（Draft 07 仅支持 bool）
/// - `maximum`/`minimum` 超过 i32 范围时移除（部分 AWS validator 不接受超大整数约束）
fn normalize_property_schema(schema: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut obj) = schema else {
        return schema;
    };

    obj.remove("$schema");

    // exclusiveMinimum/exclusiveMaximum：Draft 2019-09+ 为数字，Draft 07 为 bool；移除数字形式
    if obj.get("exclusiveMinimum").and_then(|v| v.as_f64()).is_some() {
        obj.remove("exclusiveMinimum");
    }
    if obj.get("exclusiveMaximum").and_then(|v| v.as_f64()).is_some() {
        obj.remove("exclusiveMaximum");
    }

    // maximum/minimum 超过 i64::MAX 或为 JavaScript MAX_SAFE_INTEGER (9007199254740991) 时移除
    for key in &["maximum", "minimum"] {
        if let Some(v) = obj.get(*key).and_then(|v| v.as_f64()) {
            if v > 2_147_483_647.0 || v < -2_147_483_648.0 {
                obj.remove(*key);
            }
        }
    }

    // 递归处理嵌套 properties
    if let Some(serde_json::Value::Object(props)) = obj.remove("properties") {
        let normalized: serde_json::Map<String, serde_json::Value> = props
            .into_iter()
            .map(|(k, v)| (k, normalize_property_schema(v)))
            .collect();
        obj.insert("properties".to_string(), serde_json::Value::Object(normalized));
    }

    // 递归处理 items（数组元素 schema）
    if let Some(items) = obj.remove("items") {
        obj.insert("items".to_string(), normalize_property_schema(items));
    }

    serde_json::Value::Object(obj)
}

/// 追加到 Write 工具 description 末尾的内容
const WRITE_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: If the content to write exceeds 150 lines, you MUST only write the first 50 lines using this tool, then use `Edit` tool to append the remaining content in chunks of no more than 50 lines each. If needed, leave a unique placeholder to help append content. Do NOT attempt to write all content at once.";

/// 追加到 Edit 工具 description 末尾的内容
const EDIT_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: If the `new_string` content exceeds 50 lines, you MUST split it into multiple Edit calls, each replacing no more than 50 lines at a time. If used to append content, leave a unique placeholder to help append content. On the final chunk, do NOT include the placeholder.";

/// 追加到 Bash 工具 description 末尾的内容（上游可能在超大命令处截断）
const BASH_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: Do not send very large commands, inline scripts, or heredocs. If a command would exceed 100 lines or ~8000 characters, first create/modify a script file with chunked Write/Edit calls, then run a short command that executes it. Do not retry the same oversized command after a failure; split it smaller.";

/// 追加到系统提示词的分块写入策略
const SYSTEM_CHUNKED_POLICY: &str = "\
When the Write or Edit tool has content size limits, always comply silently. \
Never suggest bypassing these limits via alternative tools. \
Never ask the user whether to switch approaches. \
Complete all chunked operations without commentary.";

const MAX_MODEL_ID_LEN: usize = 256;

fn invalid_model_reason(model: &str) -> Option<&'static str> {
    if model.trim().is_empty() {
        Some("模型 ID 不能为空")
    } else if model.len() > MAX_MODEL_ID_LEN {
        Some("模型 ID 过长")
    } else if model.chars().any(char::is_control) {
        Some("模型 ID 不能包含控制字符")
    } else {
        None
    }
}



/// 把客户端模型名映射到上游 Kiro modelId。
///
/// 本部署只服务 Anthropic 协议上的 `claude-opus-5`，其余一律 `None`（由调用方回
/// 400）。不做家族/版本号的模糊推断，也不再透传未知模型——透传等于把不受支持的
/// id 直接甩给上游，换来一个语义不明的 400。
pub fn map_model(model: &str) -> Option<String> {
    if invalid_model_reason(model).is_some() {
        return None;
    }
    // 白名单是唯一权威：只认 claude-opus-5 与 gpt-5.6-sol（含别名后缀），
    // 其余一律 None。协议与模型的一对一绑定由端点层（handlers / responses）校验，
    // 这里不区分协议——converter 是两条端点共用的下游。
    crate::model::allowlist::normalize(model).map(str::to_string)
}

/// 根据模型名称返回对应的上下文窗口大小
///
/// 上下文窗口，取自实测 `ListAvailableModels` 的 `tokenLimits.maxInputTokens`。
///
/// 本端点只服务 `claude-opus-5`（1M in / 128k out）。
pub fn get_context_window_size(model: &str) -> i32 {
    match map_model(model).as_deref() {
        Some(crate::model::allowlist::MODEL_OPUS_5) => 1_000_000,
        _ => 200_000,
    }
}

/// 是否向该模型下发 `additionalModelRequestFields.output_config`。
///
/// 实测 `claude-opus-5` 的 schema 接受 `output_config.effort ∈
/// {low,medium,high,xhigh,max}` 与 `thinking.{type,display}`。本端点只服务它。
fn model_supports_native_reasoning(model_id: &str) -> bool {
    model_id.eq_ignore_ascii_case(crate::model::allowlist::MODEL_OPUS_5)
}

/// 取出一条消息里的可见文本（拼接所有 text 块）。
///
/// 用于 assistant prefill 转换：只关心 text，其余块类型（thinking / tool_use /
/// image）在 prefill 场景下不会出现 —— opencode 的 MAX_STEPS_PROMPT 是纯 text。
fn collect_message_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter(|item| {
                item.get("type").and_then(|v| v.as_str()).unwrap_or("text") == "text"
            })
            .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 客户端是否请求了原生 reasoning。
///
/// 三处信号任一成立即算，必须与 [`select_native_reasoning_effort`] 的取值链一致：
///   - `thinking` 启用（adaptive / enabled）
///   - `req.effort` —— `/v1/responses` 的内部通路（那条路径 thinking /
///     output_config 恒为 None，见 `MessagesRequest::effort`）
///   - `output_config.effort` —— Anthropic 官方位置
///
/// 漏掉 `req.effort` 会让 GPT 请求在此直接短路，`additionalModelRequestFields`
/// 返回 None，上游收不到档位而回落 default —— 即「虚假推理强度」。
fn native_reasoning_requested(req: &MessagesRequest) -> bool {
    req.thinking.as_ref().is_some_and(|t| t.is_enabled())
        || req.effort.as_deref().is_some_and(|e| !e.trim().is_empty())
        || req
            .output_config
            .as_ref()
            .is_some_and(|oc| !oc.effort.trim().is_empty())
}

/// 由 Anthropic `thinking.budget_tokens` 推导 effort 档位。
///
/// 当客户端只发标准 `thinking:{type:"enabled",budget_tokens:N}`、不带 `output_config`
/// 时，用它把「思考预算」映射到 Kiro 的 effort。（本项目 budget_tokens 上限 24576，
/// 故经此推导实际最高到 `high`；`xhigh` 仍需客户端显式 `output_config.effort`。）
fn effort_from_budget_tokens(tokens: i32) -> &'static str {
    match tokens {
        i32::MIN..=4_000 => "low",
        4_001..=16_000 => "medium",
        16_001..=64_000 => "high",
        _ => "xhigh",
    }
}

/// 选定最终下发的 effort：优先显式 `output_config.effort`；否则据 `budget_tokens`
/// 推导；再统一过 [`normalize_effort_for_model`]（按模型把 xhigh 安全降级等）。
/// 选定最终下发的 effort 档位。
///
/// 优先级：`req.effort`（`/v1/responses` 内部通路）> `output_config.effort`（Anthropic
/// 官方规范，也是 `@ai-sdk/anthropic` 的实际线格式）
/// > `thinking.budget_tokens` 推导（旧式客户端兜底）> 默认 `high`（与上游 default 一致）。
/// 随后按该模型实测枚举裁剪。
fn select_native_reasoning_effort(
    req: &MessagesRequest,
    model_id: &str,
) -> Result<String, String> {
    let raw = req
        .effort
        .as_deref()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .or_else(|| {
            req.output_config
                .as_ref()
                .map(|oc| oc.effort.trim().to_string())
                .filter(|e| !e.is_empty())
        })
        .or_else(|| {
            req.thinking
                .as_ref()
                .filter(|t| t.is_enabled())
                .map(|t| effort_from_budget_tokens(t.budget_tokens).to_string())
        })
        .unwrap_or_else(|| "high".to_string());
    // 客户端未指定时才用 default high；显式指定但不被支持则上抛，不静默改写。
    Ok(normalize_effort_for_model(model_id, &raw)?.unwrap_or_else(|| "high".to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffortTier {
    /// 仅 gpt-5.6-sol 支持；claude-opus-5 的枚举里没有这一档。
    None,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl EffortTier {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "x-high" | "x_high" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// 该模型实测支持的 effort 枚举（来自 `ListAvailableModels` 的 schema）。
///
/// - `claude-opus-5`：low / medium / high / xhigh / max（**无 none**）
/// - `gpt-5.6-sol` ：none / low / medium / high / xhigh / max
fn model_effort_tiers(model_id: &str) -> &'static [EffortTier] {
    use EffortTier::*;
    if model_id.eq_ignore_ascii_case(crate::model::allowlist::MODEL_GPT_56_SOL) {
        &[None, Low, Medium, High, XHigh, Max]
    } else {
        &[Low, Medium, High, XHigh, Max]
    }
}

/// 把请求的 effort 裁剪到该模型支持的枚举内。
///
/// 命中则原样下发；无法识别或该模型不支持则回落 `high`（上游 default），
/// 不会静默拉到最高档 —— 拉高档位同样是篡改客户端意图。
fn normalize_effort_for_model(model_id: &str, raw_effort: &str) -> Result<Option<String>, String> {
    let trimmed = raw_effort.trim();
    if trimmed.is_empty() {
        return Ok(Option::None);
    }

    let tiers = model_effort_tiers(model_id);
    match EffortTier::parse(trimmed) {
        Some(tier) if tiers.contains(&tier) => Ok(Some(tier.as_str().to_string())),
        // 不回落：静默把档位改成 high 等于篡改客户端请求的推理强度，调用方拿到
        // 200 却以为自己设的档位生效了。直接拒绝，让它知道这个模型不支持。
        Some(tier) => Err(format!(
            "effort `{}` is not supported by model `{}`; supported tiers: {}",
            tier.as_str(),
            model_id,
            tiers.iter().map(|t| t.as_str()).collect::<Vec<_>>().join(", ")
        )),
        Option::None => Err(format!(
            "unrecognized effort value `{}`; supported tiers for `{}`: {}",
            trimmed,
            model_id,
            tiers.iter().map(|t| t.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

fn build_additional_model_request_fields(
    req: &MessagesRequest,
    model_id: &str,
) -> Result<Option<AdditionalModelRequestFields>, String> {
    // 显式关闭 thinking：不下发任何 reasoning 字段。
    if req
        .thinking
        .as_ref()
        .is_some_and(|t| t.thinking_type == "disabled")
    {
        return Ok(None);
    }

    // 需要客户端确实请求了 reasoning（thinking 启用，或任一位置的显式 effort）。
    if !native_reasoning_requested(req) {
        return Ok(None);
    }

    let effort = select_native_reasoning_effort(req, model_id)?;

    // 两个模型的推理字段路径完全不同，按实测 schema 分流，不做任何互相兼容。
    if model_id.eq_ignore_ascii_case(crate::model::allowlist::MODEL_GPT_56_SOL) {
        return Ok(Some(AdditionalModelRequestFields {
            thinking: None,
            output_config: None,
            reasoning: Some(KiroReasoningConfig { effort }),
        }));
    }

    if model_supports_native_reasoning(model_id) {
        return Ok(Some(AdditionalModelRequestFields {
            thinking: build_kiro_thinking(req, &effort),
            output_config: Some(KiroOutputConfig { effort }),
            reasoning: None,
        }));
    }

    Ok(None)
}

/// 构造 `additionalModelRequestFields.thinking`（仅 claude-opus-5）。
///
/// 上游只接受 `type ∈ {adaptive, disabled}`，发 `enabled` 会 400；且 effort 为
/// `xhigh`/`max` 时连 `disabled` 也被拒。冲突时**整体不下发 thinking 字段**，让上游
/// 走默认行为 —— 绝不改写 effort（改 effort 等于篡改客户端请求的推理强度）。
///
/// `display` 原样透传：上游缺省 `omitted` 会吞掉思考文本，opencode 会主动发
/// `summarized` 要回来。
fn build_kiro_thinking(req: &MessagesRequest, effort: &str) -> Option<KiroThinkingConfig> {
    let thinking = req.thinking.as_ref()?;

    if thinking.thinking_type == "disabled" {
        if matches!(effort, "xhigh" | "max") {
            tracing::debug!(
                effort = %effort,
                "thinking=disabled 与 effort=xhigh/max 冲突，跳过下发 thinking 字段"
            );
            return None;
        }
        return Some(KiroThinkingConfig {
            thinking_type: "disabled".to_string(),
            display: None,
        });
    }

    if !thinking.is_enabled() {
        return None;
    }
    // enabled / adaptive 一律归一为上游唯一接受的 adaptive
    // （Anthropic 官方迁移路径：enabled+budget_tokens → adaptive+output_config.effort）。
    Some(KiroThinkingConfig {
        thinking_type: "adaptive".to_string(),
        display: thinking
            .display
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string),
    })
}

/// 转换结果
#[derive(Debug)]
pub struct ConversionResult {
    /// 转换后的 Kiro 请求
    pub conversation_state: ConversationState,
    /// 工具名称映射（短名称 → 原始名称），仅当存在超长工具名时非空
    pub tool_name_map: HashMap<String, String>,
    /// 本次请求声明的所有工具名（原始 client 名）。用于 `<invoke>` 文本容错的灾难兜底：
    /// 只有合成出的工具名在此集合里，才允许把字面 `<invoke>` 捞回成结构化 tool_use；
    /// 否则当普通文本吐出，避免把「正文展示的工具调用」误执行成真命令。
    pub known_tool_names: std::collections::HashSet<String>,
    /// Additional model request fields (including `output_config.effort`), translated from the
    /// `output_config` field of the client's Anthropic request. Not sent when empty.
    pub additional_model_request_fields: Option<AdditionalModelRequestFields>,
}

/// 转换错误
#[derive(Debug)]
pub enum ConversionError {
    InvalidModel(String),
    EmptyMessages,
    /// 请求本身违反本部署的严格约束（不支持的 effort 档位、assistant prefill 等）。
    /// 与 `InvalidModel` 分开，避免把 effort / prefill 问题报成「无效模型 ID」
    /// 而误导排查方向。
    UnsupportedRequest(String),
    /// Claude Code 工具无法映射到 Kiro 内置工具（如 Read.pages 无对应、内置缺 schema）。
    UnsupportedToolMapping(String),
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversionError::InvalidModel(reason) => write!(f, "无效模型 ID: {}", reason),
            ConversionError::UnsupportedRequest(reason) => write!(f, "{}", reason),
            ConversionError::EmptyMessages => write!(f, "消息列表为空"),
            ConversionError::UnsupportedToolMapping(reason) => {
                write!(f, "工具映射不支持: {}", reason)
            }
        }
    }
}

impl std::error::Error for ConversionError {}

/// 从 metadata.user_id 中提取 session UUID
///
/// 支持两种格式:
/// 1. 字符串格式: user_xxx_account__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705
/// 2. JSON 格式: {"device_id":"...","account_uuid":"...","session_id":"UUID"}
///
/// 提取 session UUID 作为 conversationId
fn extract_session_id(user_id: &str) -> Option<String> {
    // 先尝试 JSON 解析
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(user_id) {
        if let Some(session_id) = json.get("session_id").and_then(|v| v.as_str()) {
            if is_valid_uuid(session_id) {
                return Some(session_id.to_string());
            }
        }
    }

    // 回退到字符串格式: 查找 "session_" 后面的内容
    if let Some(pos) = user_id.find("session_") {
        let session_part = &user_id[pos + 8..]; // "session_" 长度为 8
        if session_part.len() >= 36 {
            let uuid_str = &session_part[..36];
            if is_valid_uuid(uuid_str) {
                return Some(uuid_str.to_string());
            }
        }
    }
    None
}

/// 简单验证 UUID 格式（36 字符，包含 4 个连字符）
fn is_valid_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

/// 收集历史消息中使用的所有工具名称
fn collect_history_tool_names(history: &[Message]) -> Vec<String> {
    let mut tool_names = Vec::new();

    for msg in history {
        if let Message::Assistant(assistant_msg) = msg {
            if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                for tool_use in tool_uses {
                    if !tool_names.contains(&tool_use.name) {
                        tool_names.push(tool_use.name.clone());
                    }
                }
            }
        }
    }

    tool_names
}

/// 为历史中使用但不在 tools 列表中的工具创建占位符定义
/// Kiro API 要求：历史消息中引用的工具必须在 currentMessage.tools 中有定义
fn create_placeholder_tool(name: &str) -> Tool {
    Tool {
        tool_specification: ToolSpecification {
            name: name.to_string(),
            description: "Tool used in conversation history".to_string(),
            input_schema: InputSchema::from_json(serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": true
            })),
        },
    }
}

/// 将 Anthropic 请求转换为 Kiro 请求
/// 便捷入口（测试用）：默认按 ClaudeCode 模式转换。
#[cfg(test)]
pub fn convert_request(req: &MessagesRequest) -> Result<ConversionResult, ConversionError> {
    convert_request_with_mode(req, ToolCompatibilityMode::ClaudeCode)
}

pub fn convert_request_with_mode(
    req: &MessagesRequest,
    tool_compatibility_mode: ToolCompatibilityMode,
) -> Result<ConversionResult, ConversionError> {
    // 1. 映射模型
    let model_id = map_model(&req.model).ok_or_else(|| {
        ConversionError::InvalidModel(
            invalid_model_reason(&req.model)
                .unwrap_or("模型 ID 无效")
                .to_string(),
        )
    })?;

    // 2. 检查消息列表
    if req.messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    // 2.5. assistant prefill：Kiro 上游没有预填槽位，转成末尾 user 指令而非丢弃。
    //
    // opencode 在 agent 达到 `steps` 上限时会发一条末尾 assistant 消息
    // （`session/prompt.ts` 的 MAX_STEPS_PROMPT，内容形如「已达最大步数，只许输出
    // 文字」）—— 这是它的**合法正常流程**，不是客户端错误。默认四个内置 agent 都
    // 没配 `steps`，但用户一旦配了，无条件 400 会把会话打死且错误信息难以溯源。
    //
    // prefill 的语义是「约束模型接下来的输出」，转成 user 消息末尾的指令语义损失
    // 最小，也不丢任何信息。这不是静默降级：转换会记 warn。
    let prefilled: Option<Vec<super::types::Message>> =
        match req.messages.last().filter(|m| m.role != "user") {
            None => None,
            Some(tail) => {
                let directive = collect_message_text(&tail.content);
                if directive.trim().is_empty() {
                    // 空 prefill 没有可转换的约束，直接去掉尾巴。
                    tracing::warn!("末尾 assistant 消息为空，已移除（上游无 prefill 槽位）");
                    Some(req.messages[..req.messages.len() - 1].to_vec())
                } else {
                    tracing::warn!(
                        directive_len = directive.len(),
                        "检测到 assistant prefill（常见来源：opencode 的 agent steps 上限），\
                         上游无预填槽位，已转为末尾 user 指令"
                    );
                    let mut rebuilt = req.messages[..req.messages.len() - 1].to_vec();
                    match rebuilt.last_mut().filter(|m| m.role == "user") {
                        // 末尾已是 user：把指令并入该消息，避免产生连续 user 消息。
                        Some(last_user) => {
                            let mut merged = collect_message_text(&last_user.content);
                            if !merged.is_empty() {
                                merged.push_str("\n\n");
                            }
                            merged.push_str(&directive);
                            last_user.content = serde_json::Value::String(merged);
                        }
                        None => rebuilt.push(super::types::Message {
                            role: "user".to_string(),
                            content: serde_json::Value::String(directive),
                        }),
                    }
                    Some(rebuilt)
                }
            }
        };
    let messages: &[_] = prefilled.as_deref().unwrap_or(&req.messages);
    if messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    // 3. 生成会话 ID 和代理 ID
    // 优先从 metadata.user_id 中提取 session UUID 作为 conversationId
    let conversation_id = req
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_ref())
        .and_then(|user_id| extract_session_id(user_id))
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let agent_continuation_id = Uuid::new_v4().to_string();

    // 4. 确定触发类型
    let chat_trigger_type = determine_chat_trigger_type(req);

    // 5. 处理最后一条消息作为 current_message（经过 prefill 预处理，末尾必为 user）
    let last_message = messages.last().unwrap();
    let (text_content, images, tool_results) = process_message_content(&last_message.content)?;

    // 6. 转换工具定义（超长名称自动缩短并记录映射；ClaudeCode 模式做内置工具适配）
    let mut tool_name_map = HashMap::new();
    let mut tools = convert_tools(&req.tools, &mut tool_name_map, tool_compatibility_mode)?;

    // 收集本次请求声明的所有工具名（原始 client 名），供 `<invoke>` 容错的工具表校验。
    let mut known_tool_names: std::collections::HashSet<String> = req
        .tools
        .as_ref()
        .map(|ts| ts.iter().map(|t| t.name.clone()).collect())
        .unwrap_or_default();
    // 建议3 修复：超长工具名（>63）会被 shorten 成短名发给上游，模型回吐的也是短名。
    // tool_name_map 的 key 正是这些短名，一并加入，避免「超长名工具的合法 invoke 被漏捞」。
    for short in tool_name_map.keys() {
        known_tool_names.insert(short.clone());
    }

    // 7. 构建历史消息（需要先构建，以便收集历史中使用的工具）
    let mut history = build_history(
        req,
        messages,
        &model_id,
        &mut tool_name_map,
        tool_compatibility_mode,
    )?;

    // 8. 验证并过滤 tool_use/tool_result 配对
    // 移除孤立的 tool_result（没有对应的 tool_use）
    // 同时返回孤立的 tool_use_id 集合，用于后续清理
    let (validated_tool_results, orphaned_tool_use_ids) =
        validate_tool_pairing(&history, &tool_results);

    // 9. 从历史中移除孤立的 tool_use（Kiro API 要求 tool_use 必须有对应的 tool_result）
    remove_orphaned_tool_uses(&mut history, &orphaned_tool_use_ids);

    // 10. 收集历史中使用的工具名称，为缺失的工具生成占位符定义
    // Kiro API 要求：历史消息中引用的工具必须在 tools 列表中有定义
    // 注意：Kiro 匹配工具名称时忽略大小写，所以这里也需要忽略大小写比较
    let history_tool_names = collect_history_tool_names(&history);
    let existing_tool_names: std::collections::HashSet<_> = tools
        .iter()
        .map(|t| t.tool_specification.name.to_lowercase())
        .collect();

    for tool_name in history_tool_names {
        if !existing_tool_names.contains(&tool_name.to_lowercase()) {
            tools.push(create_placeholder_tool(&tool_name));
        }
    }

    // 11. 构建 UserInputMessageContext
    let mut context = UserInputMessageContext::new();
    if !tools.is_empty() {
        context = context.with_tools(tools);
    }
    if !validated_tool_results.is_empty() {
        context = context.with_tool_results(validated_tool_results);
    }

    // 12. 构建当前消息
    // 保留文本内容，即使有工具结果也不丢弃用户文本
    let content = text_content;

    let mut user_input = UserInputMessage::new(content, &model_id)
        .with_context(context)
        .with_origin("AI_EDITOR");

    if !images.is_empty() {
        user_input = user_input.with_images(images);
    }

    let current_message = CurrentMessage::new(user_input);

    // 13. 构建 ConversationState
    let conversation_state = ConversationState::new(conversation_id)
        .with_agent_continuation_id(agent_continuation_id)
        .with_agent_task_type("vibe")
        .with_chat_trigger_type(chat_trigger_type)
        .with_current_message(current_message)
        .with_history(history);

    if !tool_name_map.is_empty() {
        tracing::info!(
            "工具名称映射: {} 个超长名称已缩短",
            tool_name_map.len()
        );
    }

    // 14. Extract effort into AdditionalModelRequestFields only for models that accept it.
    //
    // The system-prompt thinking prefix remains available for every thinking mode. The real
    // wire field is narrower: newer/non-adaptive models reject it with
    // `additionalModelRequestFields is not supported for this model`, so keep the field opt-in
    // by upstream model capability rather than by the mere presence of client output_config.
    let additional_model_request_fields = build_additional_model_request_fields(req, &model_id)
        .map_err(ConversionError::UnsupportedRequest)?;

    Ok(ConversionResult {
        conversation_state,
        tool_name_map,
        known_tool_names,
        additional_model_request_fields,
    })
}

/// 确定聊天触发类型
/// "AUTO" 模式可能会导致 400 Bad Request 错误
fn determine_chat_trigger_type(_req: &MessagesRequest) -> String {
    "MANUAL".to_string()
}

/// 处理消息内容，提取文本、图片和工具结果
fn process_message_content(
    content: &serde_json::Value,
) -> Result<(String, Vec<KiroImage>, Vec<ToolResult>), ConversionError> {
    process_message_content_dedup(content, None)
}

/// Same as `process_message_content`, but when `dedup` is `Some` it deduplicates images by SHA256:
/// the same image (identical base64) recurring across history is kept only on first sight and later replaced with placeholder text,
/// avoiding the same screenshot being re-sent as base64 over multiple turns and burning tokens.
fn process_message_content_dedup(
    content: &serde_json::Value,
    mut dedup: Option<&mut std::collections::HashSet<String>>,
) -> Result<(String, Vec<KiroImage>, Vec<ToolResult>), ConversionError> {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    let mut tool_results = Vec::new();

    match content {
        serde_json::Value::String(s) => {
            text_parts.push(s.clone());
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                // 反序列化失败不静默跳过：客户端发的 content block 凭空消失会让
                // 对话内容缺失且无从察觉，至少要留下痕迹。
                let block = match serde_json::from_value::<ContentBlock>(item.clone()) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            block_type = %item.get("type").and_then(|v| v.as_str()).unwrap_or("<none>"),
                            "客户端 content block 解析失败，已跳过: {}",
                            e
                        );
                        continue;
                    }
                };
                {
                    match block.block_type.as_str() {
                        "text" => {
                            if let Some(text) = block.text {
                                text_parts.push(text);
                            }
                        }
                        "image" => {
                            if let Some(source) = block.source
                                && let Some(placeholder) =
                                    extract_kiro_image(&source, &mut dedup, &mut images)
                            {
                                text_parts.push(placeholder);
                            }
                        }
                        "tool_result" => {
                            if let Some(tool_use_id) = block.tool_use_id {
                                let result_content =
                                    extract_tool_result_content(&block.content, &mut dedup, &mut images);
                                let is_error = block.is_error.unwrap_or(false);

                                let mut result = if is_error {
                                    ToolResult::error(&tool_use_id, result_content)
                                } else {
                                    ToolResult::success(&tool_use_id, result_content)
                                };
                                result.status =
                                    Some(if is_error { "error" } else { "success" }.to_string());

                                tool_results.push(result);
                            }
                        }
                        "tool_use" => {
                            // tool_use 在 assistant 消息中处理，这里忽略
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    Ok((text_parts.join("\n"), images, tool_results))
}

/// 从 media_type 获取图片格式
fn get_image_format(media_type: &str) -> Option<String> {
    match media_type {
        "image/jpeg" => Some("jpeg".to_string()),
        "image/png" => Some("png".to_string()),
        "image/gif" => Some("gif".to_string()),
        "image/webp" => Some("webp".to_string()),
        _ => None,
    }
}

/// Converts an image block's source into a `KiroImage` and pushes it onto the top-level `images`.
///
/// Reuses the same conversion chain as top-level images (format validation + SHA256 dedup + resize + `from_base64`),
/// so an image inside a tool_result is lifted into the top-level images field the same way.
/// Returns `Some(placeholder)` when history dedup hit and the image was omitted; `None` when it was lifted or the format is unsupported.
fn extract_kiro_image(
    source: &ImageSource,
    dedup: &mut Option<&mut std::collections::HashSet<String>>,
    images: &mut Vec<KiroImage>,
) -> Option<String> {
    let format = get_image_format(&source.media_type)?;
    // History dedup: an already-seen image omits its base64 and returns placeholder text
    if let Some(seen) = dedup.as_deref_mut() {
        let mut hasher = Sha256::new();
        hasher.update(source.data.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        if !seen.insert(digest) {
            return Some("[image omitted: identical to an earlier screenshot]".to_string());
        }
    }
    let cfg = ResizeConfig::from_env();
    let processed = maybe_shrink_image(cfg, &format, &source.data);
    images.push(KiroImage::from_base64(processed.format, processed.data_base64));
    None
}

/// 提取工具结果内容
///
/// Text elements remain as tool_result placeholder text; blocks with `type=="image"` are extracted into a `KiroImage`
/// and lifted to the top-level `images` (Amazon Q's `ToolResult` has no image field, so images can only go through the top-level channel).
/// If a tool_result has only images and no text, the placeholder text "[image attached]" is used.
fn extract_tool_result_content(
    content: &Option<serde_json::Value>,
    dedup: &mut Option<&mut std::collections::HashSet<String>>,
    images: &mut Vec<KiroImage>,
) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let mut parts = Vec::new();
            let mut had_image = false;
            for item in arr {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                } else if item.get("type").and_then(|v| v.as_str()) == Some("image")
                    && let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone())
                    && let Some(source) = block.source
                {
                    had_image = true;
                    if let Some(placeholder) = extract_kiro_image(&source, dedup, images) {
                        parts.push(placeholder);
                    }
                }
            }
            if parts.is_empty() && had_image {
                "[image attached]".to_string()
            } else {
                parts.join("\n")
            }
        }
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

/// 验证并过滤 tool_use/tool_result 配对
///
/// 收集所有 tool_use_id，验证 tool_result 是否匹配
/// 静默跳过孤立的 tool_use 和 tool_result，输出警告日志
///
/// # Arguments
/// * `history` - 历史消息引用
/// * `tool_results` - 当前消息中的 tool_result 列表
///
/// # Returns
/// 元组：(经过验证和过滤后的 tool_result 列表, 孤立的 tool_use_id 集合)
fn validate_tool_pairing(
    history: &[Message],
    tool_results: &[ToolResult],
) -> (Vec<ToolResult>, std::collections::HashSet<String>) {
    use std::collections::HashSet;

    // 1. 收集所有历史中的 tool_use_id
    let mut all_tool_use_ids: HashSet<String> = HashSet::new();
    // 2. 收集历史中已经有 tool_result 的 tool_use_id
    let mut history_tool_result_ids: HashSet<String> = HashSet::new();

    for msg in history {
        match msg {
            Message::Assistant(assistant_msg) => {
                if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                    for tool_use in tool_uses {
                        all_tool_use_ids.insert(tool_use.tool_use_id.clone());
                    }
                }
            }
            Message::User(user_msg) => {
                // 收集历史 user 消息中的 tool_results
                for result in &user_msg
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                {
                    history_tool_result_ids.insert(result.tool_use_id.clone());
                }
            }
        }
    }

    // 3. 计算真正未配对的 tool_use_ids（排除历史中已配对的）
    let mut unpaired_tool_use_ids: HashSet<String> = all_tool_use_ids
        .difference(&history_tool_result_ids)
        .cloned()
        .collect();

    // 4. 过滤并验证当前消息的 tool_results
    let mut filtered_results = Vec::new();

    for result in tool_results {
        if unpaired_tool_use_ids.contains(&result.tool_use_id) {
            // 配对成功
            filtered_results.push(result.clone());
            unpaired_tool_use_ids.remove(&result.tool_use_id);
        } else if all_tool_use_ids.contains(&result.tool_use_id) {
            // tool_use 存在但已经在历史中配对过了，这是重复的 tool_result
            tracing::warn!(
                "跳过重复的 tool_result：该 tool_use 已在历史中配对，tool_use_id={}",
                result.tool_use_id
            );
        } else {
            // 孤立 tool_result - 找不到对应的 tool_use
            tracing::warn!(
                "跳过孤立的 tool_result：找不到对应的 tool_use，tool_use_id={}",
                result.tool_use_id
            );
        }
    }

    // 5. 检测真正孤立的 tool_use（有 tool_use 但在历史和当前消息中都没有 tool_result）
    for orphaned_id in &unpaired_tool_use_ids {
        tracing::warn!(
            "检测到孤立的 tool_use：找不到对应的 tool_result，将从历史中移除，tool_use_id={}",
            orphaned_id
        );
    }

    (filtered_results, unpaired_tool_use_ids)
}

/// 从历史消息中移除孤立的 tool_use
///
/// Kiro API 要求每个 tool_use 必须有对应的 tool_result，否则返回 400 Bad Request。
/// 此函数遍历历史中的 assistant 消息，移除没有对应 tool_result 的 tool_use。
///
/// # Arguments
/// * `history` - 可变的历史消息列表
/// * `orphaned_ids` - 需要移除的孤立 tool_use_id 集合
fn remove_orphaned_tool_uses(
    history: &mut [Message],
    orphaned_ids: &std::collections::HashSet<String>,
) {
    if orphaned_ids.is_empty() {
        return;
    }

    for msg in history.iter_mut() {
        if let Message::Assistant(assistant_msg) = msg {
            if let Some(ref mut tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                let original_len = tool_uses.len();
                tool_uses.retain(|tu| !orphaned_ids.contains(&tu.tool_use_id));

                // 如果移除后为空，设置为 None
                if tool_uses.is_empty() {
                    assistant_msg.assistant_response_message.tool_uses = None;
                } else if tool_uses.len() != original_len {
                    tracing::debug!(
                        "从 assistant 消息中移除了 {} 个孤立的 tool_use",
                        original_len - tool_uses.len()
                    );
                }
            }
        }
    }
}

/// Kiro API 工具名称最大长度限制
const TOOL_NAME_MAX_LEN: usize = 63;

/// 生成确定性短名称：截断前缀 + "_" + 8 位 SHA256 hex
fn shorten_tool_name(name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    let hash_hex = format!("{:x}", hasher.finalize());
    let hash_suffix = &hash_hex[..8];
    // 54 prefix + 1 underscore + 8 hash = 63
    let prefix_max = TOOL_NAME_MAX_LEN - 1 - 8;
    let prefix = match name.char_indices().nth(prefix_max) {
        Some((idx, _)) => &name[..idx],
        None => name,
    };
    format!("{}_{}", prefix, hash_suffix)
}

/// 如果名称超长则缩短，并记录映射（short → original）
fn map_tool_name(name: &str, tool_name_map: &mut HashMap<String, String>) -> String {
    if name.len() <= TOOL_NAME_MAX_LEN {
        return name.to_string();
    }
    let short = shorten_tool_name(name);
    tool_name_map.insert(short.clone(), name.to_string());
    short
}

/// 转换工具定义
/// Claude Code 内置工具名 → Kiro 内置工具名。
fn claude_code_tool_name_to_kiro(name: &str) -> Option<&'static str> {
    match name {
        "Write" => Some("fs_write"),
        "Edit" => Some("str_replace"),
        "Bash" => Some("execute_bash"),
        "Read" => Some("read_file"),
        "Glob" => Some("file_search"),
        "Grep" => Some("grep_search"),
        "LS" => Some("list_directory"),
        "WebSearch" => Some("web_search"),
        _ => None,
    }
}

fn is_claude_code_mode(mode: ToolCompatibilityMode) -> bool {
    mode == ToolCompatibilityMode::ClaudeCode
}

/// 出站工具名映射：ClaudeCode 模式命中内置则改名并记录 `kiro名 → 客户端名`；
/// 否则回退到长名缩短逻辑（map_tool_name）。
fn map_client_tool_name_to_kiro(
    name: &str,
    tool_name_map: &mut HashMap<String, String>,
    mode: ToolCompatibilityMode,
) -> String {
    if is_claude_code_mode(mode)
        && let Some(kiro_name) = claude_code_tool_name_to_kiro(name)
    {
        tool_name_map
            .entry(kiro_name.to_string())
            .or_insert_with(|| name.to_string());
        return kiro_name.to_string();
    }
    map_tool_name(name, tool_name_map)
}

fn optional_number(value: &serde_json::Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_u64().map(|v| v as i64))
}

fn take_first(
    obj: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<serde_json::Value> {
    keys.iter().find_map(|key| obj.get(*key).cloned())
}

fn maybe_insert(
    out: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: Option<serde_json::Value>,
) {
    if let Some(value) = value
        && !value.is_null()
    {
        out.insert(key.to_string(), value);
    }
}

fn default_explanation(tool_name: &str) -> serde_json::Value {
    serde_json::Value::String(format!("Mapped from Claude Code {} tool.", tool_name))
}

/// 出站入参重写（Anthropic → Kiro 内置工具入参键）。Raw / 非内置直通。
fn map_tool_input_to_kiro(
    client_name: &str,
    input: serde_json::Value,
    mode: ToolCompatibilityMode,
) -> Result<serde_json::Value, ConversionError> {
    if !is_claude_code_mode(mode) {
        return Ok(input);
    }
    let Some(kiro_name) = claude_code_tool_name_to_kiro(client_name) else {
        return Ok(input);
    };
    let serde_json::Value::Object(obj) = input else {
        return Ok(input);
    };

    let mut out = serde_json::Map::new();
    match (client_name, kiro_name) {
        ("Write", "fs_write") => {
            maybe_insert(&mut out, "path", take_first(&obj, &["file_path", "path"]));
            maybe_insert(&mut out, "text", take_first(&obj, &["content", "text"]));
        }
        ("Edit", "str_replace") => {
            maybe_insert(&mut out, "path", take_first(&obj, &["file_path", "path"]));
            maybe_insert(&mut out, "oldStr", take_first(&obj, &["old_string", "oldStr"]));
            maybe_insert(&mut out, "newStr", take_first(&obj, &["new_string", "newStr"]));
        }
        ("Bash", "execute_bash") => {
            maybe_insert(&mut out, "command", take_first(&obj, &["command"]));
            maybe_insert(&mut out, "timeout", take_first(&obj, &["timeout"]));
        }
        ("Read", "read_file") => {
            if obj.contains_key("pages") && !obj.get("pages").is_some_and(|v| v.is_null()) {
                return Err(ConversionError::UnsupportedToolMapping(
                    "Claude Code Read.pages has no Kiro read_file equivalent".to_string(),
                ));
            }
            maybe_insert(&mut out, "path", take_first(&obj, &["file_path", "path"]));
            let offset = obj.get("offset").and_then(optional_number);
            let limit = obj.get("limit").and_then(optional_number);
            if let Some(start) = offset {
                out.insert("start_line".to_string(), serde_json::json!(start));
            }
            if let Some(limit) = limit {
                let end = offset.map(|start| start + limit - 1).unwrap_or(limit);
                out.insert("end_line".to_string(), serde_json::json!(end));
            }
            maybe_insert(&mut out, "explanation", take_first(&obj, &["explanation"]));
            out.entry("explanation".to_string())
                .or_insert_with(|| default_explanation(client_name));
        }
        ("Glob", "file_search") => {
            maybe_insert(&mut out, "query", take_first(&obj, &["pattern", "query"]));
            maybe_insert(
                &mut out,
                "excludePattern",
                take_first(&obj, &["excludePattern", "exclude"]),
            );
            if let Some(v) = take_first(&obj, &["includeIgnoredFiles", "include_ignored"]) {
                let mapped = match v {
                    serde_json::Value::Bool(true) => serde_json::json!("yes"),
                    serde_json::Value::Bool(false) => serde_json::json!("no"),
                    other => other,
                };
                out.insert("includeIgnoredFiles".to_string(), mapped);
            }
            maybe_insert(&mut out, "explanation", take_first(&obj, &["explanation"]));
            out.entry("explanation".to_string())
                .or_insert_with(|| default_explanation(client_name));
        }
        ("Grep", "grep_search") => {
            maybe_insert(&mut out, "query", take_first(&obj, &["pattern", "query"]));
            maybe_insert(
                &mut out,
                "includePattern",
                take_first(&obj, &["glob", "includePattern"]),
            );
            maybe_insert(
                &mut out,
                "excludePattern",
                take_first(&obj, &["excludePattern", "exclude"]),
            );
            maybe_insert(
                &mut out,
                "caseSensitive",
                take_first(&obj, &["caseSensitive", "case_sensitive"]),
            );
            maybe_insert(&mut out, "explanation", take_first(&obj, &["explanation"]));
        }
        ("LS", "list_directory") => {
            maybe_insert(&mut out, "path", take_first(&obj, &["path"]));
            maybe_insert(&mut out, "depth", take_first(&obj, &["depth"]));
            maybe_insert(&mut out, "explanation", take_first(&obj, &["explanation"]));
            out.entry("explanation".to_string())
                .or_insert_with(|| default_explanation(client_name));
        }
        ("WebSearch", "web_search") => {
            maybe_insert(&mut out, "query", take_first(&obj, &["query"]));
        }
        _ => return Ok(serde_json::Value::Object(obj)),
    }
    Ok(serde_json::Value::Object(out))
}

/// 入站入参还原（Kiro 内置工具入参键 → Anthropic）。**以 Kiro 名匹配**，故自动只在
/// 出站确实映射过（ClaudeCode）时生效；Raw 模式 / 长名缩短 / 透传工具一律直通。
///
/// 这是相对参考实现的一处修正：参考以“客户端名”匹配，导致 Raw 模式下客户端自带的、
/// 恰好叫 `Read` 的工具入参也会被误改写。以 Kiro 名匹配避免了该误伤，且入站无需穿透 mode。
fn map_tool_input_from_kiro(kiro_name: &str, input: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(obj) = input else {
        return input;
    };
    let mut out = serde_json::Map::new();
    match kiro_name {
        "fs_write" => {
            maybe_insert(&mut out, "file_path", take_first(&obj, &["path", "file_path"]));
            maybe_insert(&mut out, "content", take_first(&obj, &["text", "content"]));
        }
        "str_replace" => {
            maybe_insert(&mut out, "file_path", take_first(&obj, &["path", "file_path"]));
            maybe_insert(
                &mut out,
                "old_string",
                take_first(&obj, &["oldStr", "old_string"]),
            );
            maybe_insert(
                &mut out,
                "new_string",
                take_first(&obj, &["newStr", "new_string"]),
            );
        }
        "execute_bash" => {
            maybe_insert(&mut out, "command", take_first(&obj, &["command"]));
            maybe_insert(&mut out, "timeout", take_first(&obj, &["timeout"]));
        }
        "read_file" => {
            maybe_insert(&mut out, "file_path", take_first(&obj, &["path", "file_path"]));
            let start = obj.get("start_line").and_then(optional_number);
            let end = obj.get("end_line").and_then(optional_number);
            if let Some(start) = start {
                out.insert("offset".to_string(), serde_json::json!(start));
            }
            if let Some(end) = end {
                let limit = start.map(|s| end - s + 1).unwrap_or(end);
                if limit > 0 {
                    out.insert("limit".to_string(), serde_json::json!(limit));
                }
            }
        }
        "file_search" => {
            maybe_insert(&mut out, "pattern", take_first(&obj, &["query", "pattern"]));
        }
        "grep_search" => {
            maybe_insert(&mut out, "pattern", take_first(&obj, &["query", "pattern"]));
            maybe_insert(&mut out, "glob", take_first(&obj, &["includePattern", "glob"]));
            maybe_insert(
                &mut out,
                "case_sensitive",
                take_first(&obj, &["caseSensitive", "case_sensitive"]),
            );
        }
        "list_directory" => {
            maybe_insert(&mut out, "path", take_first(&obj, &["path"]));
        }
        "web_search" => {
            maybe_insert(&mut out, "query", take_first(&obj, &["query"]));
        }
        _ => return serde_json::Value::Object(obj),
    }
    serde_json::Value::Object(out)
}

/// 入站还原工具名 + 入参给客户端。名字从 `tool_name_map`（kiro名→客户端名）还原；
/// 入参按 kiro_name 反向重写（对非内置 / 长名缩短是 no-op）。
pub fn restore_tool_use_for_client(
    kiro_name: &str,
    input: serde_json::Value,
    tool_name_map: &HashMap<String, String>,
) -> (String, serde_json::Value) {
    let client_name = tool_name_map
        .get(kiro_name)
        .cloned()
        .unwrap_or_else(|| kiro_name.to_string());
    let client_input = map_tool_input_from_kiro(kiro_name, input);
    (client_name, client_input)
}

fn optional_schema(schema: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "anyOf": [schema, {"type": "null"}] })
}

/// Kiro 内置工具的描述（含防截断后缀）。
fn kiro_builtin_tool_description(kiro_name: &str, fallback: &str) -> String {
    match kiro_name {
        "fs_write" => format!(
            "Write text content to a file.\n{}",
            WRITE_TOOL_DESCRIPTION_SUFFIX
        ),
        "str_replace" => format!(
            "Replace an exact string in a file.\n{}",
            EDIT_TOOL_DESCRIPTION_SUFFIX
        ),
        "execute_bash" => format!(
            "Execute the specified bash command.\n{}",
            BASH_TOOL_DESCRIPTION_SUFFIX
        ),
        "read_file" => "Read a single file with optional line range specification.".to_string(),
        "file_search" => "Search for files by fuzzy file path query.".to_string(),
        "grep_search" => "Search file contents using a regex pattern.".to_string(),
        "list_directory" => "List directory contents.".to_string(),
        "web_search" => "Search the web for up-to-date information.".to_string(),
        _ if fallback.trim().is_empty() => kiro_name.to_string(),
        _ => fallback.to_string(),
    }
}

/// Kiro 内置工具的硬编码 input schema（Draft-07 子集）。
fn kiro_builtin_tool_schema(kiro_name: &str) -> Option<serde_json::Value> {
    Some(match kiro_name {
        "fs_write" => serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute path to file."},
                "text": {"type": "string", "description": "Contents to write into the file."}
            },
            "required": ["path", "text"],
            "additionalProperties": false
        }),
        "str_replace" => serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute path to file."},
                "oldStr": {"type": "string", "description": "Exact string to replace."},
                "newStr": {"type": "string", "description": "Replacement string."}
            },
            "required": ["path", "oldStr", "newStr"],
            "additionalProperties": false
        }),
        "execute_bash" => serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Bash command to execute."},
                "timeout": optional_schema(serde_json::json!({"type": "number", "description": "Optional timeout in milliseconds."}))
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        "read_file" => serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to file to read."},
                "start_line": optional_schema(serde_json::json!({"type": "number", "description": "Starting line number."})),
                "end_line": optional_schema(serde_json::json!({"type": "number", "description": "Ending line number."})),
                "explanation": {"type": "string", "description": "Why this file is being read."}
            },
            "required": ["path", "explanation"],
            "additionalProperties": false
        }),
        "file_search" => serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Fuzzy filename query."},
                "explanation": {"type": "string", "description": "Why this search is being performed."},
                "excludePattern": optional_schema(serde_json::json!({"type": "string", "description": "Glob pattern for files to exclude."})),
                "includeIgnoredFiles": optional_schema(serde_json::json!({"type": "string", "description": "Whether to include ignored files, yes or no."}))
            },
            "required": ["query", "explanation"],
            "additionalProperties": false
        }),
        "grep_search" => serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1, "description": "Regex pattern to search for."},
                "caseSensitive": optional_schema(serde_json::json!({"type": "boolean", "description": "Whether the search should be case sensitive."})),
                "includePattern": optional_schema(serde_json::json!({"type": "string", "description": "Glob pattern for files to include."})),
                "excludePattern": optional_schema(serde_json::json!({"type": "string", "description": "Glob pattern for files to exclude."})),
                "explanation": optional_schema(serde_json::json!({"type": "string", "description": "Why this search is being performed."}))
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        "list_directory" => serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to directory."},
                "depth": optional_schema(serde_json::json!({"type": "number", "description": "Depth of recursive listing."})),
                "explanation": {"type": "string", "description": "Why this directory is being listed."}
            },
            "required": ["path", "explanation"],
            "additionalProperties": false
        }),
        "web_search" => serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query."}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        _ => return None,
    })
}

fn convert_tools(
    tools: &Option<Vec<super::types::Tool>>,
    tool_name_map: &mut HashMap<String, String>,
    mode: ToolCompatibilityMode,
) -> Result<Vec<Tool>, ConversionError> {
    let Some(tools) = tools else {
        return Ok(Vec::new());
    };

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();

    for t in tools {
        // ClaudeCode 模式隐藏 Kiro 不支持的 fs_append。
        if is_claude_code_mode(mode) && t.name == "fs_append" {
            tracing::debug!("Claude Code 兼容模式隐藏 fs_append 工具");
            continue;
        }

        let mapped_name = map_client_tool_name_to_kiro(&t.name, tool_name_map, mode);
        // 按小写名去重（Kiro 工具名大小写不敏感），首个出现者胜出。
        if !seen.insert(mapped_name.to_lowercase()) {
            tracing::debug!("跳过重复的映射工具名: {}", mapped_name);
            continue;
        }

        let is_builtin =
            is_claude_code_mode(mode) && claude_code_tool_name_to_kiro(&t.name).is_some();

        let description = if is_builtin {
            kiro_builtin_tool_description(&mapped_name, &t.description)
        } else {
            let mut description = t.description.clone();
            // 非内置（或 Raw 模式）：保留旧的 Write/Edit/Bash 后缀（按原始名匹配）。
            let suffix = match t.name.as_str() {
                "Write" => WRITE_TOOL_DESCRIPTION_SUFFIX,
                "Edit" => EDIT_TOOL_DESCRIPTION_SUFFIX,
                "Bash" => BASH_TOOL_DESCRIPTION_SUFFIX,
                _ => "",
            };
            if !suffix.is_empty() {
                description.push('\n');
                description.push_str(suffix);
            }
            // kiro API 不接受空描述，填充占位符
            if description.trim().is_empty() {
                t.name.clone()
            } else {
                description
            }
        };

        // 限制描述长度为 10000 字符（安全截断 UTF-8，单次遍历）
        let description = match description.char_indices().nth(10000) {
            Some((idx, _)) => description[..idx].to_string(),
            None => description,
        };

        let schema = if is_builtin {
            kiro_builtin_tool_schema(&mapped_name).ok_or_else(|| {
                ConversionError::UnsupportedToolMapping(format!(
                    "{} 无法映射到 Kiro 工具 schema",
                    t.name
                ))
            })?
        } else {
            normalize_json_schema(serde_json::json!(t.input_schema))
        };

        out.push(Tool {
            tool_specification: ToolSpecification {
                name: mapped_name,
                description,
                input_schema: InputSchema::from_json(schema),
            },
        });
    }

    Ok(out)
}

/// 构建历史消息
///
/// # Arguments
/// * `req` - 原始请求，用于读取 `system`、`thinking` 等配置字段
/// * `messages` - 经过 prefill 预处理的消息切片，末尾必定是 user 消息。
///   注意：该切片与 `req.messages` 可能不同（prefill 时会截断末尾的 assistant 消息），
///   调用方应始终使用此参数而非 `req.messages`。
/// * `model_id` - 已映射的 Kiro 模型 ID
fn build_history(req: &MessagesRequest, messages: &[super::types::Message], model_id: &str, tool_name_map: &mut HashMap<String, String>, mode: ToolCompatibilityMode) -> Result<Vec<Message>, ConversionError> {
    let mut history = Vec::new();

    // 生成thinking前缀（如果需要）
    // 本部署只服务 claude-opus-5 / gpt-5.6-sol，两者都走 Kiro 原生 reasoning 字段
    // （output_config.effort / reasoning.effort），因此不再注入 `<thinking_mode>`
    // prompt 标签 —— 上游版本对该注入**无模型门控**，会与原生 effort 同时下发：
    // 既污染历史上下文，又把推理强度变成「写在提示词里的字面量」而非上游真实档位。

    // 1. 处理系统消息
    if let Some(ref system) = req.system {
        let system_content: String = system
            .iter()
            .map(|s| s.text.clone())
            .collect::<Vec<_>>()
            .join("\n");

        if !system_content.is_empty() {
            // 追加分块写入策略到系统消息
            let system_content = format!("{}\n{}", system_content, SYSTEM_CHUNKED_POLICY);

            // 注入thinking标签到系统消息最前面（如果需要且不存在）
            let final_content = system_content;

            // 系统消息作为 user + assistant 配对
            let user_msg = HistoryUserMessage::new(final_content, model_id);
            history.push(Message::User(user_msg));

            let assistant_msg = HistoryAssistantMessage::new("I will follow these instructions.");
            history.push(Message::Assistant(assistant_msg));
        }
    }

    // 2. 处理常规消息历史
    // 最后一条消息作为 currentMessage，不加入历史
    // 经过 prefill 预处理后，messages 末尾必定是 user，故直接截掉最后一条即可
    let history_end_index = messages.len().saturating_sub(1);

    // 收集并配对消息
    let mut user_buffer: Vec<&super::types::Message> = Vec::new();
    let mut assistant_buffer: Vec<&super::types::Message> = Vec::new();
    // SHA256 dedup set for images spanning the whole history; a repeated image is kept only on first sight
    let mut image_dedup: std::collections::HashSet<String> = std::collections::HashSet::new();

    for i in 0..history_end_index {
        let msg = &messages[i];

        if msg.role == "user" {
            // 先处理累积的 assistant 消息
            if !assistant_buffer.is_empty() {
                let merged = merge_assistant_messages(&assistant_buffer, tool_name_map, mode)?;
                history.push(Message::Assistant(merged));
                assistant_buffer.clear();
            }
            user_buffer.push(msg);
        } else if msg.role == "assistant" {
            // 先处理累积的 user 消息
            if !user_buffer.is_empty() {
                let merged_user = merge_user_messages(&user_buffer, model_id, &mut image_dedup)?;
                history.push(Message::User(merged_user));
                user_buffer.clear();
            }
            // 累积 assistant 消息（支持连续多条）
            assistant_buffer.push(msg);
        }
    }

    // 处理末尾累积的 assistant 消息
    if !assistant_buffer.is_empty() {
        let merged = merge_assistant_messages(&assistant_buffer, tool_name_map, mode)?;
        history.push(Message::Assistant(merged));
    }

    // 处理结尾的孤立 user 消息
    if !user_buffer.is_empty() {
        let merged_user = merge_user_messages(&user_buffer, model_id, &mut image_dedup)?;
        history.push(Message::User(merged_user));

        // 自动配对一个 "OK" 的 assistant 响应
        let auto_assistant = HistoryAssistantMessage::new("OK");
        history.push(Message::Assistant(auto_assistant));
    }

    Ok(history)
}

/// 合并多个 user 消息
fn merge_user_messages(
    messages: &[&super::types::Message],
    model_id: &str,
    dedup: &mut std::collections::HashSet<String>,
) -> Result<HistoryUserMessage, ConversionError> {
    let mut content_parts = Vec::new();
    let mut all_images = Vec::new();
    let mut all_tool_results = Vec::new();

    for msg in messages {
        let (text, images, tool_results) =
            process_message_content_dedup(&msg.content, Some(dedup))?;
        if !text.is_empty() {
            content_parts.push(text);
        }
        all_images.extend(images);
        all_tool_results.extend(tool_results);
    }

    let content = content_parts.join("\n");
    // 保留文本内容，即使有工具结果也不丢弃用户文本
    let mut user_msg = UserMessage::new(&content, model_id);

    if !all_images.is_empty() {
        user_msg = user_msg.with_images(all_images);
    }

    if !all_tool_results.is_empty() {
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(all_tool_results);
        user_msg = user_msg.with_context(ctx);
    }

    Ok(HistoryUserMessage {
        user_input_message: user_msg,
    })
}

/// 转换 assistant 消息
fn convert_assistant_message(
    msg: &super::types::Message,
    tool_name_map: &mut HashMap<String, String>,
    mode: ToolCompatibilityMode,
) -> Result<HistoryAssistantMessage, ConversionError> {
    let mut thinking_content = String::new();
    let mut thinking_signature: Option<String> = None;
    let mut redacted_thinking: Option<String> = None;
    let mut text_content = String::new();
    let mut tool_uses = Vec::new();

    match &msg.content {
        serde_json::Value::String(s) => {
            text_content = s.clone();
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                // 反序列化失败不静默跳过：客户端发的 content block 凭空消失会让
                // 对话内容缺失且无从察觉，至少要留下痕迹。
                let block = match serde_json::from_value::<ContentBlock>(item.clone()) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            block_type = %item.get("type").and_then(|v| v.as_str()).unwrap_or("<none>"),
                            "客户端 content block 解析失败，已跳过: {}",
                            e
                        );
                        continue;
                    }
                };
                {
                    match block.block_type.as_str() {
                        // 历史思考走上游的结构化 `reasoningContent` 字段，**不再**拼成
                        // `<thinking>` 塞进 content。原因：上游解密 signature 重建思维链，
                        // content 里的明文对模型无效，只会污染上下文并诱导模型把内部
                        // 推理写进可见输出。Smithy union 每条消息只能带一个 reasoning
                        // 块，故多个 thinking 块的文本累加、签名取最后一个非空值。
                        "thinking" => {
                            // 只回传带**真签名**的思考。
                            //
                            // 上游解密 signature 重建思维链，签名对不上会返回
                            // `THINKING_SIGNATURE_INVALID`（本代理据此直接报错、不剥离
                            // 重试）。而当上游本轮没下发真签名时，出站侧会填一个占位串
                            // 以满足客户端「thinking 块必须带非空 signature」的本地校验
                            // —— 那个占位串会被客户端原样回传到这里，若继续转发给上游
                            // 就是**必然失败**的一次请求。
                            //
                            // 因此这里识别并丢弃占位签名的块：本轮用户照常看得到思考，
                            // 下一轮不会因为一个假签名把整个会话打死。
                            let is_placeholder = block
                                .signature
                                .as_deref()
                                .is_some_and(|s| s == super::stream::THINKING_SIGNATURE_PLACEHOLDER);
                            if is_placeholder {
                                tracing::debug!(
                                    "历史 thinking 块携带占位签名（上游本轮未下发真签名），\
                                     已跳过回传以避免上游验签失败"
                                );
                                continue;
                            }
                            if let Some(thinking) = block.thinking {
                                thinking_content.push_str(&thinking);
                            }
                            if let Some(sig) = block.signature.filter(|s| !s.is_empty()) {
                                thinking_signature = Some(sig);
                            }
                        }
                        // 加密思考：原样回传 base64 内容（与 reasoningText 互斥）。
                        "redacted_thinking" => {
                            if let Some(data) = block.data.filter(|d| !d.is_empty()) {
                                redacted_thinking = Some(data);
                            }
                        }
                        "text" => {
                            if let Some(text) = block.text {
                                text_content.push_str(&text);
                            }
                        }
                        "tool_use" => {
                            if let (Some(id), Some(name)) = (block.id, block.name) {
                                let input = block.input.unwrap_or(serde_json::json!({}));
                                let mapped_name =
                                    map_client_tool_name_to_kiro(&name, tool_name_map, mode);
                                let input = map_tool_input_to_kiro(&name, input, mode)?;
                                tool_uses
                                    .push(ToolUseEntry::new(id, mapped_name).with_input(input));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    let has_reasoning = redacted_thinking.is_some() || !thinking_content.is_empty();
    // content 只放可见正文；思考走 `reasoningContent` 结构化字段。
    // Kiro API 要求 content 非空，仅有 tool_use / 仅有思考时用空格占位。
    let final_content = if text_content.is_empty() && (!tool_uses.is_empty() || has_reasoning) {
        " ".to_string()
    } else {
        text_content
    };

    let mut assistant = AssistantMessage::new(final_content);
    if !tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(tool_uses);
    }
    // union 二选一：加密思考优先（它本身就是上游下发的完整密文），
    // 否则用明文 + 签名。签名缺失时仍下发 text —— Smithy 里 signature 是可选字段。
    if let Some(data) = redacted_thinking {
        assistant = assistant.with_reasoning_content(ReasoningContent::redacted(data));
    } else if !thinking_content.is_empty() {
        assistant = assistant
            .with_reasoning_content(ReasoningContent::text(thinking_content, thinking_signature));
    }

    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

/// 合并多个连续的 assistant 消息为一条
/// 用于处理网络不稳定时产生的连续 assistant 消息（Issue #79）
fn merge_assistant_messages(
    messages: &[&super::types::Message],
    tool_name_map: &mut HashMap<String, String>,
    mode: ToolCompatibilityMode,
) -> Result<HistoryAssistantMessage, ConversionError> {
    assert!(!messages.is_empty());
    if messages.len() == 1 {
        return convert_assistant_message(messages[0], tool_name_map, mode);
    }

    let mut all_tool_uses: Vec<ToolUseEntry> = Vec::new();
    let mut content_parts: Vec<String> = Vec::new();
    // `reasoningContent` 是 Smithy union，合并后的单条消息只能带一个。
    // 保留最后一条非空的：它对应最近一次推理，与紧随其后的 tool_use 配对。
    let mut merged_reasoning: Option<ReasoningContent> = None;

    for msg in messages {
        let converted = convert_assistant_message(msg, tool_name_map, mode)?;
        let am = converted.assistant_response_message;
        if !am.content.trim().is_empty() {
            content_parts.push(am.content);
        }
        if let Some(tus) = am.tool_uses {
            all_tool_uses.extend(tus);
        }
        if let Some(rc) = am.reasoning_content {
            merged_reasoning = Some(rc);
        }
    }

    let content = if content_parts.is_empty()
        && (!all_tool_uses.is_empty() || merged_reasoning.is_some())
    {
        " ".to_string()
    } else {
        content_parts.join("\n\n")
    };

    let mut assistant = AssistantMessage::new(content);
    if !all_tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(all_tool_uses);
    }
    if let Some(rc) = merged_reasoning {
        assistant = assistant.with_reasoning_content(rc);
    }
    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;











    #[test]
    fn test_map_model_rejects_invalid_ids() {
        assert!(map_model("").is_none());
        assert!(map_model("   ").is_none());
        assert!(map_model("bad\nmodel").is_none());
        assert!(map_model(&"x".repeat(MAX_MODEL_ID_LEN + 1)).is_none());
    }






    fn minimal_request_with_output_config(model: &str) -> MessagesRequest {
        minimal_request_with_effort(model, "high")
    }

    fn minimal_request_with_effort(model: &str, effort: &str) -> MessagesRequest {
        use super::super::types::{Message as AnthropicMessage, OutputConfig};

        MessagesRequest {
            model: model.to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: Some(OutputConfig {
                effort: effort.to_string(),
            }),
            effort: None,
            metadata: None,
        }
    }

    fn minimal_adaptive_thinking_request_with_output_config(model: &str) -> MessagesRequest {
        use super::super::types::Thinking;

        let mut req = minimal_request_with_output_config(model);
        req.thinking = Some(Thinking {
            display: None,
            thinking_type: "adaptive".to_string(),
            budget_tokens: 20000,
        });
        req
    }

    fn minimal_adaptive_thinking_request_with_effort(model: &str, effort: &str) -> MessagesRequest {
        use super::super::types::Thinking;

        let mut req = minimal_request_with_effort(model, effort);
        req.thinking = Some(Thinking {
            display: None,
            thinking_type: "adaptive".to_string(),
            budget_tokens: 20000,
        });
        req
    }

    fn minimal_thinking_request(model: &str, thinking_type: &str) -> MessagesRequest {
        use super::super::types::Thinking;

        let mut req = minimal_request_with_output_config(model);
        req.output_config = None;
        req.thinking = Some(Thinking {
            display: None,
            thinking_type: thinking_type.to_string(),
            budget_tokens: 20000,
        });
        req
    }










    #[test]
    fn test_output_config_normalizes_effort_case_and_spacing() {
        let req =
            minimal_adaptive_thinking_request_with_effort("claude-opus-5", "  MAX  ");
        let result = convert_request(&req).unwrap();

        let fields = result
            .additional_model_request_fields
            .expect("opus 4.6 adaptive thinking should keep output_config");
        assert_eq!(
            fields.output_config.unwrap().effort,
            "max",
            "effort should be normalized before being sent to upstream"
        );
    }

    /// 回归锁：非法 / 不受支持的 effort **不再静默回落 high**，直接拒绝。
    ///
    /// 静默改写档位等于篡改客户端请求的推理强度：调用方拿到 200，以为自己设的
    /// 档位生效了，实际跑的是另一档。
    #[test]
    fn unsupported_effort_is_rejected_not_downgraded() {
        // 无法识别的值
        let req = minimal_adaptive_thinking_request_with_effort("claude-opus-5", "extreme");
        let err = convert_request(&req).expect_err("非法 effort 必须被拒绝");
        let msg = format!("{err:?}");
        assert!(msg.contains("unrecognized effort"), "got: {msg}");
        assert!(
            msg.contains("low, medium, high, xhigh, max"),
            "应列出支持档位: {msg}"
        );

        // gpt-5.6-sol 独有的 none 档：opus-5 不支持，同样拒绝而非回落
        let req = minimal_adaptive_thinking_request_with_effort("claude-opus-5", "none");
        let err = convert_request(&req).expect_err("opus-5 不支持 none 档");
        assert!(format!("{err:?}").contains("not supported by model"));

        // 合法档位仍正常下发
        let req = minimal_adaptive_thinking_request_with_effort("claude-opus-5", "xhigh");
        let fields = convert_request(&req)
            .expect("合法 effort 应通过")
            .additional_model_request_fields
            .expect("应下发 output_config");
        assert_eq!(fields.output_config.unwrap().effort, "xhigh");
    }

    // ---- Fix 3: 原生 thinking effort 下发拓宽 + budget 推导 ----

    #[test]
    fn effort_from_budget_tokens_maps_tiers() {
        assert_eq!(effort_from_budget_tokens(2_000), "low");
        assert_eq!(effort_from_budget_tokens(4_000), "low");
        assert_eq!(effort_from_budget_tokens(10_000), "medium");
        assert_eq!(effort_from_budget_tokens(16_000), "medium");
        assert_eq!(effort_from_budget_tokens(20_000), "high");
        assert_eq!(effort_from_budget_tokens(64_000), "high");
        assert_eq!(effort_from_budget_tokens(100_000), "xhigh");
    }





    #[test]
    fn disabled_thinking_emits_nothing_even_for_supported_model() {
        let req = minimal_thinking_request("claude-opus-5", "disabled");
        let result = convert_request(&req).unwrap();
        assert!(
            result.additional_model_request_fields.is_none(),
            "thinking disabled 不应下发任何字段"
        );
    }


    // ---- normalize_json_schema: 顶层 type / 组合关键字（PR#6）----

    #[test]
    fn normalize_schema_forces_top_level_type_object() {
        let s = normalize_json_schema(serde_json::json!({
            "type": "array",
            "items": {"type": "string"}
        }));
        assert_eq!(s["type"], "object", "顶层 type 非 object 应被强制修正");
    }

    #[test]
    fn normalize_schema_strips_top_level_oneof_and_recovers_fields() {
        let s = normalize_json_schema(serde_json::json!({
            "oneOf": [
                {"type": "object", "properties": {"locs": {"type": "array"}}, "required": ["locs"]},
                {"type": "object", "properties": {"labels": {"type": "array"}}, "required": ["labels"]}
            ]
        }));
        assert_eq!(s["type"], "object");
        assert!(s.get("oneOf").is_none(), "顶层 oneOf 应被剥离");
        assert!(
            s["properties"].get("locs").is_some(),
            "应从首个 object variant 恢复 properties"
        );
        assert_eq!(s["required"], serde_json::json!(["locs"]));
    }

    #[test]
    fn normalize_schema_strips_top_level_anyof_without_object_variant() {
        let s = normalize_json_schema(serde_json::json!({
            "anyOf": [{"type": "string"}, {"type": "number"}]
        }));
        assert_eq!(s["type"], "object");
        assert!(s.get("anyOf").is_none());
        assert_eq!(s["properties"], serde_json::json!({}));
    }

    #[test]
    fn test_determine_chat_trigger_type() {
        // 无工具时返回 MANUAL
        let req = MessagesRequest {
            model: "claude-opus-5".to_string(),
            max_tokens: 1024,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            effort: None,
            metadata: None,
        };
        assert_eq!(determine_chat_trigger_type(&req), "MANUAL");
    }

    #[test]
    fn test_collect_history_tool_names() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 创建包含工具使用的历史消息
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
            ToolUseEntry::new("tool-2", "write")
                .with_input(serde_json::json!({"path": "/out.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-opus-5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let tool_names = collect_history_tool_names(&history);
        assert_eq!(tool_names.len(), 2);
        assert!(tool_names.contains(&"read".to_string()));
        assert!(tool_names.contains(&"write".to_string()));
    }

    #[test]
    fn test_create_placeholder_tool() {
        let tool = create_placeholder_tool("my_custom_tool");

        assert_eq!(tool.tool_specification.name, "my_custom_tool");
        assert!(!tool.tool_specification.description.is_empty());

        // 验证 JSON 序列化正确
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"name\":\"my_custom_tool\""));
    }

    #[test]
    fn test_shorten_tool_name_deterministic() {
        let long_name = "mcp__some_very_long_server_name__some_very_long_tool_name_that_exceeds_limit";
        assert!(long_name.len() > TOOL_NAME_MAX_LEN);

        let short1 = shorten_tool_name(long_name);
        let short2 = shorten_tool_name(long_name);
        assert_eq!(short1, short2, "相同输入应产生相同的短名称");
        assert!(short1.len() <= TOOL_NAME_MAX_LEN, "短名称长度应 <= 63，实际 {}", short1.len());
    }

    #[test]
    fn test_shorten_tool_name_uniqueness() {
        let name_a = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_a";
        let name_b = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_b";
        let short_a = shorten_tool_name(name_a);
        let short_b = shorten_tool_name(name_b);
        assert_ne!(short_a, short_b, "不同输入应产生不同的短名称");
    }

    #[test]
    fn test_map_tool_name_short_passthrough() {
        let mut map = HashMap::new();
        let result = map_tool_name("short_name", &mut map);
        assert_eq!(result, "short_name");
        assert!(map.is_empty(), "短名称不应产生映射");
    }

    #[test]
    fn test_map_tool_name_long_creates_mapping() {
        let mut map = HashMap::new();
        let long_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
        let result = map_tool_name(long_name, &mut map);
        assert!(result.len() <= TOOL_NAME_MAX_LEN);
        assert_eq!(map.get(&result), Some(&long_name.to_string()));
    }

    // ---- Tool Call 双向兼容（ClaudeCode 内置工具名/入参映射）----

    fn cc_tool(name: &str) -> super::super::types::Tool {
        let mut schema = std::collections::BTreeMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));
        super::super::types::Tool {
            name: name.to_string(),
            description: String::new(),
            input_schema: schema,
            tool_type: None,
            max_uses: None,
            cache_control: None,
        }
    }

    #[test]
    fn cc_builtin_name_table() {
        assert_eq!(claude_code_tool_name_to_kiro("Write"), Some("fs_write"));
        assert_eq!(claude_code_tool_name_to_kiro("Edit"), Some("str_replace"));
        assert_eq!(claude_code_tool_name_to_kiro("Bash"), Some("execute_bash"));
        assert_eq!(claude_code_tool_name_to_kiro("Read"), Some("read_file"));
        assert_eq!(claude_code_tool_name_to_kiro("Glob"), Some("file_search"));
        assert_eq!(claude_code_tool_name_to_kiro("Grep"), Some("grep_search"));
        assert_eq!(claude_code_tool_name_to_kiro("LS"), Some("list_directory"));
        assert_eq!(claude_code_tool_name_to_kiro("WebSearch"), Some("web_search"));
        assert_eq!(claude_code_tool_name_to_kiro("MyTool"), None);
    }

    #[test]
    fn cc_kiro_builtin_schema_covers_all_eight() {
        for k in [
            "fs_write",
            "str_replace",
            "execute_bash",
            "read_file",
            "file_search",
            "grep_search",
            "list_directory",
            "web_search",
        ] {
            assert!(kiro_builtin_tool_schema(k).is_some(), "{k} 应有内置 schema");
        }
        assert!(kiro_builtin_tool_schema("fs_append").is_none());
    }

    #[test]
    fn cc_convert_tools_maps_names_hides_fs_append_and_dedups() {
        let mut map = HashMap::new();
        // Write 与 fs_write 都映射/命中 fs_write（小写去重）；fs_append 被隐藏。
        let tools = Some(vec![
            cc_tool("Write"),
            cc_tool("Read"),
            cc_tool("fs_append"),
            cc_tool("fs_write"),
        ]);
        let out = convert_tools(&tools, &mut map, ToolCompatibilityMode::ClaudeCode).unwrap();
        let names: Vec<&str> = out
            .iter()
            .map(|t| t.tool_specification.name.as_str())
            .collect();
        assert!(names.contains(&"fs_write"), "Write 应映射为 fs_write");
        assert!(names.contains(&"read_file"), "Read 应映射为 read_file");
        assert!(!names.contains(&"fs_append"), "fs_append 应被隐藏");
        assert_eq!(
            names.iter().filter(|n| **n == "fs_write").count(),
            1,
            "Write 与 fs_write 应按小写去重为一个"
        );
        assert_eq!(map.get("read_file").map(|s| s.as_str()), Some("Read"));
    }

    #[test]
    fn cc_convert_tools_substitutes_builtin_schema() {
        let mut map = HashMap::new();
        let out = convert_tools(
            &Some(vec![cc_tool("Write")]),
            &mut map,
            ToolCompatibilityMode::ClaudeCode,
        )
        .unwrap();
        // 硬编码 fs_write schema：包含 path/text。
        let s = serde_json::to_string(&out[0]).unwrap();
        assert!(s.contains("\"path\""), "内置 schema 应含 path");
        assert!(s.contains("\"text\""), "内置 schema 应含 text");
    }

    #[test]
    fn cc_raw_mode_keeps_client_tool_names() {
        let mut map = HashMap::new();
        let out = convert_tools(
            &Some(vec![cc_tool("Write")]),
            &mut map,
            ToolCompatibilityMode::Raw,
        )
        .unwrap();
        assert_eq!(out[0].tool_specification.name, "Write", "Raw 模式不改名");
        assert!(map.is_empty(), "Raw 模式不记录内置映射");
    }

    #[test]
    fn cc_outbound_input_write_and_read() {
        let out = map_tool_input_to_kiro(
            "Write",
            serde_json::json!({"file_path": "/a.txt", "content": "hi"}),
            ToolCompatibilityMode::ClaudeCode,
        )
        .unwrap();
        assert_eq!(out, serde_json::json!({"path": "/a.txt", "text": "hi"}));

        let read = map_tool_input_to_kiro(
            "Read",
            serde_json::json!({"file_path": "/a", "offset": 10, "limit": 5}),
            ToolCompatibilityMode::ClaudeCode,
        )
        .unwrap();
        assert_eq!(read["path"], serde_json::json!("/a"));
        assert_eq!(read["start_line"], serde_json::json!(10));
        assert_eq!(read["end_line"], serde_json::json!(14)); // 10 + 5 - 1
        assert!(read.get("explanation").is_some(), "Read 缺省注入 explanation");
    }

    #[test]
    fn cc_outbound_read_pages_errors() {
        let err = map_tool_input_to_kiro(
            "Read",
            serde_json::json!({"file_path": "/a", "pages": "1-3"}),
            ToolCompatibilityMode::ClaudeCode,
        )
        .unwrap_err();
        assert!(matches!(err, ConversionError::UnsupportedToolMapping(_)));
    }

    #[test]
    fn cc_raw_mode_input_passthrough() {
        let input = serde_json::json!({"file_path": "/a.txt", "content": "hi"});
        let out = map_tool_input_to_kiro("Write", input.clone(), ToolCompatibilityMode::Raw).unwrap();
        assert_eq!(out, input, "Raw 模式入参原样透传");
    }

    #[test]
    fn cc_roundtrip_write_out_then_in() {
        let client = serde_json::json!({"file_path": "/a.txt", "content": "hello"});
        let kiro =
            map_tool_input_to_kiro("Write", client.clone(), ToolCompatibilityMode::ClaudeCode)
                .unwrap();
        assert_eq!(kiro, serde_json::json!({"path": "/a.txt", "text": "hello"}));
        let mut map = HashMap::new();
        map.insert("fs_write".to_string(), "Write".to_string());
        let (name, restored) = restore_tool_use_for_client("fs_write", kiro, &map);
        assert_eq!(name, "Write");
        assert_eq!(restored, client, "出入站往返应还原客户端入参");
    }

    #[test]
    fn cc_inbound_restore_read_file() {
        let mut map = HashMap::new();
        map.insert("read_file".to_string(), "Read".to_string());
        let (name, restored) = restore_tool_use_for_client(
            "read_file",
            serde_json::json!({"path": "/a", "start_line": 10, "end_line": 14}),
            &map,
        );
        assert_eq!(name, "Read");
        assert_eq!(restored["file_path"], serde_json::json!("/a"));
        assert_eq!(restored["offset"], serde_json::json!(10));
        assert_eq!(restored["limit"], serde_json::json!(5)); // 14 - 10 + 1
    }

    /// 优化点回归：入站还原以 **Kiro 名** 匹配，故 Raw 模式下客户端自带、恰好叫
    /// "Read" 的工具，其入参不会被误改写（tool_name_map 无该条目，"Read" 非 Kiro 内置名）。
    #[test]
    fn cc_inbound_restore_keyed_on_kiro_name_not_client_name() {
        let map = HashMap::new();
        let input = serde_json::json!({"offset": 3, "custom_field": true});
        let (name, restored) = restore_tool_use_for_client("Read", input.clone(), &map);
        assert_eq!(name, "Read");
        assert_eq!(
            restored, input,
            "客户端自带 Read 工具在 Raw 下入参必须原样保留（不被误映射）"
        );
    }

    #[test]
    fn cc_convert_request_default_maps_builtin_names() {
        use super::super::types::Message as AnthropicMessage;
        let req = MessagesRequest {
            model: "claude-opus-5".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            system: None,
            stream: false,
            tools: Some(vec![cc_tool("Write"), cc_tool("Read")]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            effort: None,
            metadata: None,
        };
        // convert_request 测试垫片默认 ClaudeCode 模式。
        let result = convert_request(&req).unwrap();
        assert_eq!(result.tool_name_map.get("fs_write").map(|s| s.as_str()), Some("Write"));
        assert_eq!(result.tool_name_map.get("read_file").map(|s| s.as_str()), Some("Read"));
    }

    #[test]
    fn test_tool_name_mapping_in_convert_request() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
        assert!(long_tool_name.len() > TOOL_NAME_MAX_LEN);

        let mut schema = std::collections::BTreeMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-opus-5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("test"),
                },
            ],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                cache_control: None,
            }]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            effort: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();

        // 应该有映射
        assert_eq!(result.tool_name_map.len(), 1);

        // 映射中的值应该是原始名称
        let (short, original) = result.tool_name_map.iter().next().unwrap();
        assert_eq!(original, long_tool_name);
        assert!(short.len() <= TOOL_NAME_MAX_LEN);

        // Kiro 请求中的工具名应该是短名称
        let tools = &result.conversation_state.current_message.user_input_message
            .user_input_message_context.tools;
        assert_eq!(tools[0].tool_specification.name, *short);
    }

    #[test]
    fn test_tool_name_mapping_in_history() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";

        let mut schema = std::collections::BTreeMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-opus-5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("use the tool"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "calling tool"},
                        {"type": "tool_use", "id": "toolu_01", "name": long_tool_name, "input": {}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01", "content": "done"}
                    ]),
                },
            ],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                cache_control: None,
            }]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            effort: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();
        let short_name = result.tool_name_map.iter().next().unwrap().0.clone();

        // 历史中 assistant 消息的 tool_use name 也应该被映射
        let history = &result.conversation_state.history;
        let mut found = false;
        for msg in history {
            if let Message::Assistant(a) = msg {
                if let Some(ref tool_uses) = a.assistant_response_message.tool_uses {
                    for tu in tool_uses {
                        if tu.tool_use_id == "toolu_01" {
                            assert_eq!(tu.name, short_name, "历史中的 tool_use name 应该是短名称");
                            found = true;
                        }
                    }
                }
            }
        }
        assert!(found, "应该在历史中找到 tool_use");
    }

    #[test]
    fn test_history_tools_added_to_tools_list() {
        use super::super::types::Message as AnthropicMessage;

        // 创建一个请求，历史中有工具使用，但 tools 列表为空
        let req = MessagesRequest {
            model: "claude-opus-5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "I'll read the file."},
                        {"type": "tool_use", "id": "tool-1", "name": "read", "input": {"path": "/test.txt"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": "file content"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None, // 没有提供工具定义
            tool_choice: None,
            thinking: None,
            output_config: None,
            effort: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();

        // 验证 tools 列表中包含了历史中使用的工具的占位符定义
        let tools = &result
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;

        assert!(!tools.is_empty(), "tools 列表不应为空");
        assert!(
            tools.iter().any(|t| t.tool_specification.name == "read"),
            "tools 列表应包含 'read' 工具的占位符定义"
        );
    }

    #[test]
    fn test_extract_session_id_valid() {
        // 测试有效的 user_id 格式
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_8bb5523b-ec7c-4540-a9ca-beb6d79f1552";
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_format() {
        // 测试 JSON 格式的 user_id
        let user_id = r#"{"device_id":"0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd","account_uuid":"","session_id":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_invalid_session() {
        // 测试 JSON 格式但 session_id 不是有效 UUID
        let user_id = r#"{"device_id":"abc","session_id":"not-a-uuid"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_no_session() {
        // 测试没有 session 的 user_id
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_invalid_uuid() {
        // 测试无效的 UUID 格式
        let user_id = "user_xxx_session_invalid-uuid";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_convert_request_with_session_metadata() {
        use super::super::types::{Message as AnthropicMessage, Metadata};

        // 测试带有 metadata 的请求，应该使用 session UUID 作为 conversationId
        let req = MessagesRequest {
            model: "claude-opus-5".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            effort: None,
            metadata: Some(Metadata {
                user_id: Some(
                    "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_a0662283-7fd3-4399-a7eb-52b9a717ae88".to_string(),
                ),
            }),
        };

        let result = convert_request(&req).unwrap();
        assert_eq!(
            result.conversation_state.conversation_id,
            "a0662283-7fd3-4399-a7eb-52b9a717ae88"
        );
    }

    #[test]
    fn test_convert_request_without_metadata() {
        use super::super::types::Message as AnthropicMessage;

        // 测试没有 metadata 的请求，应该生成新的 UUID
        let req = MessagesRequest {
            model: "claude-opus-5".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            effort: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();
        // 验证生成的是有效的 UUID 格式
        assert_eq!(result.conversation_state.conversation_id.len(), 36);
        assert_eq!(
            result
                .conversation_state
                .conversation_id
                .chars()
                .filter(|c| *c == '-')
                .count(),
            4
        );
    }

    #[test]
    fn test_validate_tool_pairing_orphaned_result() {
        // 测试孤立的 tool_result 被过滤
        // 历史中没有 tool_use，但 tool_results 中有 tool_result
        let history = vec![
            Message::User(HistoryUserMessage::new("Hello", "claude-opus-5")),
            Message::Assistant(HistoryAssistantMessage::new("Hi there!")),
        ];

        let tool_results = vec![ToolResult::success("orphan-123", "some result")];

        let (filtered, _) = validate_tool_pairing(&history, &tool_results);

        // 孤立的 tool_result 应该被过滤掉
        assert!(filtered.is_empty(), "孤立的 tool_result 应该被过滤");
    }

    #[test]
    fn test_validate_tool_pairing_orphaned_use() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试孤立的 tool_use（有 tool_use 但没有对应的 tool_result）
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-orphan", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-opus-5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // 没有 tool_result
        let tool_results: Vec<ToolResult> = vec![];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 结果应该为空（因为没有 tool_result）
        // 同时应该返回孤立的 tool_use_id
        assert!(filtered.is_empty());
        assert!(orphaned.contains("tool-orphan"));
    }

    #[test]
    fn test_validate_tool_pairing_valid() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试正常配对的情况
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-opus-5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let tool_results = vec![ToolResult::success("tool-1", "file content")];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 配对成功，应该保留，无孤立
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tool_use_id, "tool-1");
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_validate_tool_pairing_mixed() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试混合情况：部分配对成功，部分孤立
        let mut assistant_msg = AssistantMessage::new("I'll use two tools.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-opus-5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // tool_results: tool-1 配对，tool-3 孤立
        let tool_results = vec![
            ToolResult::success("tool-1", "result 1"),
            ToolResult::success("tool-3", "orphan result"), // 孤立
        ];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 只有 tool-1 应该保留
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tool_use_id, "tool-1");
        // tool-2 是孤立的 tool_use（无 result），tool-3 是孤立的 tool_result
        assert!(orphaned.contains("tool-2"));
    }

    #[test]
    fn test_validate_tool_pairing_history_already_paired() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试历史中已配对的 tool_use 不应该被报告为孤立
        // 场景：多轮对话中，之前的 tool_use 已经在历史中有对应的 tool_result
        let mut assistant_msg1 = AssistantMessage::new("I'll read the file.");
        assistant_msg1 = assistant_msg1.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        // 构建历史中的 user 消息，包含 tool_result
        let mut user_msg_with_result = UserMessage::new("", "claude-opus-5");
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
        user_msg_with_result = user_msg_with_result.with_context(ctx);

        let history = vec![
            // 第一轮：用户请求
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-opus-5",
            )),
            // 第一轮：assistant 使用工具
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg1,
            }),
            // 第二轮：用户返回工具结果（历史中已配对）
            Message::User(HistoryUserMessage {
                user_input_message: user_msg_with_result,
            }),
            // 第二轮：assistant 响应
            Message::Assistant(HistoryAssistantMessage::new("The file contains...")),
        ];

        // 当前消息没有 tool_results（用户只是继续对话）
        let tool_results: Vec<ToolResult> = vec![];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 结果应该为空，且不应该有孤立 tool_use
        // 因为 tool-1 已经在历史中配对了
        assert!(filtered.is_empty());
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_validate_tool_pairing_duplicate_result() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试重复的 tool_result（历史中已配对，当前消息又发送了相同的 tool_result）
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        // 历史中已有 tool_result
        let mut user_msg_with_result = UserMessage::new("", "claude-opus-5");
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
        user_msg_with_result = user_msg_with_result.with_context(ctx);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-opus-5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
            Message::User(HistoryUserMessage {
                user_input_message: user_msg_with_result,
            }),
            Message::Assistant(HistoryAssistantMessage::new("Done")),
        ];

        // 当前消息又发送了相同的 tool_result（重复）
        let tool_results = vec![ToolResult::success("tool-1", "file content again")];

        let (filtered, _) = validate_tool_pairing(&history, &tool_results);

        // 重复的 tool_result 应该被过滤掉
        assert!(filtered.is_empty(), "重复的 tool_result 应该被过滤");
    }

    #[test]
    fn test_convert_assistant_message_tool_use_only() {
        use super::super::types::Message as AnthropicMessage;

        // 测试仅包含 tool_use 的 assistant 消息（无 text 块）
        // Kiro API 要求 content 字段不能为空
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let result = convert_assistant_message(&msg, &mut HashMap::new(), ToolCompatibilityMode::Raw).expect("应该成功转换");

        // 验证 content 不为空（使用占位符）
        assert!(
            !result.assistant_response_message.content.is_empty(),
            "content 不应为空"
        );
        assert_eq!(
            result.assistant_response_message.content, " ",
            "仅 tool_use 时应使用 ' ' 占位符"
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
        assert_eq!(tool_uses[0].name, "read_file");
    }

    #[test]
    fn test_convert_assistant_message_with_text_and_tool_use() {
        use super::super::types::Message as AnthropicMessage;

        // 测试同时包含 text 和 tool_use 的 assistant 消息
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "text", "text": "Let me read that file for you."},
                {"type": "tool_use", "id": "toolu_02XYZ", "name": "read_file", "input": {"path": "/data.json"}}
            ]),
        };

        let result = convert_assistant_message(&msg, &mut HashMap::new(), ToolCompatibilityMode::Raw).expect("应该成功转换");

        // 验证 content 使用原始文本（不是占位符）
        assert_eq!(
            result.assistant_response_message.content,
            "Let me read that file for you."
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_02XYZ");
    }

    #[test]
    fn test_remove_orphaned_tool_uses() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试从历史中移除孤立的 tool_use
        let mut assistant_msg = AssistantMessage::new("I'll use multiple tools.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-3", "delete").with_input(serde_json::json!({})),
        ]);

        let mut history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-opus-5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // 移除 tool-1 和 tool-3
        let mut orphaned = std::collections::HashSet::new();
        orphaned.insert("tool-1".to_string());
        orphaned.insert("tool-3".to_string());

        remove_orphaned_tool_uses(&mut history, &orphaned);

        // 验证只剩下 tool-2
        if let Message::Assistant(ref assistant_msg) = history[1] {
            let tool_uses = assistant_msg
                .assistant_response_message
                .tool_uses
                .as_ref()
                .expect("应该还有 tool_uses");
            assert_eq!(tool_uses.len(), 1);
            assert_eq!(tool_uses[0].tool_use_id, "tool-2");
        } else {
            panic!("应该是 Assistant 消息");
        }
    }

    #[test]
    fn test_remove_orphaned_tool_uses_all_removed() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试移除所有 tool_use 后，tool_uses 变为 None
        let mut assistant_msg = AssistantMessage::new("I'll use a tool.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
        ]);

        let mut history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-opus-5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let mut orphaned = std::collections::HashSet::new();
        orphaned.insert("tool-1".to_string());

        remove_orphaned_tool_uses(&mut history, &orphaned);

        // 验证 tool_uses 变为 None
        if let Message::Assistant(ref assistant_msg) = history[1] {
            assert!(
                assistant_msg.assistant_response_message.tool_uses.is_none(),
                "移除所有 tool_use 后应为 None"
            );
        } else {
            panic!("应该是 Assistant 消息");
        }
    }

    /// 回归锁：assistant prefill 转成末尾 user 指令，不丢内容也不报错。
    ///
    /// opencode 在 agent 达到 `steps` 上限时会发末尾 assistant 消息
    /// （MAX_STEPS_PROMPT），那是它的合法流程。Kiro 上游没有预填槽位，
    /// 转成 user 指令语义损失最小。
    #[test]
    fn assistant_prefill_is_converted_to_trailing_user_directive() {
        use super::super::types::Message as AnthropicMessage;

        let mut req = minimal_request_with_output_config("claude-opus-5");
        req.messages = vec![
            AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("do the thing"),
            },
            AnthropicMessage {
                role: "assistant".to_string(),
                content: serde_json::json!([
                    {"type": "text", "text": "MAXIMUM STEPS REACHED. Respond with text only."}
                ]),
            },
        ];

        let result = convert_request(&req).expect("prefill 应被转换而非拒绝");
        let current = &result
            .conversation_state
            .current_message
            .user_input_message
            .content;
        // 转换后末尾必须是 user 消息，且同时含原始提问与 prefill 指令。
        assert!(current.contains("do the thing"), "原始 user 内容应保留: {current}");
        assert!(
            current.contains("MAXIMUM STEPS REACHED"),
            "prefill 指令应并入 user 消息: {current}"
        );
        // 历史里不应再出现那条 assistant 消息（它已被并入 user）。
        let rendered = serde_json::to_string(&result.conversation_state.history).unwrap();
        assert!(
            !rendered.contains("MAXIMUM STEPS REACHED"),
            "prefill 不应同时留在历史里"
        );
    }

    /// 回归锁：携带占位签名的历史 thinking 块不回传上游。
    ///
    /// 出站侧在上游未下发真签名时会填占位串（满足客户端「signature 必须非空」的
    /// 本地校验），客户端下一轮会原样回传。若继续转发，上游验签必然失败 →
    /// `THINKING_SIGNATURE_INVALID` → 会话被打死。真签名则必须原样回传。
    #[test]
    fn placeholder_signature_thinking_is_not_replayed_upstream() {
        use super::super::types::Message as AnthropicMessage;

        let with_sig = |sig: &str| {
            let mut req = minimal_request_with_output_config("claude-opus-5");
            req.messages = vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("q1"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "internal reasoning", "signature": sig},
                        {"type": "text", "text": "a1"}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("q2"),
                },
            ];
            convert_request(&req).expect("应转换成功")
        };

        // 占位签名 → 整块跳过，历史里没有 reasoningContent
        let placeholder = with_sig(crate::anthropic::stream::THINKING_SIGNATURE_PLACEHOLDER);
        let rendered = serde_json::to_string(&placeholder.conversation_state.history).unwrap();
        assert!(
            !rendered.contains("reasoningContent"),
            "占位签名的思考不应回传: {rendered}"
        );
        assert!(rendered.contains("a1"), "可见正文仍应保留");

        // 真签名 → 必须回传，且签名逐字节保真
        let real = with_sig("CAISyRAKcAgQEAEYAipAREALSIG");
        let rendered = serde_json::to_string(&real.conversation_state.history).unwrap();
        assert!(
            rendered.contains("CAISyRAKcAgQEAEYAipAREALSIG"),
            "真签名必须原样回传: {rendered}"
        );
        assert!(rendered.contains("internal reasoning"), "思考文本应随签名回传");
    }

    #[test]
    fn test_merge_consecutive_assistant_messages() {
        // 测试连续 assistant 消息被正确合并（Issue #79）
        use super::super::types::Message as AnthropicMessage;

        let msg1 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "Let me think about this..."},
                {"type": "text", "text": " "}
            ]),
        };

        let msg2 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "I should read the file."},
                {"type": "text", "text": "Let me read that file."},
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let messages: Vec<&AnthropicMessage> = vec![&msg1, &msg2];
        let result = merge_assistant_messages(&messages, &mut HashMap::new(), ToolCompatibilityMode::Raw).expect("合并应成功");

        let content = &result.assistant_response_message.content;
        // 思考不再拼进 content，而是走结构化 reasoningContent（上游解密 signature
        // 重建思维链，content 里的明文对模型无效且会污染上下文）。
        assert!(
            !content.contains("<thinking>"),
            "content 不应再包含 thinking XML 标签"
        );
        assert!(content.contains("Let me read that file"), "应包含第二条消息的 text 内容");
        let rc = result
            .assistant_response_message
            .reasoning_content
            .as_ref()
            .expect("合并后应保留 reasoningContent");
        let rt = rc.reasoning_text.as_ref().expect("应为 reasoningText 分支");
        assert!(
            rt.text.contains("I should read the file"),
            "应保留最后一条消息的思考文本, got: {}",
            rt.text
        );

        let tool_uses = result.assistant_response_message.tool_uses.expect("应有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
    }

    #[test]
    fn test_consecutive_assistant_with_tool_use_result_pairing() {
        // 测试 Issue #79 的完整场景
        use super::super::types::Message as AnthropicMessage;

        let req = MessagesRequest {
            model: "claude-opus-5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the config file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "I need to read the file..."},
                        {"type": "text", "text": " "}
                    ]),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "Let me read the config."},
                        {"type": "text", "text": "I'll read the config file for you."},
                        {"type": "tool_use", "id": "toolu_01XYZ", "name": "read_file", "input": {"path": "/config.json"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01XYZ", "content": "{\"key\": \"value\"}"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            effort: None,
            metadata: None,
        };

        let result = convert_request(&req);
        assert!(result.is_ok(), "连续 assistant 消息场景不应报错: {:?}", result.err());

        let state = result.unwrap().conversation_state;
        let mut found_tool_use = false;
        for msg in &state.history {
            if let Message::Assistant(assistant_msg) = msg {
                if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                    if tool_uses.iter().any(|t| t.tool_use_id == "toolu_01XYZ") {
                        found_tool_use = true;
                        break;
                    }
                }
            }
        }
        assert!(found_tool_use, "合并后的 assistant 消息应包含 tool_use");
    }

    // base64 of a 1x1 PNG (valid PNG header, so resize just passes it through)
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

    #[test]
    fn test_tool_result_image_lifts_to_top_level() {
        use super::super::types::Message as AnthropicMessage;

        // user question -> assistant tool_use -> user tool_result (with image + text)
        let req = MessagesRequest {
            model: "claude-opus-5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("take a screenshot"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_use", "id": "tool-1", "name": "screenshot", "input": {}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": [
                            {"type": "text", "text": "here is the screen"},
                            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": TINY_PNG_B64}}
                        ]}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            effort: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();
        let msg = &result.conversation_state.current_message.user_input_message;

        // image is lifted to the top-level images
        assert_eq!(msg.images.len(), 1, "image in tool_result should be lifted to top-level images");
        assert_eq!(msg.images[0].format, "png");
        assert_eq!(msg.images[0].source.bytes, TINY_PNG_B64);

        // tool_result itself keeps only the text placeholder (image stripped out)
        let tr = &msg.user_input_message_context.tool_results;
        assert_eq!(tr.len(), 1);
        assert_eq!(
            tr[0].content[0].get("text").and_then(|v| v.as_str()),
            Some("here is the screen"),
            "tool_result content should keep the text and contain no base64"
        );
    }

    #[test]
    fn test_tool_result_text_only_unchanged() {
        use super::super::types::Message as AnthropicMessage;

        // text-only tool_result: regression unchanged, should produce no top-level image
        let req = MessagesRequest {
            model: "claude-opus-5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("read the file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_use", "id": "tool-1", "name": "read", "input": {"path": "/a.txt"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": "file content"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            effort: None,
            metadata: None,
        };

        let result = convert_request(&req).unwrap();
        let msg = &result.conversation_state.current_message.user_input_message;

        assert!(msg.images.is_empty(), "text-only tool_result should produce no top-level image");
        let tr = &msg.user_input_message_context.tool_results;
        assert_eq!(tr.len(), 1);
        assert_eq!(
            tr[0].content[0].get("text").and_then(|v| v.as_str()),
            Some("file content"),
            "text-only tool_result content should be preserved as-is"
        );
    }
}
