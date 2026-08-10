//! 模型白名单、模型族与 API 入口策略
//!
//! 本部署只服务 5 个模型。客户端入口与上游模型族是两个独立维度：
//!
//! | 入口 | 允许的模型 |
//! |---|---|
//! | `/v1/messages` | 全部 5 个白名单模型 |
//! | `/v1/responses` | GPT 族 3 个模型 |
//!
//! 上游推理字段仍严格按模型族分流：Claude 族使用
//! `output_config.effort` + `thinking.{type,display}`，GPT 族只使用
//! `reasoning.effort`。入口不得参与上游 schema 判断。

/// Claude 族上游 Kiro modelId。
pub const MODEL_OPUS_5: &str = "claude-opus-5";
/// Claude 族上游 Kiro modelId。
pub const MODEL_SONNET_5: &str = "claude-sonnet-5";
/// GPT 族上游 Kiro modelId。
pub const MODEL_GPT_56_SOL: &str = "gpt-5.6-sol";
/// GPT 族上游 Kiro modelId。
pub const MODEL_GPT_56_TERRA: &str = "gpt-5.6-terra";
/// GPT 族上游 Kiro modelId。
pub const MODEL_GPT_56_LUNA: &str = "gpt-5.6-luna";

const CLAUDE_MODELS: &[&str] = &[MODEL_OPUS_5, MODEL_SONNET_5];
const GPT_MODELS: &[&str] = &[MODEL_GPT_56_SOL, MODEL_GPT_56_TERRA, MODEL_GPT_56_LUNA];
const ALL_MODELS: &[&str] = &[
    MODEL_OPUS_5,
    MODEL_SONNET_5,
    MODEL_GPT_56_SOL,
    MODEL_GPT_56_TERRA,
    MODEL_GPT_56_LUNA,
];

/// 上游 Kiro 模型族。推理字段路径、上下文窗口和 effort 枚举都按此分流。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    Claude,
    Gpt,
}

/// 客户端实际请求的 API 入口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiEndpoint {
    Messages,
    Responses,
}

impl ApiEndpoint {
    /// 用于日志、Trace 数据库和 Admin API 的稳定值。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::Responses => "responses",
        }
    }

    pub const fn path(self) -> &'static str {
        match self {
            Self::Messages => "/v1/messages",
            Self::Responses => "/v1/responses",
        }
    }

    pub const fn allowed_models(self) -> &'static [&'static str] {
        match self {
            Self::Messages => ALL_MODELS,
            Self::Responses => GPT_MODELS,
        }
    }
}

/// 已归一化的白名单 modelId 属于哪个上游模型族。
pub fn family_for_model(model_id: &str) -> Option<ModelFamily> {
    if CLAUDE_MODELS
        .iter()
        .any(|m| m.eq_ignore_ascii_case(model_id))
    {
        return Some(ModelFamily::Claude);
    }
    if GPT_MODELS.iter().any(|m| m.eq_ignore_ascii_case(model_id)) {
        return Some(ModelFamily::Gpt);
    }
    None
}

/// 归一化客户端传入的模型名到上游 modelId。
///
/// 只接受白名单模型本身及常见别名后缀（`-thinking`、`-latest`、8 位日期戳）。
/// 不做家族或版本号的模糊推断。
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

/// 在指定 API 入口下解析模型，同时执行入口策略校验。
pub fn resolve(model: &str, endpoint: ApiEndpoint) -> Result<&'static str, RejectedModel> {
    match normalize(model) {
        Some(resolved) if endpoint.allowed_models().contains(&resolved) => Ok(resolved),
        Some(resolved) => Err(RejectedModel::WrongEndpoint { model: resolved }),
        None => Err(RejectedModel::Unsupported),
    }
}

fn strip_aliases(model: &str) -> String {
    let mut m = model.trim().to_ascii_lowercase();
    loop {
        let before = m.len();
        for suffix in ["-thinking", "-latest"] {
            if let Some(stripped) = m.strip_suffix(suffix) {
                m = stripped.to_string();
            }
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectedModel {
    Unsupported,
    /// 模型在白名单里，但这个入口不收它。
    ///
    /// 不带 endpoint 字段：只有 `/v1/responses` 会产生它。`/v1/messages` 的允许集
    /// 就是 `ALL_MODELS`（见 [`ApiEndpoint::allowed_models`]），任何能被
    /// [`normalize`] 认出的模型都在里面，那一侧永远走不到这个分支。
    /// `messages_accepts_every_allowlisted_model` 锁住这个前提 —— 若哪天
    /// Messages 也开始拒模型，下面那句「use `/v1/messages`」就会变成谎话。
    WrongEndpoint {
        model: &'static str,
    },
}

impl RejectedModel {
    pub fn message(&self, requested: &str) -> String {
        match self {
            Self::Unsupported => format!(
                "model `{requested}` is not supported. This deployment serves only [{}].",
                ALL_MODELS.join(", ")
            ),
            Self::WrongEndpoint { model } => format!(
                "model `{model}` is not supported on `{}`; use `{}`. \
                 `{}` accepts only [{}].",
                ApiEndpoint::Responses.path(),
                ApiEndpoint::Messages.path(),
                ApiEndpoint::Responses.path(),
                GPT_MODELS.join(", ")
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_allowlisted_models_and_aliases() {
        for (requested, expected) in [
            ("claude-opus-5", MODEL_OPUS_5),
            ("Claude-Opus-5-thinking", MODEL_OPUS_5),
            ("claude-opus-5-20260101-latest", MODEL_OPUS_5),
            ("claude-sonnet-5", MODEL_SONNET_5),
            ("claude-sonnet-5-20260101-thinking", MODEL_SONNET_5),
            ("gpt-5.6-sol", MODEL_GPT_56_SOL),
            ("gpt-5-6-sol-latest", MODEL_GPT_56_SOL),
            ("GPT-5.6-Terra", MODEL_GPT_56_TERRA),
            ("gpt-5-6-terra", MODEL_GPT_56_TERRA),
            ("gpt-5.6-luna-thinking", MODEL_GPT_56_LUNA),
        ] {
            assert_eq!(normalize(requested), Some(expected), "{requested}");
        }
    }

    /// 回归锁：白名单是穷举的，绝不做家族/版本号模糊推断。
    ///
    /// 这些 id 上游账号里确实有，但本部署不服务它们。最危险的是「差一点点」的名字：
    /// 相邻版本号（`claude-sonnet-4.6` vs `claude-sonnet-5`）、多一个字母
    /// （`gpt-5.6-terran` vs `gpt-5.6-terra`、`gpt-5.6-lunar` vs `gpt-5.6-luna`）。
    /// 一旦有人把匹配改松（例如「名字里带 claude-sonnet 就算 claude-sonnet-5」），
    /// 客户端会以为自己在用 5 代，实际不是 —— 这条测试就是拦它的。
    #[test]
    fn normalize_rejects_everything_else() {
        for model in [
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
            assert_eq!(normalize(model), None, "{model} 必须被拒绝");
        }
    }

    #[test]
    fn family_for_model_uses_upstream_schema_family() {
        for model in CLAUDE_MODELS {
            assert_eq!(family_for_model(model), Some(ModelFamily::Claude));
        }
        for model in GPT_MODELS {
            assert_eq!(family_for_model(model), Some(ModelFamily::Gpt));
        }
        assert_eq!(family_for_model("gpt-5.6"), None);
    }

    /// 回归锁：模型名单只有 `CLAUDE_MODELS` + `GPT_MODELS` 两份真源，
    /// `ALL_MODELS` 与 [`normalize`] 必须与它们逐个对上。
    ///
    /// 加一个模型要同时改三处：所属族的常量、`ALL_MODELS`、`normalize` 的 match。
    /// 漏改任何一处，其余测试都不会变红 —— `messages_accepts_every_allowlisted_model`
    /// 遍历的就是 `ALL_MODELS` 本身，拿漏改后的名单去验漏改后的行为，永远绿。
    ///
    /// 漏改 `ALL_MODELS` 的实际后果：该模型从 `/v1/responses` 请求正常，从
    /// `/v1/messages` 请求返回 400，且错误消息自相矛盾（叫调用方去用刚拒了它的入口）。
    #[test]
    fn model_lists_stay_in_sync() {
        assert_eq!(
            ALL_MODELS.len(),
            CLAUDE_MODELS.len() + GPT_MODELS.len(),
            "ALL_MODELS 必须恰好是两个族的并集"
        );
        for model in CLAUDE_MODELS.iter().chain(GPT_MODELS) {
            assert!(ALL_MODELS.contains(model), "{model} 漏进 ALL_MODELS");
        }
        for model in ALL_MODELS {
            assert!(family_for_model(model).is_some(), "{model} 查不到模型族");
            assert_eq!(normalize(model), Some(*model), "{model} 未登记进 normalize");
        }
    }

    #[test]
    fn messages_accepts_every_allowlisted_model() {
        for model in ALL_MODELS {
            assert_eq!(resolve(model, ApiEndpoint::Messages), Ok(*model), "{model}");
        }
        assert_eq!(
            resolve("gpt-5-6-terra-latest", ApiEndpoint::Messages),
            Ok(MODEL_GPT_56_TERRA)
        );
        // `RejectedModel::WrongEndpoint` 不带 endpoint 字段、消息直接写
        // 「use /v1/messages」，前提就是 Messages 收全部白名单模型。
        assert_eq!(ApiEndpoint::Messages.allowed_models(), ALL_MODELS);
    }

    #[test]
    fn responses_remains_gpt_only() {
        for model in GPT_MODELS {
            assert_eq!(
                resolve(model, ApiEndpoint::Responses),
                Ok(*model),
                "{model}"
            );
        }
        for model in CLAUDE_MODELS {
            assert_eq!(
                resolve(model, ApiEndpoint::Responses),
                Err(RejectedModel::WrongEndpoint { model })
            );
        }
    }

    #[test]
    fn unsupported_models_fail_on_both_endpoints() {
        for endpoint in [ApiEndpoint::Messages, ApiEndpoint::Responses] {
            assert_eq!(
                resolve("claude-sonnet-4.6", endpoint),
                Err(RejectedModel::Unsupported)
            );
            assert_eq!(
                resolve("gpt-5.6", endpoint),
                Err(RejectedModel::Unsupported)
            );
        }
    }

    #[test]
    fn errors_describe_the_current_endpoint_policy() {
        let wrong = resolve("claude-opus-5", ApiEndpoint::Responses)
            .unwrap_err()
            .message("claude-opus-5");
        assert!(wrong.contains("/v1/responses"), "{wrong}");
        assert!(wrong.contains("/v1/messages"), "{wrong}");

        let unsupported = resolve("glm-5", ApiEndpoint::Messages)
            .unwrap_err()
            .message("glm-5");
        for model in ALL_MODELS {
            assert!(
                unsupported.contains(model),
                "missing {model}: {unsupported}"
            );
        }
    }
}
