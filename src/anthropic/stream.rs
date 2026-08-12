//! 流式响应处理模块
//!
//! 实现 Kiro → Anthropic 流式响应转换和 SSE 状态管理

use std::collections::HashMap;

use serde_json::json;
use uuid::Uuid;

use crate::kiro::model::events::{Event, MeteringEvent};

/// 剥掉块前文本尾部的独立 stray token 行（单独一行的 `call` 或 `count`）
///
/// 实测里 `<invoke>` 前常出现一行裸 `call`/`count`，需要从块前叙述文本里剥掉，
/// 避免泄漏给客户端。只剥“尾部、且独占一行”的 stray token，前面的正常叙述保留。
/// 已实测到的 stray token 集合：Opus 长上下文退化时，泄漏的 `<invoke>` 前常有一行裸的
/// `call` / `count` / `card`。集合形式便于以后扩充。
const STRAY_INVOKE_TOKENS: &[&str] = &["call", "count", "card"];

/// 复读熔断阈值：同一个 stray token（call/count/card）连续作为独占一行重复出现
/// 超过这么多次，判定为「Opus 长上下文退化复读死循环」，立即熔断本轮文本输出。
///
/// 取值权衡：正常工具调用前最多出现 1 个引导词行（偶有 2~3），绝不会连续几十次。
/// 设为 32 远高于正常上限、又远低于退化时的数万次，既不误伤正常引导词，又能尽早止血。
const REPEAT_GUARD_TRIP_THRESHOLD: u32 = 32;

/// 累积完成的工具调用（`ToolUseEvent` 的所有分片拼接、解析成功后的结果）。
#[derive(Debug, Clone)]
pub struct CompletedToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

impl CompletedToolUse {
    /// 从 Kiro 侧 (name, input) 还原为客户端可见的完整工具调用。
    ///
    /// 这是**唯一的还原入口**：名字按 `tool_name_map` 还原、入参按 Kiro 名反向重写
    /// （见 `converter::restore_tool_use_for_client`）。结构化事件、`<invoke>` 文本捞回、
    /// websearch 三条来源都经此收敛，避免各站点各自调用还原逻辑。
    pub fn from_kiro(
        id: String,
        kiro_name: &str,
        input: serde_json::Value,
        tool_name_map: &HashMap<String, String>,
    ) -> Self {
        let (name, input) =
            super::converter::restore_tool_use_for_client(kiro_name, input, tool_name_map);
        Self { id, name, input }
    }

    /// 产出非流式 Anthropic `tool_use` 内容块。**唯一的非流式块拼装点。**
    pub fn to_anthropic_block(&self) -> serde_json::Value {
        json!({
            "type": "tool_use",
            "id": self.id,
            "name": self.name,
            "input": self.input,
        })
    }
}

/// 工具调用 JSON 累积过程中的错误。
///
/// - `InvalidJson`：上游把某个 tool_use 的完整 `input` 拼出来后，仍不是合法 JSON。
/// - `IncompleteJson`：整条流结束时仍有 tool_use 从未收到 `stop=true`，即上游在
///   工具参数写到一半时截断（“流式半截 JSON”）。
///
/// 两种情况都**不能**把半截 / 非法 JSON 当成完整工具调用转发给客户端——那会让
/// 客户端拿到无法解析或语义错误的参数去执行工具。这里显式暴露为错误，由上层
/// 决定回 502（非流式）或在 SSE 里补一个 `error` 事件（流式）。
#[derive(Debug, Clone)]
pub enum ToolJsonAccumulatorError {
    InvalidJson {
        tool_use_id: String,
        name: String,
        message: String,
    },
    IncompleteJson {
        tool_use_id: String,
        name: String,
        bytes: usize,
    },
}

impl ToolJsonAccumulatorError {
    /// Anthropic error 事件里统一的 error.type。
    pub fn error_type(&self) -> &'static str {
        "upstream_tool_json_error"
    }

    pub fn message(&self) -> String {
        match self {
            Self::InvalidJson {
                tool_use_id,
                name,
                message,
            } => format!(
                "Upstream returned invalid JSON for tool_use {} ({}): {}",
                tool_use_id, name, message
            ),
            Self::IncompleteJson {
                tool_use_id,
                name,
                bytes,
            } => format!(
                "Upstream ended before completing tool_use {} ({}) JSON input; buffered {} bytes. The tool call was not forwarded to the client.",
                tool_use_id, name, bytes
            ),
        }
    }
}

impl std::fmt::Display for ToolJsonAccumulatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ToolJsonAccumulatorError {}

/// 工具调用参数（JSON）累积器。
///
/// Kiro 把 tool_use 的 `input` JSON 拆成多个 `toolUseEvent` 分片下发，最后一片
/// 带 `stop=true`。分片可能切在 JSON 的任意字节位置（甚至 token 中间），因此
/// **不能**逐片当作 `input_json_delta` 直接转发——必须按 `tool_use_id` 累积，
/// 只在收到 `stop=true` 时整体解析，成功后一次性发出完整的工具调用。
#[derive(Debug, Default)]
pub struct ToolJsonAccumulator {
    /// tool_use_id -> (工具名, 已累积的 JSON 分片)
    buffers: HashMap<String, (String, String)>,
}

impl ToolJsonAccumulator {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
        }
    }

    /// 累积一个 `toolUseEvent` 分片。
    ///
    /// - 未收到 `stop` 时返回 `Ok(None)`（继续缓冲，不发出任何事件）。
    /// - 收到 `stop` 时把累积的 JSON 整体解析：成功返回 `Ok(Some(CompletedToolUse))`，
    ///   失败返回 `Err(InvalidJson)`。空参数按 `{}` 处理。
    /// - 工具名按 `tool_name_map` 还原为客户端原始名（短名 → 原名）。
    pub fn push(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
        tool_name_map: &HashMap<String, String>,
    ) -> Result<Option<CompletedToolUse>, ToolJsonAccumulatorError> {
        let entry = self
            .buffers
            .entry(tool_use.tool_use_id.clone())
            .or_insert_with(|| (tool_use.name.clone(), String::new()));
        if entry.0.is_empty() {
            entry.0 = tool_use.name.clone();
        }
        entry.1.push_str(&tool_use.input);

        if !tool_use.stop {
            return Ok(None);
        }

        let (kiro_name, input_json) = self
            .buffers
            .remove(&tool_use.tool_use_id)
            .unwrap_or_else(|| (tool_use.name.clone(), tool_use.input.clone()));
        let input = if input_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str::<serde_json::Value>(&input_json).map_err(|e| {
                ToolJsonAccumulatorError::InvalidJson {
                    tool_use_id: tool_use.tool_use_id.clone(),
                    name: kiro_name.clone(),
                    message: e.to_string(),
                }
            })?
        };

        // 通过统一入口还原客户端工具名 + 入参。
        Ok(Some(CompletedToolUse::from_kiro(
            tool_use.tool_use_id.clone(),
            &kiro_name,
            input,
            tool_name_map,
        )))
    }

    /// 流结束时收尾：若仍有从未收到 `stop=true` 的缓冲，说明上游在工具参数
    /// 写到一半时截断，返回 `IncompleteJson`（取字节数最多的那个作代表）。
    pub fn finish(&mut self) -> Result<(), ToolJsonAccumulatorError> {
        if let Some((tool_use_id, (name, input))) = self
            .buffers
            .iter()
            .max_by_key(|(_, (_, input))| input.len())
            .map(|(id, (name, input))| (id.clone(), (name.clone(), input.clone())))
        {
            self.buffers.remove(&tool_use_id);
            return Err(ToolJsonAccumulatorError::IncompleteJson {
                tool_use_id,
                name,
                bytes: input.len(),
            });
        }
        Ok(())
    }
}

/// SSE 事件
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

impl SseEvent {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    /// 格式化为 SSE 字符串
    pub fn to_sse_string(&self) -> String {
        format!(
            "event: {}\ndata: {}\n\n",
            self.event,
            serde_json::to_string(&self.data).unwrap_or_default()
        )
    }
}

/// 内容块状态
#[derive(Debug, Clone)]
struct BlockState {
    block_type: String,
    started: bool,
    stopped: bool,
}

impl BlockState {
    fn new(block_type: impl Into<String>) -> Self {
        Self {
            block_type: block_type.into(),
            started: false,
            stopped: false,
        }
    }
}

/// SSE 状态管理器
///
/// 确保 SSE 事件序列符合 Claude API 规范：
/// 1. message_start 只能出现一次
/// 2. content_block 必须先 start 再 delta 再 stop
/// 3. message_delta 只能出现一次，且在所有 content_block_stop 之后
/// 4. message_stop 在最后
#[derive(Debug)]
pub struct SseStateManager {
    /// message_start 是否已发送
    message_started: bool,
    /// message_delta 是否已发送
    message_delta_sent: bool,
    /// 活跃的内容块状态
    active_blocks: HashMap<i32, BlockState>,
    /// 消息是否已结束
    message_ended: bool,
    /// 下一个块索引
    next_block_index: i32,
    /// 当前 stop_reason
    stop_reason: Option<String>,
    /// 是否有工具调用
    has_tool_use: bool,
}

impl Default for SseStateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SseStateManager {
    pub fn new() -> Self {
        Self {
            message_started: false,
            message_delta_sent: false,
            active_blocks: HashMap::new(),
            message_ended: false,
            next_block_index: 0,
            stop_reason: None,
            has_tool_use: false,
        }
    }

    /// 判断指定块是否处于可接收 delta 的打开状态
    fn is_block_open_of_type(&self, index: i32, expected_type: &str) -> bool {
        self.active_blocks
            .get(&index)
            .is_some_and(|b| b.started && !b.stopped && b.block_type == expected_type)
    }

    /// 获取下一个块索引
    pub fn next_block_index(&mut self) -> i32 {
        let index = self.next_block_index;
        self.next_block_index += 1;
        index
    }

    /// 记录工具调用
    pub fn set_has_tool_use(&mut self, has: bool) {
        self.has_tool_use = has;
    }

    /// 设置 stop_reason
    pub fn set_stop_reason(&mut self, reason: impl Into<String>) {
        self.stop_reason = Some(reason.into());
    }

    /// 获取最终的 stop_reason
    pub fn get_stop_reason(&self) -> String {
        if let Some(ref reason) = self.stop_reason {
            reason.clone()
        } else if self.has_tool_use {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        }
    }

    /// 处理 message_start 事件
    pub fn handle_message_start(&mut self, event: serde_json::Value) -> Option<SseEvent> {
        if self.message_started {
            tracing::debug!("跳过重复的 message_start 事件");
            return None;
        }
        self.message_started = true;
        Some(SseEvent::new("message_start", event))
    }

    /// 处理 content_block_start 事件
    pub fn handle_content_block_start(
        &mut self,
        index: i32,
        block_type: &str,
        data: serde_json::Value,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果是 tool_use 块，先关闭之前的文本块
        if block_type == "tool_use" {
            self.has_tool_use = true;
            for (block_index, block) in self.active_blocks.iter_mut() {
                if block.block_type == "text" && block.started && !block.stopped {
                    // 自动发送 content_block_stop 关闭文本块
                    events.push(SseEvent::new(
                        "content_block_stop",
                        json!({
                            "type": "content_block_stop",
                            "index": block_index
                        }),
                    ));
                    block.stopped = true;
                }
            }
        }

        // 检查块是否已存在
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.started {
                tracing::debug!("块 {} 已启动，跳过重复的 content_block_start", index);
                return events;
            }
            block.started = true;
        } else {
            let mut block = BlockState::new(block_type);
            block.started = true;
            self.active_blocks.insert(index, block);
        }

        events.push(SseEvent::new("content_block_start", data));
        events
    }

    /// 处理 content_block_delta 事件
    pub fn handle_content_block_delta(
        &mut self,
        index: i32,
        data: serde_json::Value,
    ) -> Option<SseEvent> {
        // 确保块已启动
        if let Some(block) = self.active_blocks.get(&index) {
            if !block.started || block.stopped {
                tracing::warn!(
                    "块 {} 状态异常: started={}, stopped={}",
                    index,
                    block.started,
                    block.stopped
                );
                return None;
            }
        } else {
            // 块不存在，可能需要先创建
            tracing::warn!("收到未知块 {} 的 delta 事件", index);
            return None;
        }

        Some(SseEvent::new("content_block_delta", data))
    }

    /// 处理 content_block_stop 事件
    pub fn handle_content_block_stop(&mut self, index: i32) -> Option<SseEvent> {
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.stopped {
                tracing::debug!("块 {} 已停止，跳过重复的 content_block_stop", index);
                return None;
            }
            block.stopped = true;
            return Some(SseEvent::new(
                "content_block_stop",
                json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
        None
    }

    /// 生成最终事件序列
    pub fn generate_final_events(
        &mut self,
        input_tokens: i32,
        output_tokens: i32,
        metering: Option<&MeteringEvent>,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 关闭所有未关闭的块
        for (index, block) in self.active_blocks.iter_mut() {
            if block.started && !block.stopped {
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop",
                        "index": index
                    }),
                ));
                block.stopped = true;
            }
        }

        // 发送 message_delta
        if !self.message_delta_sent {
            self.message_delta_sent = true;
            let mut usage_json = json!({
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
            });
            // 透传上游 meteringEvent 的 credit_* 字段，让客户端拿到与 Kiro
            // 后端口径一致的计费元数据；只在收到过 meteringEvent 时才追加。
            if let Some(m) = metering {
                usage_json["credit_usage"] = json!(m.usage);
                usage_json["credit_unit"] = json!(m.unit);
                usage_json["credit_unit_plural"] = json!(m.unit_plural);
            }
            events.push(SseEvent::new(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": self.get_stop_reason(),
                        "stop_sequence": null
                    },
                    "usage": usage_json
                }),
            ));
        }

        // 发送 message_stop
        if !self.message_ended {
            self.message_ended = true;
            events.push(SseEvent::new(
                "message_stop",
                json!({ "type": "message_stop" }),
            ));
        }

        events
    }
}

use super::converter::get_context_window_size;

/// 流处理上下文
pub struct StreamContext {
    /// SSE 状态管理器
    pub state_manager: SseStateManager,
    /// 请求的模型名称
    pub model: String,
    /// 消息 ID
    pub message_id: String,
    /// 输入 tokens（估算值）
    pub input_tokens: i32,
    /// 从 contextUsageEvent 计算的实际输入 tokens
    pub context_input_tokens: Option<i32>,
    /// 输出 tokens 累计
    pub output_tokens: i32,
    /// 工具块索引映射 (tool_id -> block_index)
    pub tool_block_indices: HashMap<String, i32>,
    /// 工具名称反向映射（短名称 → 原始名称），用于响应时还原
    pub tool_name_map: HashMap<String, String>,
    /// thinking 是否启用（客户端请求里 `thinking.type` 为 enabled / adaptive）
    pub thinking_enabled: bool,
    /// thinking 块索引
    pub thinking_block_index: Option<i32>,
    /// 上游原生 reasoningContentEvent 下发的 thinking 签名
    pending_thinking_signature: Option<String>,
    /// 文本块索引（thinking 启用时动态分配）
    pub text_block_index: Option<i32>,
    /// meteringEvent 上报的 credit 计费量（上游真实下发，多次事件累加得到本次总量）
    pub credits: f64,
    /// 最近一次 meteringEvent 完整 payload（含 unit / unit_plural / usage）。
    /// 透传到 message_delta.usage 的 `credit_usage` / `credit_unit` / `credit_unit_plural`
    /// 字段，与 kiro-rs /v1/messages 行为对齐；上游只下发一次则取该次。
    pub metering: Option<MeteringEvent>,
    /// 复读熔断：最近一次作为文本吐出的「尾行」内容（去空白）。
    /// Opus 长上下文退化时会把同一个 stray token（call/count/card）一行一行无限复读，
    /// 我们在文本出口处统计「同一短行连续重复了多少次」。
    repeat_guard_last_line: String,
    /// 复读熔断：当前尾行已连续重复的次数。
    repeat_guard_run: u32,
    /// 复读熔断：是否已经触发过熔断（触发后本轮后续文本一律丢弃，不再吐、不写历史）。
    repeat_guard_tripped: bool,
    /// 工具调用参数 JSON 累积器：按 tool_use_id 缓冲分片，`stop` 时整体解析，
    /// 避免把“流式半截 JSON”当成完整工具调用转发。
    tool_json_accumulator: ToolJsonAccumulator,
    /// 工具调用 JSON 错误（非法 / 半截）。一旦置位，收尾时补发 `error` 事件，
    /// 上层据此把本次请求记为 error 而非 success。
    tool_json_error: Option<ToolJsonAccumulatorError>,
    /// 客户端未启用 thinking 时被丢弃的 reasoning 文本累计字节数。
    ///
    /// 只在收尾时汇总告警一条。曾经每个 reasoning 分片各打一条 warn —— 上游一轮能发
    /// 400 多个分片，日志瞬间被刷爆，真正的协议异常会被埋掉（§2.6）。
    dropped_reasoning_bytes: usize,
    /// 上游下发的错误 / 异常事件（`(类型或错误码, 消息)`）。与 `tool_json_error` 同口径：
    /// 收尾时补发 Anthropic `error` 事件，上层据此把本次请求记为 error。
    ///
    /// 只记第一条：上游一旦开始报错，后续往往是同一故障的连带事件，首条最接近根因。
    upstream_failure: Option<(String, String)>,
}

impl StreamContext {
    /// 最终上报的 input_tokens：优先用 contextUsage 换算出的真值
    /// （上游百分比 × 模型窗口），拿不到时回退客户端估算值。
    ///
    /// 上游不回传 cache 明细，故无 cache 字段可分摊（见 AGENTS.md §4.1）。
    pub fn resolved_usage(&self) -> i32 {
        self.context_input_tokens.unwrap_or(self.input_tokens)
    }

    /// 工具调用 JSON 错误信息（非法 / 半截）。上层据此把本次请求记为 error、
    /// 或在非流式路径返回 502。无错误时返回 `None`。
    pub fn tool_json_error_message(&self) -> Option<String> {
        self.tool_json_error.as_ref().map(|err| err.message())
    }

    /// 上游错误 / 异常事件的描述。上层据此把本次请求记为 error 而非 success。
    pub fn upstream_failure_message(&self) -> Option<String> {
        self.upstream_failure
            .as_ref()
            .map(|(kind, message)| format!("{kind}: {message}"))
    }

    /// 创建 StreamContext
    pub fn new_with_thinking(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        Self {
            state_manager: SseStateManager::new(),
            model: model.into(),
            message_id: format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
            input_tokens,
            context_input_tokens: None,
            output_tokens: 0,
            tool_block_indices: HashMap::new(),
            tool_name_map,
            thinking_enabled,
            thinking_block_index: None,
            pending_thinking_signature: None,
            text_block_index: None,
            credits: 0.0,
            metering: None,
            repeat_guard_last_line: String::new(),
            repeat_guard_run: 0,
            repeat_guard_tripped: false,
            tool_json_accumulator: ToolJsonAccumulator::new(),
            tool_json_error: None,
            dropped_reasoning_bytes: 0,
            upstream_failure: None,
        }
    }

    /// 记下上游报的第一条错误 / 异常，并把 stop_reason 置为 `error`。
    fn record_upstream_failure(&mut self, kind: String, message: String) {
        if self.upstream_failure.is_none() {
            self.upstream_failure = Some((kind, message));
            self.state_manager.set_stop_reason("error");
        }
    }

    /// 生成 message_start 事件
    pub fn create_message_start_event(&self) -> serde_json::Value {
        json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": self.input_tokens,
                    "output_tokens": 1,
                }
            }
        })
    }

    /// 生成初始事件序列 (message_start + 文本块 start)
    ///
    /// 当 thinking 启用时，不在初始化时创建文本块，而是等到实际收到内容时再创建。
    /// 这样可以确保 thinking 块（索引 0）在文本块（索引 1）之前。
    pub fn generate_initial_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // message_start
        let msg_start = self.create_message_start_event();
        if let Some(event) = self.state_manager.handle_message_start(msg_start) {
            events.push(event);
        }

        // thinking 启用时不预先开文本块：上游可能先发 reasoning，thinking 块得排在
        // 文本块之前。文本块由 `create_text_delta_events` 在第一片正文到达时懒创建。
        if self.thinking_enabled {
            return events;
        }

        // 创建初始文本块（仅在未启用 thinking 时）
        let text_block_index = self.state_manager.next_block_index();
        self.text_block_index = Some(text_block_index);
        let text_block_events = self.state_manager.handle_content_block_start(
            text_block_index,
            "text",
            json!({
                "type": "content_block_start",
                "index": text_block_index,
                "content_block": {
                    "type": "text",
                    "text": ""
                }
            }),
        );
        events.extend(text_block_events);

        events
    }

    /// 处理 Kiro 事件并转换为 Anthropic SSE 事件
    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<SseEvent> {
        match event {
            Event::AssistantResponse(resp) => self.process_assistant_response(&resp.content),
            Event::ToolUse(tool_use) => self.process_tool_use(tool_use),
            Event::ReasoningContent(reasoning) => self.process_reasoning_content(reasoning),
            Event::ContextUsage(context_usage) => {
                // 从上下文使用百分比计算实际的 input_tokens
                let window_size = get_context_window_size(&self.model);
                let actual_input_tokens =
                    (context_usage.context_usage_percentage * (window_size as f64) / 100.0) as i32;
                self.context_input_tokens = Some(actual_input_tokens);
                // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                if context_usage.context_usage_percentage >= 100.0 {
                    self.state_manager
                        .set_stop_reason("model_context_window_exceeded");
                }
                tracing::debug!(
                    "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                    context_usage.context_usage_percentage,
                    actual_input_tokens
                );
                Vec::new()
            }
            Event::Metering(metering) => {
                // 上游 meteringEvent 只下发 credit；token / cache 字段不存在。
                self.credits += metering.usage;
                tracing::debug!(
                    usage = metering.usage,
                    unit = %metering.unit,
                    unit_plural = %metering.unit_plural,
                    "metering credits +{:.6}", metering.usage
                );
                // 保留最近一次完整 payload，用于在 message_delta 里透传 credit_*
                // 字段；如果上游真的多次下发，则以最后一次为准（与 kiro-rs 一致）。
                self.metering = Some(metering.clone());
                Vec::new()
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                // 上游明确报错，必须让客户端知道。
                //
                // 这里曾经只写一行日志就 `Vec::new()`：流照旧以
                // `message_delta{stop_reason:end_turn}` + `message_stop` 正常收尾，客户端
                // 拿到一条「正常结束」的半截回复，把上游的失败当成了模型的完整答案。
                // 现在记下来，由 `generate_final_events` 补发 Anthropic `error` 事件，
                // 上层据此把本次请求记为 error 而非 success。
                tracing::error!("收到错误事件: {} - {}", error_code, error_message);
                self.record_upstream_failure(error_code.clone(), error_message.clone());
                Vec::new()
            }
            Event::Exception {
                exception_type,
                message,
            } => {
                tracing::warn!("收到异常事件: {} - {}", exception_type, message);
                // `ContentLengthExceededException` 是上游对「输出被长度截断」的正常告知，
                // 用 stop_reason 表达即可，不是失败。其余异常一律当失败上报，不再只留日志。
                if exception_type == "ContentLengthExceededException" {
                    self.state_manager.set_stop_reason("max_tokens");
                } else {
                    self.record_upstream_failure(exception_type.clone(), message.clone());
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// 处理助手响应事件：上游说是正文，就原样当正文发出去。
    ///
    /// **不读文本内容做语义判断。** 曾经有两条这样的路径，都会误伤正文：
    ///
    /// 1. 扫 `<thinking>` 字面量当思考块开始 —— 实测 `gpt-5.6-sol` 把
    ///    「答案是 `<thinking>`…」整段作为最终答案下发（签名里 `phase=final_answer`），
    ///    代理照样劈成 text + thinking 两块，闭合标签又被分片切断，导致回复
    ///    尾巴整段变成一条折叠的「思考」。上游从不使用这个约定，它是本代理家族
    ///    早年自创的；非流式路径早已删除，流式漏了。
    /// 2. 删掉字面 `<tool_use ...>` 及其之后的内容 —— 助手正常提到这个标签名时
    ///    半条回复会消失，客户端毫无提示。现在只告警，不删。
    ///
    /// thinking 块只由上游原生 `reasoningContentEvent` 产生，见
    /// [`Self::process_reasoning_content`]。
    fn process_assistant_response(&mut self, content: &str) -> Vec<SseEvent> {
        if content.is_empty() {
            return Vec::new();
        }
        if crate::kiro::model::events::contains_tool_use_xml_leak(content) {
            // 只记录不删除：见函数文档第 2 条。长度而非内容，避免把对话正文写进日志。
            tracing::warn!(
                chunk_len = content.len(),
                "上游正文里出现字面 <tool_use 标签，已原样透传（不再删除）"
            );
        }

        let mut events = Vec::new();
        // 上游从 reasoning 切到正文：先把还开着的 thinking 块收尾。
        if self.is_thinking_block_open() {
            events.extend(self.close_open_thinking_block());
        }

        self.output_tokens += estimate_tokens(content);
        // 统一的明文出口：tool_use 自动关闭文本块后能自愈重建，避免“吞字”。
        events.extend(self.create_text_delta_events(content));
        events
    }

    /// 创建 text_delta 事件：统一的明文出口，正文原样吐出。
    ///
    /// 这里曾经先把文本累进一个缓冲区、嗅探字面 `<invoke>` 工具调用块，命中就把那段正文
    /// **改写成结构化 tool_use** 交给客户端执行。该机制已整条删除：它是「读正文内容猜
    /// 语义」的最后一处，误判的后果最重（客户端会真的去改文件、跑命令），而且为了等
    /// `</invoke>` 闭合还要把正文暂存住，本身就构成一条正文延迟与丢失的通路。
    ///
    /// 上游若把工具调用吐成了字面文字，那就照字面文字交给客户端 —— 上游发什么就给什么。
    fn create_text_delta_events(&mut self, text: &str) -> Vec<SseEvent> {
        if text.is_empty() {
            return Vec::new();
        }
        self.emit_text_delta_raw(text)
    }

    /// 创建 text_delta 事件（原始逻辑，无嗅探）
    ///
    /// 如果文本块尚未创建，会先创建文本块。
    /// 当发生 tool_use 时，状态机会自动关闭当前文本块；后续文本会自动创建新的文本块继续输出。
    ///
    /// 返回值包含可能的 content_block_start 事件和 content_block_delta 事件。
    /// 复读检测：只统计并告警，**不丢弃任何正文**。
    ///
    /// 上游模型长上下文退化时会把同一个引导词（`STRAY_INVOKE_TOKENS`：call/count/card）
    /// 一行一行刷上万次。这里曾经据此「跳闸」：一旦同一个词连续重复超过
    /// [`REPEAT_GUARD_TRIP_THRESHOLD`] 次，就丢掉这一行及**本轮后续所有正文**，且跳闸
    /// 是粘性的。那等于代理替上游决定用户能看到多少内容 —— 客户端拿到一条被截断的
    /// 回复，却不知道后面还有内容被扣下了。
    ///
    /// 现在改为：命中即 `warn` 一次（每轮只报一次，避免刷日志），正文一律原样吐出。
    /// 代价是退化时用户会看到完整的复读洪水，输出额度也照实消耗 —— 那正是上游真实
    /// 发出的内容。
    fn repeat_guard_filter(&mut self, text: &str) {
        for segment in text.split_inclusive('\n') {
            let line = segment.trim();
            if STRAY_INVOKE_TOKENS.contains(&line) {
                if line == self.repeat_guard_last_line {
                    self.repeat_guard_run += 1;
                } else {
                    self.repeat_guard_last_line = line.to_string();
                    self.repeat_guard_run = 1;
                }
                if self.repeat_guard_run >= REPEAT_GUARD_TRIP_THRESHOLD
                    && !self.repeat_guard_tripped
                {
                    self.repeat_guard_tripped = true;
                    tracing::warn!(
                        token = %line,
                        run = self.repeat_guard_run,
                        "上游模型疑似退化复读（同一引导词连续重复）；正文原样透传，不做丢弃"
                    );
                }
            } else if !line.is_empty() {
                self.repeat_guard_last_line = line.to_string();
                self.repeat_guard_run = 0;
            }
        }
    }

    fn emit_text_delta_raw(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 复读只告警、不丢弃：上游发什么就给客户端什么。
        self.repeat_guard_filter(text);

        // 如果当前 text_block_index 指向的块已经被关闭（例如 tool_use 开始时自动 stop），
        // 则丢弃该索引并创建新的文本块继续输出，避免 delta 被状态机拒绝导致“吞字”。
        if let Some(idx) = self.text_block_index {
            if !self.state_manager.is_block_open_of_type(idx, "text") {
                self.text_block_index = None;
            }
        }

        // 获取或创建文本块索引
        let text_index = if let Some(idx) = self.text_block_index {
            idx
        } else {
            // 文本块尚未创建，需要先创建
            let idx = self.state_manager.next_block_index();
            self.text_block_index = Some(idx);

            // 发送 content_block_start 事件
            let start_events = self.state_manager.handle_content_block_start(
                idx,
                "text",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "text",
                        "text": ""
                    }
                }),
            );
            events.extend(start_events);
            idx
        };

        // 发送 content_block_delta 事件
        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            text_index,
            json!({
                "type": "content_block_delta",
                "index": text_index,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ) {
            events.push(delta_event);
        }

        events
    }

    fn is_thinking_block_open(&self) -> bool {
        self.thinking_block_index
            .is_some_and(|idx| self.state_manager.is_block_open_of_type(idx, "thinking"))
    }

    fn close_open_text_block(&mut self) -> Vec<SseEvent> {
        let Some(idx) = self.text_block_index else {
            return Vec::new();
        };
        if !self.state_manager.is_block_open_of_type(idx, "text") {
            self.text_block_index = None;
            return Vec::new();
        }
        self.text_block_index = None;
        self.state_manager
            .handle_content_block_stop(idx)
            .into_iter()
            .collect()
    }

    fn ensure_thinking_block(&mut self) -> Vec<SseEvent> {
        if self.is_thinking_block_open() {
            return Vec::new();
        }

        let mut events = self.close_open_text_block();

        let idx = self.state_manager.next_block_index();
        self.thinking_block_index = Some(idx);
        events.extend(self.state_manager.handle_content_block_start(
            idx,
            "thinking",
            json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "thinking",
                    "thinking": ""
                }
            }),
        ));
        events
    }

    fn close_open_thinking_block(&mut self) -> Vec<SseEvent> {
        let Some(idx) = self.thinking_block_index else {
            return Vec::new();
        };
        if !self.state_manager.is_block_open_of_type(idx, "thinking") {
            return Vec::new();
        }

        let mut events = vec![self.create_thinking_delta_event(idx, "")];
        // 只有上游真给了签名才发 signature_delta。
        //
        // 这里曾经在上游没给签名时填一个自造的占位串，好让客户端 SDK 不要因为
        // 「thinking 块缺 signature」而整块丢弃。那是给客户端发上游从未发过的字段，
        // 而且必须在下一轮入站时再把它识别出来剔除，否则回传即触发上游验签失败 ——
        // 一对互相打补丁的改写。现在不编：上游没给，就不发这个字段。
        if let Some(signature) = self.pending_thinking_signature.take() {
            events.push(self.create_signature_delta_event_with(idx, &signature));
        }
        if let Some(stop_event) = self.state_manager.handle_content_block_stop(idx) {
            events.push(stop_event);
        }
        events
    }

    fn process_reasoning_content(
        &mut self,
        reasoning: &crate::kiro::model::events::ReasoningContentEvent,
    ) -> Vec<SseEvent> {
        // 客户端没要思考（请求里没有 thinking 字段，或显式 disabled）时，上游仍会
        // 下发 `reasoningContentEvent`：2026-08-12 实测 claude-opus-5 在**不带**
        // thinking 字段时回一段 summarized 思考（反而带 `thinking.type=adaptive`
        // 且不带 display 时完全不回）。
        //
        // 这里曾经把这段思考写进正文 delta。那是伪造上游从未发过的块 —— 思考与正文
        // 是两个通道，客户端拿到的却是一个标着「回答」的块，它无从分辨。实测后果：
        // OpenCode 的压缩（compaction 请求不带 thinking）把模型的内心独白当成摘要
        // 存了下来（该轮正文为空，整篇摘要只剩独白，压缩后会话记忆即被污染）。
        //
        // 客户端没要，就不发。丢弃必须留痕（§2.3），但**不在这里**打日志：上游一轮能发
        // 400 多个 reasoning 分片，逐片一条 warn 会把日志刷爆，真正的协议异常反而被埋掉
        // （§2.6）。所以这里只累加字节数，由 `generate_final_events` 整轮汇总一条 warn，
        // 且只报长度不报内容（§6）。
        if !self.thinking_enabled {
            if let Some(text) = reasoning.text.as_deref() {
                self.dropped_reasoning_bytes += text.len();
            }
            return Vec::new();
        }

        let mut events = Vec::new();

        // 诊断：只报字段有无与长度，不打印思考内容或签名本身。
        tracing::debug!(
            "reasoningContentEvent: text={} signature={} redacted={}",
            reasoning.text.as_deref().map_or(0, str::len),
            reasoning.signature.as_deref().map_or(0, str::len),
            reasoning.redacted_content.as_deref().map_or(0, str::len),
        );

        if let Some(signature) = reasoning.signature.as_deref()
            && !signature.is_empty()
        {
            self.pending_thinking_signature = Some(signature.to_string());
        }

        if let Some(text) = reasoning.text.as_deref()
            && !text.is_empty()
        {
            self.output_tokens += estimate_tokens(text);
            // 上游给了思考内容就照原样开块、照原样发出，不看内容是什么。
            //
            // 这里曾经丢弃「只含换行/空格」的分片：上游会在正文之后补一个纯空白的
            // reasoning 分片，而此时前一个 thinking 块已被正文关闭，于是又开一个只有
            // 空白的新块，客户端把它渲染成回复末尾一条空的「思考」。当时按「空白等于
            // 没有思考」把它扔了 —— 那仍然是代理替上游决定用户能看到什么。
            events.extend(self.ensure_thinking_block());
            if let Some(idx) = self.thinking_block_index {
                events.push(self.create_thinking_delta_event(idx, text));
            }
        }

        if let Some(redacted) = reasoning.redacted_content.as_deref()
            && !redacted.is_empty()
        {
            self.output_tokens += 8;
            events.extend(self.create_redacted_thinking_events(redacted));
        }

        events
    }

    fn create_redacted_thinking_events(&mut self, data: &str) -> Vec<SseEvent> {
        let mut events = self.close_open_thinking_block();
        events.extend(self.close_open_text_block());

        let idx = self.state_manager.next_block_index();
        events.extend(self.state_manager.handle_content_block_start(
            idx,
            "redacted_thinking",
            json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "redacted_thinking",
                    "data": data
                }
            }),
        ));
        if let Some(stop_event) = self.state_manager.handle_content_block_stop(idx) {
            events.push(stop_event);
        }
        events
    }

    /// 创建 thinking_delta 事件
    fn create_thinking_delta_event(&self, index: i32, thinking: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": thinking
                }
            }),
        )
    }

    /// 创建 signature_delta 事件（仅在上游真给了签名时调用）
    ///
    /// 上游 Kiro 确实下发真实签名（实测 `reasoningContentEvent.signature` 为长度
    /// 308~10920 的 base64，流式时在思考文本之后单独下发）。签名由
    /// `pending_thinking_signature` 暂存，并原样回传到上游历史的
    /// `assistantResponseMessage.reasoningContent.reasoningText.signature` ——
    /// 上游解密它来重建思维链，所以**必须逐字节保真**。
    fn create_signature_delta_event_with(&self, index: i32, signature: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "signature_delta",
                    "signature": signature,
                }
            }),
        )
    }

    /// 统一的工具调用流式发出口：结构化 `toolUseEvent` 与 `<invoke>` 文本捞回都经此发出。
    ///
    /// 块索引按 `completed.id` 复用/分配（结构化按 tool_use_id 复用；invoke 合成用新 id 故新分配），
    /// 依次发 `content_block_start{name, input:{}}` → 单个完整 `input_json_delta` → `content_block_stop`。
    fn emit_completed_tool_use(&mut self, completed: CompletedToolUse) -> Vec<SseEvent> {
        let mut events = Vec::new();
        self.state_manager.set_has_tool_use(true);

        let block_index = if let Some(&idx) = self.tool_block_indices.get(&completed.id) {
            idx
        } else {
            let idx = self.state_manager.next_block_index();
            self.tool_block_indices.insert(completed.id.clone(), idx);
            idx
        };

        events.extend(self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": completed.id,
                    "name": completed.name,
                    "input": {}
                }
            }),
        ));

        // 一次性发出完整参数 JSON（来源已保证是合法 JSON）。
        self.output_tokens += estimate_tokens(&completed.input.to_string());
        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            block_index,
            json!({
                "type": "content_block_delta",
                "index": block_index,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": serde_json::to_string(&completed.input).unwrap_or_else(|_| "{}".to_string())
                }
            }),
        ) {
            events.push(delta_event);
        }

        if let Some(stop_event) = self.state_manager.handle_content_block_stop(block_index) {
            events.push(stop_event);
        }

        events
    }

    /// 处理工具使用事件
    fn process_tool_use(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        self.state_manager.set_has_tool_use(true);

        // tool_use 之前先把还开着的 thinking 块收尾。
        if self.is_thinking_block_open() {
            events.extend(self.close_open_thinking_block());
        }

        // 通过累积器缓冲工具参数 JSON 分片：只有收到 stop=true 且解析成功时才
        // 发出完整的工具调用；半截 / 非法 JSON 记为错误，交由收尾（generate_final_events）
        // 统一补发 error 事件，避免把无法解析的参数当成完整调用转发给客户端。
        let completed = match self
            .tool_json_accumulator
            .push(tool_use, &self.tool_name_map)
        {
            Ok(Some(completed)) => completed,
            Ok(None) => return events,
            Err(e) => {
                tracing::error!("{}", e);
                self.tool_json_error = Some(e);
                self.state_manager.set_stop_reason("error");
                return events;
            }
        };

        // 统一发出（与 <invoke> 文本捞回路径共用同一发出口）。
        events.extend(self.emit_completed_tool_use(completed));
        events
    }

    /// 生成最终事件序列
    pub fn generate_final_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        if self.is_thinking_block_open() {
            events.extend(self.close_open_thinking_block());
        }

        // 客户端没要思考却收到了 reasoning：整轮汇总一条，只报字节数不报内容（§6）。
        if self.dropped_reasoning_bytes > 0 {
            tracing::warn!(
                bytes = self.dropped_reasoning_bytes,
                "客户端未启用 thinking，本轮丢弃上游 reasoning 文本（不混入正文）"
            );
        }

        // 上游只发了 reasoning、没发正文时，**原样如实转达**：响应里就只有 thinking 块，
        // stop_reason 走默认的 end_turn。
        //
        // 这里曾经伪造过两样东西：把 stop_reason 改成 `max_tokens`，并补发一个内容为空格
        // 的 text 块。两者都是猜测 —— 上游从未说过本轮是因为耗尽输出预算才没有正文
        // （真被截断时它会下发 `ContentLengthExceededException`，那条路径另有处理）。
        // 伪造的后果是客户端收到一个假的「被截断」信号，可能据此做多余的续写或重试。
        //
        // 若整轮连 thinking 都没有，那也是上游本来的样子，同样不替它编造内容。

        // 收尾检查工具调用累积器：若仍有 tool_use 从未收到 stop=true（上游在参数
        // 写到一半时截断），记为错误。process_tool_use 中已置位的错误保持不变。
        if self.tool_json_error.is_none()
            && let Err(e) = self.tool_json_accumulator.finish()
        {
            tracing::error!("{}", e);
            self.tool_json_error = Some(e);
            self.state_manager.set_stop_reason("error");
        }

        let final_input_tokens = self.resolved_usage();

        // 生成最终事件（message_delta + message_stop）
        events.extend(self.state_manager.generate_final_events(
            final_input_tokens,
            self.output_tokens,
            self.metering.as_ref(),
        ));

        // 上游错误 / 异常事件：补一个 Anthropic `error` 事件（实时流已返回 200，无法再改
        // 状态码）。已经发出的正文照旧保留 —— 那是上游真发过的内容，不因后续报错而回收。
        if let Some((kind, message)) = &self.upstream_failure {
            events.push(SseEvent::new(
                "error",
                json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": format!("upstream {kind}: {message}")
                    }
                }),
            ));
        }

        // 工具调用 JSON 错误：在最终事件之后补一个 Anthropic `error` 事件，明确告知
        // 客户端本次工具调用因上游半截 / 非法 JSON 未被转发（实时流已返回 200，无法再改状态码）。
        if let Some(err) = &self.tool_json_error {
            events.push(SseEvent::new(
                "error",
                json!({
                    "type": "error",
                    "error": {
                        "type": err.error_type(),
                        "message": err.message()
                    }
                }),
            ));
        }

        events
    }
}

/// 简单的 token 估算（中英文字符混合）
///
/// 公开供 token 计数等模块复用同一估算口径。
pub fn estimate_tokens(text: &str) -> i32 {
    let chars: Vec<char> = text.chars().collect();
    let mut chinese_count = 0;
    let mut other_count = 0;

    for c in &chars {
        if *c >= '\u{4E00}' && *c <= '\u{9FFF}' {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }

    // 中文约 1.5 字符/token，英文约 4 字符/token
    let chinese_tokens = (chinese_count * 2 + 2) / 3;
    let other_tokens = (other_count + 3) / 4;

    (chinese_tokens + other_tokens).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ToolJsonAccumulator: 流式半截 / 非法工具调用 JSON ----

    fn tool_evt(
        id: &str,
        name: &str,
        input: &str,
        stop: bool,
    ) -> crate::kiro::model::events::ToolUseEvent {
        crate::kiro::model::events::ToolUseEvent {
            name: name.to_string(),
            tool_use_id: id.to_string(),
            input: input.to_string(),
            stop,
        }
    }

    #[test]
    fn tool_json_accumulator_reassembles_split_fragments() {
        let mut acc = ToolJsonAccumulator::new();
        let map = HashMap::new();
        // 用非内置工具名，专注验证分片重组本身（内置名的双向映射另有专项测试）。
        // JSON 被切成三片（切在 token 中间），只有最后一片带 stop。
        assert!(
            acc.push(&tool_evt("t1", "custom_tool", "{\"pa", false), &map)
                .unwrap()
                .is_none()
        );
        assert!(
            acc.push(&tool_evt("t1", "custom_tool", "th\":\"/a", false), &map)
                .unwrap()
                .is_none()
        );
        let completed = acc
            .push(&tool_evt("t1", "custom_tool", ".txt\"}", true), &map)
            .unwrap()
            .unwrap();
        assert_eq!(completed.id, "t1");
        assert_eq!(completed.name, "custom_tool");
        assert_eq!(completed.input, serde_json::json!({"path": "/a.txt"}));
    }

    #[test]
    fn tool_json_accumulator_empty_input_is_empty_object() {
        let mut acc = ToolJsonAccumulator::new();
        let completed = acc
            .push(&tool_evt("t1", "noop", "", true), &HashMap::new())
            .unwrap()
            .unwrap();
        assert_eq!(completed.input, serde_json::json!({}));
    }

    #[test]
    fn tool_json_accumulator_invalid_json_errors() {
        let mut acc = ToolJsonAccumulator::new();
        let err = acc
            .push(
                &tool_evt("t1", "read_file", "{not json", true),
                &HashMap::new(),
            )
            .unwrap_err();
        assert_eq!(err.error_type(), "upstream_tool_json_error");
        assert!(matches!(err, ToolJsonAccumulatorError::InvalidJson { .. }));
    }

    #[test]
    fn tool_json_accumulator_incomplete_on_missing_stop() {
        let mut acc = ToolJsonAccumulator::new();
        // 只来了半截、从未 stop → finish() 报 IncompleteJson。
        assert!(
            acc.push(
                &tool_evt("t1", "read_file", "{\"path\":\"/a", false),
                &HashMap::new()
            )
            .unwrap()
            .is_none()
        );
        let err = acc.finish().unwrap_err();
        assert!(matches!(
            err,
            ToolJsonAccumulatorError::IncompleteJson { .. }
        ));
        // 已取出残留后再 finish() 应成功。
        assert!(acc.finish().is_ok());
    }

    #[test]
    fn tool_json_accumulator_restores_short_tool_name() {
        let mut acc = ToolJsonAccumulator::new();
        let mut map = HashMap::new();
        map.insert(
            "short_abc123".to_string(),
            "the_original_very_long_tool_name".to_string(),
        );
        let completed = acc
            .push(&tool_evt("t1", "short_abc123", "{}", true), &map)
            .unwrap()
            .unwrap();
        assert_eq!(completed.name, "the_original_very_long_tool_name");
    }

    /// 防回归：统一管道的两个去向（流式 emit_completed_tool_use 与非流式 to_anthropic_block）
    /// 对同一 CompletedToolUse 产出一致的 id / name / input。
    #[test]
    fn emit_and_block_agree_on_shape() {
        let completed = CompletedToolUse {
            id: "toolu_1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "/a"}),
        };

        // 非流式块
        let block = completed.to_anthropic_block();
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "toolu_1");
        assert_eq!(block["name"], "read_file");
        assert_eq!(block["input"], serde_json::json!({"path": "/a"}));

        // 流式发出：start 的 id/name 与块一致；delta 的 partial_json 解析后与块 input 一致。
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let events = ctx.emit_completed_tool_use(completed);
        let start = events
            .iter()
            .find(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
            })
            .expect("应有 tool_use content_block_start");
        assert_eq!(start.data["content_block"]["id"], block["id"]);
        assert_eq!(start.data["content_block"]["name"], block["name"]);
        let delta = events
            .iter()
            .find(|e| e.event == "content_block_delta")
            .expect("应有 input_json_delta");
        let partial = delta.data["delta"]["partial_json"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(partial).unwrap();
        assert_eq!(
            parsed, block["input"],
            "流式增量拼出的 input 应与非流式块一致"
        );
        assert!(events.iter().any(|e| e.event == "content_block_stop"));
    }

    // ---- 上游发什么就传什么：正文里的标签不再被解释 ----

    /// 回归锁：正文里出现字面 `<tool_use ...>` 时**一个字都不能少**。
    ///
    /// 旧行为是把标签连同它之后的内容一起删掉（未闭合时直接丢到流末尾），
    /// 客户端拿到一条被截断的回复且没有任何提示。真实事故：助手在回复里正常
    /// 提到这个标签名（解释协议时），半条回复凭空消失。
    #[test]
    fn literal_tool_use_tag_in_text_is_passed_through_untouched() {
        let mut ctx = StreamContext::new_with_thinking("m", 1, false, HashMap::new());
        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response("hello <tool"));
        events.extend(ctx.process_assistant_response("_use name=\"W\">{\"a\":1}</tool_use> world"));
        events.extend(ctx.generate_final_events());
        assert_eq!(
            collect_text_content(&events),
            "hello <tool_use name=\"W\">{\"a\":1}</tool_use> world"
        );
    }

    /// 回归锁：正文里出现字面 `<thinking>` 时，既不能新开 thinking 块，
    /// 也不能把后面的正文吞进去。
    ///
    /// 实测 `gpt-5.6-sol` 会把「答案是 `<thinking>`…」整段作为最终答案下发
    /// （签名里 `phase=final_answer`）。旧行为把它劈成 text + thinking 两块，
    /// 而且闭合标签被分片切断后检测失败，回复尾巴整段变成一条折叠的「思考」。
    /// 这个标签是本代理家族早年自创的约定，上游协议里没有它。
    #[test]
    fn literal_thinking_tag_in_text_stays_text() {
        for thinking_enabled in [true, false] {
            let mut ctx = StreamContext::new_with_thinking(
                "gpt-5.6-sol",
                1,
                thinking_enabled,
                HashMap::new(),
            );
            let mut events = ctx.generate_initial_events();
            // 按上游真实分片切开，闭合标签横跨三片。
            for chunk in ["答案是 <thinking>内部推", "理</", "thi", "nking> 完毕"] {
                events.extend(ctx.process_assistant_response(chunk));
            }
            events.extend(ctx.generate_final_events());

            assert_eq!(
                collect_text_content(&events),
                "答案是 <thinking>内部推理</thinking> 完毕",
                "thinking_enabled={thinking_enabled}"
            );
            assert!(
                !events.iter().any(|e| {
                    e.event == "content_block_start"
                        && e.data["content_block"]["type"] == "thinking"
                }),
                "正文里的标签不能产出 thinking 块 (thinking_enabled={thinking_enabled})"
            );
        }
    }

    #[test]
    fn test_sse_event_format() {
        let event = SseEvent::new("message_start", json!({"type": "message_start"}));
        let sse_str = event.to_sse_string();

        assert!(sse_str.starts_with("event: message_start\n"));
        assert!(sse_str.contains("data: "));
        assert!(sse_str.ends_with("\n\n"));
    }

    #[test]
    fn test_sse_state_manager_message_start() {
        let mut manager = SseStateManager::new();

        // 第一次应该成功
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_some());

        // 第二次应该被跳过
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_none());
    }

    #[test]
    fn test_sse_state_manager_block_lifecycle() {
        let mut manager = SseStateManager::new();

        // 创建块
        let events = manager.handle_content_block_start(0, "text", json!({}));
        assert_eq!(events.len(), 1);

        // delta
        let event = manager.handle_content_block_delta(0, json!({}));
        assert!(event.is_some());

        // stop
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_some());

        // 重复 stop 应该被跳过
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_none());
    }

    #[test]
    fn test_tool_name_reverse_mapping_in_stream() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert(
            "short_abc12345".to_string(),
            "mcp__very_long_original_tool_name".to_string(),
        );

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, map);
        let _ = ctx.generate_initial_events();

        // 模拟 Kiro 返回短名称的 tool_use
        let tool_event = Event::ToolUse(ToolUseEvent {
            name: "short_abc12345".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"key":"value"}"#.to_string(),
            stop: true,
        });

        let events = ctx.process_kiro_event(&tool_event);

        // content_block_start 中的 name 应该是原始长名称
        let start_event = events
            .iter()
            .find(|e| e.event == "content_block_start")
            .unwrap();
        assert_eq!(
            start_event.data["content_block"]["name"], "mcp__very_long_original_tool_name",
            "应还原为原始工具名称"
        );
    }

    #[test]
    fn test_text_delta_after_tool_use_restarts_text_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());

        let initial_events = ctx.generate_initial_events();
        assert!(
            initial_events
                .iter()
                .any(|e| e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "text")
        );

        let initial_text_index = ctx
            .text_block_index
            .expect("initial text block index should exist");

        // tool_use 开始会自动关闭现有 text block
        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true, // 累积器仅在 stop=true 时整体发出工具调用（含关闭前一个块）
        });
        assert!(
            tool_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(initial_text_index as i64)
            }),
            "tool_use should stop the previous text block"
        );

        // 之后再来文本增量，应自动创建新的 text block 而不是往已 stop 的块里写 delta
        let text_events = ctx.process_assistant_response("hello");
        let new_text_start_index = text_events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        assert!(
            new_text_start_index.is_some(),
            "should start a new text block"
        );
        assert_ne!(
            new_text_start_index.unwrap(),
            initial_text_index as i64,
            "new text block index should differ from the stopped one"
        );
        assert!(
            text_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "hello"
            }),
            "should emit text_delta after restarting text block"
        );
    }

    /// 回归锁：tool_use 之前的短文本必须完整发出，不能被 tool_use 吞掉。
    ///
    /// 旧实现在 thinking 模式下会把短文本暂存起来等 `<thinking>` 的跨 chunk 匹配，
    /// 于是这段文本可能被后面的 tool_use 连带吞掉。现在正文不再被暂存 ——
    /// 每一片来了就发，所以这里改为直接断言两片都已发出。
    #[test]
    fn short_text_before_tool_use_is_not_swallowed() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut before_tool = ctx.process_assistant_response("有修");
        before_tool.extend(ctx.process_assistant_response("改："));
        assert_eq!(
            collect_text_content(&before_tool),
            "有修改：",
            "正文分片必须立即发出，不再暂存"
        );

        let mut all = before_tool;
        all.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "Write".to_string(),
                tool_use_id: "tool_1".to_string(),
                input: "{}".to_string(),
                stop: true, // 累积器仅在 stop=true 时整体发出工具调用（含关闭前一个块）
            }),
        );

        let text_index = all
            .iter()
            .find_map(|e| {
                if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                    e.data["index"].as_i64()
                } else {
                    None
                }
            })
            .expect("应开出一个 text 块");
        let pos_text_delta = all
            .iter()
            .position(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
            })
            .expect("应发出 text_delta");
        let pos_text_stop = all
            .iter()
            .position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(text_index)
            })
            .expect("text 块应在 tool_use 之前关闭");
        let pos_tool_start = all
            .iter()
            .position(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
            })
            .expect("应开出 tool_use 块");

        assert!(
            pos_text_delta < pos_text_stop && pos_text_stop < pos_tool_start,
            "顺序必须是 text_delta -> text_stop -> tool_use_start"
        );
    }

    #[test]
    fn test_estimate_tokens() {
        assert!(estimate_tokens("Hello") > 0);
        assert!(estimate_tokens("你好") > 0);
        assert!(estimate_tokens("Hello 你好") > 0);
    }

    #[test]
    fn test_thinking_block_emits_signature_delta_before_stop() {
        // 客户端在 thinking 模式下要求 thinking 块带 signature 字段，否则下一轮回传时
        // 会抛出 "must be passed back to the API"。本测试验证 thinking 块结束前发送了
        // 一个非空的 signature_delta 事件。
        //
        // thinking 块只由上游原生 reasoningContentEvent 产生 —— 正文里的
        // `<thinking>` 字面量已不再被解释，见 literal_thinking_tag_in_text_stays_text。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("abc".to_string()),
                signature: Some("real-upstream-signature".to_string()),
                redacted_content: None,
            },
        )));
        all.extend(ctx.process_assistant_response("hello"));
        all.extend(ctx.generate_final_events());

        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");

        let pos_sig = all.iter().position(|e| {
            e.event == "content_block_delta"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"] == "real-upstream-signature"
        });
        let pos_stop = all.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });

        assert!(pos_sig.is_some(), "signature_delta should be emitted");
        assert!(pos_stop.is_some(), "content_block_stop should be emitted");
        assert!(
            pos_sig.unwrap() < pos_stop.unwrap(),
            "signature_delta must precede content_block_stop"
        );
    }

    /// 回归锁：上游报错时，必须让客户端知道，不许当成正常结束。
    ///
    /// 这里曾经只写一行日志就丢掉 `Event::Error`：流照旧以
    /// `message_delta{stop_reason:end_turn}` + `message_stop` 收尾，客户端拿到一条
    /// 「正常结束」的半截回复，把上游的失败当成了模型的完整答案。
    #[test]
    fn upstream_error_event_is_surfaced_to_client() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let mut all = ctx.generate_initial_events();

        all.extend(ctx.process_assistant_response("半截答案"));
        all.extend(ctx.process_kiro_event(&Event::Error {
            error_code: "InternalServerException".to_string(),
            error_message: "upstream blew up".to_string(),
        }));
        all.extend(ctx.generate_final_events());

        let err = all
            .iter()
            .find(|e| e.event == "error")
            .expect("必须补发一个 error 事件");
        let msg = err.data["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains("InternalServerException") && msg.contains("upstream blew up"),
            "错误事件要带上上游的原始错误码与消息: {msg}"
        );

        let delta = all
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("应有 message_delta");
        assert_eq!(
            delta.data["delta"]["stop_reason"], "error",
            "不许再报成 end_turn"
        );

        assert_eq!(
            collect_text_content(&all),
            "半截答案",
            "报错前已发出的正文照旧保留，不因后续失败而回收"
        );
        assert!(
            ctx.upstream_failure_message().is_some(),
            "上层要能据此把本次请求记为 error"
        );
    }

    /// 回归锁：`ContentLengthExceededException` 是「输出被长度截断」的正常告知，
    /// 不是失败；其余异常一律当失败上报。
    #[test]
    fn content_length_exception_is_stop_reason_not_failure() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_assistant_response("很长的答案"));
        all.extend(ctx.process_kiro_event(&Event::Exception {
            exception_type: "ContentLengthExceededException".to_string(),
            message: "too long".to_string(),
        }));
        all.extend(ctx.generate_final_events());

        assert!(
            ctx.upstream_failure_message().is_none(),
            "长度截断不该被记为失败"
        );
        assert!(
            !all.iter().any(|e| e.event == "error"),
            "长度截断不该补发 error 事件"
        );
        let delta = all
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("应有 message_delta");
        assert_eq!(delta.data["delta"]["stop_reason"], "max_tokens");
    }

    /// 回归锁：流式路径下，正文里的字面 `<invoke>` 原样透传，不许改写成工具调用。
    ///
    /// 曾经有一整套嗅探器：把文本累进缓冲区、等 `</invoke>` 闭合、命中就把那段正文改写成
    /// 结构化 tool_use 交给客户端执行，并配有行首、代码围栏、工具表三道防线。整条机制已
    /// 删除 —— 它是「读正文内容猜语义」的最后一处，误判后果最重（客户端会真的改文件、跑
    /// 命令），而为了等闭合标签还必须把正文暂存住，本身就是一条正文延迟与丢失的通路。
    #[test]
    fn literal_invoke_in_text_is_passed_through_untouched() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let mut all = ctx.generate_initial_events();

        let leaked = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">rm -rf /</parameter>\n</invoke>";
        all.extend(ctx.process_assistant_response(leaked));
        all.extend(ctx.generate_final_events());

        assert!(
            !all.iter()
                .any(|e| e.data["content_block"]["type"] == "tool_use"),
            "字面 <invoke> 不许被改写成工具调用"
        );
        assert_eq!(
            collect_text_content(&all),
            leaked,
            "正文必须逐字保真，连引导词 call 也不许剥掉"
        );
    }

    /// 回归锁：上游没给签名，就不许自造一个发给客户端。
    ///
    /// 这里曾经在缺签名时填一个占位串 `kiro-rs-thinking-signature`，好让客户端 SDK 不要
    /// 因为「thinking 块缺 signature」而整块丢弃。那是发上游从未发过的字段，而且必须在
    /// 下一轮入站时再把它识别出来剔除，否则回传即触发上游验签失败 —— 一对互相打补丁的
    /// 改写。代价是这种情况下客户端可能不显示这段思考，那是客户端的选择，不是我们编造
    /// 数据的理由。
    #[test]
    fn missing_upstream_signature_is_not_fabricated() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let mut all = ctx.generate_initial_events();

        all.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("abc".to_string()),
                signature: None,
                redacted_content: None,
            },
        )));
        all.extend(ctx.process_assistant_response("hello"));
        all.extend(ctx.generate_final_events());

        assert!(
            !all.iter()
                .any(|e| e.data["delta"]["type"] == "signature_delta"),
            "上游没给签名就不该出现 signature_delta"
        );
        assert_eq!(
            collect_thinking_content(&all),
            "abc",
            "思考文本照常原样转达"
        );
        assert_eq!(collect_text_content(&all), "hello", "正文照常原样转达");
    }

    /// 辅助函数：从事件列表中提取所有 thinking_delta 的拼接内容
    fn collect_thinking_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 辅助函数：从事件列表中提取所有 text_delta 的拼接内容
    fn collect_text_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect()
    }

    fn block_start_position(events: &[SseEvent], block_type: &str) -> (usize, i64) {
        let pos = events
            .iter()
            .position(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == block_type
            })
            .unwrap_or_else(|| panic!("{block_type} block should start"));
        let idx = events[pos].data["index"]
            .as_i64()
            .unwrap_or_else(|| panic!("{block_type} block index should exist"));
        (pos, idx)
    }

    fn block_stop_position(events: &[SseEvent], index: i64) -> usize {
        events
            .iter()
            .position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(index)
            })
            .unwrap_or_else(|| panic!("block {index} should stop"))
    }

    /// 回归锁：上游只发 reasoning、没发正文时，不许伪造「被截断」也不许补空正文。
    ///
    /// 这里曾经把 stop_reason 改成 `max_tokens` 并补发一个内容为空格的 text 块。两者都是
    /// 猜测：上游从未说过本轮是因为耗尽输出预算才没有正文（真截断会下发
    /// `ContentLengthExceededException`，另有处理）。伪造的后果是客户端收到假的「被截断」
    /// 信号，可能据此做多余的续写或重试。
    #[test]
    fn thinking_only_turn_is_reported_verbatim() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("abc".to_string()),
                signature: None,
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");
        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "end_turn",
            "上游没说被截断，就不许伪造 max_tokens"
        );

        assert!(
            !all_events.iter().any(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "text"
            }),
            "上游没发正文，就不许凭空补一个 text 块"
        );
        assert_eq!(
            collect_thinking_content(&all_events),
            "abc",
            "上游给的思考内容照常原样转达"
        );
    }

    #[test]
    fn test_thinking_with_text_keeps_end_turn_stop_reason() {
        // thinking + text 的情况，stop_reason 应为 end_turn
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("abc".to_string()),
                signature: None,
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.process_assistant_response("Hello"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "end_turn",
            "stop_reason should be end_turn when text is also produced"
        );
    }

    #[test]
    fn test_thinking_with_tool_use_keeps_tool_use_stop_reason() {
        // thinking + tool_use 的情况，stop_reason 应为 tool_use
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "test_tool".to_string(),
                tool_use_id: "tool_1".to_string(),
                input: "{}".to_string(),
                stop: true,
            }),
        );
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "tool_use",
            "stop_reason should be tool_use when tool_use is present"
        );
    }

    // ---- 复读检测：只告警，不丢弃（上游发什么就给客户端什么）----

    /// 回归锁：模型退化复读时，正文一律原样透传，**不许丢弃**。
    ///
    /// 真实形态取自 thread 019ea4e9：模型一句正常话后无限复读 `count`。这里曾经
    /// 「跳闸」丢掉超阈值之后的全部正文，且跳闸是粘性的 —— 等于代理替上游决定用户能
    /// 看到多少内容，客户端拿到一条被截断的回复却毫不知情。现在只 `warn` 一次。
    #[test]
    fn stray_token_flood_is_passed_through_untouched() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        let mut payload = String::from("先看 crawlee 状态。\n\ncall\n\n");
        for _ in 0..5000 {
            payload.push_str("count\n\n");
        }
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(&payload));
        all.extend(ctx.generate_final_events());

        let text = collect_text_content(&all);
        assert_eq!(
            text.matches("count").count(),
            5000,
            "上游刷了 5000 次，就得原样吐 5000 次，一个都不许丢"
        );
        assert!(
            text.contains("先看 crawlee 状态"),
            "正常正文照常保留: {:?}",
            &text[..text.len().min(80)]
        );
    }

    /// 🟢 不误伤：正常多行文本里偶尔出现 count 单词（非独占行复读）不熔断。
    #[test]
    fn repeat_guard_does_not_trip_on_normal_prose() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let payload =
            "我数了一下 count = 3，然后继续做别的事。\n这是第二行正常文字。\n第三行也正常。";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let text = collect_text_content(&all);
        assert!(
            text.contains("我数了一下"),
            "正常正文不应被熔断: {:?}",
            text
        );
        assert!(
            text.contains("第三行也正常"),
            "正常正文应完整保留: {:?}",
            text
        );
    }

    /// 回归锁：跨分片到达的复读同样原样透传，不许丢弃。
    #[test]
    fn stray_token_flood_across_chunks_is_passed_through() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("call\n\n"));
        for _ in 0..2000 {
            all.extend(ctx.process_assistant_response("count\n\n"));
        }
        all.extend(ctx.generate_final_events());
        assert_eq!(
            collect_text_content(&all).matches("count").count(),
            2000,
            "跨分片的复读也得一个不丢地吐出去"
        );
    }

    #[test]
    fn test_native_reasoning_event_emits_thinking_with_signature() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("native reasoning".to_string()),
                signature: Some("real-signature".to_string()),
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.process_assistant_response("final answer"));
        all_events.extend(ctx.generate_final_events());

        assert_eq!(collect_thinking_content(&all_events), "native reasoning");
        assert_eq!(collect_text_content(&all_events), "final answer");
        assert!(all_events.iter().any(|e| {
            e.event == "content_block_delta"
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"] == "real-signature"
        }));
    }

    #[test]
    fn test_native_reasoning_signature_only_applies_to_next_thinking_text() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: None,
                signature: Some("signature-before-text".to_string()),
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("delayed native reasoning".to_string()),
                signature: None,
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.generate_final_events());

        assert_eq!(
            collect_thinking_content(&all_events),
            "delayed native reasoning"
        );
        assert!(all_events.iter().any(|e| {
            e.event == "content_block_delta"
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"] == "signature-before-text"
        }));
    }

    #[test]
    /// 回归锁：客户端没启用 thinking 时，上游的 reasoning 文本一律丢弃，**不许**
    /// 混进正文。旧行为把它写成 text delta，客户端收到一个标着「回答」的块却装着
    /// 模型的内心独白，OpenCode 的压缩摘要因此被污染（2026-08-12 实测）。
    fn test_native_reasoning_text_is_dropped_when_thinking_disabled() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("visible reasoning fallback".to_string()),
                signature: Some("ignored-signature".to_string()),
                redacted_content: Some("ignored-redacted".to_string()),
            },
        )));
        all_events.extend(ctx.generate_final_events());

        assert_eq!(
            collect_text_content(&all_events),
            "",
            "reasoning 文本不许混入正文"
        );
        assert_eq!(collect_thinking_content(&all_events), "");
        assert!(!all_events.iter().any(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "signature_delta"
        }));
        assert!(!all_events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "redacted_thinking"
        }));
    }

    #[test]
    fn test_native_redacted_thinking_is_ordered_between_thinking_and_text() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("native reasoning".to_string()),
                signature: Some("real-signature".to_string()),
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: None,
                signature: None,
                redacted_content: Some("encrypted-thinking".to_string()),
            },
        )));
        all_events.extend(ctx.process_assistant_response("final answer"));
        all_events.extend(ctx.generate_final_events());

        let (_, thinking_idx) = block_start_position(&all_events, "thinking");
        let thinking_stop_pos = block_stop_position(&all_events, thinking_idx);
        let (redacted_start_pos, redacted_idx) =
            block_start_position(&all_events, "redacted_thinking");
        let redacted_stop_pos = block_stop_position(&all_events, redacted_idx);
        let (text_start_pos, _) = block_start_position(&all_events, "text");

        assert!(
            thinking_stop_pos < redacted_start_pos,
            "thinking block must close before redacted_thinking starts"
        );
        assert!(
            redacted_stop_pos < text_start_pos,
            "redacted_thinking block must close before text starts"
        );
        assert_eq!(collect_thinking_content(&all_events), "native reasoning");
        assert_eq!(collect_text_content(&all_events), "final answer");
    }

    /// 回归锁：上游发的每一片思考都照原样开块、照原样发出，包括纯空白的那种。
    ///
    /// 上游会在正文之后补一个只含 `\n\n` 的 reasoning 分片。此时第一个 thinking 块已被
    /// 正文关闭，所以这一片会开出第二个块，客户端把它渲染成回复末尾一条内容为空的
    /// 「思考」。这里曾经按「空白等于没有思考」把它丢掉 —— 那仍然是代理替上游决定用户
    /// 能看到什么。判据用 thinking 块的数量：块里只有空白，比对文本看不出区别。
    #[test]
    fn whitespace_only_reasoning_still_opens_its_own_thinking_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("real thinking".to_string()),
                signature: None,
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.process_assistant_response("visible answer"));
        // 上游在正文之后补的空白分片：照原样开块发出。
        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("\n\n".to_string()),
                signature: Some("real-signature".to_string()),
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.generate_final_events());

        let thinking_starts = all_events
            .iter()
            .filter(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "thinking"
            })
            .count();
        assert_eq!(
            thinking_starts, 2,
            "上游发了两片思考，就该有两个 thinking 块，空白那片也不例外"
        );
        assert_eq!(collect_thinking_content(&all_events), "real thinking\n\n");
    }

    #[test]
    fn test_native_reasoning_event_emits_redacted_thinking() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: None,
                signature: None,
                redacted_content: Some("encrypted-thinking".to_string()),
            },
        )));
        all_events.extend(ctx.generate_final_events());

        assert!(all_events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "redacted_thinking"
                && e.data["content_block"]["data"] == "encrypted-thinking"
        }));
    }

    // ---- credit_usage 透传 ----

    fn parse_metering(payload: &str) -> MeteringEvent {
        serde_json::from_str(payload).unwrap()
    }

    #[test]
    fn test_generate_final_events_omits_credit_fields_without_metering() {
        // 没有 meteringEvent 时不应在 usage 里写 credit_* 字段。
        let mut manager = SseStateManager::new();
        let events = manager.generate_final_events(10, 5, None);
        let delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert!(usage.get("credit_usage").is_none());
        assert!(usage.get("credit_unit").is_none());
        assert!(usage.get("credit_unit_plural").is_none());
    }

    #[test]
    fn test_generate_final_events_carries_credit_fields_when_metering_present() {
        let mut manager = SseStateManager::new();
        let metering = parse_metering(r#"{"unit":"credit","unitPlural":"credits","usage":0.75}"#);
        let events = manager.generate_final_events(10, 5, Some(&metering));
        let delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert_eq!(usage["credit_usage"], json!(0.75));
        assert_eq!(usage["credit_unit"], json!("credit"));
        assert_eq!(usage["credit_unit_plural"], json!("credits"));
        // 既有字段保持原样
        assert_eq!(usage["input_tokens"], json!(10));
        assert_eq!(usage["output_tokens"], json!(5));
    }

    #[test]
    fn test_stream_context_keeps_latest_metering_in_message_delta() {
        use crate::kiro::model::events::MeteringEvent;

        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-7", 100, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        // 第一次下发（异常：上游几乎只会下发一次）
        ctx.process_kiro_event(&Event::Metering(MeteringEvent {
            unit: "credit".into(),
            unit_plural: "credits".into(),
            usage: 0.10,
        }));
        // 第二次下发（应覆盖）
        ctx.process_kiro_event(&Event::Metering(MeteringEvent {
            unit: "credit".into(),
            unit_plural: "credits".into(),
            usage: 0.42,
        }));

        let final_events = ctx.generate_final_events();
        let delta = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert_eq!(usage["credit_usage"], json!(0.42));
        assert_eq!(usage["credit_unit"], json!("credit"));
        assert_eq!(usage["credit_unit_plural"], json!("credits"));
        // 累计 credit 仍然是两次之和
        assert!((ctx.credits - 0.52).abs() < 1e-9);
    }

    #[test]
    fn test_stream_context_omits_credit_fields_without_metering() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-7", 100, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let final_events = ctx.generate_final_events();
        let delta = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert!(usage.get("credit_usage").is_none());
        assert!(usage.get("credit_unit").is_none());
        assert!(usage.get("credit_unit_plural").is_none());
    }
}
