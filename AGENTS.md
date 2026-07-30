# AGENTS.md

本文件是本仓库的工程约定。它约束**所有**在此仓库工作的人与 AI agent。

本项目**只为一条链路服务**：

```
Kiro / Amazon Q 后端  ←→  本项目  ←→  @ai-sdk/{anthropic,openai}  ←→  任意消费该 SDK 的客户端
```

对接目标是 **Vercel AI SDK 的实际线格式**——它是当前 agent 客户端的事实标准。
凡 SDK 线格式与官方 REST 规范不一致，**以 SDK 抓包为准**（§4.2 有实例）。

不针对任何特定 harness 做适配。任何不服务于这条链路的代码都是负债，不论它是否"无害"。

---

## 一、项目目标（硬边界）

只服务 5 个模型，协议与模型组**严格绑定**（一协议一组模型）：

| 端点 | 允许的模型 | 推理档位字段 |
|---|---|---|
| `POST /v1/messages` | `claude-opus-5`、`claude-sonnet-5` | `output_config.effort` |
| `POST /v1/responses` | `gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna` | `reasoning.effort` |

同族内推理字段路径与请求体形状完全一致，扩充模型只是多几个 id，对本项目无新增成本；
跨族则根本不同。字段路径、上下文窗口、effort 枚举一律按**族**分流
（`allowlist::protocol_for_model`），禁止再逐个模型 `eq_ignore_ascii_case` —— 那种写法
在加模型时必然漏改，漏改的后果是静默走错字段路径。

配套端点：`GET /v1/models`、`POST /v1/messages/count_tokens`。**路由总数 4 条**（模型数变化不影响路由数），新增端点需要明确理由。

绝对禁止：

- 跨协议请求（用 `/v1/messages` 请求 gpt、或用 `/v1/responses` 请求 claude）→ 一律 400
- 白名单外的任何模型 → 一律 400
- 为兼容而生的第二条路径（legacy 端点、模糊模型匹配、自定义模型表）

Admin 面板（`/api/admin` + `/admin` UI，43 个端点）保留。四条认证路径（`idc` / `social` / `external_idp` / `api_key`）保留——Admin 的 9 个 OAuth 登录端点依赖它们。

**工具 schema 原样透传，不做内置工具名映射。** 客户端声明什么工具名与 `input_schema`，
就原样发给上游；工具调用的 input 一个键都不改写。仅保留两项与客户端无关的必要处理：
超长工具名缩短（上游限制 63 字符，入站按反向表还原）与按小写名去重。

理由：曾有过「工具兼容模式」按客户端类型枚举内置工具名并改写入参（claude-code 一套、
open-code 第二套）。那个机制的前提假设 —— 客户端内置工具集可枚举 —— 是错的：加第二个
客户端就已证伪它，再来一个还得加第三套。与其做枚举式适配，不如把理解工具语义的成本
转嫁给足够强的上游模型。

---

## 二、核心工程哲学

### 2.1 不考虑向后兼容，永远向最新标准看齐

宁可重构全部代码，也要紧跟官方基线。删除 legacy 路径不需要论证"是否还有人用"，只需确认"最新标准不需要它"。

### 2.2 无害但没用 = 故障代码

判据不是"会不会出错"，而是"对上面那条链路是否有用"。没用的一律清除，包括：

- 未被 `mod` 声明的孤岛文件（编译器看不见，但占据仓库）
- 无人读取的配置字段（会让人误以为配置生效）
- 零调用的函数、空壳结构体
- 未使用的依赖、构建副产物、零引用的工具脚本

**保留"以防万一"的代码是被禁止的。**

### 2.3 绝不静默降级 —— 宁可失败也不要假装成功

静默降级的危害是调用方拿到 200，却不知道请求已被改写或数据已被丢弃。三条铁律：

1. **坏数据一律上抛**。上游返回非法 JSON、半截 JSON、签名失效 → 报错。绝不降级成 `{}` 或空对象——客户端会拿着空参数真的去执行工具（改文件、跑命令）。
2. **不改写客户端意图**。不受支持的 effort 档位 → 400 并列出支持档位，**不回落 `high`**，也不拉到 `max`。静默改档位等于篡改推理强度。
3. **确实要丢弃时必须留下痕迹**。事件解析失败、content block 解析失败、未知事件类型 → 至少 `warn!`/`error!` 记录，说明丢了什么。

已知的例外（有意保留，各有理由）：

| 位置 | 行为 | 理由 |
|---|---|---|
| 孤儿 `tool_use`/`tool_result` 清理 | 跳过并 warn | 修复上游会 400 的历史不一致，属修复而非降级 |
| `image_resize` 环境变量解析失败 | 用默认值 | 配置写错就崩服务更糟 |
| assistant prefill（末尾 assistant 消息） | 转成末尾 user 指令 + warn | Anthropic 协议的合法形态，客户端用它约束下一轮输出；Kiro 上游无预填槽位，报错会打死会话，转换不丢信息 |
| 占位签名的历史 thinking 块 | 入站剔除 | 见 §4.2 |
| `save_stats_debounced` 的 30 秒去抖 | 窗口内只标脏不落盘 | 高频写盘不可取；但**必须**有 graceful shutdown 调 `flush_stats()` 兜住，否则重启就丢——`Drop` 在生产中不执行（信号退出 + 多处 `process::exit` + `Arc` 被 app state 持有） |

### 2.4 代码不许说谎

注释、文档、字段名都必须与实际行为一致。**文档说谎与代码说谎同等有害。**

- 注释里的断言性陈述（"上游不会下发 X"）必须有实测支撑，否则删掉
- README 宣称的端点必须与 `router.rs` 逐字一致
- 已失效的配置字段要么删除，要么显式标注"已忽略"

本仓库已有三条注释被实测推翻（见 §5），这类错误陈述会让后来者基于假前提做决策。

### 2.5 不改写客户端请求的内容

**客户端发什么，就发什么给上游。** 不追加提示词、不注入客户端未声明的工具、不伪造对话轮次、
不改写入参键名、不截断文本。这条比 §2.3（不静默降级）更严格：降级至少是在处理异常，
而改写请求是在**替客户端做它没要求的决定**，且客户端完全无从察觉。

2026-07-29 一次穷尽审查在本仓库（继承自 hank9999 → ZyphrZero）挖出 11 处，逐项实测后
删掉 9 处。典型形态与它们当初的"理由"：

| 形态 | 实例 | 实测结论 |
|---|---|---|
| 往 system 追加行为指令 | `"always comply silently / Never ask the user whether to switch approaches / without commentary"` | 方向与本项目原则正相反；客户端不知情；且它假设的 `Write`/`Edit` 工具名在工具映射删除后已不存在 |
| 注入约 300 字符的搜索督促 | `"never claim something did not happen without searching first / Do not call any other tool"` | 只改「是否带该文本」一个变量实测：模型看到 `web_search` 工具就会主动调用，**提示词不改变行为**；而 `Do not call any other tool` 直接否定客户端自己声明的工具 |
| 注入客户端未声明的工具 | 名为 `noop` 的假工具 | 唯一目的是把 `tools.len()` 顶到 2 以绕开自家的单工具分支 —— **为操纵本进程的 if 判断而向上游发假数据** |
| 伪造对话轮次 | system 后跟 `assistant("I will follow these instructions.")`、末尾孤立 user 后补 `assistant("OK")` | 实测上游**不要求** user/assistant 交替：相邻两条 user、孤立 user、孤立 assistant 全部 200 |
| 结果里掺入自己的话 | 搜索摘要尾部 `"Please note that these are web search results and may not be fully accurate"` | 它进 `tool_result` → `payload.messages`，**每一轮**都在历史里重复；中文会话里插英文句子 |
| 静默截断 | `budget_tokens.min(24576)`、工具描述 `.nth(10000)` | 前者把 `effort` 推导的 `xhigh` 门槛（>64000）锁成永不可达；后者实测上游 60000 字符照样 200 |
| 伪造环境信息 | `envState{operatingSystem:"macos", cwd:std::env::current_dir()}` | `macos` 是硬编码而宿主是 Linux；cwd 把**中转机真实路径**发给上游。实测该字段整个可省 |

判据：**如果这段代码的效果无法从客户端的请求推导出来，它就是改写。** 唯一例外是上游协议
硬性要求（要有实测支撑，写进 §4），且必须在注释里写明是哪条约束。

### 2.6 告警噪音会让告警失效

已知无害的事件不能每次告警，否则日志常驻噪音，真正的协议变更会被埋没。正确流程：

1. 加告警 → 发现盲区
2. 实测确认无害 → **显式识别**，让告警只对真正未知的情况触发

`metadataEvent` 就是这样处理的：先靠 warn 抓到它一直在下发，实测确认只含 `stopReason` 后，加 `EventType::Metadata` 显式识别，告警数回到 0。

---

## 三、验证要求

### 3.1 返回 200 不等于字段生效

这是本仓库最重要的验证教训。曾经发生过：515 个单测全绿 + 真实请求返回 200 + 两个 effort 档位都能正常回答，但 `additionalModelRequestFields` 实际是 `<absent>` —— 推理档位被整条丢弃。

**涉及请求体字段的改动，必须用诊断日志确认实际下发内容**，不能只看状态码或响应内容。仓库里保留了两条诊断日志用于此目的（都不含对话内容）：

```rust
// provider.rs
"additionalModelRequestFields = {}"          // 推理档位实际下发值
"history reasoningContent x{}: [{}]"         // 历史推理回传的长度骨架
// stream.rs
"reasoningContentEvent: text={} signature={} redacted={}"
// events/base.rs
"未知事件骨架: {}"                            // 键名+数值，字符串只报长度
// 说明：骨架诊断只输出结构与长度，字符串一律只报 len —— 事件可能携带对话文本
```

这些日志请勿删除——它们是排查此类问题的唯一直接证据。

### 3.2 单测绿 ≠ 功能正确

单测锁的是内部自洽。跨越进程边界的行为（上游是否接受某字段、客户端 SDK 是否解析某形状）**只能靠实测**。

改动后的标准流程：

```bash
export CARGO_TARGET_DIR=/path/to/target
export CARGO_BUILD_JOBS=1 CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_LTO=false
export RUSTFLAGS="-C debuginfo=0"
nice -n 10 cargo test --release -j 1 -- --test-threads=1
nice -n 10 cargo build --release -j 1
# 再用真实凭据起一个临时实例（非生产端口）做端到端实测
```

低内存机器（≈1G）必须 `-j 1`，且**不要并发跑 test 与 build**，否则 OOM。

### 3.3 过时测试改写为回归锁，不要简单删除

当改动使某测试失败时，判断它属于哪类：

- 断言**旧行为**的 → 改写成断言**新行为**的回归锁，这样以后有人想把旧行为加回来会被挡住
- 测试对象**已不存在**的 → 删除

例：`test_output_config_unknown_effort_falls_back_to_high` 改成了 `unsupported_effort_is_rejected_not_downgraded`。

### 3.4 弃用会误报的判据

自动化判据必须先自检。曾经用"生产区零调用、仅测试在用"来找死代码，它把 `convert_request`（核心转换函数）报成了死代码。**误报的判据比没有判据更危险**——会导致斩掉活代码。

### 3.5 批量替换必须先列出全部匹配点，逐个确认后再改

正则区分不了**定义与调用**、**代码与注释**、**生产与测试**。已发生的实际事故：

- 批量改 `record_usage` 调用时，把函数**定义**的参数列表截断成 2 个
- 批量 grep `chat/completions` 时，把**注释**里的字样当成活路由
- 删结构体字段时，留下孤立的 `#[serde(default)]`（编译器只报 "duplicate serde attribute"，不指出根因）

流程：先 `grep -n` 打印全部匹配点 → 肉眼确认形态一致、无变体 → 再替换 → 编译验证。

### 3.6 删字段/删列必须逐项核对下游

删一个字段会连带打断一串东西，编译器**不会**全部告诉你（SQL 是字符串）。删除后必须逐项核对：

- `SELECT` 的列顺序 ↔ `row.get(N)` 的索引（删列会让后续索引全部错位）
- `INSERT` 的列名数 ↔ 占位符数 ↔ `params!` 数（三者必须相等）
- 数组/元组的长度声明（如 `[(&str, &str); 7]`）
- 结构体字段删除后残留的孤立属性与文档注释
- 上游生产者（本仓库真实发生过：admin 侧删干净了，`anthropic/handlers.rs` 作为唯一生产者一行没改）

### 3.7 「命令返回」不等于「命令成功」

必须看输出内容，而不是看命令是否返回。已发生的事故：

- 后台任务输出 `bun: command not found`，被当成"前端构建成功"上报。前端类型检查
  应用 `node_modules/.bin/tsc -b`（本机可能没有 bun）
- 验证代理生效的脚本用错 HTTP 方法（`POST` vs 实际 `PUT`）与字段名（`url` vs
  `proxyUrl`），Admin 返回 **405**，而随后的数据面请求返回 200 —— 若只看那个 200，
  会得出"修复生效"的**反向错误**结论。实际上代理从未被设置

**验证脚本自身出错比被测代码出错更危险**，因为它给出的是虚假的通过信号。所以：

1. 每一步都要断言中间态（设置类操作先确认返回 2xx，再 `GET` 回读确认真的写进去了）
2. 端点的方法与字段名从 router / 请求结构体里读，不要凭印象写

### 3.8 验证「静默失效」类 BUG 必须用反向判据

这类 BUG 的特征是**修复前后都返回 200**，状态码和单测都无法区分。有效手法是
故意构造一个"若修复生效就必然失败"的条件：

| BUG | 反向判据 | 修复前 | 修复后 |
|---|---|---|---|
| 全局代理只改控制面、数据面仍用启动快照 | Admin 设一个**不可达**代理（`socks5://127.0.0.1:59999`），再打数据面 | 200（用旧配置） | **502**（真用了新代理） |
| `Drop` 承诺落盘但永不执行 | 请求若干次 → 发 SIGTERM → 比对落盘文件里的计数与日志里的"累计 N 次" | 文件无更新 | 计数吻合、写入时刻与信号同秒 |
| 推理档位被静默丢弃 | 看诊断日志里 `additionalModelRequestFields` 的实际内容 | `<absent>` | `{"output_config":{"effort":...}}` |

"改完还是 200" 不是证据。要么找到一个能翻转的可观测量，要么读日志里的实际下发内容。

### 3.9 进程操作要用 `/proc/<pid>/exe` 精确锁定目标

`pgrep -f <pattern>` 会匹配到**包裹脚本的 bash 进程**（其 cmdline 含被搜索的字符串）。
已发生的事故：SIGTERM 发给了 bash 而非真正的服务进程，日志零信号记录，一度误判
graceful shutdown 失效。

正确做法：遍历候选 PID，用 `readlink /proc/<pid>/exe` 确认指向目标二进制。这条在
本机尤其重要 —— 生产实例（`/opt/kiro-rs-ktool/kiro-rs`）与测试实例
（`ktool-target2/release/kiro-rs`）同名，靠名字区分会误杀生产。

---

### 3.10 端到端实测基线（改动后按此回归）

单测覆盖内部自洽，跨进程行为只能靠真实凭据实测。本仓库的完整矩阵（2026-07 跑通
59 项）：

| 批 | 覆盖 | 关键判据 |
|---|---|---|
| 1 | 白名单与协议隔离 | 白名单内 5 个模型各在对应端点 200（Claude 组 2 个走 `/v1/messages`、GPT 组 3 个走 `/v1/responses`）；白名单外模型全 400（含相邻版本 `claude-sonnet-4.6`、`claude-opus-4.8`）；协议交叉双向 400（Claude 组任一走 `/v1/responses`、GPT 组任一走 `/v1/messages`）；`-thinking`/`-latest`/日期戳别名 200 |
| 2 | 已移除端点 absent | `/cc/v1/*`、`/v1/chat/completions` 全 404 |
| 3 | 推理档位全矩阵 | Claude 组五档全 200、`none`/非法值 400；GPT 组六档全 200、非法 400 |
| 4 | thinking 类型 | `enabled`→归一 `adaptive`；`disabled`+`effort=max`→整个字段 `<absent>`（Claude 组两个模型都要验） |
| 5 | signature 端到端 | 真签名回传后模型能基于历史推理续算；占位签名被剔除不打死会话 |
| 6 | 工具 schema 透传 | 工具名与 input_schema 原样下发；仅超长名缩短 + 入站还原 |
| 7 | 静默降级已移除 | prefill 转换 200 + warn；坏 JSON / 空 messages 400 |
| 8 | 安全与可观测 | 日志无明文 Key / Bearer；usage 无 cache 字段；未知事件告警 0 |
| 9 | graceful shutdown | SIGTERM → 落盘计数与日志吻合 |
| 10 | 全局代理现取 | 见 §3.8 反向判据 |
| 11 | 多模型白名单 | 5 个模型 × 正确端点 200 / 错误端点 400；同族 effort 枚举一致；白名单外 11 个模型两端点全 400 |

配置项类改动（如 `extractThinking`、`traceEnabled`）需要**单独起实例**，它们只在启动时读取。
测试客户端行为时不必安装该客户端 —— 从其源码读出真实的工具 id / 参数形状后手工
构造请求即可。

### 3.11 Rebase 上游后必须做双向验证

只验一个方向会漏。两个方向都要查：

1. **我方改造是否被回退** —— 逐项确认核心改动仍在（路由数、白名单、签名回传、
   工具 schema 透传…）。git 能自动合并的代码，语义上可能已被上游覆盖
2. **上游功能是否完整吸收** —— 确认新配置项、新逻辑真的存在。最有力的证据是
   **测试数变化**（本轮 518 → 542，说明上游 24 个新测试全部纳入并通过）
3. **已删符号是否复活** —— 上游改动可能把删掉的字段/函数带回来

三项都过之后再跑一遍端到端实测（§3.10），因为上游改动可能落在我方改过的同一文件里。

### 3.12 写断言前先读实现，不要让证据服从预期

本轮连续犯了四次同类错误，共同点是**先形成预期，再让证据服从预期**：

| 预期 | 实际 | 后果 |
|---|---|---|
| `pgrep -f 'cargo\|rustc'` 只匹配编译进程 | 匹配到含该模式的自身 grep 命令 | 判断"编译进程在重生"，连杀五六轮，实际根本没有编译在跑 |
| `"    }\n"` 能定位函数结尾 | 匹配到内层 `match` 的闭合括号 | 删函数留下 4 行残骸，`unexpected closing delimiter` |
| 优先级高的凭据就该被选中 | priority 模式下 `current_id` 是**粘性**的 | 写出错误断言，测试失败 |
| 同族模型 effort 枚举一致 | 恰好成立 | 但当时只是推断，未实测就写进了代码分支 |

前两条是 §3.9 / §3.5 已记录的坑，写下之后自己又踩。规律：**改动前先读实现确认语义，
断言失败时先怀疑自己的假设而不是代码**。第三条尤其典型 —— 测试挂了，挂的是我新加的
断言，而被测代码的行为是设计意图。

同理，把「推断」写进代码分支前要标注它未经实测，或者干脆先实测。第四条虽然结果正确，
但当时若不成立，按族分流就是错的。

---

## 四、已固化的实测结论

以下结论均有实测证据，改动相关代码前请先读这一节，不要重新猜测。

### 4.1 上游 Kiro 协议

**推理档位 schema**（2026-07-26 实测 `ListAvailableModels`，2026-07-29 复测补齐 sonnet-5 / terra / luna）：

推理字段路径与 effort 枚举**按协议族一致**，同族内各模型无差异；只有窗口大小逐模型不同。
这一条不是从 schema 推断的，而是 2026-07-29 端到端实测确认：GPT 族三个模型 `effort=none`
全部 200、Claude 族两个模型 `effort=none` 全部 400、五个模型 `effort=bogus` 全部 400。

- **Claude 族**（`claude-opus-5`、`claude-sonnet-5`）：`output_config.effort ∈ {low,medium,high,xhigh,max}`（default `high`，**无 `none`**）；`thinking.type ∈ {adaptive, disabled}`（**无 `enabled`**，发了会 400）；`thinking.display ∈ {summarized, omitted}`（上游默认 `omitted`，会吞掉思考文本）
- **GPT 族**（`gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna`）：**只有** `reasoning`，`additionalProperties: false`。`reasoning.effort ∈ {none,low,medium,high,xhigh,max}`（default `high`）；`reasoning.mode ∈ {standard, pro}`。下发 `output_config` / `max_tokens` / `thinking` 会 400 `REQUEST_BODY_INVALID`
- `effort=xhigh|max` 与 `thinking.type=disabled` 冲突时**整体不下发 thinking 字段**，绝不反向改写 effort

**窗口大小**（`tokenLimits`，2026-07-29 实测）：

**上游并不要求的东西**（2026-07-29 直连 `/generateAssistantResponse` 变异测试，
以代理的真实成功请求为模板，每次只改一处）：

| 变异 | 结果 | 推论 |
|---|---|---|
| 删掉 `envState` 字段 | 200 | 「envState 是 CLI endpoint 必填字段」对 `ide` endpoint **不成立** |
| `envState = {}` | 200 | 同上 |
| 删掉整个 `userInputMessageContext` | 200 | 同上 |
| `envState` 两字段都是空串 | **400** `REQUEST_BODY_INVALID` | 唯一的硬约束：字段若存在则值不能为空串（Smithy `@length(min:1)`） |
| history 里相邻两条 `userInputMessage` | 200 | **不要求 user/assistant 交替** |
| history 只有一条 user（末尾孤立） | 200 | 不需要补 `assistant("OK")` |
| history 只有一条 assistant | 200 | 同上 |
| `assistantResponseMessage.content` 为空串 | 200 | — |
| 工具 `description` 9000 / 10000 / 10001 / 15000 / 30000 / 60000 字符 | 全部 200 | **没有 10000 字符上限**，那个截断无依据 |
| 工具 `description` 为空串 | **400** `Invalid tool use format` | 非空是硬要求（`ToolSpecification.description` 非 Option） |

排查这类问题时的两个坑：上游端点是 **`/generateAssistantResponse`**（不是
`SendMessageStreaming`），`profileArn` 注入在 **body 根对象**（不是 query）；
且 `provider.rs` 的诊断日志打的是 **endpoint 注入前**的中间产物，照抄会 400。

**被生态广泛照抄的两个错误假设**（2026-07-30 以 AWS 官方源码 + 实测双向推翻）：

| 流传的说法 | 真相 | 依据 |
|---|---|---|
| `envState` 是必填字段，必须带 `operatingSystem` + `currentWorkingDirectory` | **全部可选** | AWS 官方 `amazon-q-developer-cli`（`crates/chat-cli/src/api_client/model.rs`）：`UserInputMessageContext.env_state: Option<EnvState>`，且 `EnvState` 内部 `operating_system: Option<String>`、`current_working_directory: Option<String>` 也都是 `Option`；序列化走 Smithy builder 的 `.set_env_state(…)`，`None` 即整个字段不发。本仓库实测亦然（删字段 / `{}` / 删整个 context 全部 200） |
| 工具 `description` 有 10240 字符上限，超出须截断 | **无此上限** | 实测 9000 / 10000 / 10001 / 15000 / 30000 / 60000 全部 200。唯一硬约束是非空（Smithy `@length(min:1)`） |

第一条在本仓库的来源是 `cfd132e` 的提交信息「CLI endpoint 的 Smithy schema 要求此字段
非空」。那次提交同时修了两件事 —— 工具空描述触发的 `ValidationException` 与新增
`envState`，作者把前者的报错**误归因**到了后者。同一提交里「工具描述非空」那条是真的
（已实测复现）。第二条出现在第三方项目 `mucsbr/amq2api` 的注释里，同样无实测支撑。

教训：同源项目之间会互相照抄未经验证的协议假设，而错误假设一旦写进注释就会被当成
既定事实。**判断上游协议约束时，优先查 AWS 官方客户端源码（它就是 Smithy 生成的
权威定义），其次自己发变异请求实测；同源 fork 的注释不能作为依据。**

**响应侧必填字段**：`web_search_tool_result.tool_use_id` 必须存在且等于同组
`server_tool_use.id`。`@ai-sdk/anthropic` 的 zod schema 是 `tool_use_id: z.string()`
（无 `.nullish()`），缺了它 SDK 以 `Invalid JSON response` 拒绝**整个响应** ——
搜索成功但客户端一个字都拿不到。同源项目全都缺这个字段。

| 模型 | maxInputTokens | maxOutputTokens |
|---|---|---|
| `claude-opus-5` | 1,000,000 | 128,000 |
| `claude-sonnet-5` | 1,000,000 | **64,000** |
| `gpt-5.6-sol` | 272,000 | 128,000 |
| `gpt-5.6-terra` | 272,000 | 128,000 |
| `gpt-5.6-luna` | 272,000 | 128,000 |

`get_context_window_size` 只取 `maxInputTokens`（Claude 族 1M / GPT 族 272k），它参与 contextUsage 百分比 → 绝对 token 换算，取错会让上报的 input_tokens 系统性偏差。`maxOutputTokens` 由 `/v1/models` 从上游原样透出 —— 注意 `claude-sonnet-5` 是 64k，与同族 opus-5 的 128k 不同，不可按族推断。

**prompt cache 不可用**（端到端实证）：

- 请求侧 `cachePoint` 被**静默丢弃** —— 同形状的 bogus 字段同样返回 200，发 9 个断点（声明上限 4）也返回 200。对照实验：`modelId` 传 int 确实 400，说明校验是有的，只是不校验 cachePoint
- 响应侧 `metadataEvent` 只含 `{stopReason}`，**无 `tokenUsage`、无 cache 明细**
- 缓存在上游隐式生效且免费：重复长前缀第二次起 credit 稳定 ×0.528，唯一前缀对照组无下降

因此响应 usage **不输出** `cache_creation_input_tokens` / `cache_read_input_tokens`。任何"缓存计量"实现都是本地伪造，禁止重新引入。

**历史推理可以真实回传**（AWS 官方 Smithy 模型 + 实测）：

字段是 `conversationState.history[].assistantResponseMessage.reasoningContent`，Smithy union：

```json
{ "reasoningContent": { "reasoningText": { "text": "...", "signature": "..." } } }
{ "reasoningContent": { "redactedContent": "<base64>" } }
```

`reasoningText.text` 必填，`signature` 可选但**必须逐字节原样**。两个成员互斥，每条历史消息只能带一个。

上游确实下发真签名：`reasoningContentEvent.signature` 长度 308~10920 的 base64，流式时在思考文本**之后**单独下发（406 个 reasoning 事件的统计：404 个纯 text 分片 + 2 个纯 signature）。

上游确实验签：签名失效返回 `THINKING_SIGNATURE_INVALID`。本仓库据此**直接报错、不剥离重试** —— 剥离会让请求看似成功而实际丢掉整段推理。

**`claude-opus-5` 100% 走原生 `reasoningContentEvent`**：4 组场景（长推理 max / 带工具调用 / 非流式 / 无 display）共 406 个事件，`<thinking>` XML 标签模式 **0 次触发**。故 XML 提取路径已从非流式删除。

**Builder ID 账号**：不支持 `ListAvailableProfiles`（固定 403 `AWS Builder ID is not supported for this operation.`），必须用占位 profileArn 并跳过探测；MCP（WebSearch）路径要用 `streaming_profile_arn()` 而非 `effective_profile_arn()`，否则 400 `profileArn is required`。

### 4.2 客户端 SDK 线格式

结论来自 npm 包源码 + 真 SDK 打 mock server 抓包。取样版本 `@ai-sdk/anthropic@3.0.82`、`@ai-sdk/openai@3.0.84`（`effort` 位置额外回溯了 2.0.91~4.0.23 共 11 个版本）。

**顶层 `effort` 不存在。** 回溯 11 个 SDK 版本，`effort` **只出现在 `output_config.effort`**。

易混淆点：客户端代码里写的 `{thinking, effort}` 是 **SDK 的 `providerOptions`（camelCase 中间层）**，不是 HTTP body —— SDK 会把它降级到 `output_config.effort`。读客户端源码时必须区分这两层。

`MessagesRequest::effort` 字段带 `#[serde(skip_serializing)]`，是 `/v1/responses` 的**内部通路**（那条路径 `thinking`/`output_config` 恒为 None），不可从 HTTP 注入。

**历史 thinking 块会回传，字段名精确是 `signature`**，位于 `content[].type=="thinking"`。`redacted_thinking` 用 **`data`** 字段（不是 signature）。

**SDK 对无 signature 的 reasoning 块整块丢弃**（warning `unsupported reasoning metadata`）。所以响应里 thinking 块必须带非空 signature，否则用户看不到思考。

由此产生一个双向策略：

| 方向 | 行为 | 原因 |
|---|---|---|
| 出站 | 有真签名则原样发；无真签名填占位串 | 满足客户端"signature 非空"的本地校验，用户每轮都看得到思考 |
| 入站 | 真签名回传上游；**占位串识别后剔除** | 占位串回传必然 `THINKING_SIGNATURE_INVALID`，会打死会话 |

**响应 usage 移除 cache 字段是安全的**：SDK schema 两字段均 `.nullish()` + `?? 0` 兜底。实测下游完整换算链（SDK usage adapter → 客户端用量统计 → 上下文溢出判定 → 成本计算）无 NaN、压缩阈值正常。

**客户端会发 assistant prefill**：典型场景是 agent 达到步数上限时追加一条"只许输出文字"的 assistant 消息。属低频但合法的路径。

**`{"type":"text","text":" "}` 单空格块是刻意的**：客户端用它占住签名块的位置（删掉会移位签名块，留空串会被 Anthropic 拒）。**不要当畸形请求拒掉。**

**`/v1/messages/count_tokens` 实际调用率低**：SDK 与主流客户端多用响应 usage 反推做上下文管理，不预估。端点仍保留——它是 Anthropic 官方标准的一部分。

**`@ai-sdk/openai` 默认走 Responses API**：`provider.languageModel()` 即 `createResponsesModel()`；只有显式 `.chat()` 才走 `/v1/chat/completions`（本仓库未实现该端点）。客户端配置需确保用的是前者。

---

## 五、被实测推翻的错误前提

这些错误陈述曾存在于代码注释中，已修正。记录在此以防重现：

| 错误陈述 | 实测真相 |
|---|---|
| "上游 Kiro 不是 Anthropic 服务端，不会下发真实签名" | 下发 308~10920 字符的 base64 真签名 |
| "客户端发顶层 `effort`" | 11 个 SDK 版本抓包证明只有 `output_config.effort`；混淆了 providerOptions 与 wire format |
| `metadataEvent` 完全静默无害 | 一直在下发，只是从未被看见；加 warn 后立刻暴露 |
| 上游 `reasoningContent` "仅响应侧支持，请求 history 传入会 400" | Smithy 模型中该 shape 只有序列化器无反序列化器（input-only），实测回传成功 |

---

## 六、安全与隐私

- **禁止把凭据、token、密钥写进日志**。请求头逐条过 `security::redact_header_value`；请求体只记长度摘要；代理 URL 过 `redact_proxy_url`；密钥首次生成时走 `println!`（stdout），日志只留 `key_fingerprint` 指纹
- 诊断日志只输出**结构与长度**，不输出对话内容、思考内容或签名本体
- 读取 `credentials.json` 做实验时用临时副本，用完 `shred -u`
- 实测起临时实例要用**非生产端口**。生产实例的 nginx 反代可能正被其他服务依赖，重启会断流 —— 必须蓝绿部署

---

## 七、提交约定

- Commit message 用中文，说明**为什么**而不只是改了什么；涉及实测结论时附上关键数据（字段值、长度、状态码）
- 破坏性变更用 `feat!:` / `fix!:` / `refactor!:` 前缀
- 只在被明确要求时提交；只推送到指定分支
- 前端构建产物（`admin-ui/dist/`）与包管理器锁文件冲突（本仓库以 `bun.lock` 为准）不要提交
