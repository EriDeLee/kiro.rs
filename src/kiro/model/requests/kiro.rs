//! Kiro 请求类型定义
//!
//! 定义 Kiro API 的主请求结构

use serde::{Deserialize, Serialize};

use super::conversation::ConversationState;

/// Kiro API 请求
///
/// 用于构建发送给 Kiro API 的请求
///
/// # 示例
///
/// ```rust
/// use kiro_rs::kiro::model::requests::{
///     KiroRequest, ConversationState, CurrentMessage, UserInputMessage, Tool
/// };
///
/// // 创建简单请求
/// let state = ConversationState::new("conv-123")
///     .with_agent_task_type("vibe")
///     .with_current_message(CurrentMessage::new(
///         UserInputMessage::new("Hello", "claude-3-5-sonnet")
///     ));
///
/// let request = KiroRequest::new(state);
/// let json = request.to_json().unwrap();
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroRequest {
    /// 对话状态
    pub conversation_state: ConversationState,
    /// Profile ARN（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    /// `additionalModelRequestFields` 原样下发。
    ///
    /// 具体形状由每个模型自己的 `additionalModelRequestFieldsSchema` 决定：
    /// 实测有 `output_config.effort` 与 `reasoning.effort` 两条路径，未来若上游
    /// 声明了别的属性，也能按同一份 schema 原样构造。这里用 `Value` 而不是固定
    /// 结构体，避免把新增字段路径写死。
    ///
    /// 注意内层属性名是 snake_case（`output_config`），外层键是 camelCase
    /// （`additionalModelRequestFields`）；`Value` 不参与字段改名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_model_request_fields: Option<serde_json::Value>,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_kiro_request_deserialize() {
        let json = r#"{
            "conversationState": {
                "conversationId": "conv-456",
                "currentMessage": {
                    "userInputMessage": {
                        "content": "Test message",
                        "modelId": "claude-3-5-sonnet",
                        "userInputMessageContext": {}
                    }
                }
            }
        }"#;

        let request: KiroRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.conversation_state.conversation_id, "conv-456");
        assert_eq!(
            request
                .conversation_state
                .current_message
                .user_input_message
                .content,
            "Test message"
        );
    }

    #[test]
    fn test_additional_model_request_fields_wire_format() {
        // The wire format requires the outer key to be camelCase
        // (`additionalModelRequestFields`) while the inner key stays snake_case
        // (`output_config`), matching real Kiro CLI traffic.
        let fields = serde_json::json!({
            "output_config": { "effort": "max" }
        });
        let v = serde_json::to_value(&fields).unwrap();
        assert_eq!(v["output_config"]["effort"], "max");
        assert!(
            v.get("outputConfig").is_none(),
            "inner key must stay snake_case output_config, got {v}"
        );
    }
}
