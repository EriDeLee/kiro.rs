//! Kiro 端点抽象
//!
//! 不同 Kiro 端点（如 `ide` / `cli`）在 URL、请求头、请求体上存在差异，
//! 但共享凭据池、Token 刷新、重试逻辑和 AWS event-stream 响应解码。
//!
//! [`KiroEndpoint`] 抽象了请求侧的差异点；`KiroProvider` 持有一个 endpoint 注册表，
//! 按凭据的 `endpoint` 字段选择对应实现。

use reqwest::RequestBuilder;

use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

pub mod cli;
pub mod ide;

pub use cli::CliEndpoint;
pub use ide::IdeEndpoint;

/// Kiro 端点
///
/// 同一个 `KiroProvider` 可持有多个 endpoint 实现，按凭据级字段切换。
pub trait KiroEndpoint: Send + Sync {
    /// 端点名称（对应 credentials.endpoint / config.defaultEndpoint 的取值）
    fn name(&self) -> &'static str;

    /// API 请求的 Content-Type（默认 application/json）
    fn content_type(&self) -> &'static str {
        "application/json"
    }

    /// API endpoint URL
    fn api_url(&self, ctx: &RequestContext<'_>) -> String;

    /// MCP endpoint URL
    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String;

    /// 装饰 API 请求的端点特有 header
    ///
    /// Provider 已经设置好 URL、content-type、Connection 和 body；
    /// 实现负责追加 Authorization、host、user-agent 等端点相关头。
    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder;

    /// 装饰 MCP 请求的端点特有 header
    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder;

    /// 对已序列化的 API 请求体做端点特有加工（如注入 profileArn）
    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String;

    /// 对已序列化的 MCP 请求体做端点特有加工（默认不变）
    fn transform_mcp_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
        body.to_string()
    }

    /// 判断响应体是否表示"月度配额用尽"（禁用凭据并转移）
    fn is_monthly_request_limit(&self, body: &str) -> bool {
        default_is_monthly_request_limit(body)
    }

    /// 判断响应体是否表示"上游 bearer token 失效"（触发强制刷新）
    fn is_bearer_token_invalid(&self, body: &str) -> bool {
        default_is_bearer_token_invalid(body)
    }

    /// 判断响应体是否表示"账号级临时风控"（429 + suspicious activity）
    ///
    /// 与普通 429（high traffic）区分：账号级风控只针对当前凭据生效，
    /// 故障转移到其它凭据后可立即恢复；普通 429 是上游全局过载，切换无意义。
    fn is_account_throttled(&self, body: &str) -> bool {
        default_is_account_throttled(body)
    }

    /// 判断响应体是否表示"客户端请求格式错误"（messages 数组本身违反协议）
    ///
    /// 这类错误（tool_use↔tool_result 不配对、消息序列非法等）的根因是调用方的
    /// 请求体，而非上游故障。无论上游以 4xx 还是 5xx 返回，重试都不可能成功；
    /// 尤其当上游以 5xx 返回时，若按瞬态错误重试，会把一个永不可能成功的坏请求
    /// 放大成多次 503（503 风暴）并无谓占用重试预算。识别后应立即终止，
    /// 不重试、不切换凭据。
    fn is_client_validation_error(&self, body: &str) -> bool {
        default_is_client_validation_error(body)
    }

    /// 判断响应体是否明确表示图片数量超过上游限制。
    fn is_image_count_exceeded(&self, body: &str) -> bool {
        default_is_image_count_exceeded(body)
    }

    /// 判断响应体是否表示上游网关超时。
    ///
    /// 524 通常来自 Cloudflare/边缘层，继续在同一次客户端调用里重试会把等待时间
    /// 放大到客户端自己的重试上限；让调用方快速失败更利于下一次请求重新建连。
    fn is_gateway_timeout(&self, body: &str) -> bool {
        default_is_gateway_timeout(body)
    }

    /// 判断响应体是否表示"账号被封禁/停用"（403 + 明确封禁文案）。
    ///
    /// 与普通 403（权限/WAF/区域抖动）区分：账号封禁是不可自动恢复的终态，
    /// 需人工联系客服核实。识别后立即禁用该凭据且**不参与自愈**，避免死循环。
    fn is_account_suspended(&self, body: &str) -> bool {
        default_is_account_suspended(body)
    }

    /// 判断响应体是否表示"该凭据当前取不到请求的模型"（400 + `INVALID_MODEL_ID`）。
    ///
    /// 与其它 400 的区别：其它 400（请求体非法、图片超限、签名失效、正文超长）换凭据
    /// 毫无意义，必须快速失败；这一个换个凭据往往就能成功。见
    /// [`MODEL_UNAVAILABLE_REASON`]。
    fn is_model_unavailable(&self, body: &str) -> bool {
        default_is_model_unavailable(body)
    }
}

/// 装饰请求时可用的上下文
///
/// 包含单次调用已确定的所有运行时信息。引用形式避免无谓 clone。
pub struct RequestContext<'a> {
    /// 当前凭据
    pub credentials: &'a KiroCredentials,
    /// 有效的 access token（API Key 凭据下即 kiroApiKey）
    pub token: &'a str,
    /// 当前凭据对应的 machineId
    pub machine_id: &'a str,
    /// 全局配置
    pub config: &'a Config,
}

/// 触发"额度耗尽 → 禁用并切换"的 reason 取值集合
///
/// - `MONTHLY_REQUEST_COUNT`: 月度请求额度用尽
/// - `OVERAGE_REQUEST_LIMIT_EXCEEDED`: 超额（overage）额度也耗尽
///
/// 两类语义都是「该凭据当前计费周期内不能再用」，处理方式一致：
/// 立刻禁用凭据并故障转移到下一个可用凭据。
const QUOTA_EXHAUSTED_REASONS: &[&str] = &[
    "MONTHLY_REQUEST_COUNT",
    "OVERAGE_REQUEST_LIMIT_EXCEEDED",
];

/// 触发"账号被封禁 → 立即禁用且不参与自愈"的 `reason` 取值
///
/// 结构化字段远比文案稳定：2026-08-13 实测上游把封禁文案从
/// `We've locked your account` 改成了 `and locked it as a security precaution`，
/// 只认文案的旧判据当场失效（详见 [`default_is_account_suspended`]）。
const SUSPENDED_REASON: &str = "TEMPORARILY_SUSPENDED";

/// 触发"该凭据当前取不到此模型 → 冷却并换号"的 `reason` 取值
///
/// 上游对此返回 **400**（不是 403）：
/// `{"message":"Invalid model ID. Please select a different model to continue.","reason":"INVALID_MODEL_ID"}`
///
/// 2026-08-13 从 trace DB 取样确认存在且已发生 13 次（凭据 #4 / #5，
/// `claude-opus-5` 12 次 + `claude-sonnet-5` 1 次），全部 `finalStatus=error`
/// —— 即当时整条客户端请求直接失败，没有换号。
///
/// **这个状态是临时的，不代表订阅不含该模型。** 已观察到订阅确实支持该模型的账号
/// 也会收到它，数小时后同一模型又能正常请求。故绝不可据此把凭据对该模型永久标记为
/// 不支持 —— 那等于把已删除的 `supports_opus()` 硬否决换个形式加回来
/// （见 `token_manager` 的 `credential_match_does_not_gate_on_subscription_tier`）。
/// 正确处置是"短时冷却 + 换号"，靠冷却到期自动恢复。
const MODEL_UNAVAILABLE_REASON: &str = "INVALID_MODEL_ID";

/// 默认的"请求额度耗尽"判断逻辑
///
/// 同时识别顶层 `reason` 字段和嵌套 `error.reason` 字段。
/// 任一已知额度耗尽 reason 命中即返回 true。
pub fn default_is_monthly_request_limit(body: &str) -> bool {
    // 先快速字符串扫描，避免对 99% 不命中的响应体做 JSON 解析
    if QUOTA_EXHAUSTED_REASONS.iter().any(|r| body.contains(r)) {
        // 进一步用 JSON 解析确认 reason 字段而非偶然出现的子串
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
            let top = value.get("reason").and_then(|v| v.as_str());
            let nested = value.pointer("/error/reason").and_then(|v| v.as_str());
            return [top, nested]
                .into_iter()
                .flatten()
                .any(|r| QUOTA_EXHAUSTED_REASONS.contains(&r));
        }
        // body 是非 JSON 但包含关键词（兼容简单文本响应）
        return true;
    }
    false
}

/// 默认的 bearer token 失效判断逻辑
pub fn default_is_bearer_token_invalid(body: &str) -> bool {
    body.contains("The bearer token included in the request is invalid")
}

/// 默认的账号级风控判断逻辑
///
/// 上游 Kiro/Q-Developer 风控会返回 429 + 类似：
/// `Due to suspicious activity, we are imposing temporary limits on how
/// frequently your account (d-...) can send a request to Kiro while we investigate.`
///
/// 与普通 429（high traffic / rate limit exceeded）的关键差异是
/// 提到 "suspicious activity" 与具体账号 ID。
pub fn default_is_account_throttled(body: &str) -> bool {
    body.contains("suspicious activity")
        && body.contains("temporary limits")
}

/// 默认的"账号被封禁/停用"判断逻辑
///
/// 上游对被封账号返回 403，已实测到**两种**文案，且都带
/// `"reason":"TEMPORARILY_SUSPENDED"` 之外的形态差异：
///
/// - 2025 版（`reason` 为 `null`，只能靠文案）：
///   `Your User ID (...) temporarily is suspended. We've locked your account as a
///   security precaution. To restore access, please contact our support team ...`
/// - 2026-08-13 实测版（带结构化 `reason`）：
///   `Your User ID is temporarily suspended. We detected unusual user activity and
///   locked it as a security precaution. To restore access, please contact our
///   support team ...`
///
/// 判定顺序据此分两层：
///
/// 1. **结构化优先**：顶层或 `error.reason` 等于 `TEMPORARILY_SUSPENDED` 即命中。
///    与 [`default_is_monthly_request_limit`] 同构，是上游改文案时唯一稳的锚点。
/// 2. **文案兜底**：`reason` 缺失（2025 版）时仍需文案判定。要求 "suspended" 与
///    "锁定"类短语同时出现，大小写不敏感；单独出现任一短语不判定，避免把偶发
///    403（权限不足 / WAF / 区域抖动）误判为封禁。
///
/// 历史教训：旧实现只有第 2 层且"锁定"类短语写死为 `locked your account`。
/// 上游把它改成 `locked it as a security precaution` 后判据静默失效 ——
/// 被封凭据退化成普通 403，按 `TooManyFailures` 禁用，而自愈恰好只复活
/// `TooManyFailures`，于是每 5 分钟复活一次再被 403 打死（issue #51 的死循环
/// 以另一种形式复发）。所以第 1 层必须在，且不能再让文案成为唯一判据。
pub fn default_is_account_suspended(body: &str) -> bool {
    // 第 1 层：结构化 reason。先做字符串快扫，避免对绝大多数响应体做 JSON 解析
    if body.contains(SUSPENDED_REASON) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
            let top = value.get("reason").and_then(|v| v.as_str());
            let nested = value.pointer("/error/reason").and_then(|v| v.as_str());
            if [top, nested]
                .into_iter()
                .flatten()
                .any(|reason| reason == SUSPENDED_REASON)
            {
                return true;
            }
        } else {
            // body 非 JSON 但含该 reason 关键词（兼容简单文本响应）
            return true;
        }
    }

    // 第 2 层：文案兜底
    let lower = body.to_ascii_lowercase();
    lower.contains("suspended")
        && (lower.contains("locked your account")
            || lower.contains("locked it as a security precaution"))
}

/// 默认的"该凭据当前取不到此模型"判断逻辑
///
/// **只认结构化 `reason`，不做任何文案匹配。** 理由：`message` 那句
/// `Invalid model ID. Please select a different model to continue.` 是完全可能出现在
/// 模型正文里的普通英文（比如用户就在问这条报错是什么意思），靠文案匹配会把一次正常
/// 回答误判成"模型不可用"并冷却掉一个健康凭据。而 `reason` 是结构化字段，不会被正文污染。
///
/// 顶层与嵌套 `error.reason` 两种形状都认（与额度、封禁两个判据同构）。
pub fn default_is_model_unavailable(body: &str) -> bool {
    if !body.contains(MODEL_UNAVAILABLE_REASON) {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        // 非 JSON body 不判定：这里没有可信的结构化字段，宁可放过也不冷却健康凭据
        return false;
    };
    let top = value.get("reason").and_then(|v| v.as_str());
    let nested = value.pointer("/error/reason").and_then(|v| v.as_str());
    [top, nested]
        .into_iter()
        .flatten()
        .any(|reason| reason == MODEL_UNAVAILABLE_REASON)
}

/// 默认的上游网关超时判断逻辑。
pub fn default_is_gateway_timeout(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    body.contains("524")
        && (lower.contains("status code")
            || lower.contains("gateway timeout")
            || lower.contains("server-side issue"))
}

/// 默认的图片数量超限判断逻辑。
///
/// Kiro 的实测响应使用精确 reason `IMAGE_COUNT_EXCEEDED`。仅匹配顶层或
/// `error.reason` 中的这个结构化值，避免因错误文案碰巧提到图片数量而误删图片。
pub fn default_is_image_count_exceeded(body: &str) -> bool {
    const REASON: &str = "IMAGE_COUNT_EXCEEDED";
    if !body.contains(REASON) {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let top = value.get("reason").and_then(|value| value.as_str());
    let nested = value.pointer("/error/reason").and_then(|value| value.as_str());
    [top, nested]
        .into_iter()
        .flatten()
        .any(|reason| reason == REASON)
}

/// 触发"客户端请求格式错误 → 立即终止、不重试"的精确 reason 取值集合
///
/// 这些都是上游对 messages 数组本身的协议校验失败（根因在调用方请求体，
/// 而非上游故障）。仅收录**精确 reason 值**，不收录 `ValidationException`
/// 这类宽泛异常类型——后者语义过宽，裸子串匹配会把恰好携带该词的真实上游
/// 瞬态故障误判为"不可重试"，反而杀掉本可重试恢复的请求。
/// `THINKING_SIGNATURE_INVALID`：历史 `reasoningContent.reasoningText.signature`
/// 验签失败（跨账号轮换、模型版本变更、或签名被中途改写）。归入本类是刻意选择：
/// **不剥离 reasoningContent 重试**。剥离会让请求"看似成功"，实际已经悄悄丢掉
/// 整段历史推理 —— 宁可让调用方看到失败并自行处理（新开会话或换回原账号），
/// 也不要静默降级成没有推理的对话。
const CLIENT_VALIDATION_REASONS: &[&str] = &[
    "TOOL_USE_RESULT_MISMATCH",
    "TOOL_SCHEMA_INVALID",
    "THINKING_SIGNATURE_INVALID",
];

/// 触发同类判定的 message 级特征短语（用于无结构化 reason、仅文本报文的场景）
///
/// 例如 Bedrock 的 "Expected toolResult blocks ..." 纯文本错误。短语需具备
/// 足够特异性，不会与正常响应内容冲突。
const CLIENT_VALIDATION_MESSAGE_MARKERS: &[&str] = &["Expected toolResult blocks"];

/// 默认的"客户端请求格式错误"判断逻辑
///
/// 与 [`default_is_monthly_request_limit`] 同构：先做廉价子串快扫，命中后再用
/// JSON 解析确认 `reason`（顶层与嵌套 `error.reason`）字段，避免把偶然出现在
/// 普通字段里的关键词误判。结构化确认失败时，回退到 message 级特异短语匹配，
/// 以覆盖非 JSON 的纯文本错误报文。
pub fn default_is_client_validation_error(body: &str) -> bool {
    let reason_hit = CLIENT_VALIDATION_REASONS.iter().any(|r| body.contains(r));
    if reason_hit {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
            let top = value.get("reason").and_then(|v| v.as_str());
            let nested = value.pointer("/error/reason").and_then(|v| v.as_str());
            if [top, nested]
                .into_iter()
                .flatten()
                .any(|r| CLIENT_VALIDATION_REASONS.contains(&r))
            {
                return true;
            }
        } else {
            // 非 JSON 但含精确 reason 关键词（兼容简单文本响应）
            return true;
        }
    }
    // message 级兜底：纯文本错误报文（无结构化 reason）
    CLIENT_VALIDATION_MESSAGE_MARKERS
        .iter()
        .any(|m| body.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_monthly_request_limit_detects_reason() {
        let body = r#"{"message":"You have reached the limit.","reason":"MONTHLY_REQUEST_COUNT"}"#;
        assert!(default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_monthly_request_limit_nested_reason() {
        let body = r#"{"error":{"reason":"MONTHLY_REQUEST_COUNT"}}"#;
        assert!(default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_monthly_request_limit_false() {
        let body = r#"{"message":"nope","reason":"DAILY_REQUEST_COUNT"}"#;
        assert!(!default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_quota_exhausted_overage() {
        let body = r#"{"message":"You have reached the limit for overages.","reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}"#;
        assert!(default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_quota_exhausted_overage_nested() {
        let body = r#"{"error":{"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}}"#;
        assert!(default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_quota_exhausted_substring_does_not_false_match() {
        // 关键字出现在普通字段而非 reason 字段：仍然命中（向后兼容旧行为）
        // 但 reason 字段是其他值时应严格不命中
        let body =
            r#"{"message":"some text MONTHLY_REQUEST_COUNT-like phrase","reason":"OTHER"}"#;
        assert!(!default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_bearer_token_invalid() {
        assert!(default_is_bearer_token_invalid(
            "The bearer token included in the request is invalid"
        ));
        assert!(!default_is_bearer_token_invalid("unrelated error"));
    }

    #[test]
    fn test_default_is_account_suspended() {
        let body = r#"{"message":"Your User ID (736048611274) temporarily is suspended. We've locked your account as a security precaution. To restore access, please contact our support team to verify your identity: https://aws.amazon.com/contact-us/","reason":null}"#;
        assert!(default_is_account_suspended(body));

        // 大小写不敏感
        assert!(default_is_account_suspended(
            "Account SUSPENDED. We've LOCKED YOUR ACCOUNT."
        ));

        // 普通 403 权限错误不应命中
        assert!(!default_is_account_suspended(
            r#"{"message":"User is not authorized to perform this action","reason":null}"#
        ));
        // 仅命中一个短语时不判定为封禁
        assert!(!default_is_account_suspended("your account is suspended"));
        assert!(!default_is_account_suspended(
            "we have locked your account temporarily"
        ));
    }

    /// 回归锁：2026-08-13 实测的真实封禁响应必须被判定为封禁。
    ///
    /// 这条文案里**没有** `locked your account`（上游改成了 `locked it as a
    /// security precaution`），旧判据在它上面静默返回 false，导致被封凭据被当成
    /// 普通 403 → `TooManyFailures` → 每 5 分钟被自愈复活再被 403 打死。
    #[test]
    fn account_suspended_matches_2026_08_13_upstream_wording() {
        let body = r#"{"message":"Your User ID is temporarily suspended. We detected unusual user activity and locked it as a security precaution. To restore access, please contact our support team to verify your identity: https://support.aws.amazon.com/#/contacts/kiro","reason":"TEMPORARILY_SUSPENDED"}"#;
        assert!(
            default_is_account_suspended(body),
            "2026-08-13 实测封禁文案必须命中，否则被封凭据会退化成 TooManyFailures 并被自愈反复复活"
        );
    }

    /// 回归锁：只要结构化 `reason` 命中就判定封禁，不依赖任何文案。
    ///
    /// 上游可以随时再改一次 message；`reason` 是唯一稳的锚点。
    /// 顶层与嵌套 `error.reason` 两种形状都要认（与额度判据同构）。
    #[test]
    fn account_suspended_matches_structured_reason_without_known_wording() {
        assert!(default_is_account_suspended(
            r#"{"message":"totally new wording nobody has seen","reason":"TEMPORARILY_SUSPENDED"}"#
        ));
        assert!(default_is_account_suspended(
            r#"{"error":{"reason":"TEMPORARILY_SUSPENDED"}}"#
        ));
    }

    /// 回归锁：2026-08-13 从 trace DB 取出的**两种真实 403 响应字节**必须被分到不同处置。
    ///
    /// 取样自 17 个凭据里全部失败尝试的去重结果（8 个凭据、两种 body）：
    /// - 6 个凭据（#7 #10 #11 #12 #13 #15）只吐封禁 body → 走 `Suspended`
    /// - 2 个凭据（#8 #9）只吐 bearer-invalid body，**从未**吐过封禁文案
    ///   → 只能走刷新后仍被拒的终态，封禁判据必须对它返回 false
    ///
    /// 两条 body 都不含账号 ID，可安全作为固定测试数据。
    /// 这条锁的是"分流"而非单个判据：把两者混为一谈会让 #8/#9 被标成封禁
    /// （原因说谎），或让 #7 类退回 `TooManyFailures`（被自愈反复复活）。
    #[test]
    fn real_403_payloads_route_to_distinct_verdicts() {
        const SUSPENDED_BODY: &str = r#"{"message":"Your User ID is temporarily suspended. We detected unusual user activity and locked it as a security precaution. To restore access, please contact our support team to verify your identity: https://support.aws.amazon.com/#/contacts/kiro","reason":"TEMPORARILY_SUSPENDED"}"#;
        const BEARER_INVALID_BODY: &str =
            r#"{"message":"The bearer token included in the request is invalid.","reason":null}"#;

        assert!(
            default_is_account_suspended(SUSPENDED_BODY),
            "#7 #10 #11 #12 #13 #15 的真实 body 必须判为封禁"
        );
        assert!(
            !default_is_bearer_token_invalid(SUSPENDED_BODY),
            "封禁 body 不应同时命中 bearer-invalid，否则会先被刷新分支截走"
        );

        assert!(
            default_is_bearer_token_invalid(BEARER_INVALID_BODY),
            "#8 #9 的真实 body 必须判为 token 失效"
        );
        assert!(
            !default_is_account_suspended(BEARER_INVALID_BODY),
            "#8 #9 从未吐过封禁文案，不能被标成账号封禁 —— 那是编造禁用原因"
        );

        // 两条都不是额度问题，别和 402 路径串台
        assert!(!default_is_monthly_request_limit(SUSPENDED_BODY));
        assert!(!default_is_monthly_request_limit(BEARER_INVALID_BODY));
    }

    /// 回归锁：`INVALID_MODEL_ID` 只按结构化 `reason` 判定，正文提到那句话不算。
    ///
    /// 上游那句 `Invalid model ID. Please select a different model to continue.` 是
    /// 完全可能出现在模型正文里的普通英文（用户就可能在问这条报错是什么意思）。
    /// 若靠文案匹配，一次正常回答会被判成"模型不可用"并冷却掉一个健康凭据。
    #[test]
    fn model_unavailable_matches_structured_reason_only() {
        // 真实 body（2026-08-13 trace DB，13 次）
        assert!(default_is_model_unavailable(
            r#"{"message":"Invalid model ID. Please select a different model to continue.","reason":"INVALID_MODEL_ID"}"#
        ));
        // 嵌套形状同样认
        assert!(default_is_model_unavailable(
            r#"{"error":{"reason":"INVALID_MODEL_ID"}}"#
        ));

        // 正文提到关键词但 reason 不是它 → 不命中
        assert!(!default_is_model_unavailable(
            r#"{"message":"what does INVALID_MODEL_ID mean?","reason":null}"#
        ));
        // 只有文案、没有结构化字段 → 不命中（宁可放过也不冷却健康凭据）
        assert!(!default_is_model_unavailable(
            "Invalid model ID. Please select a different model to continue."
        ));
        // 非 JSON 且含关键词 → 不命中
        assert!(!default_is_model_unavailable("INVALID_MODEL_ID"));

        // 别和其它 400 串台：这几种换凭据没意义，必须快速失败
        for other in [
            r#"{"message":"too many inline media segments: 101 exceeds limit 100","reason":"IMAGE_COUNT_EXCEEDED"}"#,
            r#"{"message":"messages.1.content.30: Invalid `signature` in `thinking` block","reason":"THINKING_SIGNATURE_INVALID"}"#,
            r#"{"message":"Improperly formed request.","reason":"REQUEST_BODY_INVALID"}"#,
            r#"{"message":"Input content length exceeds threshold.","reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#,
        ] {
            assert!(
                !default_is_model_unavailable(other),
                "{other} 不应被判为模型不可用"
            );
        }
    }

    /// 正文顺口提到关键词不算封禁：`reason` 字段值不对，文案也只命中半边。
    ///
    /// 防的是把模型回答里出现 "TEMPORARILY_SUSPENDED" 字样的响应误判成封禁 ——
    /// 误判的代价是把一个健康凭据永久踢出轮换且不参与自愈。
    #[test]
    fn account_suspended_ignores_keyword_in_unrelated_field() {
        assert!(!default_is_account_suspended(
            r#"{"message":"explain what TEMPORARILY_SUSPENDED means","reason":null}"#
        ));
    }

    #[test]
    fn test_default_is_account_throttled() {
        let body = r#"{"message":"Due to suspicious activity, we are imposing temporary limits on how frequently your account (d-9067c98495.84f894a8) can send a request to Kiro while we investigate.","reason":null}"#;
        assert!(default_is_account_throttled(body));
        // 普通 429 不应被识别为账号风控
        assert!(!default_is_account_throttled(
            "{\"message\":\"Too many requests\"}"
        ));
        // 仅有一半关键词时也不命中
        assert!(!default_is_account_throttled("suspicious activity detected"));
    }

    #[test]
    fn test_default_is_gateway_timeout() {
        assert!(default_is_gateway_timeout(
            "API Error: 524 status code (no body). This is a server-side issue"
        ));
        assert!(default_is_gateway_timeout("524 Gateway Timeout"));
        assert!(!default_is_gateway_timeout(
            r#"{"message":"some unrelated field mentions 524 tokens"}"#
        ));
    }

    #[test]
    fn test_default_is_image_count_exceeded() {
        assert!(default_is_image_count_exceeded(
            r#"{"message":"too many inline media segments: 101 exceeds limit 100","reason":"IMAGE_COUNT_EXCEEDED"}"#
        ));
        assert!(default_is_image_count_exceeded(
            r#"{"error":{"reason":"IMAGE_COUNT_EXCEEDED"}}"#
        ));
        assert!(!default_is_image_count_exceeded(
            r#"{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#
        ));
        assert!(!default_is_image_count_exceeded(
            r#"{"message":"too many images","reason":"OTHER"}"#
        ));
        assert!(!default_is_image_count_exceeded(
            r#"{"message":"trace mentions IMAGE_COUNT_EXCEEDED","reason":"OTHER"}"#
        ));
        assert!(!default_is_image_count_exceeded(
            "upstream error: IMAGE_COUNT_EXCEEDED"
        ));
    }

    #[test]
    fn test_default_is_client_validation_error() {
        // 顶层 reason 命中（结构化确认）
        assert!(default_is_client_validation_error(
            r#"{"reason":"TOOL_USE_RESULT_MISMATCH"}"#
        ));
        // 嵌套 error.reason 命中
        assert!(default_is_client_validation_error(
            r#"{"error":{"reason":"TOOL_USE_RESULT_MISMATCH"}}"#
        ));
        // 非 JSON 但含精确 reason 关键词
        assert!(default_is_client_validation_error(
            "upstream error: TOOL_USE_RESULT_MISMATCH"
        ));
        // TOOL_SCHEMA_INVALID：工具 inputSchema 不合规（如顶层 oneOf / 非 object），
        // 根因在请求体，重试/换号不会好，应立即终止。
        assert!(default_is_client_validation_error(
            r#"{"__type":"ValidationException","message":"input_schema does not support oneOf, allOf, or anyOf at the top level","reason":"TOOL_SCHEMA_INVALID"}"#
        ));
        // message 级特异短语（纯文本，无结构化 reason）
        assert!(default_is_client_validation_error(
            "Expected toolResult blocks but found none"
        ));

        // 普通上游错误不应被误判（否则会跳过应有的重试）
        assert!(!default_is_client_validation_error(
            r#"{"message":"Internal server error"}"#
        ));
        assert!(!default_is_client_validation_error("connection reset by peer"));
        // 关键回归：reason 关键词偶然出现在普通字段，但真实 reason 是别的值 —— 不应命中
        // （否则会把一个本可重试恢复的真实上游故障误杀）
        assert!(!default_is_client_validation_error(
            r#"{"message":"trace mentions TOOL_USE_RESULT_MISMATCH internally","reason":"INTERNAL_SERVER_ERROR"}"#
        ));
        // 宽泛的 ValidationException 不再单独命中（无精确 reason / 无特异短语时）
        assert!(!default_is_client_validation_error(
            r#"{"__type":"ValidationException","message":"some other validation"}"#
        ));
    }
}
