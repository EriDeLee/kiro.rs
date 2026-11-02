//! Shared typed errors for Kiro upstream calls.

/// HTTP 429 for the client, with an optional Retry-After.
///
/// Usually the upstream still returned 429 after any applicable failover/retry.
/// The same type also carries the gateway's own per-account RPM rejection (no
/// request reached Kiro), so every caller keeps mapping it to 429；`local` 只改变
/// trace 与日志里的归类，不改变对外行为。
#[derive(Debug, Clone)]
pub struct UpstreamRateLimitError {
    retry_after: Option<String>,
    local: bool,
}

impl std::fmt::Display for UpstreamRateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.local {
            "local per-account RPM limit reached"
        } else {
            "upstream rate limited"
        })
    }
}

impl std::error::Error for UpstreamRateLimitError {}

impl UpstreamRateLimitError {
    pub(crate) fn new(retry_after: Option<String>) -> Self {
        Self {
            retry_after: retry_after.and_then(normalize_retry_after),
            local: false,
        }
    }

    /// 网关自己的单账号 RPM 打满：请求没有发到上游。
    pub(crate) fn local_rpm_limit(retry_after_secs: u64) -> Self {
        Self {
            retry_after: Some(retry_after_secs.to_string()),
            local: true,
        }
    }

    /// 是否为网关本地拒绝（而非上游返回 429）。
    pub fn is_local(&self) -> bool {
        self.local
    }

    pub(crate) fn from_headers(headers: &http::HeaderMap) -> Self {
        let retry_after = headers
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Self::new(retry_after)
    }

    pub fn retry_after(&self) -> Option<&str> {
        self.retry_after.as_deref()
    }

    /// Without an explicit upstream delay, a short local retry is still useful.
    pub(crate) fn should_retry_locally(&self) -> bool {
        self.retry_after.is_none()
    }
}

fn normalize_retry_after(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    if value.parse::<u64>().is_ok() || httpdate::parse_http_date(value).is_ok() {
        Some(value.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_delta_seconds_and_http_date() {
        let seconds = UpstreamRateLimitError::new(Some(" 1800 ".to_string()));
        assert_eq!(seconds.retry_after(), Some("1800"));
        assert!(!seconds.should_retry_locally());

        let date = "Sun, 12 Jul 2026 02:30:00 GMT";
        let http_date = UpstreamRateLimitError::new(Some(date.to_string()));
        assert_eq!(http_date.retry_after(), Some(date));
        assert!(!http_date.should_retry_locally());
    }

    #[test]
    fn rejects_invalid_retry_after() {
        let error = UpstreamRateLimitError::new(Some("not-a-retry-delay".to_string()));
        assert_eq!(error.retry_after(), None);
        assert!(error.should_retry_locally());
    }

    /// 本地 RPM 限流与上游 429 必须能区分，否则 trace 与日志会把网关自己的拒绝
    /// 记成「上游限流」（2026-10-30 上游线上 8 小时 1101 次误报）。
    #[test]
    fn local_rpm_limit_is_labelled_apart_from_upstream_429() {
        let local = UpstreamRateLimitError::local_rpm_limit(42);
        assert!(local.is_local());
        assert_eq!(local.retry_after(), Some("42"));
        assert_eq!(local.to_string(), "local per-account RPM limit reached");

        let upstream = UpstreamRateLimitError::new(Some("42".to_string()));
        assert!(!upstream.is_local());
        assert_eq!(upstream.to_string(), "upstream rate limited");
    }
}
