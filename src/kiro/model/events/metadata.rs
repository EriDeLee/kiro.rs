//! `metadataEvent`：上游本轮结束原因与拒答详情
//!
//! 常规轮次的 `stopReason` 是 `END_TURN` / `TOOL_USE`。被内容过滤拦截时，上游
//! **只**下发 reasoning、不下发正文与 meteringEvent，并用
//! `stopReason: CONTENT_FILTERED` + `stopDetails.refusal.{category,explanation}`
//! 结束本轮。把整个事件丢掉会把拒答伪装成“成功的空回答”，客户端只能反复重发
//! 同一段被拦截的历史。
//!
//! 该结论来自 2026-10-31 真实抓包（opus-5.5，`refusal_category=CYBER`）。

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 上游内容过滤拦截时 `metadataEvent.stopReason` 的取值。
pub const CONTENT_FILTERED_STOP_REASON: &str = "CONTENT_FILTERED";

/// `metadataEvent.stopDetails.refusal`：上游拒答的分类与说明。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Refusal {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub explanation: Option<String>,
}

/// `metadataEvent.stopDetails`。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StopDetails {
    #[serde(default)]
    pub refusal: Option<Refusal>,
}

/// `metadataEvent` payload。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEvent {
    /// 上游停止原因，如 `END_TURN` / `TOOL_USE` / `CONTENT_FILTERED`。
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_details: Option<StopDetails>,
}

impl MetadataEvent {
    /// 本轮是否被内容过滤拦截。
    pub fn is_content_filtered(&self) -> bool {
        self.stop_reason.as_deref() == Some(CONTENT_FILTERED_STOP_REASON)
    }

    /// 拒答的可读说明（`category: explanation`），非拒答返回 `None`。
    ///
    /// 只用于日志；调用方不得把解释正文塞进客户端响应。
    pub fn refusal_summary(&self) -> Option<String> {
        if !self.is_content_filtered() {
            return None;
        }
        let refusal = self.stop_details.as_ref().and_then(|d| d.refusal.as_ref());
        let category = refusal.and_then(|r| r.category.as_deref());
        let explanation = refusal.and_then(|r| r.explanation.as_deref());
        Some(match (category, explanation) {
            (Some(c), Some(e)) => format!("{c}: {e}"),
            (Some(c), None) => c.to_string(),
            (None, Some(e)) => e.to_string(),
            (None, None) => CONTENT_FILTERED_STOP_REASON.to_string(),
        })
    }
}

impl EventPayload for MetadataEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_filtered_is_refusal_with_upstream_explanation() {
        let event: MetadataEvent = serde_json::from_str(
            r#"{"stopReason":"CONTENT_FILTERED","stopDetails":{"refusal":{"category":"CYBER","explanation":"The selected model cannot continue this conversation."}}}"#,
        )
        .unwrap();
        assert!(event.is_content_filtered());
        assert_eq!(
            event.refusal_summary().as_deref(),
            Some("CYBER: The selected model cannot continue this conversation.")
        );
    }

    #[test]
    fn tool_use_and_end_turn_are_not_refusals() {
        for raw in [
            r#"{"stopReason":"END_TURN"}"#,
            r#"{"stopReason":"TOOL_USE"}"#,
            r#"{"stopReason":"MAX_TOKENS"}"#,
        ] {
            let event: MetadataEvent = serde_json::from_str(raw).unwrap();
            assert!(!event.is_content_filtered(), "{raw}");
            assert!(event.refusal_summary().is_none(), "{raw}");
        }
    }
}
