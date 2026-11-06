//! 命令行入口：多轮对话循环，输入 `exit` 退出。

mod agent;
mod config;
mod llm;
mod logger;
mod tools;
mod types;

use std::io::{self, Write};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::json;

use crate::config::Config;
use crate::llm::Llm;
use crate::logger::Logger;
use crate::tools::Registry;
use crate::types::Message;

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("启动失败：{e:#}");
        std::process::exit(1);
    }
}

/// 组装配置与客户端，然后进入对话循环。
async fn real_main() -> Result<()> {
    // .env 可选：没有也不影响，只要环境变量里给了 Key。
    let _ = dotenvy::dotenv();

    let cfg = Config::from_env()?;
    std::fs::create_dir_all(&cfg.workspace)
        .with_context(|| format!("创建工作目录失败：{}", cfg.workspace.display()))?;
    let workspace = std::fs::canonicalize(&cfg.workspace)
        .with_context(|| format!("规范化工作目录失败：{}", cfg.workspace.display()))?;

    let llm = Llm::new(&cfg)?;
    let tools = Arc::new(Registry::with_builtins(&workspace));
    let logger = Arc::new(Logger::new(&cfg)?);
    logger.emit(
        None,
        "session_start",
        json!({
            "model": cfg.model,
            "base_url": cfg.base_url,
            "max_steps": cfg.max_steps,
            "max_tokens": cfg.max_tokens,
            "workspace": workspace.display().to_string(),
            "log_enabled": cfg.log_enabled,
            "system_prompt": logger.clip(&cfg.system_prompt),
        }),
    );

    println!("tiny_ai_agent 已就绪（模型 {}，最大循环 {} 步）", cfg.model, cfg.max_steps);
    println!("工作目录：{}", workspace.display());
    if let Some(path) = logger.path() {
        println!("运行日志：{}", path.display());
    }
    println!("输入任务开始对话，输入 exit 退出。\n");

    let mut history: Vec<Message> = Vec::new();

    loop {
        print!("你> ");
        io::stdout().flush().ok();

        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => {
                println!();
                break; // Ctrl+D / EOF
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("读取输入失败：{e}");
                break;
            }
        }

        let task = line.trim();
        if task.is_empty() {
            continue;
        }
        if task.eq_ignore_ascii_case("exit") || task.eq_ignore_ascii_case("quit") {
            println!("再见。");
            break;
        }

        // 中断时把历史回滚到任务开始前，避免留下半截对话。
        let snapshot = history.clone();
        let result = tokio::select! {
            r = agent::run_agent(task, &llm, &tools, &logger, &cfg, &mut history) => Some(r),
            _ = tokio::signal::ctrl_c() => None,
        };

        match result {
            Some(Ok(answer)) => println!("\n助手> {answer}\n"),
            Some(Err(e)) => {
                history = snapshot;
                eprintln!("\n[错误] 本次任务失败：{e:#}\n");
            }
            None => {
                history = snapshot;
                logger.emit(
                    None,
                    "task_end",
                    json!({ "outcome": "interrupted", "task": task }),
                );
                println!("\n[已中断] 本次任务已取消，可以继续输入下一个任务。\n");
            }
        }
    }

    logger.emit(None, "session_end", json!({ "history_len": history.len() }));
    if let Some(path) = logger.path() {
        println!("本次运行日志：{}", path.display());
    }

    Ok(())
}