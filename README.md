# tiny_ai_agent

一个用 Rust 从零实现的命令行 AI agent：大模型通过 **思考 → 调用工具 → 观察结果** 的循环，自主完成多步任务。
需求文档见 [todo.md](todo.md)。

## 快速开始

```bash
cargo build
./target/debug/tiny_ai_agent
```

启动后逐行输入任务，`exit` 退出：

```text
你> 帮我算一下 (1234 + 5678) × 3.5
── 第 1/10 步 ──
调用工具 calculator，参数：{"expression": "(1234 + 5678) * 3.5"}
工具 calculator 返回（成功）：24192
── 第 2/10 步 ──
最终回答（stop_reason=stop）

助手> (1234 + 5678) × 3.5 = 24192。
```

## 配置

配置全部走环境变量，也可以写在 `.env`（已被 `.gitignore` 忽略）。

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `LLM_API_KEY` | 必填 | API Key，不写进代码 |
| `LLM_BASE_URL` | `https://your-api-endpoint/v1` | OpenAI 兼容的 API 根地址，**必须自己填**（见下） |
| `LLM_MODEL` | `deepseek-v4.1-flash` | 模型名 |
| `AGENT_MAX_STEPS` | `10` | 单个任务的循环上限 |
| `AGENT_MAX_TOKENS` | `4096` | 单次回复的 token 上限 |
| `AGENT_WORKSPACE` | `workspace` | 文件工具唯一可访问的目录 |
| `AGENT_LOG` | 开 | 设为 `0` 关闭终端里的步骤日志 |
| `AGENT_LOG_FILE` | 开 | 设为 `0` 关闭 JSONL 运行日志文件 |
| `AGENT_LOG_DIR` | `logs` | 运行日志目录，每次启动一个文件 |
| `AGENT_LOG_CONTENT_LIMIT` | `4000` | 日志里单个长文本字段最多保留的字符数 |

`LLM_BASE_URL` 需要指向一个**支持 function calling / tool use 的 OpenAI 兼容端点**，
例如 DashScope（`https://dashscope.aliyuncs.com/compatible-mode/v1`）、DeepSeek、
OpenRouter，或本地 vLLM / Ollama。最小步骤：

```bash
cp .env.example .env
# 编辑 .env，填入 LLM_API_KEY 和 LLM_BASE_URL
```

端点必须支持 tool calling，否则模型收不到 `tools` 字段、agent 会退化成普通聊天。

## 架构

```text
src/main.rs    命令行入口：多轮对话、Ctrl+C 中断、错误回滚
src/agent.rs   核心循环：调用模型 → 执行工具 → 结果回传，最多 N 步
src/llm.rs     模型客户端：请求/响应结构、消息格式转换、失败重试
src/tools.rs   Tool trait、注册表与 4 个工具
src/config.rs  模型名、最大步数、工作目录、系统提示词
src/types.rs   与协议无关的会话消息结构
```

新增一个工具：实现 `Tool` trait，然后在 `Registry::with_builtins` 里 `register` 一次，循环代码不用改。

## 工具

| 工具 | 用途 | 参数 |
| --- | --- | --- |
| `calculator` | 精确计算数学表达式（手写递归下降，不执行任意代码） | `expression` |
| `get_weather` | 查城市当前天气（Open-Meteo，免 Key，10s 超时） | `city` |
| `read_file` | 读 `workspace/` 内的文本文件，超 5000 字符截断 | `path` |
| `write_file` | 写 `workspace/` 内的文本文件 | `path`、`content` |

工具失败不会让程序崩溃：错误文本会以 `is_error` 结果回传给模型，由模型自行修正或换方案。

## 运行日志（学习与排障用）

每次启动会写一个 **JSONL** 文件到 `logs/`，一行一个事件，逐行 flush（崩溃或
Ctrl+C 也不丢已发生的事件）。终端打印的是摘要，日志文件里是完整链路。

```bash
./scripts/logview.sh            # 最近一次运行的可读执行轨迹
./scripts/logview.sh --summary  # 事件统计、任务结果、每个请求的 token 用量
./scripts/logview.sh --step 2   # 只看某一步
./scripts/logview.sh --task 1   # 只看某个任务（多轮对话里用）
./scripts/logview.sh --json     # 原始 JSON，自己接 jq
```

轨迹长这样（`→` 是发出、`←` 是收到，括号里是耗时）：

```text
╭─ 任务 #1  「查一下香港今天的天气，把结果写到 weather.txt」
├─ 第 1/10 步
│   ├→ 请求模型  messages=2  tools=calculator,get_weather,read_file,write_file  3960B
│   ├← 模型回复  stop=tool_calls  1.2s
│   │      · get_weather({"city":"香港"})
│   ├✓ get_weather  2.3s  37 字符
│   │    「香港：气温 23.7°C（体感 26.5°C），湿度 91%，天气：毛毛雨」
├─ 第 2/10 步
│   ├← 模型回复  stop=stop  1.0s
│   │    文字：「香港今天气温 23.7°C…结果已写入 weather.txt。」
╰─ ✓ 完成  步数=2  总耗时=3.6s
```

记录的事件：

| 事件 | 含义 | 关键字段 |
| --- | --- | --- |
| `session_start` / `session_end` | 会话起止 | 模型、base_url、最大步数、工作目录 |
| `task_start` / `task_end` | 一次任务起止 | 任务原文、结果（`answered` / `max_steps` / `interrupted` / `llm_error`）、步数、总耗时 |
| `step_start` | 进入第 N 步 | 消息历史长度 |
| `llm_request` | 发给模型的**完整请求体** | messages、tools、系统提示词、字节数 |
| `llm_turn` | 模型这一步的回复 | 文字、`stop_reason`、工具调用列表、耗时 |
| `tool_call` / `tool_result` | 工具执行 | 工具名、参数、结果、是否错误、耗时 |
| `llm_retry` / `llm_http_error` / `llm_error` | 请求失败与重试 | HTTP 状态、是否可重试、错误原文、退避时长 |

每条事件都带 `ts`（毫秒精度）、`seq`（严格递增，还原真实顺序）、`session`、`run`、`step`。

**日志里不含 API Key**（Key 只在 HTTP 头里，从不进入被记录的结构），有单测
`logger::tests::api_key_never_reaches_the_log` 守着。超长内容按
`AGENT_LOG_CONTENT_LIMIT` 截断并标注原长度。`logs/` 已在 `.gitignore` 中。

## 与需求文档的偏差（重要）

`todo.md` §7.6 描述的是 **Anthropic Messages API**；实际联调发现所用的模型服务端点是
**OpenAI 兼容协议**（`POST {base_url}/chat/completions`，工具调用走 `tools` / `tool_calls`）。
因此实现按真实端点落地，并做了如下等价替换：

| todo.md | 实际实现 |
| --- | --- |
| `x-api-key` + `anthropic-version` 头 | `Authorization: Bearer <key>` |
| `stop_reason == "tool_use"` | `finish_reason == "tool_calls"`（判据是回复里有无 `tool_calls`） |
| `content` 块数组（text / tool_use / tool_result） | assistant 的 `tool_calls` + 独立 `role: "tool"` 消息 |
| `input_schema` | `function.parameters`（JSON Schema 不变） |
| 临时改源码验证最大步数（A6） | `AGENT_MAX_STEPS` 环境变量 |

模块划分、`Tool` trait、注册表分发、消息历史、错误回传、截断与重试策略均与文档一致。

## 验收

```bash
bash scripts/acceptance.sh      # A1–A8
cargo clippy --all-targets -- -D warnings
cargo test
```

结果记录在 [docs/acceptance-report.md](docs/acceptance-report.md)。