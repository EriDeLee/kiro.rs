//! 模型白名单与端点隔离
//!
//! 本部署只服务下列模型，且**协议与模型组严格绑定**，不做任何跨协议兼容：
//!
//! | 端点                        | 允许的模型                                     | 推理字段路径           |
//! |-----------------------------|------------------------------------------------|------------------------|
//! | `/v1/messages`（Anthropic） | `claude-opus-5`、`claude-sonnet-5`             | `output_config.effort` |
//! | `/v1/responses`（OpenAI）   | `gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna` | `reasoning.effort`     |
//!
//! 依据实测 `ListAvailableModels` 的 `additionalModelRequestFieldsSchema`
//! （2026-07-26 实测 `claude-opus-5` / `gpt-5.6-sol`，2026-07-29 复测补齐另外三个）：
//!
//! - **Claude 族**（`claude-opus-5`、`claude-sonnet-5`）：`thinking.type ∈ {adaptive,
//!   disabled}`、`thinking.display ∈ {summarized, omitted}`、`output_config.effort ∈
//!   {low,medium,high,xhigh,max}`（default `high`，**无 `none`**）。窗口 1M in；
//!   输出上限 `claude-opus-5` 128k、`claude-sonnet-5` 64k。
//! - **GPT 族**（`gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna`）：**只有** `reasoning`，
//!   `additionalProperties:false` —— `reasoning.effort ∈ {none,low,medium,high,xhigh,max}`
//!   （default `high`）、`reasoning.mode ∈ {standard, pro}`。下发 `output_config` /
//!   `max_tokens` / `thinking` 都会被上游以 `400 REQUEST_BODY_INVALID` 拒绝。
//!   窗口 272k in / 128k out。
//!
//! 同一族内推理字段路径与请求体形状完全一致，扩充模型只是多几个 id；跨族则根本不同。
//! 因此不允许用 OpenAI 协议请求 Claude 模型，也不允许用 Anthropic 协议请求 GPT 模型：
//! 跨协议“兼容”只会产出上游拒绝的请求，或更糟——静默丢弃客户端的推理强度设置。
//!
//! 白名单是穷举的，不做家族/版本号的模糊推断：上游账号里还有 `claude-opus-4.8`、
//! `claude-sonnet-4.6`、`claude-haiku-4.5`、`glm-5`、`auto` 等模型，本部署一律 400。
//! 模糊匹配会把 `claude-sonnet-4.6` 误判成 `claude-sonnet-5`，或把未受支持的 id
//! 透传给上游换回一个语义不明的 400。

/// Anthropic `/v1/messages` 端点允许的模型（上游 Kiro modelId）。
pub const MODEL_OPUS_5: &str = "claude-opus-5";
/// Anthropic `/v1/messages` 端点允许的模型（上游 Kiro modelId）。
pub const MODEL_SONNET_5: &str = "claude-sonnet-5";
/// OpenAI `/v1/responses` 端点允许的模型（上游 Kiro modelId）。
pub const MODEL_GPT_56_SOL: &str = "gpt-5.6-sol";
/// OpenAI `/v1/responses` 端点允许的模型（上游 Kiro modelId）。
pub const MODEL_GPT_56_TERRA: &str = "gpt-5.6-terra";
/// OpenAI `/v1/responses` 端点允许的模型（上游 Kiro modelId）。
pub const MODEL_GPT_56_LUNA: &str = "gpt-5.6-luna";

/// Anthropic 协议的模型组：推理字段走 `output_config.effort` + `thinking.{type,display}`。
const ANTHROPIC_MODELS: &[&str] = &[MODEL_OPUS_5, MODEL_SONNET_5];
/// OpenAI Responses 协议的模型组：推理字段只走 `reasoning.effort`。
const OPENAI_RESPONSES_MODELS: &[&str] = &[MODEL_GPT_56_SOL, MODEL_GPT_56_TERRA, MODEL_GPT_56_LUNA];

/// 请求所用的客户端协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// `/v1/messages`、`/v1/messages/count_tokens`
    Anthropic,
    /// `/v1/responses`
    OpenAiResponses,
}

impl Protocol {
    /// 该协议允许的上游模型组。
    pub const fn allowed_models(self) -> &'static [&'static str] {
        match self {
            Self::Anthropic => ANTHROPIC_MODELS,
            Self::OpenAiResponses => OPENAI_RESPONSES_MODELS,
        }
    }
}

/// 已归一化的上游 modelId 属于哪个协议族；不在白名单内返回 `None`。
///
/// 入参应是 [`normalize`] 的输出（或白名单常量本身）——它不剥离别名后缀。
///
/// 推理字段路径、上下文窗口、effort 档位枚举都按**族**分流，调用方一律查这里，
/// 不要再逐个模型 `eq_ignore_ascii_case`：那种写法在加模型时必然漏改，而漏改的
/// 后果是静默走错字段路径（上游 400，或推理档位被整条丢弃）。
pub fn protocol_for_model(model_id: &str) -> Option<Protocol> {
    if ANTHROPIC_MODELS
        .iter()
        .any(|m| m.eq_ignore_ascii_case(model_id))
    {
        return Some(Protocol::Anthropic);
    }
    if OPENAI_RESPONSES_MODELS
        .iter()
        .any(|m| m.eq_ignore_ascii_case(model_id))
    {
        return Some(Protocol::OpenAiResponses);
    }
    None
}

/// 归一化客户端传入的模型名到上游 modelId。
///
/// 只接受白名单模型本身及其常见别名后缀（`-thinking`、`-latest`、8 位日期戳），
/// 其余一律返回 `None`。不做家族/版本号的模糊推断——模糊匹配会把
/// `claude-opus-4-5`、`claude-sonnet-4.6` 之类误判成 5 代，或把未受支持的模型
/// 透传给上游。
pub fn normalize(model: &str) -> Option<&'static str> {
    let m = strip_aliases(model);
    match m.as_str() {
        "claude-opus-5" => Some(MODEL_OPUS_5),
        "claude-sonnet-5" => Some(MODEL_SONNET_5),
        "gpt-5.6-sol" | "gpt-5-6-sol" => Some(MODEL_GPT_56_SOL),
        "gpt-5.6-terra" | "gpt-5-6-terra" => Some(MODEL_GPT_56_TERRA),
        "gpt-5.6-luna" | "gpt-5-6-luna" => Some(MODEL_GPT_56_LUNA),
        _ => None,
    }
}

/// 在指定协议下解析模型，同时校验模型与协议是否匹配。
pub fn resolve(model: &str, protocol: Protocol) -> Result<&'static str, RejectedModel> {
    match normalize(model) {
        // 归一化后的模型必须落在该协议的允许组内。
        Some(resolved) if protocol.allowed_models().contains(&resolved) => Ok(resolved),
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
                 [{}] on /v1/messages and [{}] on /v1/responses.",
                ANTHROPIC_MODELS.join(", "),
                OPENAI_RESPONSES_MODELS.join(", ")
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
    fn normalize_accepts_the_allowlisted_models_and_their_aliases() {
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
        for m in [
            "claude-sonnet-5",
            "Claude-Sonnet-5",
            "claude-sonnet-5-thinking",
            "claude-sonnet-5-latest",
            "claude-sonnet-5-20260101",
            "claude-sonnet-5-20260101-thinking",
        ] {
            assert_eq!(normalize(m), Some(MODEL_SONNET_5), "{m}");
        }
        for m in ["gpt-5.6-sol", "gpt-5-6-sol", "GPT-5.6-Sol", "gpt-5.6-sol-latest"] {
            assert_eq!(normalize(m), Some(MODEL_GPT_56_SOL), "{m}");
        }
        for m in [
            "gpt-5.6-terra",
            "gpt-5-6-terra",
            "GPT-5.6-Terra",
            "gpt-5.6-terra-latest",
        ] {
            assert_eq!(normalize(m), Some(MODEL_GPT_56_TERRA), "{m}");
        }
        for m in [
            "gpt-5.6-luna",
            "gpt-5-6-luna",
            "GPT-5.6-Luna",
            "gpt-5.6-luna-thinking",
        ] {
            assert_eq!(normalize(m), Some(MODEL_GPT_56_LUNA), "{m}");
        }
    }

    /// 回归锁：白名单是穷举的，绝不做家族/版本号模糊推断。
    ///
    /// 上游账号里确实有这些模型，但本部署不服务它们。相邻版本号（`claude-sonnet-4.6`
    /// vs `claude-sonnet-5`）是模糊匹配最容易出错的地方。
    #[test]
    fn normalize_rejects_everything_else() {
        for m in [
            // 相邻版本：模糊匹配最容易在这里出错
            "claude-opus-4-5",
            "claude-opus-4.5",
            "claude-opus-4-5-20251101",
            "claude-opus-4.8",
            "claude-opus-4-8-thinking",
            "claude-sonnet-4.6",
            "claude-sonnet-4-6",
            "claude-sonnet-4-6-20260101",
            "claude-sonnet-4.5",
            "claude-fable-5",
            "claude-haiku-4.5",
            "claude-haiku-5",
            // 同族但不在白名单内的 id
            "gpt-5.6",
            "gpt-5.6-pro",
            "gpt-5.7-sol",
            "gpt-5.6-terran",
            "gpt-5.6-lunar",
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
    fn protocol_for_model_groups_by_family() {
        for m in [MODEL_OPUS_5, MODEL_SONNET_5] {
            assert_eq!(protocol_for_model(m), Some(Protocol::Anthropic), "{m}");
        }
        for m in [MODEL_GPT_56_SOL, MODEL_GPT_56_TERRA, MODEL_GPT_56_LUNA] {
            assert_eq!(protocol_for_model(m), Some(Protocol::OpenAiResponses), "{m}");
        }
        // 白名单外的 id 没有协议族 —— 调用方必须把它当不受支持处理，
        // 不能落进任一族的字段路径。
        for m in ["claude-sonnet-4.6", "gpt-5.6", "glm-5", ""] {
            assert_eq!(protocol_for_model(m), None, "{m}");
        }
    }

    #[test]
    fn allowed_models_are_disjoint_per_protocol() {
        for m in Protocol::Anthropic.allowed_models() {
            assert!(
                !Protocol::OpenAiResponses.allowed_models().contains(m),
                "{m} 不能同时属于两个协议组"
            );
        }
        assert_eq!(Protocol::Anthropic.allowed_models().len(), 2);
        assert_eq!(Protocol::OpenAiResponses.allowed_models().len(), 3);
    }

    #[test]
    fn resolve_enforces_protocol_binding() {
        // Anthropic 组全体在 /v1/messages 上通过
        for (requested, expected) in [
            ("claude-opus-5", MODEL_OPUS_5),
            ("claude-sonnet-5", MODEL_SONNET_5),
            ("claude-sonnet-5-thinking", MODEL_SONNET_5),
        ] {
            assert_eq!(resolve(requested, Protocol::Anthropic), Ok(expected));
        }
        // GPT 组全体在 /v1/responses 上通过
        for (requested, expected) in [
            ("gpt-5.6-sol", MODEL_GPT_56_SOL),
            ("gpt-5.6-terra", MODEL_GPT_56_TERRA),
            ("gpt-5-6-terra", MODEL_GPT_56_TERRA),
            ("gpt-5.6-luna", MODEL_GPT_56_LUNA),
        ] {
            assert_eq!(resolve(requested, Protocol::OpenAiResponses), Ok(expected));
        }
        // 交叉请求必须被拒
        assert_eq!(
            resolve("gpt-5.6-sol", Protocol::Anthropic),
            Err(RejectedModel::WrongProtocol {
                model: MODEL_GPT_56_SOL,
                protocol: Protocol::Anthropic
            })
        );
        assert_eq!(
            resolve("claude-sonnet-5", Protocol::OpenAiResponses),
            Err(RejectedModel::WrongProtocol {
                model: MODEL_SONNET_5,
                protocol: Protocol::OpenAiResponses
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
        // 白名单外的模型即使走对了「看起来该去的」端点也必须是 Unsupported，
        // 不能因为名字像 Claude/GPT 就被归进某一族。
        assert_eq!(
            resolve("claude-sonnet-4.6", Protocol::Anthropic),
            Err(RejectedModel::Unsupported)
        );
        assert_eq!(
            resolve("gpt-5.6", Protocol::OpenAiResponses),
            Err(RejectedModel::Unsupported)
        );
    }

    /// 回归锁：交叉协议**必须**是错误，不能因为「模型在白名单里」就放行。
    ///
    /// 曾经的实现为了让 `/v1/responses` 内部复用 `post_messages` 而整类放行
    /// `WrongProtocol`，导致外部直接用 `/v1/messages` 请求 gpt-5.6-sol 也能 200。
    /// 内部转发现在靠 `InternalForward` Extension 区分，此处必须保持严格。
    ///
    /// 白名单从 2 个模型扩到 5 个后，隔离语义不变：每侧是**一组**模型，跨组仍 400。
    #[test]
    fn cross_protocol_is_always_an_error() {
        for m in [
            "gpt-5.6-sol",
            "gpt-5.6-sol-thinking",
            "gpt-5.6-terra",
            "gpt-5-6-terra",
            "gpt-5.6-luna",
            "gpt-5.6-luna-latest",
        ] {
            assert!(
                resolve(m, Protocol::Anthropic).is_err(),
                "{m} 不能从 /v1/messages 请求"
            );
        }
        for m in [
            "claude-opus-5",
            "claude-opus-5-20260101",
            "claude-sonnet-5",
            "claude-sonnet-5-thinking",
            "claude-sonnet-5-20260101",
        ] {
            assert!(
                resolve(m, Protocol::OpenAiResponses).is_err(),
                "{m} 不能从 /v1/responses 请求"
            );
        }
    }

    #[test]
    fn wrong_protocol_message_names_the_right_endpoint() {
        let msg = resolve("gpt-5.6-sol", Protocol::Anthropic)
            .unwrap_err()
            .message("gpt-5.6-sol");
        assert!(msg.contains("/v1/responses"), "{msg}");

        let msg = resolve("claude-sonnet-5", Protocol::OpenAiResponses)
            .unwrap_err()
            .message("claude-sonnet-5");
        assert!(msg.contains("/v1/messages"), "{msg}");
    }

    /// 不受支持的模型的错误消息必须列全两侧的模型组，否则客户端只能靠猜。
    #[test]
    fn unsupported_message_lists_every_allowed_model() {
        let msg = resolve("glm-5", Protocol::Anthropic)
            .unwrap_err()
            .message("glm-5");
        for m in ANTHROPIC_MODELS.iter().chain(OPENAI_RESPONSES_MODELS) {
            assert!(msg.contains(m), "错误消息缺少 {m}: {msg}");
        }
    }
}
