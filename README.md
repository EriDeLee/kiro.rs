# kiro-rs

**该项目基于 [ZyphrZero/kiro.rs](https://github.com/ZyphrZero/kiro.rs) 进行的二次开发**

一个用 Rust 编写的严格双端点代理，把 Anthropic Messages API 与 OpenAI Responses API 请求转换为 Kiro / Amazon Q 后端请求，并提供 Web Admin 面板管理凭据、用量与请求日志。

本分支的 schema 严格对齐 **[Vercel AI SDK](https://ai-sdk.dev)**（`@ai-sdk/anthropic` / `@ai-sdk/openai`）的实际线格式 —— 该 SDK 是当前 agent 客户端的事实标准，其线格式与官方 REST 规范存在差异之处一律以 SDK 抓包为准。

本分支**只服务两个模型，协议与模型严格一对一绑定**，跨协议请求一律拒绝。

| 端点 | 唯一允许的模型 | 推理档位字段 |
|---|---|---|
| `POST /v1/messages` | `claude-opus-5` | `output_config.effort` |
| `POST /v1/responses` | `gpt-5.6-sol` | `reasoning.effort` |

工程约定见 [AGENTS.md](AGENTS.md)

---

## 与同源项目的能力对比

下表为源码核验结果（非宣称值）

| 能力 | 本 fork | ZyphrZero | hank9999 | Kiro-RS-Tool |
|---|---|---|---|---|
| **历史推理真实回传**<br><sub>请求侧 `assistantResponseMessage.reasoningContent`</sub> | ✅ | ❌ 拼 `<thinking>` 进 content<sup>1</sup> | ❌ | ❌ 直接丢弃 |
| **真 signature 透传** | ✅ | ✅ | ❌ | ❌ 一律占位符 |
| **gpt-5.6 `reasoning.effort`** | ✅ | ❌ 塞进 `output_config`<sup>2</sup> | ❌ | ✅ |
| **模型白名单 + 协议隔离** | ✅ 跨协议 400 | ❌ 未知模型透传 | ❌ | ❌ |
| **prompt cache 计量** | 已移除<sup>3</sup> | 本地伪造 | 本地伪造 | 本地伪造 |
| **静默降级** | 全部移除<sup>4</sup> | 存在 | 存在 | 存在 |
| `<invoke>` XML 泄漏捞回 | ✅ | ✅ | ❌ | ❌ |
| 工具 JSON 累积器 | ✅ 全路径 | ✅ 主路径 | ❌ | ✅ |
| 日志脱敏 | ✅ | ❌ | ❌ | ✅ |
| legacy 端点 | 无 | `/v1/chat/completions`、`/cc/v1/*` | `/cc/v1/*` | `/cc/v1/*` |
| 源码规模 | 42.4k 行 | 44.9k | 15.3k | 34.6k |

<sup>1</sup> Anthropic 官方明确：模型靠解密 `signature` 重建思维链，`thinking` 字段的明文**被忽略**。拼进 content 只会浪费上下文，并诱导模型把内部推理写进可见输出。

<sup>2</sup> `gpt-5.6-sol` 的上游 schema 是 `additionalProperties: false` 且只接受 `reasoning`，塞 `output_config` 会被 400 拒绝——表现为推理档位完全不生效。

<sup>3</sup> 端到端实证：请求侧 `cachePoint` 被静默丢弃（同形状 bogus 字段同样返回 200），响应侧 `metadataEvent` 只含 `stopReason`、无 `tokenUsage`。上游缓存隐式生效且免费（重复长前缀 credit ×0.528），但中转层拿不到明细，任何计量都是本地伪造。

<sup>4</sup> 坏 JSON、不支持的 effort 档位、签名失效一律报错而非降级；确实要丢弃的（事件解析失败、未知事件类型）必须留下日志痕迹。详见 [AGENTS.md](AGENTS.md) §2.3。

---

## 快速开始

```bash
# 前端（仓库以 bun.lock 为准；无 bun 时用 npm/pnpm 也可，锁文件仅 bun 权威）
cd admin-ui && bun install --frozen-lockfile && bun run build && cd ..

# 后端
cargo build --release
./target/release/kiro-rs -c data/config.json --credentials data/credentials.json
```

低内存机器（≈1G）需要 `cargo build --release -j 1`，且不要与 `cargo test` 并发。

---

## 配置

最小可用配置：

```json
{
  "host": "127.0.0.1",
  "port": 8990,
  "apiKey": "sk-your-key",
  "adminApiKey": "your-admin-key"
}
```

| 字段 | 默认 | 说明 |
|---|---|---|
| `host` / `port` | `127.0.0.1` / `8990` | 监听地址 |
| `apiKey` | 自动生成 | 客户端鉴权 Key |
| `adminApiKey` | 自动生成 | Admin 面板密钥；留空则不启动面板 |
| `loadBalancingMode` | `priority` | `priority`（固定优先级）或 `balanced`（均衡） |
| `traceEnabled` | `false` | 请求链路记录，供 Admin 面板查询 |
| `proxyUrl` | 无 | 全局 HTTP 代理，日志中自动脱敏 |

多凭据故障处理（上游 v0.7.4 引入，用于打断 403 死循环）：

| 字段 | 默认 | 说明 |
|---|---|---|
| `accountThrottleFailover` | `true` | 账号级 429（suspicious activity）时冷却并切换凭据 |
| `accountThrottleCooldownSecs` | `1800` | 账号级风控冷却秒数 |
| `suspendedDetectionEnabled` | `true` | 识别 403 封禁文案（`suspended` + `locked your account`）后立即禁用该凭据且不参与自愈 |
| `selfHealEnabled` | `true` | 全部凭据被自动禁用时，重置失败计数并重新启用 |
| `selfHealMinIntervalSecs` | `300` | 两次自愈的最小间隔，打断持续 403 死循环的关键 |
| `selfHealMaxConsecutiveRounds` | `5` | 连续自愈且无成功的最大轮数（`0` = 不限），超限即停并提示人工介入 |

凭据放在 `credentials.json`，支持 Builder ID、Social、Enterprise/IdC、企业 SSO（Entra ID）、Kiro API Key 五类，token 过期自动刷新并回写。

---


## API 路由

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/v1/messages` | Anthropic Messages，仅 `claude-opus-5` |
| `POST` | `/v1/messages/count_tokens` | Token 估算，覆盖 text / tool_use / tool_result / image / thinking |
| `POST` | `/v1/responses` | OpenAI Responses，仅 `gpt-5.6-sol` |
| `GET` | `/v1/models` | 模型列表，仅列白名单内的两个 |
| — | `/api/admin/*` | Admin API（43 个端点，需 `adminApiKey`） |
| — | `/admin` | Admin Web UI |

---

## 开发

```bash
cargo test --release                      # 后端测试（542 个）
cargo build --release                     # release 构建
RUST_LOG=kiro_rs=debug ./kiro-rs ...      # debug 日志
cd admin-ui && bun run build              # 前端
```

debug 级别会输出三条诊断日志，用于验证"字段是否真的下发"——都只打印结构与长度，不含对话内容：

```
additionalModelRequestFields = {"output_config":{"effort":"xhigh"},...}
history reasoningContent x1: [text=44,sig=308,redacted=0]
reasoningContentEvent: text=12 signature=0 redacted=0
```

---

## License

MIT

## 💬 社区支持

欢迎到 [linux.do](https://linux.do/) 交流、分享和反馈。

## 🙏 致谢

本项目的实现离不开社区项目和反馈的帮助：

- [hank9999/kiro.rs](https://github.com/hank9999/kiro.rs) — 项目源头
- [ZyphrZero/kiro.rs](https://github.com/ZyphrZero/kiro.rs) — 本分支的上游
- [GreyGunG/Kiro-RS-Tool](https://github.com/GreyGunG/Kiro-RS-Tool) — 日志脱敏与 thinking 边界处理的参考实现
- [kiro2api](https://github.com/caidaoli/kiro2api)
- [proxycast](https://github.com/aiclientproxy/proxycast)
- [Kiro-account-manager](https://github.com/chaogei/Kiro-account-manager) — `reasoningContent` 历史回传与 `THINKING_SIGNATURE_INVALID` 处理的关键线索

感谢所有 issue、PR、测试和部署反馈的贡献者。
