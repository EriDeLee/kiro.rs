//! 事件基础定义
//!
//! 定义事件类型枚举、trait 和统一事件结构

use crate::kiro::parser::error::{ParseError, ParseResult};
use crate::kiro::parser::frame::Frame;

/// 事件类型枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventType {
    /// 助手响应事件
    AssistantResponse,
    /// 工具使用事件
    ToolUse,
    /// 计费事件
    Metering,
    /// 上下文使用率事件
    ContextUsage,
    /// 推理内容事件
    ReasoningContent,
    /// 元数据事件。**实测只含 `stopReason`**（2026-07-28：三次请求骨架均为
    /// `{stopReason:str(8)}` = `END_TURN`，无 `tokenUsage`、无 cache 明细）。
    /// 显式识别以消除未知事件告警噪音 —— 否则真正的协议变更会被埋在噪音里。
    Metadata,
    /// 未知事件类型
    Unknown,
}

impl EventType {
    /// 从事件类型字符串解析
    pub fn from_str(s: &str) -> Self {
        match s {
            "assistantResponseEvent" => Self::AssistantResponse,
            "toolUseEvent" => Self::ToolUse,
            "meteringEvent" => Self::Metering,
            "contextUsageEvent" => Self::ContextUsage,
            "reasoningContentEvent" => Self::ReasoningContent,
            "metadataEvent" => Self::Metadata,
            // 未知事件类型：记录后丢弃。**必须留下痕迹** —— 上游加新事件时若完全
            // 静默，我们对协议变更就是失明的。
            //
            // 历史教训：`metadataEvent` 长期落在这条分支里被无声丢掉，谁也不知道
            // 它一直在下发。加上本条 warn 后第一次真实运行就抓到了它，随后实测
            // 确认其内容只有 `stopReason`（见上面 `Metadata` 变体的说明）。
            // 教训不在于「丢了重要数据」，而在于**我们当时无从判断丢的是什么**。
            other => {
                tracing::warn!(
                    event_type = %other,
                    "上游下发了未知事件类型，已丢弃；若反复出现说明上游协议已变更"
                );
                Self::Unknown
            }
        }
    }

    /// 转换为事件类型字符串
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AssistantResponse => "assistantResponseEvent",
            Self::ToolUse => "toolUseEvent",
            Self::Metering => "meteringEvent",
            Self::ContextUsage => "contextUsageEvent",
            Self::ReasoningContent => "reasoningContentEvent",
            Self::Metadata => "metadataEvent",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// 事件 payload trait
///
/// 所有具体事件类型都需要实现此 trait
pub trait EventPayload: Sized {
    /// 从帧解析事件负载
    fn from_frame(frame: &Frame) -> ParseResult<Self>;
}

/// 统一事件枚举
///
/// 封装所有可能的事件类型
#[derive(Debug, Clone)]
pub enum Event {
    /// 助手响应
    AssistantResponse(super::AssistantResponseEvent),
    /// 工具使用
    ToolUse(super::ToolUseEvent),
    /// 计费
    Metering(super::MeteringEvent),
    /// 上下文使用率
    ContextUsage(super::ContextUsageEvent),
    /// 推理内容
    ReasoningContent(super::ReasoningContentEvent),
    /// 未知事件 (保留原始帧数据)
    Unknown {},
    /// 服务端错误
    Error {
        /// 错误代码
        error_code: String,
        /// 错误消息
        error_message: String,
    },
    /// 服务端异常
    Exception {
        /// 异常类型
        exception_type: String,
        /// 异常消息
        message: String,
    },
}

/// 描述 JSON 的结构骨架：保留键名与数值，字符串只留长度。
///
/// 用于诊断未知上游事件的形状而**不泄露内容** —— 事件可能携带对话文本。
fn describe_json_skeleton(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, val)| format!("{k}:{}", describe_json_skeleton(val)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        serde_json::Value::Array(arr) => format!("[{} items]", arr.len()),
        // 数值与布尔是元数据（token 数、开关），可安全打印
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        // 字符串可能是对话内容：只报类型与长度
        serde_json::Value::String(s) => format!("str({})", s.len()),
    }
}

impl Event {
    /// 从帧解析事件
    pub fn from_frame(frame: Frame) -> ParseResult<Self> {
        let message_type = frame.message_type().unwrap_or("event");

        match message_type {
            "event" => Self::parse_event(frame),
            "error" => Self::parse_error(frame),
            "exception" => Self::parse_exception(frame),
            other => Err(ParseError::InvalidMessageType(other.to_string())),
        }
    }

    /// 解析事件类型消息
    fn parse_event(frame: Frame) -> ParseResult<Self> {
        let event_type_str = frame.event_type().unwrap_or("unknown");
        let event_type = EventType::from_str(event_type_str);

        match event_type {
            EventType::AssistantResponse => {
                let payload = super::AssistantResponseEvent::from_frame(&frame)?;
                Ok(Self::AssistantResponse(payload))
            }
            EventType::ToolUse => {
                let payload = super::ToolUseEvent::from_frame(&frame)?;
                Ok(Self::ToolUse(payload))
            }
            EventType::Metering => {
                let payload = super::MeteringEvent::from_frame(&frame)?;
                Ok(Self::Metering(payload))
            }
            EventType::ContextUsage => {
                let payload = super::ContextUsageEvent::from_frame(&frame)?;
                Ok(Self::ContextUsage(payload))
            }
            EventType::ReasoningContent => {
                let payload = super::ReasoningContentEvent::from_frame(&frame)?;
                Ok(Self::ReasoningContent(payload))
            }
            // `metadataEvent` 只携带 `stopReason`，而停止原因已由
            // `assistantResponseEvent` / 流式收尾逻辑决定，这里无需二次处理。
            // 显式匹配的意义在于：让未知事件告警只对**真正未见过的**事件类型触发。
            EventType::Metadata => Ok(Self::Unknown {}),
            EventType::Unknown => {
                // 诊断：打印未知事件的**结构骨架**（顶层键名 + 数值型叶子），不含
                // 任何文本内容。上游加新字段时这是唯一的发现途径 ——
                // `metadataEvent` 当年就是因为完全静默而被漏掉整整一个版本周期。
                if tracing::enabled!(tracing::Level::DEBUG) {
                    let raw = frame.payload_as_str();
                    let skeleton = serde_json::from_str::<serde_json::Value>(&raw)
                        .map(|v| describe_json_skeleton(&v))
                        .unwrap_or_else(|_| format!("<non-json, {} bytes>", raw.len()));
                    tracing::debug!(
                        event_type = %event_type_str,
                        "未知事件骨架: {}",
                        skeleton
                    );
                }
                Ok(Self::Unknown {})
            }
        }
    }

    /// 解析错误类型消息
    fn parse_error(frame: Frame) -> ParseResult<Self> {
        let error_code = frame
            .headers
            .error_code()
            .unwrap_or("UnknownError")
            .to_string();
        let error_message = frame.payload_as_str();

        Ok(Self::Error {
            error_code,
            error_message,
        })
    }

    /// 解析异常类型消息
    fn parse_exception(frame: Frame) -> ParseResult<Self> {
        let exception_type = frame
            .headers
            .exception_type()
            .unwrap_or("UnknownException")
            .to_string();
        let message = frame.payload_as_str();

        Ok(Self::Exception {
            exception_type,
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_type_from_str() {
        assert_eq!(
            EventType::from_str("assistantResponseEvent"),
            EventType::AssistantResponse
        );
        assert_eq!(EventType::from_str("toolUseEvent"), EventType::ToolUse);
        assert_eq!(EventType::from_str("meteringEvent"), EventType::Metering);
        assert_eq!(
            EventType::from_str("contextUsageEvent"),
            EventType::ContextUsage
        );
        assert_eq!(
            EventType::from_str("reasoningContentEvent"),
            EventType::ReasoningContent
        );
        assert_eq!(EventType::from_str("unknown_type"), EventType::Unknown);
    }

    #[test]
    fn test_event_type_as_str() {
        assert_eq!(
            EventType::AssistantResponse.as_str(),
            "assistantResponseEvent"
        );
        assert_eq!(EventType::ToolUse.as_str(), "toolUseEvent");
    }
}
