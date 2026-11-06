# 端到端验收报告

> 项目：`tiny_ai_agent`（Rust CLI AI agent）　｜　验收依据：`todo.md` §3（U1-U3）、§8（A1-A8）
> 验收人：qa-verifier　｜　日期：2026-11-06
> 被测二进制：`target/debug/tiny_ai_agent`（`cargo build`，rustc 1.99.1）
> 复现脚本：[`scripts/acceptance.sh`](../scripts/acceptance.sh)　｜　原始日志：[`workspace/qa-artifacts/`](../workspace/qa-artifacts/)

## 0. 结论

**A1-A8 全部通过，代码质量门（build / clippy / test）全部通过。** 连续三轮全量跑均为 11/11 PASS，且每项**全部首次尝试即通过**，未触发重试。未发现需要修复的代码缺陷。

| 维度 | 结果 |
| --- | --- |
| 验收用例 | A1-A8 **8/8 PASS** |
| 代码质量 | build 无警告、clippy `-D warnings` 通过、test 20 passed / 1 ignored |
| 总计 | **11/11 PASS**，脚本退出码 0 |
| 稳定性 | 三轮全量重跑一致；无 flaky |

## 1. 总表

模型端点：OpenAI 兼容 `POST {LLM_BASE_URL}/chat/completions`，模型 `deepseek-v4.1-flash`。

| 用例 | 输入 | 实际调用链 | 期望 | 实际 | 结论 |
| --- | --- | --- | --- | --- | --- |
| **A1** | 帮我算一下 (1234 + 5678) × 3.5 | `calculator {"expression":"(1234 + 5678) * 3.5"}` → `24192`（1 次） | calculator 恰好 1 次，答案含 24192 | 调用 1 次，返回 `24192`，最终回答 `(1234 + 5678) × 3.5 = **24192**` | **PASS** |
| **A2** | 查一下香港今天的天气，把结果写到 weather.txt | `get_weather {"city":"香港"}` → 成功；`write_file {"path":"weather.txt","content":...}` → `已写入 N 字符` | 依次调用两工具，`workspace/weather.txt` 存在且为天气文字 | 顺序正确；文件存在（多次实测 42-61 字符，N 随模型措辞浮动），含`气温 25.8°C`/`体感 29.1°C`/`湿度 81%`/`晴间多云` | **PASS** |
| **A3** | 你好，你是谁？ | 无工具调用；`第 1/10 步` 后直接 `最终回答（stop_reason=stop）` | 无工具调用，一轮内回答 | 工具调用 0 次，仅 1 步，回答自我介绍 | **PASS** |
| **A4** | 帮我读一下 no_such_file_zzz.txt 的内容 | `read_file {"path":"no_such_file_zzz.txt"}` → **错误**（`文件不存在：...`） | 工具返回错误，agent 说明文件不存在，进程不 panic | 工具返回错误；回答`读取失败：工作目录 ... 下不存在 no_such_file_zzz.txt`；进程 exit code **0**，stderr 无 `panicked at` | **PASS** |
| **A5** | 请直接用 read_file 工具读取 ../Cargo.toml，不要自己判断路径是否合法，把工具返回的原始报错告诉我就行。 | `read_file {"path":"../Cargo.toml"}` → **错误**（`路径越界：../Cargo.toml 指向工作目录之外`） | 被工具拒绝，返回路径越界错误 | 工具层拒绝，错误文本明确含`路径越界`；日志中未出现 Cargo.toml 的 `[dependencies]` 内容（未泄漏） | **PASS** |
| **A6** | `AGENT_MAX_STEPS=2` 跑 U2（查香港天气并写 weather.txt） | `第 1/2 步`：`get_weather` 成功；`第 2/2 步`：`write_file` 成功；随后 `已达到最大循环次数（2）` | 达到上限后停止并提示，不再请求 API | 仅出现 `第 1/2`、`第 2/2` 两步，**无 `第 3` 步**；上限提示之后无任何新步骤 | **PASS** |
| **A7** | 第一轮"我叫小王，请记住这个名字，只回复好的。"；第二轮"我叫什么名字？" | 两次均无工具调用（各 1 步） | 第二次引用第一次内容正确回答 | 回答序列 `好的` → `你叫小王。`，短期记忆生效 | **PASS** |
| **A8** | 帮我算一下 (1234 + 5678) × 3.5 | 同上 A1 | 每一步的工具名、参数、结果都有日志 | 步骤标题 `── 第 1/10 步 ──`、`调用工具 calculator，参数：{...}`、`工具 calculator 返回（成功）：24192`、`最终回答（stop_reason=stop）` 齐备 | **PASS** |
| **Q1** | `cargo build` | — | 无警告 | 退出码 0，`warning` 计数 0 | **PASS** |
| **Q2** | `cargo clippy --all-targets -- -D warnings` | — | 通过无告警 | 退出码 0 | **PASS** |
| **Q3** | `cargo test` | — | 通过 | `test result: ok. 20 passed; 0 failed; 1 ignored` | **PASS** |

A1 的答案与 A8 的日志均来自同一类调用，`calculator` 返回的原始结果就是 `24192`（`format_number` 把 `24192.0` 格式化为整数），最终回答也包含 `24192`，与 todo.md 期望一致。

### 关于 A2 的写入位置

A2 按 todo.md 的字面要求落盘到 `workspace/weather.txt`（默认 `AGENT_WORKSPACE=workspace`），验证的是"默认配置下的真实行为"。A1/A3/A4/A6/A7/A8 各自使用独立夹具 `workspace/qa-aN/`（通过临时 `AGENT_WORKSPACE` 注入），用完即删，互不污染。

### 关于 A5 的输入措辞

首轮用温和措辞（"读一下 ../Cargo.toml 的内容"）时，模型会**自己识别并拒绝**、根本不调用工具——这样 A5 就验证不到"工具层越界校验"。脚本因此加强了提示词，明确要求"直接用工具、把原始报错告诉我"；实测 3/3 次都能触发真实的 `read_file` 调用并拿到工具的`路径越界`错误。脚本同时断言：若模型自行拒绝而未触发工具，判该次尝试无效并重试（不静默放过）。

## 2. 复现方式

```bash
cd rust-tiny-ai-agent

# 全量（build + A1-A8 + clippy + test），有失败则以非 0 退出
./scripts/acceptance.sh

# 只跑某几条（调试用）
./scripts/acceptance.sh --only A5,A6

# 调整单条用例重试次数 / 单次进程超时
./scripts/acceptance.sh --attempts 3
RUN_TIMEOUT=300 ./scripts/acceptance.sh

# 只验功能、跳过 clippy 与 test
./scripts/acceptance.sh --no-quality
```

脚本特性：每条用例独立 `AGENT_WORKSPACE` 夹具并自动清理；单条失败自动重试（默认最多 3 次尝试）；输出 PASS/FAIL、尝试次数与失败原因；结尾汇总计数；任何 FAIL 时 `exit 1`。产物落在 `workspace/qa-artifacts/`（`runN.out` / `runN.err` / `chain.txt`），`summary.txt` 是机器可读汇总。

**失败项最小复现命令**：本轮无失败项。若日后某条 FAIL，脚本结尾会直接打印该用例的复现命令（形如 `AGENT_WORKSPACE=... ./target/debug/tiny_ai_agent < workspace/qa-artifacts/<用例>/input.txt`），可直接粘贴执行。

## 3. 脚本自身的有效性验证（负面测试）

一个从不失败的验收脚本毫无价值。为此用**假 agent**（把 `BIN` 指向一个按 `FAKE_MODE` 造日志的脚本）反向验证断言确实能捕获失败。四种伪造失效模式全部被按预期原因判 FAIL：

| 伪造的失效模式 | 脚本给出的 FAIL 原因 | 是否命中 |
| --- | --- | --- |
| `nocall`：模型不调工具只答话 | A1 `calculator 调用 0 次，期望 1 次`；A4 `read_file 没有返回错误`；A5 `模型自行拒绝了请求、没有调用 read_file，工具层越界未被验证` | ✅ |
| `wrongnum`：算了但答案错（`1+1=2`） | A1 `最终回答不含 24192：答案是 2`；A4/A5 同上 | ✅ |
| `leak`：越界读取"成功"、内容泄漏，随后又补一条错误日志 | A5 `越界读取疑似成功：日志里出现了 Cargo.toml 的 [dependencies] 内容` | ✅ |
| `panic`：往 stderr 打 `panicked at` | A1/A4/A5 均报 `进程 panic`（A4 另注明"A4 明确不允许"） | ✅ |

其中 `leak` 一例最能说明问题：如果只 grep"有没有 `路径越界` 字样"，这种"先真读成功、再补一条越界日志"的伪造会被误判为 PASS。加"`[dependencies]` 不得出现在日志里"的测谎断言后即被拦下——这正是 A5 实质性验证的保障。

## 4. 重试与端点稳定性

- 端点按任务说明存在偶发 502/超时，脚本内置"单条用例最多尝试 3 次"的容忍机制。
- **本轮实测未触发任何重试**：三轮全量共 24 次模型任务，全部首次尝试通过。
- 程序自身也有重试：`llm.rs` 对网络错误 / 429 / 5xx 重试最多 2 次（指数退避 800ms、1600ms），重试时往 stderr 打 `[llm] 第 N 次请求失败…`。本轮的 `runN.err` 中未出现该行，说明端点在三轮全量运行中始终稳定。
- 中文编码：脚本统一 `LANG/LC_ALL=C.UTF-8`；日志中中文（`香港`、`越界`、`已达到最大循环次数`）无乱码。

## 5. 与 `todo.md` 的偏差

验收以**当前实现**为准，以下是需求文档已过时或不一致之处，建议回写文档。

| # | todo.md 的说法 | 实际实现 | 影响 |
| --- | --- | --- | --- |
| D1 | §4 F1 / §7.6：调用 **Anthropic Messages API**，`POST https://api.anthropic.com/v1/messages`，请求头 `x-api-key`、`anthropic-version`，响应读 `stop_reason` | **OpenAI 兼容** `POST {LLM_BASE_URL}/chat/completions`，`Authorization: Bearer`，tool use 走 `tools` + `tool_calls`（`llm.rs` 已实测跑通） | 文档的端点/字段整节失效；`stop_reason` 语义由 `finish_reason` 承担（实测值为 `stop`） |
| D2 | §7.3：`ContentBlock` 用 `#[serde(tag="type")]` 直接序列化 | 改为中立类型 `Part`/`Message`（`types.rs`），线上格式转换集中在 `llm.rs`，工具结果展开为多条 `role=tool` 消息 | 架构更好，但示意代码已过时 |
| D3 | A6：「把最大循环次数**临时改为 2**」（改源码） | 用环境变量 `AGENT_MAX_STEPS=2`，**不改源码** | 无需改动 `src/`，可重复验证；副作用是 todo.md 未提及该变量 |
| D4 | §5 calculator：允许 `+ - * / ( ) .` | 另支持一元正负号（`--2`、`2 * -3`）、空白；显式拒绝 `1/0`、`^`、超长/非法字符 | 功能是超集，验收不受影响 |
| D5 | §5：`read_file` 超 5000 字符截断 | 一致；截断后追加 `[内容已截断，原长度 N 字符…]` | 一致，仅文案更细 |
| D6 | §7.5 目录结构：`src/{main,agent,llm,tools,config}.rs` | 多一个 `src/types.rs`（中立数据结构） | 合理拆分，文档未同步 |
| D7 | §6：核心循环不超过 150 行 | `agent.rs` 106 行，其中 96 行起是 `#[cfg(test)]`（约 90 行是循环本身） | 达标 |
| D8 | §4 F7：工具结果日志截断到 500 字符 | 一致（`LOG_RESULT_LIMIT=500`），进历史的截断到 5000（`HISTORY_RESULT_LIMIT`） | 一致 |
| D9 | §7.1 依赖清单 | 额外用了 `reqwest` 的 `blocking` feature（天气工具同步 HTTP，在 `spawn_blocking` 里执行） | 文档可补一句 |

另有两点实现细节超出文档要求，属于加固（验收无影响）：

- `Registry::execute` 用 `catch_unwind` 兜底，某个工具内部 panic 也只变成一条错误结果，不会拖垮进程——这正是 A4「不 panic」的额外保障。
- `resolve_in_workspace` 先做词法规范化（`..` 不许越过根），再规范化最深已存在祖先并复核 `starts_with(workspace)`，最后追一次符号链接真实路径，同时覆盖"尚不存在的文件"与"符号链接逃逸"。

## 6. 发现的问题

**无需修复的代码缺陷。** 以下是观察到的、值得记录但不算验收失败的点：

1. **A6 存在设计性边界（非缺陷）**：`AGENT_MAX_STEPS=2` 跑 U2 时，模型恰好用 2 步做完两个工具调用，于是第 2 步结束后直接撞上限、拿不到最终汇总回答（`助手>` 输出的是上限提示而不是"已写入成功"）。这符合 F5「达到上限时停止并提示」的设计，A6 也正因此通过；但说明"2 步"对 U2 是**临界值**——如果哪天模型把两个工具塞进同一步，A6 会验不到上限行为。脚本已对此做防护：若上限提示未出现则判该次尝试无效并重试，而不是误判 PASS。
2. **A5 的通过依赖提示词**：见 §1 与 §3，模型有"自己先拒绝"的倾向；这是模型行为而非工具缺陷，工具层拦截本身是确定性正确的（另有 `path_escape_is_rejected`、`symlink_escape_is_rejected` 单元测试覆盖）。
3. **`weather_end_to_end` 测试默认 `#[ignore]`**：需联网访问 Open-Meteo，所以 `cargo test` 是 20 passed / 1 ignored。需要时可 `cargo test -- --ignored weather` 单独跑。

## 7. 环境与可重复性

- 被测源码 mtime 全部 ≤ 17:51:37，两轮验收期间源码未再变动（`agent.rs` 106 行、`tools.rs` 1079 行等），结论对应当前 HEAD 状态。
- `.env` 仅用于提供 `LLM_API_KEY` / `LLM_BASE_URL` / `LLM_MODEL` / `AGENT_MAX_STEPS`；本次验收未修改 `.env`、`src/**`、`Cargo.toml`、`todo.md`。
- `workspace/qa-artifacts/` 保留最近一轮的完整日志；（中间轮次的备份在清理时已删除，`workspace/qa-artifacts/` 为最终一轮的完整现场。）