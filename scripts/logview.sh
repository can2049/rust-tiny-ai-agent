#!/usr/bin/env bash
# 运行日志查看器：把 JSONL 日志渲染成可读的执行轨迹。
#
# 用法：
#   ./scripts/logview.sh                    # 看最近一个日志文件
#   ./scripts/logview.sh logs/xxx.log       # 看指定文件
#   ./scripts/logview.sh --task 1           # 只看第 1 个任务
#   ./scripts/logview.sh --step 2           # 只看第 2 步
#   ./scripts/logview.sh --summary          # 只要统计汇总
#   ./scripts/logview.sh --json             # 原始 JSON（交给 jq 再处理）
#
# 依赖 jq。

set -euo pipefail

LOG_DIR="${AGENT_LOG_DIR:-logs}"
FILE=""
FILTER_TASK=""
FILTER_STEP=""
MODE="trail"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --task) FILTER_TASK="${2:?--task 需要一个 run 编号}"; shift 2 ;;
        --step) FILTER_STEP="${2:?--step 需要一个步号}"; shift 2 ;;
        --summary) MODE="summary"; shift ;;
        --json) MODE="json"; shift ;;
        -h|--help) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*) echo "未知参数：$1" >&2; exit 2 ;;
        *) FILE="$1"; shift ;;
    esac
done

if ! command -v jq >/dev/null 2>&1; then
    echo "需要 jq，请先安装。" >&2
    exit 1
fi

if [[ -z "$FILE" ]]; then
    # 取最近修改的日志文件
    FILE=$(find "$LOG_DIR" -maxdepth 1 -name 'agent-*.log' -print0 2>/dev/null \
        | xargs -0 ls -t 2>/dev/null | head -1 || true)
fi
if [[ -z "$FILE" || ! -f "$FILE" ]]; then
    echo "找不到日志文件（目录：$LOG_DIR）。先运行一次 agent 生成日志。" >&2
    exit 1
fi

# 每条事件压成一行轨迹；顺序由 seq 保证。
JQ_TRAIL='
def ms: if . then (if . >= 1000 then "\((. / 1000 * 10 | round) / 10)s" else "\(.)ms" end) else "" end;
def clip: if (. | length) > 120 then .[0:120] + "…" else . end;

select(.event != "session_start" and .event != "session_end"
       and .event != "llm_http_response" and .event != "llm_response")
| select($task == "" or (.run | tostring) == $task)
| select($step == "" or (.step // "" | tostring) == $step)
| if .event == "task_start" then
    "╭─ 任务 #\(.run)  「\(.task)」  \(.ts)"
  elif .event == "step_start" then
    "\n├─ 第 \(.step)/\(.max_steps) 步"
  elif .event == "llm_request" then
    "│   ├→ 请求模型  model=\(.model)  messages=\(.message_count)  tools=\(.tool_names | join(","))  \(.request_bytes)B"
  elif .event == "llm_turn" then    "│   ├← 模型回复  stop=\(.stop_reason)  \(.elapsed_ms | ms)"
      + (if (.text.content // "") != "" then "\n│   │    文字：「\(.text.content | clip)」" else "" end)
      + (if (.tool_calls | length) > 0 then "\n│   │    决定调用 \(.tool_calls | length) 个工具：" else "" end)
      + ([.tool_calls[]? | "\n│   │      · \(.name)(\(.input | tostring | clip))"] | join(""))
  elif .event == "tool_call" then
    "│   ├→ 执行 \(.tool)"
  elif .event == "tool_result" then
    "│   ├\(if .is_error then "✗" else "✓" end) \(.tool)  \(.elapsed_ms | ms)  \(.content_chars) 字符\(if .truncated then "（日志已截断）" else "" end)\n│   │    「\(.content | clip)」"
  elif .event == "llm_retry" then
    "│   ├! 重试第 \(.attempt) 次（\(.error | clip)），\(.wait_ms | ms) 后重试"
  elif .event == "llm_http_error" then
    "│   ├✗ HTTP \(.status)  retryable=\(.retryable)  \(.body | clip)"
  elif .event == "llm_error" then
    "│   ├✗ 请求失败（不可重试）  \(.error | clip)"
  elif .event == "step_error" then
    "│   └✗ 本步失败  \(.elapsed_ms | ms)"
  elif .event == "task_end" then
    (if .outcome == "answered" then "\n╰─ ✓ 完成"
     elif .outcome == "max_steps" then "\n╰─ ⚠ 达到最大循环次数"
     elif .outcome == "interrupted" then "\n╰─ ⚠ 被用户中断"
     else "\n╰─ ✗ \(.outcome)" end)
    + "  步数=\(.steps // "-")  总耗时=\(.elapsed_ms | ms)"
    + (if .answer then "\n   最终回答：「\(.answer.content | clip)」" else "" end)
    + (if .error then "\n   错误：\(.error | clip)" else "" end)
  else
    "│   · \(.event)"
  end
'

case "$MODE" in
    trail)
        jq -r --arg task "$FILTER_TASK" --arg step "$FILTER_STEP" "$JQ_TRAIL" "$FILE"
        ;;
    summary)
        total=$(wc -l < "$FILE")
        echo "日志文件：$FILE"
        echo "事件总数：$total"
        echo
        echo "事件统计："
        jq -r '.event' "$FILE" | sort | uniq -c | sort -rn | sed 's/^/  /'
        echo
        echo "任务汇总："
        jq -r 'select(.event=="task_end")
               | "  #\(.run)  \(.outcome)  步数=\(.steps // "-")  耗时=\((.elapsed_ms / 1000 * 10 | round) / 10)s\(if .error then "  错误=\(.error[0:80])" else "" end)"' "$FILE"
        echo
        echo "token 用量（每个请求）："
        jq -r 'select(.event=="llm_http_response" and .usage != null)
               | "  prompt=\(.usage.prompt_tokens)  completion=\(.usage.completion_tokens)  total=\(.usage.total_tokens)"' "$FILE"
        ;;
    json)
        jq . "$FILE"
        ;;
esac
