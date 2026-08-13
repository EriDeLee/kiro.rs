//! 对话类型定义
//!
//! 定义 Kiro API 中对话相关的类型，包括消息、历史记录等

use serde::{Deserialize, Serialize};

use super::tool::{Tool, ToolResult, ToolUseEntry};

/// 对话状态
///
/// Kiro API 请求中的核心结构，包含当前消息和历史记录
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationState {
    /// 代理延续 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_continuation_id: Option<String>,
    /// 代理任务类型（通常为 "vibe"）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_task_type: Option<String>,
    /// 聊天触发类型（"MANUAL" 或 "AUTO"）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_trigger_type: Option<String>,
    /// 当前消息
    pub current_message: CurrentMessage,
    /// 会话 ID
    pub conversation_id: String,
    /// 历史消息列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Message>,
}

impl ConversationState {
    /// 创建新的对话状态
    pub fn new(conversation_id: impl Into<String>) -> Self {
        Self {
            agent_continuation_id: None,
            agent_task_type: None,
            chat_trigger_type: None,
            current_message: CurrentMessage::default(),
            conversation_id: conversation_id.into(),
            history: Vec::new(),
        }
    }

    /// 设置代理延续 ID
    pub fn with_agent_continuation_id(mut self, id: impl Into<String>) -> Self {
        self.agent_continuation_id = Some(id.into());
        self
    }

    /// 设置代理任务类型
    pub fn with_agent_task_type(mut self, task_type: impl Into<String>) -> Self {
        self.agent_task_type = Some(task_type.into());
        self
    }

    /// 设置聊天触发类型
    pub fn with_chat_trigger_type(mut self, trigger_type: impl Into<String>) -> Self {
        self.chat_trigger_type = Some(trigger_type.into());
        self
    }

    /// 设置当前消息
    pub fn with_current_message(mut self, message: CurrentMessage) -> Self {
        self.current_message = message;
        self
    }

    /// 添加历史消息
    pub fn with_history(mut self, history: Vec<Message>) -> Self {
        self.history = history;
        self
    }

    /// 返回历史消息和当前消息中的图片总数。
    pub fn image_count(&self) -> usize {
        let history_count = self
            .history
            .iter()
            .filter_map(|message| match message {
                Message::User(message) => Some(message.user_input_message.images.len()),
                Message::Assistant(_) => None,
            })
            .sum::<usize>();
        history_count + self.current_message.user_input_message.images.len()
    }

    /// 按会话时间顺序丢弃最早的一张图片。
    pub fn remove_oldest_image(&mut self) -> bool {
        for message in &mut self.history {
            if let Message::User(message) = message
                && !message.user_input_message.images.is_empty()
            {
                message.user_input_message.images.remove(0);
                return true;
            }
        }

        let current_images = &mut self.current_message.user_input_message.images;
        if current_images.is_empty() {
            false
        } else {
            current_images.remove(0);
            true
        }
    }
}

/// 当前消息容器
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentMessage {
    /// 用户输入消息
    pub user_input_message: UserInputMessage,
}

impl CurrentMessage {
    /// 创建新的当前消息
    pub fn new(user_input_message: UserInputMessage) -> Self {
        Self { user_input_message }
    }
}

/// 用户输入消息
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputMessage {
    /// 用户输入消息上下文（承载工具声明与工具执行结果）
    pub user_input_message_context: UserInputMessageContext,
    /// 消息内容
    pub content: String,
    /// 模型 ID
    pub model_id: String,
    /// 图片列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<KiroImage>,
    /// 消息来源（通常为 "AI_EDITOR"）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl UserInputMessage {
    /// 创建新的用户输入消息
    pub fn new(content: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            user_input_message_context: UserInputMessageContext::default(),
            content: content.into(),
            model_id: model_id.into(),
            images: Vec::new(),
            origin: Some("AI_EDITOR".to_string()),
        }
    }

    /// 设置消息上下文
    pub fn with_context(mut self, context: UserInputMessageContext) -> Self {
        self.user_input_message_context = context;
        self
    }

    /// 添加图片
    pub fn with_images(mut self, images: Vec<KiroImage>) -> Self {
        self.images = images;
        self
    }

    /// 设置来源
    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }
}

/// 用户输入消息上下文
///
/// 包含工具定义和工具执行结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputMessageContext {
    /// 工具执行结果列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResult>,
    /// 可用工具列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
}

impl Default for UserInputMessageContext {
    fn default() -> Self {
        Self {
            tool_results: Vec::new(),
            tools: Vec::new(),
        }
    }
}

fn is_empty_context(ctx: &UserInputMessageContext) -> bool {
    ctx.tools.is_empty() && ctx.tool_results.is_empty()
}

impl UserInputMessageContext {
    /// 创建新的消息上下文
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置工具列表
    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = tools;
        self
    }

    /// 设置工具结果
    pub fn with_tool_results(mut self, results: Vec<ToolResult>) -> Self {
        self.tool_results = results;
        self
    }
}

/// Kiro 图片
///
/// API 中使用的图片格式
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroImage {
    /// 图片格式（"jpeg", "png", "gif", "webp"）
    pub format: String,
    /// 图片数据源
    pub source: KiroImageSource,
}

impl KiroImage {
    /// 从 base64 数据创建图片
    pub fn from_base64(format: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            format: format.into(),
            source: KiroImageSource { bytes: data.into() },
        }
    }
}

/// Kiro 图片数据源
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroImageSource {
    /// base64 编码的图片数据
    pub bytes: String,
}

/// 历史消息
///
/// 可以是用户消息或助手消息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    /// 用户消息
    User(HistoryUserMessage),
    /// 助手消息
    Assistant(HistoryAssistantMessage),
}

/// 历史用户消息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryUserMessage {
    /// 用户输入消息
    pub user_input_message: UserMessage,
}

impl HistoryUserMessage {
    /// 创建新的历史用户消息
    pub fn new(content: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            user_input_message: UserMessage::new(content, model_id),
        }
    }
}

/// 用户消息（历史记录中使用）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    /// 消息内容
    pub content: String,
    /// 模型 ID
    pub model_id: String,
    /// 消息来源
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// 图片列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<KiroImage>,
    /// 用户输入消息上下文（历史消息无工具时跳过）
    #[serde(default, skip_serializing_if = "is_empty_context")]
    pub user_input_message_context: UserInputMessageContext,
}

impl UserMessage {
    /// 创建新的用户消息
    pub fn new(content: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            model_id: model_id.into(),
            origin: Some("AI_EDITOR".to_string()),
            images: Vec::new(),
            user_input_message_context: UserInputMessageContext::default(),
        }
    }

    /// 设置图片
    pub fn with_images(mut self, images: Vec<KiroImage>) -> Self {
        self.images = images;
        self
    }

    /// 设置上下文
    pub fn with_context(mut self, context: UserInputMessageContext) -> Self {
        self.user_input_message_context = context;
        self
    }
}

/// 历史助手消息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryAssistantMessage {
    /// 助手响应消息
    pub assistant_response_message: AssistantMessage,
}

impl HistoryAssistantMessage {
    /// 用纯文本构造一条历史 assistant 消息。**仅测试使用**。
    ///
    /// 生产路径不再有调用者：客户端真实的 assistant 消息经
    /// `converter::convert_assistant_message` 构造（要携带 tool_uses /
    /// reasoningContent），而代理**不再伪造** assistant 轮次 ——
    /// 此前 system 后面跟一条 `"I will follow these instructions."`、
    /// 末尾孤立 user 后面补一条 `"OK"`，两者都已删除（实测上游不要求
    /// user/assistant 交替：相邻两条 user、孤立 user、孤立 assistant 全部 200）。
    #[cfg(test)]
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            assistant_response_message: AssistantMessage::new(content),
        }
    }
}

/// 助手消息（历史记录中使用）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    /// 响应内容
    pub content: String,
    /// 工具使用列表
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_uses: Option<Vec<ToolUseEntry>>,
    /// 模型的内部推理过程（历史回传）。
    ///
    /// 对应 AWS CodeWhispererRuntime Smithy 模型的
    /// `AssistantResponseMessage.reasoningContent`（"Model's internal reasoning
    /// process, either as readable text or redacted binary content"）。上游会
    /// **解密 signature 重建原始思维链**，并在签名失效时返回
    /// `THINKING_SIGNATURE_INVALID` —— 说明它确实解析并验签这个字段，不是静默丢弃。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<ReasoningContent>,
}

/// 历史推理内容。Smithy 定义为 union —— 两个成员**互斥**，同一对象只能出现一个。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningContent {
    /// 明文思考 + 签名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_text: Option<ReasoningText>,
    /// 加密思考内容（Smithy `blob`，线上编码为 base64 字符串）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted_content: Option<String>,
}

impl ReasoningContent {
    /// 明文思考分支。`text` 必填，`signature` 可选但**必须原样未修改**——
    /// 上游靠它解密重建思维链，任何改动都会导致验签失败。
    pub fn text(text: impl Into<String>, signature: Option<String>) -> Self {
        Self {
            reasoning_text: Some(ReasoningText {
                text: text.into(),
                signature,
            }),
            redacted_content: None,
        }
    }

    /// 加密思考分支。
    pub fn redacted(data: impl Into<String>) -> Self {
        Self {
            reasoning_text: None,
            redacted_content: Some(data.into()),
        }
    }
}

/// 明文思考块。对应 Smithy `ReasoningText`（`required: ["text"]`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningText {
    pub text: String,
    /// "A token that verifies that the reasoning text was generated by the model"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl AssistantMessage {
    /// 创建新的助手消息
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            tool_uses: None,
            reasoning_content: None,
        }
    }

    /// 设置工具使用
    pub fn with_tool_uses(mut self, tool_uses: Vec<ToolUseEntry>) -> Self {
        self.tool_uses = Some(tool_uses);
        self
    }

    /// 附加历史推理内容（含签名）。
    pub fn with_reasoning_content(mut self, reasoning: ReasoningContent) -> Self {
        self.reasoning_content = Some(reasoning);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conversation_state_new() {
        let state = ConversationState::new("conv-123")
            .with_agent_task_type("vibe")
            .with_chat_trigger_type("MANUAL");

        assert_eq!(state.conversation_id, "conv-123");
        assert_eq!(state.agent_task_type, Some("vibe".to_string()));
        assert_eq!(state.chat_trigger_type, Some("MANUAL".to_string()));
    }

    #[test]
    fn test_user_input_message() {
        let msg = UserInputMessage::new("Hello", "claude-3-5-sonnet").with_origin("AI_EDITOR");

        assert_eq!(msg.content, "Hello");
        assert_eq!(msg.model_id, "claude-3-5-sonnet");
        assert_eq!(msg.origin, Some("AI_EDITOR".to_string()));
    }

    #[test]
    fn test_history_serialize() {
        let history = vec![
            Message::User(HistoryUserMessage::new("Hello", "claude-3-5-sonnet")),
            Message::Assistant(HistoryAssistantMessage::new("Hi! How can I help you?")),
        ];

        let json = serde_json::to_string(&history).unwrap();
        assert!(json.contains("userInputMessage"));
        assert!(json.contains("assistantResponseMessage"));
    }

    #[test]
    fn test_conversation_state_serialize() {
        let state = ConversationState::new("conv-123")
            .with_agent_task_type("vibe")
            .with_current_message(CurrentMessage::new(UserInputMessage::new(
                "Hello",
                "claude-3-5-sonnet",
            )));

        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"conversationId\":\"conv-123\""));
        assert!(json.contains("\"agentTaskType\":\"vibe\""));
        assert!(json.contains("\"content\":\"Hello\""));
    }

    #[test]
    fn remove_oldest_image_follows_conversation_order() {
        let image = |name: &str| KiroImage::from_base64("png", name);
        let history = vec![
            Message::Assistant(HistoryAssistantMessage::new("first")),
            Message::User(HistoryUserMessage {
                user_input_message: UserMessage::new("old", "model")
                    .with_images(vec![image("old-1"), image("old-2")]),
            }),
            Message::User(HistoryUserMessage {
                user_input_message: UserMessage::new("newer", "model")
                    .with_images(vec![image("newer-1")]),
            }),
        ];
        let mut state = ConversationState::new("conv")
            .with_history(history)
            .with_current_message(CurrentMessage::new(
                UserInputMessage::new("current", "model")
                    .with_images(vec![image("current-1")]),
            ));

        assert_eq!(state.image_count(), 4);
        for expected_first in ["old-2", "newer-1", "current-1"] {
            assert!(state.remove_oldest_image());
            let first = state
                .history
                .iter()
                .filter_map(|message| match message {
                    Message::User(message) => message.user_input_message.images.first(),
                    Message::Assistant(_) => None,
                })
                .chain(state.current_message.user_input_message.images.first())
                .next()
                .unwrap();
            assert_eq!(first.source.bytes, expected_first);
        }
        assert!(state.remove_oldest_image());
        assert!(!state.remove_oldest_image());
        assert_eq!(state.image_count(), 0);
    }
}
