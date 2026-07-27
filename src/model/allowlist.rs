//! 模型白名单与端点隔离
//!
//! 本部署只服务两个上游模型，且**协议与模型严格一对一绑定**，不做任何跨协议兼容：
//!
//! | 端点                          | 唯一允许的模型   | 推理字段路径             |
//! |-------------------------------|------------------|--------------------------|
//! | `/v1/messages`（Anthropic）   | `claude-opus-5`  | `output_config.effort`   |
//! | `/v1/responses`（OpenAI）     | `gpt-5.6-sol`    | `reasoning.effort`       |
//!
//! 依据 2026-07-26 实测 `ListAvailableModels` 的 `additionalModelRequestFieldsSchema`：
//!
//! - `claude-opus-5`：`thinking.type ∈ {adaptive, disabled}`、`thinking.display ∈
//!   {summarized, omitted}`、`output_config.effort ∈ {low,medium,high,xhigh,max}`
//!   （default `high`）、`max_tokens ∈ [1024, 128000]`；1M in / 128k out。
//! - `gpt-5.6-sol`：**只有** `reasoning`，`additionalProperties:false` ——
//!   `reasoning.effort ∈ {none,low,medium,high,xhigh,max}`（default `high`）、
//!   `reasoning.mode ∈ {standard, pro}`。下发 `output_config` / `max_tokens` /
//!   `thinking` 都会被上游以 `400 REQUEST_BODY_INVALID` 拒绝。
//!
//! 不允许用 OpenAI 协议请求 Claude 模型，也不允许用 Anthropic 协议请求 GPT 模型：
//! 两个模型的推理字段路径与请求体形状根本不同，跨协议“兼容”只会产出上游拒绝的
//! 请求，或更糟——静默丢弃客户端的推理强度设置。

/// Anthropic `/v1/messages` 端点唯一允许的模型（上游 Kiro modelId）。
pub const MODEL_OPUS_5: &str = "claude-opus-5";
/// OpenAI `/v1/responses` 端点唯一允许的模型（上游 Kiro modelId）。
pub const MODEL_GPT_56_SOL: &str = "gpt-5.6-sol";

/// 请求所用的客户端协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// `/v1/messages`、`/cc/v1/messages`、`/v1/messages/count_tokens`
    Anthropic,
    /// `/v1/responses`
    OpenAiResponses,
}

impl Protocol {
    /// 该协议唯一允许的上游模型 id。
    pub const fn allowed_model(self) -> &'static str {
        match self {
            Self::Anthropic => MODEL_OPUS_5,
            Self::OpenAiResponses => MODEL_GPT_56_SOL,
        }
    }

    pub const fn endpoint(self) -> &'static str {
        match self {
            Self::Anthropic => "/v1/messages",
            Self::OpenAiResponses => "/v1/responses",
        }
    }
}

/// 归一化客户端传入的模型名到上游 modelId。
///
/// 只接受目标模型本身及其常见别名后缀（`-thinking`、`-latest`、8 位日期戳），
/// 其余一律返回 `None`。不做家族/版本号的模糊推断——模糊匹配会把
/// `claude-opus-4-5` 之类误判成 5 代，或把未受支持的模型透传给上游。
pub fn normalize(model: &str) -> Option<&'static str> {
    let m = strip_aliases(model);
    match m.as_str() {
        "claude-opus-5" => Some(MODEL_OPUS_5),
        "gpt-5.6-sol" | "gpt-5-6-sol" => Some(MODEL_GPT_56_SOL),
        _ => None,
    }
}

/// 在指定协议下解析模型，同时校验模型与协议是否匹配。
pub fn resolve(model: &str, protocol: Protocol) -> Result<&'static str, RejectedModel> {
    match normalize(model) {
        Some(resolved) if resolved == protocol.allowed_model() => Ok(resolved),
        // 模型本身受支持，但走错了协议端点。
        Some(resolved) => Err(RejectedModel::WrongProtocol {
            model: resolved,
            protocol,
        }),
        None => Err(RejectedModel::Unsupported),
    }
}

/// 剥离客户端常加的别名后缀，得到裸模型名（小写）。
fn strip_aliases(model: &str) -> String {
    let mut m = model.trim().to_ascii_lowercase();
    loop {
        let before = m.len();
        for suffix in ["-thinking", "-latest"] {
            if let Some(stripped) = m.strip_suffix(suffix) {
                m = stripped.to_string();
            }
        }
        // 尾部 8 位日期戳，如 `-20260101`
        if let Some((base, tail)) = m.rsplit_once('-')
            && tail.len() == 8
            && tail.chars().all(|c| c.is_ascii_digit())
        {
            m = base.to_string();
        }
        if m.len() == before {
            break;
        }
    }
    m
}

/// 模型被拒的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectedModel {
    /// 不在白名单内。
    Unsupported,
    /// 模型受支持，但不该从这个协议端点请求。
    WrongProtocol {
        model: &'static str,
        protocol: Protocol,
    },
}

impl RejectedModel {
    /// 面向客户端的错误消息。
    pub fn message(&self, requested: &str) -> String {
        match self {
            Self::Unsupported => format!(
                "model `{requested}` is not supported. This deployment serves only \
                 `{MODEL_OPUS_5}` on /v1/messages and `{MODEL_GPT_56_SOL}` on /v1/responses."
            ),
            Self::WrongProtocol { model, protocol } => {
                let (right, wrong) = match protocol {
                    Protocol::Anthropic => ("/v1/responses", "/v1/messages"),
                    Protocol::OpenAiResponses => ("/v1/messages", "/v1/responses"),
                };
                format!(
                    "model `{model}` must be requested through `{right}`, not `{wrong}`. \
                     Cross-protocol requests are intentionally unsupported."
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_the_two_models_and_their_aliases() {
        for m in [
            "claude-opus-5",
            "Claude-Opus-5",
            "claude-opus-5-thinking",
            "claude-opus-5-latest",
            "claude-opus-5-20260101",
            "claude-opus-5-20260101-thinking",
        ] {
            assert_eq!(normalize(m), Some(MODEL_OPUS_5), "{m}");
        }
        for m in ["gpt-5.6-sol", "gpt-5-6-sol", "GPT-5.6-Sol"] {
            assert_eq!(normalize(m), Some(MODEL_GPT_56_SOL), "{m}");
        }
    }

    #[test]
    fn normalize_rejects_everything_else() {
        for m in [
            // 相邻版本：模糊匹配最容易在这里出错
            "claude-opus-4-5",
            "claude-opus-4.5",
            "claude-opus-4-5-20251101",
            "claude-opus-4.8",
            "claude-opus-4-8-thinking",
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-haiku-4.5",
            // 同族但非 sol
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.6",
            // 其他供应商与保留名
            "deepseek-3.2",
            "glm-5",
            "minimax-m2.5",
            "qwen3-coder-next",
            "auto",
            "",
        ] {
            assert_eq!(normalize(m), None, "{m} 必须被拒绝");
        }
    }

    #[test]
    fn resolve_enforces_protocol_binding() {
        assert_eq!(
            resolve("claude-opus-5", Protocol::Anthropic),
            Ok(MODEL_OPUS_5)
        );
        assert_eq!(
            resolve("gpt-5.6-sol", Protocol::OpenAiResponses),
            Ok(MODEL_GPT_56_SOL)
        );
        // 交叉请求必须被拒
        assert_eq!(
            resolve("gpt-5.6-sol", Protocol::Anthropic),
            Err(RejectedModel::WrongProtocol {
                model: MODEL_GPT_56_SOL,
                protocol: Protocol::Anthropic
            })
        );
        assert_eq!(
            resolve("claude-opus-5", Protocol::OpenAiResponses),
            Err(RejectedModel::WrongProtocol {
                model: MODEL_OPUS_5,
                protocol: Protocol::OpenAiResponses
            })
        );
        assert_eq!(
            resolve("claude-opus-4.8", Protocol::Anthropic),
            Err(RejectedModel::Unsupported)
        );
    }

    /// 回归锁：交叉协议**必须**是错误，不能因为「模型在白名单里」就放行。
    ///
    /// 曾经的实现为了让 `/v1/responses` 内部复用 `post_messages` 而整类放行
    /// `WrongProtocol`，导致外部直接用 `/v1/messages` 请求 gpt-5.6-sol 也能 200。
    /// 内部转发现在靠 `InternalForward` Extension 区分，此处必须保持严格。
    #[test]
    fn cross_protocol_is_always_an_error() {
        assert!(resolve("gpt-5.6-sol", Protocol::Anthropic).is_err());
        assert!(resolve("gpt-5.6-sol-thinking", Protocol::Anthropic).is_err());
        assert!(resolve("claude-opus-5", Protocol::OpenAiResponses).is_err());
        assert!(resolve("claude-opus-5-20260101", Protocol::OpenAiResponses).is_err());
    }

    #[test]
    fn wrong_protocol_message_names_the_right_endpoint() {
        let msg = resolve("gpt-5.6-sol", Protocol::Anthropic)
            .unwrap_err()
            .message("gpt-5.6-sol");
        assert!(msg.contains("/v1/responses"), "{msg}");
    }
}
