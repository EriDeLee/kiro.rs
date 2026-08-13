//! Admin API 错误类型定义

use std::fmt;

use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

use super::types::AdminErrorResponse;

/// Admin 服务错误类型
#[derive(Debug)]
pub enum AdminServiceError {
    /// 凭据不存在
    NotFound { id: u64 },

    /// 上游服务调用失败（网络、API 错误等）
    UpstreamError(String),

    /// 上游失败原因已被识别，可安全展示给管理员。
    ///
    /// 与 [`Self::UpstreamError`] 的区别：那个的原始报文可能含 AWS 账号 ID /
    /// request-id，一律不外发（见本文件的 `upstream_error_response_does_not_expose_raw_body`）。
    /// 这里的 `public` 只允许取固定字面量，原始报文仍然只进日志 —— 面板因此能
    /// 显示「账号被封禁」这种可操作的原因，而不是一句无信息量的通用失败。
    UpstreamRejected {
        public: &'static str,
        detail: String,
    },

    /// 上游明确返回限流，可选携带合法 Retry-After。
    RateLimited { retry_after: Option<String> },

    /// 内部状态错误
    InternalError(String),

    /// 凭据无效（验证失败）
    InvalidCredential(String),
}

impl fmt::Display for AdminServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdminServiceError::NotFound { id } => {
                write!(f, "凭据不存在: {}", id)
            }
            AdminServiceError::UpstreamError(_) => write!(f, "上游服务请求失败"),
            AdminServiceError::UpstreamRejected { public, .. } => write!(f, "{}", public),
            AdminServiceError::RateLimited { .. } => write!(f, "上游请求过于频繁，请稍后重试"),
            AdminServiceError::InternalError(msg) => write!(f, "内部错误: {}", msg),
            AdminServiceError::InvalidCredential(msg) => write!(f, "凭据无效: {}", msg),
        }
    }
}

impl std::error::Error for AdminServiceError {}

impl AdminServiceError {
    /// 获取对应的 HTTP 状态码
    pub fn status_code(&self) -> StatusCode {
        match self {
            AdminServiceError::NotFound { .. } => StatusCode::NOT_FOUND,
            AdminServiceError::UpstreamError(_) | AdminServiceError::UpstreamRejected { .. } => {
                StatusCode::BAD_GATEWAY
            }
            AdminServiceError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            AdminServiceError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AdminServiceError::InvalidCredential(_) => StatusCode::BAD_REQUEST,
        }
    }

    /// 转换为 API 错误响应
    pub fn into_response(self) -> AdminErrorResponse {
        match &self {
            AdminServiceError::NotFound { .. } => AdminErrorResponse::not_found(self.to_string()),
            AdminServiceError::UpstreamError(_)
            | AdminServiceError::UpstreamRejected { .. } => {
                AdminErrorResponse::api_error(self.to_string())
            }
            AdminServiceError::RateLimited { .. } => {
                AdminErrorResponse::rate_limit(self.to_string())
            }
            AdminServiceError::InternalError(_) => {
                AdminErrorResponse::internal_error(self.to_string())
            }
            AdminServiceError::InvalidCredential(_) => {
                AdminErrorResponse::invalid_request(self.to_string())
            }
        }
    }

    pub fn into_http_response(self) -> Response {
        match &self {
            AdminServiceError::UpstreamError(message) => {
                tracing::warn!(error = %message, "Admin 上游服务请求失败");
            }
            // 原始报文只进日志；外发的只有 `public` 那句固定文案
            AdminServiceError::UpstreamRejected { public, detail } => {
                tracing::warn!(reason = %public, error = %detail, "Admin 上游明确拒绝");
            }
            _ => {}
        }
        let retry_after = match &self {
            AdminServiceError::RateLimited { retry_after } => retry_after.clone(),
            _ => None,
        };
        let status = self.status_code();
        let mut response = (status, Json(self.into_response())).into_response();
        if let Some(value) = retry_after.and_then(|value| value.parse().ok()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rate_limit_response_has_status_header_and_stable_body() {
        let response = AdminServiceError::RateLimited {
            retry_after: Some("120".to_string()),
        }
        .into_http_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "120");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert!(!body["error"]["message"].as_str().unwrap().is_empty());
    }

    /// `UpstreamRejected` 只外发固定文案，原始报文仍不出现在响应体里。
    ///
    /// 这是「让面板显示真实原因」与「不泄露 AWS 账号 ID / request-id」的分界：
    /// 展示的是被识别出的原因，不是上游报文本身。
    #[tokio::test]
    async fn upstream_rejected_exposes_only_the_public_reason() {
        let detail = "权限不足，无法获取可用模型: 403 Forbidden \
             {\"message\":\"Your User ID (736048611274) is temporarily suspended\"}";
        let response = AdminServiceError::UpstreamRejected {
            public: "账号被封禁",
            detail: detail.to_string(),
        }
        .into_http_response();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("账号被封禁"), "应给出可操作的具体原因");
        assert!(!body.contains("736048611274"), "不得泄露上游账号 ID");
        assert!(!body.contains("Forbidden"), "不得回显上游原始报文");
    }

    #[tokio::test]
    async fn upstream_error_response_does_not_expose_raw_body() {
        let secret = "aws-account=123456789012 request-id=private-request";
        let response = AdminServiceError::UpstreamError(secret.to_string()).into_http_response();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(!body.contains(secret));
        assert!(body.contains("上游服务请求失败"));
    }
}
