# kiro-rs

**该项目基于 [ZyphrZero/kiro.rs](https://github.com/ZyphrZero/kiro.rs) 进行的二次开发**

一个用 Rust 编写的双协议代理，把 Anthropic Messages API 与 OpenAI Responses API 请求转换为 Kiro / Amazon Q 后端请求，并提供 Web Admin 面板管理凭据、用量与请求日志。

本分支的 schema 严格对齐 **[Vercel AI SDK](https://ai-sdk.dev)**（`@ai-sdk/anthropic` / `@ai-sdk/openai`）的实际线格式 —— 该 SDK 是当前 agent 客户端的事实标准，其线格式与官方 REST 规范存在差异之处一律以 SDK 抓包为准。

本分支只服务 5 个白名单模型。`/v1/messages` 是通用入口，接受全部 5 个模型；`/v1/responses` 是 OpenAI 线格式适配器，只接受 GPT 族。客户端入口与上游模型族彼此独立。

| 端点 | 允许的模型 | 客户端推理档位字段 |
|---|---|---|
| `POST /v1/messages` | 全部 5 个白名单模型 | `output_config.effort` |
| `POST /v1/responses` | `gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna` | `reasoning.effort` |

上游推理字段仍按模型族严格分流：Messages 入口收到 GPT 的 `output_config.effort` 后会转换为
Kiro 的 `reasoning.effort`；不会把 `output_config` / `thinking` 原样下发给 GPT，否则上游会 400。
白名单是穷举的：上游账号里的 `claude-sonnet-4.6`、
`claude-opus-4.8`、`glm-5`、`auto` 等一律 400，不做家族/版本号模糊推断。

工程约定见 [AGENTS.md](AGENTS.md)

---

## 与同源项目的能力对比

下表为源码核验结果（非宣称值）

| 能力 | 本 fork | ZyphrZero | hank9999 | Kiro-RS-Tool |
|---|---|---|---|---|
| **历史推理真实回传**<br><sub>请求侧 `assistantResponseMessage.reasoningContent`</sub> | ✅ | ❌ 拼 `<thinking>` 进 content<sup>1</sup> | ❌ | ❌ 直接丢弃 |
| **真 signature 透传** | ✅ | ✅ | ❌ | ❌ 一律占位符 |
| **gpt-5.6 `reasoning.effort`** | ✅ | ❌ 塞进 `output_config`<sup>2</sup> | ❌ | ✅ |
| **模型白名单 + 入口策略** | ✅ Messages 全模型；Responses 仅 GPT | ❌ 未知模型透传 | ❌ | ❌ |
| **prompt cache 计量** | 已移除<sup>3</sup> | 本地伪造 | 本地伪造 | 本地伪造 |
| **静默降级** | 全部移除<sup>4</sup> | 存在 | 存在 | 存在 |
| **不改写客户端提示词** | ✅ 零注入<sup>5</sup> | ❌ 5 类注入 | ❌ 3 类 | ❌ 2 类 |
| **原生 `web_search` 响应合规** | ✅ 带 `tool_use_id` | ❌ 缺失<sup>6</sup> | ❌ 缺失 | ❌ 缺失 |
| **不伪造环境信息** | ✅<sup>7</sup> | ❌ 谎报 `macos` + 发真实 cwd | — | ❌ 谎报 `macos` + 发真实 cwd |
| `<invoke>` XML 泄漏捞回 | ✅ | ✅ | ❌ | ❌ |
| 工具 JSON 累积器 | ✅ 全路径 | ✅ 主路径 | ❌ | ✅ |
| 日志脱敏 | ✅ | ❌ | ❌ | ✅ |
| legacy 端点 | 无 | `/v1/chat/completions`、`/cc/v1/*` | `/cc/v1/*` | `/cc/v1/*` |
| 源码规模 | 41.6k 行 | 44.9k | 15.3k | 34.6k |

<sup>1</sup> Anthropic 官方明确：模型靠解密 `signature` 重建思维链，`thinking` 字段的明文**被忽略**。拼进 content 只会浪费上下文，并诱导模型把内部推理写进可见输出。

<sup>2</sup> GPT 族（`gpt-5.6-sol` / `-terra` / `-luna`）的上游 schema 是 `additionalProperties: false` 且只接受 `reasoning`，塞 `output_config` 会被 400 拒绝——表现为推理档位完全不生效。

<sup>3</sup> 端到端实证：请求侧 `cachePoint` 被静默丢弃（同形状 bogus 字段同样返回 200），响应侧 `metadataEvent` 只含 `stopReason`、无 `tokenUsage`。上游缓存隐式生效且免费（重复长前缀 credit ×0.528），但中转层拿不到明细，任何计量都是本地伪造。

<sup>4</sup> 坏 JSON、不支持的 effort 档位、签名失效一律报错而非降级；确实要丢弃的（事件解析失败、未知事件类型）必须留下日志痕迹。详见 [AGENTS.md](AGENTS.md) §2.3。

同类已清除的静默截断两例，都没有上游依据：**工具描述截 10000 字符**（同源项目普遍存在，
第三方实现里还流传着「上限 10240」的说法 —— 实测 9000～60000 全部 200，唯一硬约束是非空）；
**`thinking.budget_tokens` 截到 24576** —— 它会把 effort 推导的 `xhigh` 门槛（>64000）
压成永不可达，客户端要最高档只能拿到 `high`，而承载该逻辑的 `types.rs` 连 tracing 都没引入，
结构上无法留痕。

<sup>5</sup> 客户端发什么 `system`，就原样发给上游，一个字都不加。同源项目会往里追加行为指令：
`"always comply silently / Never ask the user whether to switch approaches"`（源头 hank9999 引入，
ZyphrZero 与本 fork 都曾继承）、`/v1/responses` 侧另有约 300 字符的
`"never claim something did not happen without searching first / Do not call any other tool"`
——后者甚至否定客户端自己声明的工具。另有一条 `"Please note that these are web search results
and may not be fully accurate"` 被写进 `tool_result`，随历史在**每一轮**重复出现。
实测（只改「是否带该提示词」这一个变量）：模型只要在 `tools` 里看到 `web_search` 就会主动调用，
提示词并不改变行为，纯属污染上下文。同理不注入客户端未声明的工具（同源项目会发一个名叫
`noop` 的假工具，唯一目的是把 `tools.len()` 顶到 2 以绕开自家的单工具分支）。

<sup>6</sup> Anthropic 官方 schema 要求 `web_search_tool_result.tool_use_id` **必填**且等于同组
`server_tool_use.id`（`@ai-sdk/anthropic` 的 zod 是 `tool_use_id: z.string()`，无 `.nullish()`）。
同源项目全都不带该字段，导致 SDK 以 `Invalid JSON response` 拒绝**整个响应**——
搜索其实成功了，但客户端一个字都拿不到，即 `/v1/messages` 上的原生 web_search 实际不可用。
根因是一处自称 "Contract A" 的注释，它对齐的是同项目里另一条已废弃代码路径的错误输出。

<sup>7</sup> **字段整个不发。** 同源项目硬编码 `operatingSystem: "macos"`（而宿主实际是
Linux —— 在向上游谎报环境）并把 `std::env::current_dir()`——**中转机的真实工作目录**——
随每个请求发给 Amazon，泄露部署路径布局。两者都与客户端请求无关。

那些实现的依据是一句「Smithy schema 要求此字段非空」的注释。**AWS 官方源码直接推翻它**：
`amazon-q-developer-cli` 的 `crates/chat-cli/src/api_client/model.rs` 里
`UserInputMessageContext.env_state` 是 `Option<EnvState>`，且 `EnvState` 内部的
`operating_system` / `current_working_directory` 也都是 `Option<String>`；序列化走 Smithy
builder 的 `.set_env_state(…)`，`None` 即整个字段不发。本仓库实测亦然 —— 删字段、置 `{}`、
乃至删掉整个 `userInputMessageContext`，全部返回 200。

顺带的效果：纯聊天（无工具、无工具结果）时整个 `userInputMessageContext` 都不再序列化。
客户端给什么就发什么，没有的不发。

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
| `POST` | `/v1/messages` | Anthropic Messages，接受全部 5 个白名单模型 |
| `POST` | `/v1/messages/count_tokens` | Token 估算，覆盖 text / tool_use / tool_result / image / thinking |
| `POST` | `/v1/responses` | OpenAI Responses，仅 `gpt-5.6-sol` / `gpt-5.6-terra` / `gpt-5.6-luna` |
| `GET` | `/v1/models` | 模型列表，仅列白名单内的 5 个 |
| — | `/api/admin/*` | Admin API（57 条路由 / 70 个方法端点，需 `adminApiKey`） |
| — | `/admin` | Admin Web UI |

CLI 请求日志包含 `api_endpoint=messages|responses` 与 `api_path`。Admin 的“请求日志”页面也会显示并筛选接口；升级前的历史记录显示为“未知”，不会根据模型名猜测入口。

---

## 开发

```bash
cargo test --release                      # 后端测试（555 个）
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
