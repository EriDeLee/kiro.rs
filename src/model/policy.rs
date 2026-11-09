//! 模型入口策略与模型名解析。
//!
//! 模型清单不再由本仓库写死：可用模型由每个凭据的 `ListAvailableModels`
//! 动态决定，并汇总进 [`crate::model::catalog`] 的进程级能力表。本模块只负责
//! 两件与具体模型清单无关的事：
//!
//! 1. 入口策略：`/v1/responses` 不接收 Claude 模型（其输出语义只实测过 GPT 族）。
//! 2. 客户端模型名 → 上游池内真实 ID：精确匹配优先，其次剥离
//!    `-thinking` / `-latest` / 8 位日期后缀，最后尝试点号与横线版本变体。
//!
//! 不在池内的合法模型 ID 不在这里拒绝：选号阶段按每个凭据的真实清单判定，
//! 池外模型本地 400。本模块只做入口策略与名字解析，不维护任何静态名单。

/// 上游 Kiro 模型族。推理字段路径、默认窗口和 effort 回退枚举按此分流。
///
/// 这个判定只用于「上游 schema 缺失时的兜底」，不是模型可用性的判据。
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
}

/// 按模型名前缀判定模型族。
///
/// 只做前缀识别，不做版本号或相邻型号的模糊推断。未知族返回 `None`，
/// 由 schema 或上游决定实际能力。
pub fn family_for_model(model_id: &str) -> Option<ModelFamily> {
    let id = model_id.trim().to_ascii_lowercase();
    if id.starts_with("claude-") {
        return Some(ModelFamily::Claude);
    }
    if id.starts_with("gpt-")
        || id.starts_with("chatgpt-")
        || id.starts_with("o1-")
        || id.starts_with("o3-")
        || id.starts_with("o4-")
    {
        return Some(ModelFamily::Gpt);
    }
    None
}

/// 是否是 Claude 模型。`/v1/responses` 的拒绝判据只认这个前缀。
pub fn is_claude_model(model_id: &str) -> bool {
    model_id.trim().to_ascii_lowercase().starts_with("claude-")
}

/// 判断模型是否允许从该入口请求。
///
/// 唯一规则：Claude 只能走 `/v1/messages`。其它模型（含动态池里的未知族）
/// 两个入口都放行，由上游 schema 决定实际能力。
pub fn reject_for_endpoint(model: &str, endpoint: ApiEndpoint) -> Result<(), RejectedModel> {
    if endpoint == ApiEndpoint::Responses && is_claude_model(model) {
        return Err(RejectedModel::ClaudeOnResponses {
            model: model.trim().to_string(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectedModel {
    /// Claude 模型走 `/v1/responses`。
    ClaudeOnResponses { model: String },
}

impl RejectedModel {
    pub fn message(&self) -> String {
        match self {
            Self::ClaudeOnResponses { model } => format!(
                "model `{model}` is a Claude model and is not supported on `{}`; \
                 use `{}` instead.",
                ApiEndpoint::Responses.path(),
                ApiEndpoint::Messages.path()
            ),
        }
    }
}

/// 生成客户端模型名的候选列表，按优先级排序。
///
/// 顺序：
/// 1. 客户端原样（trim 后）
/// 2. 剥离 `-thinking` / `-latest` / 8 位日期戳
/// 3. 对上面两者做点号↔横线的版本分隔符变体（仅相邻数字之间）
///
/// 去重后返回，调用方逐个与上游清单做大小写不敏感比较。
pub fn candidate_ids(requested: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let trimmed = requested.trim();
    if trimmed.is_empty() {
        return candidates;
    }

    push_unique(&mut candidates, trimmed.to_string());
    let stripped = strip_aliases(trimmed);
    push_unique(&mut candidates, stripped.clone());

    for base in [trimmed.to_string(), stripped] {
        if let Some(variant) = swap_version_separators(&base) {
            push_unique(&mut candidates, variant);
        }
    }

    candidates
}

fn push_unique(items: &mut Vec<String>, value: String) {
    if !value.is_empty() && !items.iter().any(|item| item.eq_ignore_ascii_case(&value)) {
        items.push(value);
    }
}

/// 剥离客户端常加的别名后缀，得到裸模型名。
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

/// 把相邻数字之间的 `.` 换成 `-`，或把 `-` 换成 `.`。
///
/// 只处理版本号分隔符，不做任何模糊推断。例如：
/// - `claude-opus-4-5` → `claude-opus-4.5`
/// - `gpt-5-6-sol` → `gpt-5.6-sol`
/// - `claude-opus-4.5` → `claude-opus-4-5`
fn swap_version_separators(value: &str) -> Option<String> {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() < 3 {
        return None;
    }

    let mut out = String::with_capacity(value.len());
    let mut changed = false;
    for (idx, ch) in chars.iter().enumerate() {
        let separator = *ch == '.' || *ch == '-';
        let digit_between = idx > 0
            && idx + 1 < chars.len()
            && chars[idx - 1].is_ascii_digit()
            && chars[idx + 1].is_ascii_digit();
        if separator && digit_between {
            let replacement = if *ch == '.' { '-' } else { '.' };
            out.push(replacement);
            changed = true;
        } else {
            out.push(*ch);
        }
    }
    changed.then_some(out)
}

/// 在动态模型清单里找客户端请求对应的真实 ID。
///
/// 匹配是大小写不敏感的，返回清单里的原始字符串（保持上游大小写）。
/// 找不到返回 `None`：该凭据的清单里没有这个模型，调用方据此把它排除出候选
/// （池外模型最终由选号阶段统一 400），不要在这里单独造 400。
pub fn match_model_id<'a>(
    requested: &str,
    available: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let available: Vec<&str> = available.into_iter().collect();
    for candidate in candidate_ids(requested) {
        if let Some(hit) = available
            .iter()
            .find(|id| id.eq_ignore_ascii_case(&candidate))
        {
            return Some((*hit).to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_uses_prefix_only() {
        assert_eq!(
            family_for_model("claude-opus-5.5"),
            Some(ModelFamily::Claude)
        );
        assert_eq!(family_for_model("Claude-Sonnet-4.6"), Some(ModelFamily::Claude));
        assert_eq!(family_for_model("gpt-5.6-terra"), Some(ModelFamily::Gpt));
        assert_eq!(family_for_model("o3-mini"), Some(ModelFamily::Gpt));
        assert_eq!(family_for_model("glm-5"), None);
        assert_eq!(family_for_model("deepseek-3.2"), None);
    }

    #[test]
    fn responses_rejects_only_claude() {
        assert!(reject_for_endpoint("claude-opus-5.5", ApiEndpoint::Responses).is_err());
        assert!(reject_for_endpoint("Claude-Opus-5.5-thinking", ApiEndpoint::Responses).is_err());
        assert!(reject_for_endpoint("gpt-5.6-sol", ApiEndpoint::Responses).is_ok());
        assert!(reject_for_endpoint("glm-5", ApiEndpoint::Responses).is_ok());
        assert!(reject_for_endpoint("deepseek-3.2", ApiEndpoint::Responses).is_ok());
        assert!(reject_for_endpoint("claude-opus-5.5", ApiEndpoint::Messages).is_ok());
    }

    #[test]
    fn exact_match_wins_before_alias_stripping() {
        let pool = ["claude-opus-5.5-thinking", "claude-opus-5.5"];
        assert_eq!(
            match_model_id("claude-opus-5.5-thinking", pool),
            Some("claude-opus-5.5-thinking".to_string())
        );
        assert_eq!(
            match_model_id("Claude-Opus-5.5-THINKING", pool),
            Some("claude-opus-5.5-thinking".to_string())
        );
    }

    #[test]
    fn aliases_and_version_variants_match_pool_ids() {
        let pool = ["claude-opus-5.5", "gpt-5.6-sol"];
        for requested in [
            "claude-opus-5.5-latest",
            "claude-opus-5.5-20260101",
            "claude-opus-5.5-20260101-thinking",
            "claude-opus-5-5",
        ] {
            assert_eq!(
                match_model_id(requested, pool),
                Some("claude-opus-5.5".to_string()),
                "{requested}"
            );
        }
        for requested in ["gpt-5-6-sol", "gpt-5-6-sol-latest"] {
            assert_eq!(
                match_model_id(requested, pool),
                Some("gpt-5.6-sol".to_string()),
                "{requested}"
            );
        }
    }

    #[test]
    fn unknown_ids_are_not_forced_into_the_pool() {
        let pool = ["claude-opus-5.5", "claude-sonnet-4.6"];
        assert_eq!(match_model_id("claude-opus-4.8", pool), None);
        assert_eq!(match_model_id("claude-sonnet-4-5", pool), None);
        assert_eq!(match_model_id("", pool), None);
    }

    #[test]
    fn version_separator_swap_only_touches_digits() {
        assert_eq!(
            swap_version_separators("claude-opus-4-5"),
            Some("claude-opus-4.5".to_string())
        );
        assert_eq!(
            swap_version_separators("claude-opus-4.5"),
            Some("claude-opus-4-5".to_string())
        );
        assert_eq!(swap_version_separators("gpt-5.6-sol"), Some("gpt-5-6-sol".to_string()));
        assert_eq!(
            swap_version_separators("claude-opus-5.5-thinking"),
            Some("claude-opus-5-5-thinking".to_string())
        );
        assert_eq!(swap_version_separators("claude-fable-5"), None);
    }
}
