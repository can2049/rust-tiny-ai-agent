# 简易 AI Agent 需求文档（Rust 版）

> 日期：2026-11-06　｜　用途：个人学习　｜　语言：Rust
>
> **实现状态：已交付，A1-A8 全部通过。**
> 实现时发现所用的模型服务端点是 **OpenAI 兼容端点**（`POST {base_url}/chat/completions`），
> 而非本文档 §4 F1 / §7.6 假设的 Anthropic Messages API，因此协议层按真实端点落地。
> 偏差清单与替代方案见 [README.md](README.md#与需求文档的偏差重要) 与
> [docs/acceptance-report.md](docs/acceptance-report.md) §5。文档中以下内容为历史设计，以代码为准：
> §4 F1 的 API 形式、§7.3 的 `ContentBlock` serde 设计、§7.6 的请求/响应字段。
> A6 的验证方式改为环境变量 `AGENT_MAX_STEPS=2`，无需改源码。

## 1. 项目概述

本项目用 Rust 从零实现一个命令行 AI agent：大模型通过"思考 → 调用工具 → 观察结果"的循环，自主完成多步任务。目的是亲手搭一遍 agent 的核心机制，用于个人学习，不追求生产级。

- **背景**：普通聊天模型只能输出文字，无法查询信息或操作文件；agent 让模型能决定"下一步该做什么"并真正执行。
- **目标**：用不依赖 agent 框架的约 200 行核心代码，跑通一个带工具调用、对话记忆和基本错误处理的完整 agent。
- **使用者**：开发者本人，在本机终端运行，处理自己提出的任务。
- **成功标准**：给出一个需要两步以上工具调用的任务，agent 能自己拆解、执行并给出正确答案，且每一步在终端可见。

## 2. 范围

本期只做单 agent、命令行、3 类工具的最小完整版本；界面、长期记忆和多 agent 留到后续。

| 本期做 | 本期不做 |
| --- | --- |
| 单个 agent，终端对话（CLI） | 网页或图形界面 |
| 通过 HTTP 调用大模型 API 的 tool use 功能 | 自己训练或部署模型 |
| 内置工具：计算器、天气查询、文件读写 | 浏览器自动化、代码执行沙箱 |
| 单次会话内的消息历史（短期记忆） | 跨会话的长期记忆、向量数据库 |
| 最大循环次数、工具报错回传、步骤日志 | 多 agent 协作、任务规划器 |
| 文件操作限制在指定工作目录内 | 权限系统、多用户、部署上线 |

## 3. 典型使用场景

三个用例覆盖单工具、多步工具链和无需工具三种情况，同时作为验收依据。

| # | 用户输入 | 期望的 agent 行为 |
| --- | --- | --- |
| U1 | "帮我算一下 (1234 + 5678) × 3.5" | 调用计算器一次，得到 24192，并用自然语言回答 |
| U2 | "查一下香港今天的天气，把结果写到 weather.txt" | 先调用天气工具，再调用写文件工具，最后确认文件已写入 |
| U3 | "你好，你是谁？" | 不调用任何工具，直接回答，循环一轮结束 |

## 4. 功能需求

共 8 项功能，优先级 P0 为必做，P1 为建议做。

| 编号 | 功能 | 需求描述 | 优先级 |
| --- | --- | --- | --- |
| F1 | LLM 调用 | 通过 `reqwest` 调用 Anthropic Messages API（官方暂无 Rust SDK，直接走 HTTP）；每次请求携带系统提示词、消息历史和工具定义；API Key 从环境变量读取，不写进代码 | P0 |
| F2 | Agent 循环 | 把任务发给模型；若 `stop_reason` 为 `tool_use`，则执行工具并把结果追加到消息历史后再次请求；否则取出最终文字回答并结束循环 | P0 |
| F3 | 工具注册与调用 | 每个工具实现统一的 `Tool` trait（名称、描述、参数 JSON Schema、执行函数）；新增工具只需实现 trait 并注册一次，不改循环代码 | P0 |
| F4 | 短期记忆 | 单次会话内用 `Vec<Message>` 保存完整消息（用户输入、模型回复、工具调用、工具结果），每轮请求都带上 | P0 |
| F5 | 终止与保护 | 最大循环次数默认 10；达到上限时停止并提示用户；支持 Ctrl+C 中断 | P0 |
| F6 | 错误处理 | 工具返回 `Err` 或参数不合法时，不让程序崩溃（不使用 `unwrap` 处理外部输入），而是把错误文本作为工具结果（`is_error: true`）返回给模型，由模型决定重试或换方案；API 调用失败（网络错误、429、5xx）时最多重试 2 次 | P0 |
| F7 | 步骤日志 | 在终端打印每一步：模型的文字、调用的工具名与参数、工具返回结果（过长时截断到 500 字符）；可通过环境变量 `AGENT_LOG=0` 关闭 | P1 |
| F8 | 命令行交互 | 启动后进入多轮对话循环，用户输入 `exit` 退出；每个任务的最终答案单独输出 | P1 |

系统提示词需说明三点：agent 的角色、何时该用工具（需要精确计算、实时信息或文件操作时）、何时直接回答。

## 5. 工具规格

所有工具的执行函数签名为 `fn run(&self, input: serde_json::Value) -> anyhow::Result<String>`；失败时返回 `Err`，由注册表统一转成错误文本回传给模型。

| 工具名 | 用途 | 参数 | 返回值 | 约束 |
| --- | --- | --- | --- | --- |
| `calculator` | 精确计算数学表达式 | `expression`（字符串，如 `"(1+2)*3"`） | 计算结果字符串 | 只允许数字和 `+ - * / ( ) .`；使用 `evalexpr` crate，或手写递归下降解析器（学习推荐后者） |
| `get_weather` | 查询城市当前天气 | `city`（字符串） | 温度、天气状况的简短文字 | 使用免费天气 API（如 Open-Meteo，需先做城市地理编码）；超时 10 秒；查不到城市时返回错误 |
| `read_file` | 读取文本文件 | `path`（相对路径） | 文件内容，超过 5000 字符则截断并注明 | 路径必须在工作目录 `workspace/` 内 |
| `write_file` | 写入文本文件 | `path`（相对路径）、`content`（字符串） | `已写入 N 字符到 path` | 同上；覆盖已有文件前无需确认（本期简化） |

每个工具的描述要写清"什么时候该用"和"参数格式示例"，因为模型靠这段文字决定是否调用它。

路径限制的推荐做法：把 `workspace/` 与目标路径都 `canonicalize()` 后，检查目标路径是否 `starts_with` 工作目录；对尚不存在的文件，先规范化其父目录再拼接文件名。

## 6. 非功能需求

重点是安全、成本可控和代码易读，因为这是一个要反复阅读、修改的学习项目。

- **安全**：API Key 只放环境变量或 `.env` 文件，并加入 `.gitignore`；文件工具只能访问 `workspace/` 目录，拒绝越界和符号链接逃逸；计算器不执行任意代码。
- **成本控制**：最大循环次数限制（默认 10）；单个工具结果截断后再放入消息历史；开发时选用较小、便宜的模型。
- **可读性**：核心循环集中在 `agent.rs`，不超过 150 行；每个公开函数有一句文档注释；不引入 agent 框架。
- **代码质量**：`cargo build` 无警告，`cargo clippy` 无告警；外部输入路径上不使用 `unwrap()` / `expect()`。
- **可扩展性**：新增工具只需实现 `Tool` trait 并注册；模型名称、最大循环次数、工作目录均为配置项。
- **可观察性**：日志能完整还原一次任务的每一步，便于对照理解模型的决策过程。
- **运行环境**：Rust stable 1.75 及以上。

## 7. 技术方案

采用 Rust 加 HTTP 直连 Anthropic Messages API，拆成入口、循环、模型客户端、工具、配置五个模块；整个 agent 的行为由一个循环决定。

### 7.1 技术选型

| 用途 | crate |
| --- | --- |
| 异步运行时 | `tokio`（features: `rt-multi-thread`, `macros`） |
| HTTP 客户端 | `reqwest`（features: `json`） |
| JSON 序列化 | `serde`、`serde_json` |
| 错误处理 | `anyhow` |
| 读取 `.env` | `dotenvy` |
| 命令行输入 | 标准库 `std::io`（够用，不引入 `clap`） |

### 7.2 系统架构

```text
                    ┌──────────────────────┐
                    │ 消息历史（短期记忆）  │
                    └──────┬───────▲───────┘
                      每轮带上   追加消息
                           ▼       │
┌────────┐   任务    ┌──────────────────────────┐  消息和工具   ┌────────────┐
│  用户  │ ───────▶ │ Agent 循环（agent.rs）    │ ───────────▶ │ Claude API │
│ (CLI)  │ ◀─────── │ 调用模型→执行工具→观察结果 │ ◀─────────── │ (llm.rs)   │
└────────┘   答案    │ 每步打印日志，最多 10 轮  │ 工具调用或答案 └────────────┘
                    └──────┬───────▲───────────┘
                       调用工具   返回结果
                           ▼       │
                    ┌──────────────────────┐
                    │ 工具注册表（tools.rs）│
                    └─┬──────┬──────┬─────┬┘
                      ▼      ▼      ▼     ▼
              calculator get_weather read_file write_file
```

用户任务进入 Agent 循环；循环把消息历史和工具定义发给模型，模型要求调用工具时由注册表分发执行，结果写回历史后进入下一轮。

### 7.3 核心数据结构（示意）

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}

#[derive(Serialize, Deserialize, Clone)]
struct Message {
    role: String,               // "user" | "assistant"
    content: Vec<ContentBlock>,
}

trait Tool {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;                      // JSON Schema
    fn run(&self, input: Value) -> anyhow::Result<String>;
}
```

### 7.4 核心循环（伪代码）

对应 Messages API 的 tool use 流程：

```rust
async fn run_agent(task: &str, llm: &Llm, tools: &Registry, cfg: &Config) -> anyhow::Result<String> {
    let mut messages = vec![Message::user_text(task)];

    for _step in 0..cfg.max_steps {
        let resp = llm.send(&cfg.system_prompt, &messages, tools.definitions()).await?;
        messages.push(Message::assistant(resp.content.clone()));

        if resp.stop_reason != "tool_use" {
            return Ok(final_text(&resp.content));        // 模型给出最终回答，结束
        }

        let mut results = Vec::new();
        for block in &resp.content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                log_call(name, input);
                // 注册表内部把 Err 转成错误文本，不向上传播
                let (content, is_error) = tools.execute(name, input.clone());
                log_result(&content);
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(), content, is_error,
                });
            }
        }
        messages.push(Message::user(results));
    }
    Ok("已达到最大循环次数，任务未完成".to_string())
}
```

### 7.5 目录结构

```text
simple-agent/
├── Cargo.toml
├── .env                # ANTHROPIC_API_KEY（不提交到 git）
├── .gitignore
├── workspace/          # 文件工具唯一可访问的目录
└── src/
    ├── main.rs         # 命令行入口：读取输入、调用 run_agent
    ├── agent.rs        # 核心循环
    ├── llm.rs          # API 请求/响应结构与重试逻辑
    ├── tools.rs        # Tool trait、注册表与 4 个工具
    └── config.rs       # 模型名、最大步数、工作目录、系统提示词
```

### 7.6 API 请求要点

- 地址：`POST https://api.anthropic.com/v1/messages`
- 请求头：`x-api-key`、`anthropic-version: 2023-06-01`、`content-type: application/json`
- 请求体：`model`、`max_tokens`、`system`、`messages`、`tools`（每项含 `name`、`description`、`input_schema`）
- 响应：读取 `content`（文字块与 `tool_use` 块）和 `stop_reason`

具体字段以 Anthropic 官方 API 文档为准，开发前先对照一次。

## 8. 验收标准

以下 8 条全部通过即视为本期完成。

| # | 测试步骤 | 通过条件 |
| --- | --- | --- |
| A1 | 运行场景 U1（计算） | 调用 `calculator` 一次，最终答案包含 24192 |
| A2 | 运行场景 U2（查天气并写文件） | 依次调用 `get_weather`、`write_file`，`workspace/weather.txt` 存在且内容为天气文字 |
| A3 | 运行场景 U3（闲聊） | 不触发任何工具调用，一轮内给出回答 |
| A4 | 让 agent 读取不存在的文件 | 工具返回错误，agent 向用户说明文件不存在，程序不崩溃（不 panic） |
| A5 | 让 agent 读取 `../` 之外的路径 | 被工具拒绝，返回路径越界的错误 |
| A6 | 把最大循环次数临时改为 2，运行 U2 | 达到上限后停止并提示，不再继续请求 API |
| A7 | 连续两次提问，第二次引用第一次的内容 | agent 能根据消息历史正确回答 |
| A8 | 运行任意任务，查看终端 | 每一步的工具名、参数和结果都有日志输出 |

另外要求 `cargo build` 无警告、`cargo clippy` 无告警。

## 9. 开发里程碑与后续扩展

按下面 5 步推进，每一步都能单独运行验证，Rust 新手预计需要两到三个晚上。

1. **打通 API**：`cargo new`，加依赖，配置 API Key，用 `reqwest` 发一条消息并打印回复。
2. **定义数据结构与第一个工具**：写好 `Message` / `ContentBlock` 的 serde 结构，实现 `calculator`，手动处理一次工具调用并回传结果，跑通 A1。
3. **写成循环**：把调用、执行、回传封装进 `for` 循环，加上最大步数和消息历史，跑通 A3、A6、A7。
4. **补齐工具与保护**：实现 `Tool` trait 与注册表，加入 `get_weather`、`read_file`、`write_file`，实现路径限制和错误回传，跑通 A2、A4、A5。
5. **加日志与命令行**：打印每一步，做多轮对话入口，API 重试，全部验收项复测，跑通 A8。

**后续扩展方向**（本期之外）：

- 长期记忆：把重要信息存入本地文件，下次启动时读取
- 规划能力：复杂任务先让模型列出步骤，再逐步执行
- 人工确认：写文件、删文件等操作前先让用户确认
- 流式输出：用 SSE 边生成边显示，改善等待体验
- 并行工具调用：同一轮的多个 `tool_use` 用 `tokio::join!` 并发执行
- 接入 MCP：用标准协议复用现成的工具服务



