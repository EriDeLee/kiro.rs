use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    Rustls,
    NativeTls,
}

impl Default for TlsBackend {
    fn default() -> Self {
        Self::Rustls
    }
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 上一次成功更新前正在运行的版本号，用于在前端展示「回退到 vX.Y.Z」按钮。
    /// 实际回退动作通过 `<exe>.backup` 文件完成，无需访问网络。
    #[serde(default)]
    pub update_previous_version: Option<String>,

    /// GitHub Personal Access Token（可选）。设置后 GitHub Releases 接口会带上
    /// `Authorization: Bearer <token>`，把限流从匿名 60/h 提到认证 5000/h。
    /// 仅需 `public_repo` 读取权限即可。
    #[serde(default)]
    pub github_token: Option<String>,

    /// 上一次成功完成在线更新的时间（RFC3339）。前端用于显示「上次更新于 …」。
    #[serde(default)]
    pub update_last_applied_at: Option<String>,

    /// 是否启用无人值守自动更新。开启后服务会在每天的 `update_auto_apply_time`
    /// 时刻检查 GitHub Releases，发现新版本即自动下载二进制并替换重启。
    #[serde(default)]
    pub update_auto_apply: bool,

    /// 自动更新的每日触发时间（本地时区，`HH:MM` 24 小时制）。
    /// 默认 03:00 凌晨执行，对在线服务影响最小。
    #[serde(default = "default_update_auto_apply_time")]
    pub update_auto_apply_time: String,

    /// 负载均衡模式（"priority" 或 "balanced"）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    /// 账号级 429 风控触发时是否对当前凭据进入冷却并故障转移（默认 true）。
    ///
    /// 关闭后：429 + suspicious activity 仍按普通瞬态错误重试，不切换凭据。
    /// 开启后：识别到 suspicious activity 字符串时，把当前凭据冷却 `account_throttle_cooldown_secs` 秒，
    /// 立即切换到下一个可用凭据。
    #[serde(default = "default_account_throttle_failover")]
    pub account_throttle_failover: bool,

    /// 账号级风控冷却时长（秒，默认 1800 = 30 分钟）。
    #[serde(default = "default_account_throttle_cooldown_secs")]
    pub account_throttle_cooldown_secs: u64,

    /// 是否识别 403 账号封禁文案并立即禁用凭据（默认 true）。
    ///
    /// 开启后：某凭据收到 403 且响应体命中明确封禁文案（同时含 "suspended" 与
    /// "locked your account"）时，立即标记为 `Suspended` 并禁用。这类凭据**不参与
    /// 自愈**，需人工联系客服核实后手动重置，从根上打断持续 403 死循环（issue #51）。
    ///
    /// 只匹配这两个高特异短语同时出现的情形，不影响普通 403（权限/WAF/区域抖动），
    /// 后者仍按既有 `report_failure` 累计路径处理。关闭后：完全回退旧行为。
    #[serde(default = "default_suspended_detection_enabled")]
    pub suspended_detection_enabled: bool,

    /// 是否启用凭据自愈（默认 true）。
    ///
    /// 当前请求的 model/group 作用域没有可用凭据时，只恢复该作用域内因
    /// `TooManyFailures` 被自动禁用且仍满足冷却/上限的凭据。
    #[serde(default = "default_self_heal_enabled")]
    pub self_heal_enabled: bool,

    /// 同一凭据两次自愈之间的最小冷却间隔（秒，默认 300 = 5 分钟）。
    ///
    /// 冷却窗口内即使再次全灭也不触发自愈。这是打断 issue #51「全禁 → 自愈 →
    /// 403 → 再禁」死循环的关键：持续故障时自愈频率被限到每 5 分钟一次，
    /// 而非每个请求都重置刷屏并无效打上游。
    #[serde(default = "default_self_heal_min_interval_secs")]
    pub self_heal_min_interval_secs: u64,

    /// 连续自愈的最大轮数（默认 5，`0` 表示不限）。
    ///
    /// 同一凭据连续自愈达到此值且同一模型期间没有成功调用时，停止自愈并记录
    /// 错误日志提示人工介入。其它凭据、分组或模型的成功不会清零该计数。
    #[serde(default = "default_self_heal_max_consecutive_rounds")]
    pub self_heal_max_consecutive_rounds: u32,

    /// 按凭据缓存上游可用模型列表的 TTL（秒，默认 3600）。
    #[serde(default = "default_model_cache_ttl_secs")]
    pub model_cache_ttl_secs: u64,

    /// 是否把上游推理内容作为 thinking 块返回给客户端（默认 true）。
    ///
    /// 关闭后，`reasoningContentEvent` 的思考文本会并入正文 text 块而不单独成块。
    /// 注意：`<thinking>` XML 标签的提取路径已移除（实测 claude-opus-5 全场景走
    /// 原生 `reasoningContentEvent`），此开关只影响原生推理内容的呈现形式。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 是否启用请求链路追踪（写 traces.db）。默认 true。
    ///
    /// 关闭后：不再写入 trace 记录、不走 TraceSink，但 `GET /api/admin/traces`
    /// 仍可查询历史已存记录。适合隐私敏感或磁盘紧张的场景。
    #[serde(default = "default_trace_enabled")]
    pub trace_enabled: bool,

    /// 请求链路追踪记录保留天数（默认 7）。后台任务每天清理超期记录。
    #[serde(default = "default_trace_retention_days")]
    pub trace_retention_days: u32,

    /// 请求用量日志（usage_log.*.jsonl + 聚合桶）保留天数（默认 31）。
    #[serde(default = "default_usage_log_retention_days")]
    pub usage_log_retention_days: u32,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// 自定义模型映射表。
    ///
    /// 每条把一个客户端模型别名映射到 Kiro 后端模型 ID 并附带元数据。默认空数组
    /// （完全向后兼容）。**当前已忽略**，
    /// 供 `map_model` / `get_context_window_size` / `/v1/models` 查询。
    #[serde(default)]

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "2.3.0".to_string()
}

fn default_system_version() -> String {
    "macos".to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_load_balancing_mode() -> String {
    "priority".to_string()
}

fn default_account_throttle_failover() -> bool {
    true
}

fn default_account_throttle_cooldown_secs() -> u64 {
    30 * 60
}

fn default_suspended_detection_enabled() -> bool {
    true
}

fn default_self_heal_enabled() -> bool {
    true
}

fn default_self_heal_min_interval_secs() -> u64 {
    5 * 60
}

fn default_self_heal_max_consecutive_rounds() -> u32 {
    5
}

fn default_model_cache_ttl_secs() -> u64 {
    60 * 60
}

fn default_update_auto_apply_time() -> String {
    "03:00".to_string()
}

fn default_extract_thinking() -> bool {
    true
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

fn default_trace_enabled() -> bool {
    true
}

fn default_trace_retention_days() -> u32 {
    7
}

fn default_usage_log_retention_days() -> u32 {
    31
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            update_previous_version: None,
            github_token: None,
            update_last_applied_at: None,
            update_auto_apply: false,
            update_auto_apply_time: default_update_auto_apply_time(),
            load_balancing_mode: default_load_balancing_mode(),
            account_throttle_failover: default_account_throttle_failover(),
            account_throttle_cooldown_secs: default_account_throttle_cooldown_secs(),
            suspended_detection_enabled: default_suspended_detection_enabled(),
            self_heal_enabled: default_self_heal_enabled(),
            self_heal_min_interval_secs: default_self_heal_min_interval_secs(),
            self_heal_max_consecutive_rounds: default_self_heal_max_consecutive_rounds(),
            model_cache_ttl_secs: default_model_cache_ttl_secs(),
            extract_thinking: default_extract_thinking(),
            default_endpoint: default_endpoint(),
            trace_enabled: default_trace_enabled(),
            trace_retention_days: default_trace_retention_days(),
            usage_log_retention_days: default_usage_log_retention_days(),
            endpoints: HashMap::new(),
            config_path: None,
        }
    }
}

impl Config {
    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            let mut config = Self::default();
            config.config_path = Some(path.to_path_buf());
            return Ok(config);
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());

        // 用户手工把字符串字段清空（如 `"updateAutoApplyTime": ""`）时，serde 默认值不会
        // 介入；这里把"看起来像空"的关键字段回退到默认值，避免后续业务用到
        // 空字符串导致难以诊断的错误。
        if config.update_auto_apply_time.trim().is_empty() {
            config.update_auto_apply_time = default_update_auto_apply_time();
        }

        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        fs::write(path, content)
            .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn model_cache_ttl_defaults_for_existing_configs() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.model_cache_ttl_secs, 3600);
        assert_eq!(Config::default().model_cache_ttl_secs, 3600);
    }

    #[test]
    fn model_cache_ttl_accepts_explicit_value() {
        let config: Config = serde_json::from_str(r#"{"modelCacheTtlSecs":120}"#).unwrap();
        assert_eq!(config.model_cache_ttl_secs, 120);
    }

    #[test]
    fn self_heal_config_defaults_for_existing_configs() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(config.suspended_detection_enabled);
        assert!(config.self_heal_enabled);
        assert_eq!(config.self_heal_min_interval_secs, 300);
        assert_eq!(config.self_heal_max_consecutive_rounds, 5);

        let default = Config::default();
        assert!(default.suspended_detection_enabled);
        assert!(default.self_heal_enabled);
        assert_eq!(default.self_heal_min_interval_secs, 300);
        assert_eq!(default.self_heal_max_consecutive_rounds, 5);
    }

    #[test]
    fn self_heal_config_accepts_explicit_values() {
        let config: Config = serde_json::from_str(
            r#"{
                "suspendedDetectionEnabled": false,
                "selfHealEnabled": false,
                "selfHealMinIntervalSecs": 60,
                "selfHealMaxConsecutiveRounds": 0
            }"#,
        )
        .unwrap();
        assert!(!config.suspended_detection_enabled);
        assert!(!config.self_heal_enabled);
        assert_eq!(config.self_heal_min_interval_secs, 60);
        assert_eq!(config.self_heal_max_consecutive_rounds, 0);
    }
}
