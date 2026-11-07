//! HTTP Client 构建模块
//!
//! 提供统一的 HTTP Client 构建功能，支持代理配置

use reqwest::{Client, Proxy};
use std::time::Duration;

use crate::model::config::TlsBackend;

/// 代理配置
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ProxyConfig {
    /// 代理地址，支持 http/https/socks5
    pub url: String,
    /// 代理认证用户名
    pub username: Option<String>,
    /// 代理认证密码
    pub password: Option<String>,
}

impl ProxyConfig {
    /// 从 url 创建代理配置
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            username: None,
            password: None,
        }
    }

    /// 设置认证信息
    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

/// 构建 HTTP Client（带整体超时，用于刷新 token、查余额这类短请求）
///
/// # Arguments
/// * `proxy` - 可选的代理配置
/// * `timeout_secs` - 超时时间（秒）
///
/// # Returns
/// 配置好的 reqwest::Client
pub fn build_client(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    Ok(client_builder(proxy, tls_backend)?
        .timeout(Duration::from_secs(timeout_secs))
        .build()?)
}

/// 构建上游 API 请求（流式回答）用的 HTTP Client：**不设任何超时**。
///
/// reqwest 的整体超时包含读响应体，活跃的流也无法续期，所以它其实是回答长度的硬上限：
/// 超时那一刻还在输出的回答会被切断，已生成的 token 照样计费。连接超时与静默超时也一并
/// 不设（项目维护者的决定）。代价：上游接通后卡死时，这个请求不会自己结束，只能靠客户端取消。
pub fn build_stream_client(
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    Ok(client_builder(proxy, tls_backend)?.build()?)
}

fn client_builder(
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<reqwest::ClientBuilder> {
    let mut builder = Client::builder();

    match tls_backend {
        TlsBackend::Rustls => {
            builder = builder.use_rustls_tls();
        }
        TlsBackend::NativeTls => {
            #[cfg(feature = "native-tls")]
            {
                builder = builder.use_native_tls();
            }
            #[cfg(not(feature = "native-tls"))]
            {
                anyhow::bail!("此构建版本未包含 native-tls 后端，请在配置中改用 rustls");
            }
        }
    }

    if let Some(proxy_config) = proxy {
        let mut proxy = Proxy::all(&proxy_config.url)?;

        // 设置代理认证
        if let (Some(username), Some(password)) = (&proxy_config.username, &proxy_config.password) {
            proxy = proxy.basic_auth(&escape_percent(username), &escape_percent(password));
        }

        builder = builder.proxy(proxy);
        // 代理 URL 可能带 user:pass，必须脱敏后再写日志。
        tracing::debug!(
            "HTTP Client 使用代理: {}",
            crate::security::redact_proxy_url(&proxy_config.url)
        );
    }

    Ok(builder)
}

/// 转义 `%`，让单独配置的代理用户名 / 密码按字面值发给代理。
///
/// reqwest 0.12 的 `basic_auth` 把凭据写进代理 URL 的 userinfo，`url` 不转义 `%`，
/// 连接时 hyper-util 再按 URL 规则解码一次：字面的 `p%40ss` 会以 `p@ss` 发出。
/// 写在代理 URL 里的凭据本来就是 URL 编码形式，不走这里，仍解码一次。
fn escape_percent(value: &str) -> String {
    value.replace('%', "%25")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn test_proxy_config_new() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        assert_eq!(config.url, "http://127.0.0.1:7890");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn test_proxy_config_with_auth() {
        let config = ProxyConfig::new("socks5://127.0.0.1:1080").with_auth("user", "pass");
        assert_eq!(config.url, "socks5://127.0.0.1:1080");
        assert_eq!(config.username, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));
    }

    #[test]
    fn test_build_client_without_proxy() {
        let client = build_client(None, 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_build_client_with_proxy() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        let client = build_client(Some(&config), 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    /// 起一个本地分块响应：先发一块，停 `gap`，再发最后一块。整个过程从不沉默超过 `gap`。
    async fn serve_slow_stream(gap: Duration) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut discard = [0u8; 1024];
            let _ = socket.read(&mut discard).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n",
                )
                .await
                .unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(gap).await;
            socket.write_all(b"2\r\nho\r\n0\r\n\r\n").await.unwrap();
            socket.flush().await.unwrap();
        });
        addr
    }

    /// 回归锁：上游 API 的流客户端不设任何超时，还在输出的回答不能被切断。
    ///
    /// 整体超时包含读响应体，活跃的流续不了期，所以它其实是回答长度的硬上限。
    #[tokio::test]
    async fn stream_client_does_not_cut_an_active_answer() {
        let addr = serve_slow_stream(Duration::from_secs(2)).await;
        let client = build_stream_client(None, TlsBackend::Rustls).unwrap();

        let body = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .expect("没有超时的流客户端不能切断还在输出的回答");
        assert_eq!(body, "hiho");
    }

    /// 对照：短请求客户端仍带整体超时。同一个夹具在 1 秒时限下必须被切断，
    /// 说明上面那条测试确实能检测出超时被误加回流客户端。
    #[tokio::test]
    async fn short_request_client_keeps_its_overall_deadline() {
        let addr = serve_slow_stream(Duration::from_secs(2)).await;
        let client = build_client(None, 1, TlsBackend::Rustls).unwrap();

        let err = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .expect_err("整体超时必须切断这个慢响应");
        assert!(err.is_timeout(), "期望超时错误，实际: {err}");
    }

    const FIXTURE_DEADLINE: Duration = Duration::from_secs(5);

    /// 抓下 CONNECT 请求头，然后拒绝隧道，不产生 TLS 或任何外部流量。
    async fn capture_connect(proxy: impl FnOnce(std::net::SocketAddr) -> ProxyConfig) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(tokio::time::timeout(FIXTURE_DEADLINE, async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut buf = [0; 1024];
                let count = socket.read(&mut buf).await.unwrap();
                assert!(count > 0, "请求头没收完连接就关闭了");
                request.extend_from_slice(&buf[..count]);
            }
            socket
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        }));
        let client = build_client(Some(&proxy(addr)), 5, TlsBackend::Rustls).unwrap();
        client
            .get("https://example.invalid/")
            .send()
            .await
            .expect_err("夹具拒绝隧道");
        server.await.unwrap().expect("代理夹具超时")
    }

    /// 从 Basic Proxy-Authorization 头解出 `user:pass`。
    fn proxy_authorization(request: &str) -> Option<String> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let value = request.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("proxy-authorization")
                .then(|| value.trim())
        })?;
        let decoded = STANDARD.decode(value.strip_prefix("Basic ")?).unwrap();
        Some(String::from_utf8(decoded).unwrap())
    }

    /// 回归锁：单独配置的代理用户名 / 密码里的 `%` 是字面字符。
    ///
    /// reqwest 0.12 的 `basic_auth` 把凭据写进代理 URL 的 userinfo，`url` 不转义 `%`，
    /// 连接时 hyper-util 再按 URL 规则解码一次：`p%40ss` 会以 `p@ss` 发给代理。
    /// 上游 ZyphrZero/kiro.rs aa5398e。
    #[tokio::test]
    async fn separate_proxy_credentials_keep_literal_percent_sequences() {
        let request = capture_connect(|addr| {
            ProxyConfig::new(format!("http://{addr}")).with_auth("u%3Aser", "p%40ss%25")
        })
        .await;
        assert_eq!(
            proxy_authorization(&request).as_deref(),
            Some("u%3Aser:p%40ss%25")
        );
    }

    #[tokio::test]
    async fn separate_proxy_credentials_keep_reserved_characters() {
        let request = capture_connect(|addr| {
            ProxyConfig::new(format!("http://{addr}")).with_auth("user@name", "p@ss:/#? 密码")
        })
        .await;
        assert_eq!(
            proxy_authorization(&request).as_deref(),
            Some("user@name:p@ss:/#? 密码")
        );
    }

    /// 对照：写在代理 URL 里的凭据仍按 URL 规则解码一次（`%2540` → `%40`），不受上面的修复影响。
    #[tokio::test]
    async fn url_embedded_proxy_credentials_are_decoded_once() {
        let request =
            capture_connect(|addr| ProxyConfig::new(format!("http://url-user:p%2540ss@{addr}")))
                .await;
        assert_eq!(
            proxy_authorization(&request).as_deref(),
            Some("url-user:p%40ss")
        );
    }

    #[tokio::test]
    async fn separate_socks5_credentials_keep_literal_percent_sequences() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(tokio::time::timeout(FIXTURE_DEADLINE, async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 2];
            socket.read_exact(&mut greeting).await.unwrap();
            let mut methods = vec![0; greeting[1] as usize];
            socket.read_exact(&mut methods).await.unwrap();
            assert!(methods.contains(&2), "必须提供用户名 / 密码认证");
            socket.write_all(&[5, 2]).await.unwrap();
            let mut header = [0; 2];
            socket.read_exact(&mut header).await.unwrap();
            let mut username = vec![0; header[1] as usize];
            socket.read_exact(&mut username).await.unwrap();
            let mut password = vec![0; socket.read_u8().await.unwrap() as usize];
            socket.read_exact(&mut password).await.unwrap();
            // 拒绝认证，什么都不转发。
            socket.write_all(&[1, 1]).await.unwrap();
            (username, password)
        }));
        let proxy = ProxyConfig::new(format!("socks5h://{addr}")).with_auth("s%40user", "s%3Apass");
        build_client(Some(&proxy), 5, TlsBackend::Rustls)
            .unwrap()
            .get("https://example.invalid/")
            .send()
            .await
            .expect_err("夹具拒绝 SOCKS 认证");
        let (username, password) = server.await.unwrap().expect("SOCKS 夹具超时");
        assert_eq!(username, b"s%40user");
        assert_eq!(password, b"s%3Apass");
    }
}
