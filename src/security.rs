use sha2::{Digest, Sha256};

const REDACTED: &str = "[REDACTED]";

pub fn key_fingerprint(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    hex::encode(&digest[..6])
}

pub fn redact_header_value(name: &str, value: &str) -> String {
    if is_sensitive_header(name) {
        REDACTED.to_string()
    } else {
        redact_text(value)
    }
}

pub fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "x-api-key"
            | "x-amz-security-token"
            | "x-aws-ec2-metadata-token"
    )
}

pub fn redact_proxy_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return redact_text(url);
    };
    let Some((userinfo, host)) = rest.rsplit_once('@') else {
        return redact_text(url);
    };
    if userinfo.contains(':') {
        format!("{scheme}://{REDACTED}@{host}")
    } else {
        redact_text(url)
    }
}

pub fn body_log_summary(body: &str) -> String {
    format!("[body redacted, {} bytes]", body.len())
}

pub fn redact_text(input: &str) -> String {
    let mut out = input.to_string();
    for marker in [
        "Bearer ", "bearer ", "sk-", "sk_", "csk_", "ksk_", "AKIA", "ASIA",
    ] {
        out = redact_after_marker(&out, marker);
    }
    out
}

fn redact_after_marker(input: &str, marker: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find(marker) {
        out.push_str(&rest[..pos]);
        out.push_str(marker);
        out.push_str(REDACTED);
        let after = &rest[pos + marker.len()..];
        let end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';' | ')' | ']'))
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_logs_redact_auth_headers_body_proxy_and_prompt() {
        assert_eq!(
            redact_header_value("Authorization", "Bearer sk-secret"),
            "[REDACTED]"
        );
        assert_eq!(
            body_log_summary("prompt").as_str(),
            "[body redacted, 6 bytes]"
        );
        assert_eq!(
            redact_proxy_url("http://user:pass@127.0.0.1:8080"),
            format!("http://{REDACTED}@127.0.0.1:8080")
        );
        assert!(!redact_text("Authorization: Bearer sk-secret").contains("sk-secret"));
    }

}
