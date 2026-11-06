//! 核心循环：调用模型 → 执行工具 → 观察结果，直到模型给出最终回答。
//!
//! 循环里每个关键节点都会写一条运行日志（见 `logger.rs`）：请求/响应、
//! 每一步模型说了什么、调了哪个工具、参数是什么、结果如何、耗时多少。

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use serde_json::json;

use crate::config::Config;
use crate::llm::{Llm, truncate};
use crate::logger::Logger;
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
    logger: &Arc<Logger>,
    cfg: &Config,
    history: &mut Vec<Message>,
) -> Result<String> {
    let run = logger.begin_run(task);
    let started = Instant::now();
    let history_len_before = history.len();

    history.push(Message::user_text(task));

    for step in 1..=cfg.max_steps {
        log_step(cfg, step, cfg.max_steps);
        logger.emit(
            Some(step),
            "step_start",
            json!({ "step": step, "max_steps": cfg.max_steps, "history_len": history.len() }),
        );

        let llm_started = Instant::now();
        let turn = match llm
            .send(&cfg.system_prompt, history, &tools.definitions(), Some(logger))
            .await
        {
            Ok(turn) => turn,
            Err(e) => {
                logger.emit(
                    Some(step),
                    "step_error",
                    json!({
                        "step": step,
                        "elapsed_ms": llm_started.elapsed().as_millis() as u64,
                        "error": format!("{e:#}"),
                    }),
                );
                logger.emit(
                    None,
                    "task_end",
                    json!({
                        "run": run,
                        "outcome": "llm_error",
                        "steps": step,
                        "elapsed_ms": started.elapsed().as_millis() as u64,
                        "error": format!("{e:#}"),
                    }),
                );
                return Err(e);
            }
        };
        let llm_elapsed = llm_started.elapsed();

        history.push(Message::assistant(turn.parts.clone()));

        let calls = turn.tool_calls();
        logger.emit(
            Some(step),
            "llm_turn",
            json!({
                "step": step,
                "elapsed_ms": llm_elapsed.as_millis() as u64,
                "stop_reason": turn.stop_reason,
                "text": logger.clip(&turn.text()),
                "tool_calls": calls
                    .iter()
                    .map(|(id, name, input)| json!({ "id": id, "name": name, "input": input }))
                    .collect::<Vec<_>>(),
            }),
        );

        if calls.is_empty() {
            let answer = history.last().map(Message::text).unwrap_or_default();
            log_line(cfg, &format!("最终回答（stop_reason={}）", turn.stop_reason));
            logger.emit(
                None,
                "task_end",
                json!({
                    "run": run,
                    "outcome": "answered",
                    "steps": step,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                    "history_len": history.len(),
                    "added_messages": history.len() - history_len_before,
                    "answer": logger.clip(&answer),
                }),
            );
            return Ok(answer);
        }

        let mut results = Vec::new();
        for (id, name, input) in calls {
            log_line(cfg, &format!("调用工具 {name}，参数：{input}"));
            logger.emit(
                Some(step),
                "tool_call",
                json!({ "step": step, "id": id, "tool": name, "input": input }),
            );

            let registry = Arc::clone(tools);
            let tool_name = name.clone();
            let tool_started = Instant::now();
            // 工具是同步实现（其中天气工具用阻塞 HTTP），放到阻塞线程池里执行，
            // 避免卡住异步运行时的工作线程。
            let (content, is_error) = tokio::task::spawn_blocking(move || {
                registry.execute(&tool_name, input)
            })
            .await
            .unwrap_or_else(|e| (format!("工具执行线程异常：{e}"), true));
            let tool_elapsed = tool_started.elapsed();

            log_line(
                cfg,
                &format!(
                    "工具 {name} 返回（{}）：{}",
                    if is_error { "错误" } else { "成功" },
                    truncate(&content, LOG_RESULT_LIMIT)
                ),
            );
            let mut result_event = logger.clip(&content);
            if let serde_json::Value::Object(ref mut map) = result_event {
                map.insert("step".into(), json!(step));
                map.insert("id".into(), json!(id));
                map.insert("tool".into(), json!(name));
                map.insert("is_error".into(), json!(is_error));
                map.insert("elapsed_ms".into(), json!(tool_elapsed.as_millis() as u64));
            }
            logger.emit(Some(step), "tool_result", result_event);

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
    logger.emit(
        None,
        "task_end",
        json!({
            "run": run,
            "outcome": "max_steps",
            "steps": cfg.max_steps,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "history_len": history.len(),
            "added_messages": history.len() - history_len_before,
        }),
    );
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
