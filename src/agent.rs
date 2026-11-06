//! 核心循环：调用模型 → 执行工具 → 观察结果，直到模型给出最终回答。

use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::llm::{Llm, truncate};
use crate::tools::Registry;
use crate::types::{Message, Part};

/// 单条工具结果放入消息历史前最多保留的字符数（成本控制）。
const HISTORY_RESULT_LIMIT: usize = 5000;

/// 终端日志里单条工具结果最多打印的字符数。
const LOG_RESULT_LIMIT: usize = 500;

/// 运行一个完整任务，返回模型的最终回答。
///
/// `history` 在整个会话内复用，因此后续任务能引用前面的对话（短期记忆）。
pub async fn run_agent(
    task: &str,
    llm: &Llm,
    tools: &Arc<Registry>,
    cfg: &Config,
    history: &mut Vec<Message>,
) -> Result<String> {
    history.push(Message::user_text(task));

    for step in 1..=cfg.max_steps {
        log_step(cfg, step, cfg.max_steps);

        let turn = llm.send(&cfg.system_prompt, history, &tools.definitions()).await?;
        history.push(Message::assistant(turn.parts.clone()));

        let calls = turn.tool_calls();
        if calls.is_empty() {
            let answer = history
                .last()
                .map(Message::text)
                .unwrap_or_default();
            log_line(cfg, &format!("最终回答（stop_reason={}）", turn.stop_reason));
            return Ok(answer);
        }

        let mut results = Vec::new();
        for (id, name, input) in calls {
            log_line(cfg, &format!("调用工具 {name}，参数：{input}"));
            let registry = Arc::clone(tools);
            let tool_name = name.clone();
            // 工具是同步实现（其中天气工具用阻塞 HTTP），放到阻塞线程池里执行，
            // 避免卡住异步运行时的工作线程。
            let (content, is_error) = tokio::task::spawn_blocking(move || {
                registry.execute(&tool_name, input)
            })
            .await
            .unwrap_or_else(|e| (format!("工具执行线程异常：{e}"), true));

            log_line(
                cfg,
                &format!(
                    "工具 {name} 返回（{}）：{}",
                    if is_error { "错误" } else { "成功" },
                    truncate(&content, LOG_RESULT_LIMIT)
                ),
            );
            results.push(Part::ToolResult {
                tool_use_id: id,
                content: truncate(&content, HISTORY_RESULT_LIMIT),
                is_error,
            });
        }
        history.push(Message::tool_results(results));
    }

    let msg = format!(
        "已达到最大循环次数（{}），任务未完成。可以试试把问题拆小一点，或调大 AGENT_MAX_STEPS。",
        cfg.max_steps
    );
    log_line(cfg, &msg);
    Ok(msg)
}

/// 打印步骤标题。
fn log_step(cfg: &Config, step: usize, max: usize) {
    log_line(cfg, &format!("── 第 {step}/{max} 步 ──"));
}

/// 按配置打印一行日志（`AGENT_LOG=0` 时静默）。
fn log_line(cfg: &Config, msg: &str) {
    if cfg.log_enabled {
        println!("{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const _: () = assert!(LOG_RESULT_LIMIT <= HISTORY_RESULT_LIMIT);

    #[test]
    fn truncate_shortens_long_results() {
        let long = "x".repeat(LOG_RESULT_LIMIT + 50);
        assert!(truncate(&long, LOG_RESULT_LIMIT).chars().count() < long.chars().count());
    }
}