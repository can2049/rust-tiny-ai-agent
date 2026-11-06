//! 运行配置：模型名、最大步数、工作目录、系统提示词。
//!
//! 所有配置项都可以用环境变量覆盖，默认值指向 `data/model.txt` 里给出的
//! DeepSeek 兼容端点，方便开箱即用。

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// 默认的 API 根地址占位符。
///
/// 这里刻意不写死任何真实服务地址：请用 `LLM_BASE_URL` 环境变量指向你要用的
/// OpenAI 兼容端点（DashScope、DeepSeek、本地 vLLM、OpenRouter 等都可以）。
pub const DEFAULT_BASE_URL: &str = "https://your-api-endpoint/v1";

/// 默认的系统提示词：说明角色、何时用工具、何时直接回答。
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
你是一个运行在开发者本机的命令行 AI agent。

角色：你要准确、简洁地完成用户交给你的任务。

何时使用工具：
- 需要精确计算（算术、括号、小数）时，用 calculator，不要自己心算。
- 需要实时信息（当前天气等）时，用 get_weather，不要凭记忆编造。
- 需要读取或保存文件时，用 read_file / write_file，路径使用相对于工作目录的相对路径。

何时直接回答：
- 闲聊、常识解释、以及你已经能从对话历史中得到答案时，直接用自然语言回答，不要调用工具。

其他要求：
- 可以连续调用多个工具；每一步只做一件事，拿到结果后再决定下一步。
- 工具返回错误时，读错误信息并自行修正参数重试或换一种做法，不要假装成功。
- 最终回答用中文，直接给出结论（例如具体数值、文件是否写入成功），不要复述工具日志。";

/// Agent 的运行时配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// 模型名。
    pub model: String,
    /// 兼容 OpenAI 的 API 根地址（不带 `/chat/completions`）。
    pub base_url: String,
    /// API Key，只从环境变量读取，不写进代码。
    pub api_key: String,
    /// 单个任务的 API 循环上限。
    pub max_steps: usize,
    /// 单次回复的最大 token 数。
    pub max_tokens: u32,
    /// 文件工具唯一可访问的目录。
    pub workspace: PathBuf,
    /// 系统提示词。
    pub system_prompt: String,
    /// 是否打印每一步的日志（`AGENT_LOG=0` 关闭）。
    pub log_enabled: bool,
    /// 运行日志（JSONL）的输出目录（`AGENT_LOG_DIR`，默认 `logs`）。
    pub log_dir: PathBuf,
    /// 是否写运行日志文件（`AGENT_LOG_FILE=0` 关闭）。
    pub log_file_enabled: bool,
    /// 运行日志里单个长文本字段最多保留的字符数（`AGENT_LOG_CONTENT_LIMIT`）。
    pub log_content_limit: usize,
}

impl Config {
    /// 从环境变量（含 `.env`）读取配置，缺 API Key 时报错。
    pub fn from_env() -> Result<Self> {
        let api_key = env::var("LLM_API_KEY")
            .or_else(|_| env::var("ANTHROPIC_API_KEY"))
            .context("缺少环境变量 LLM_API_KEY（请写进 .env 或 export 后重试）")?;

        let base_url = env::var("LLM_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let base_url = base_url.trim_end_matches('/').to_string();

        let model = env::var("LLM_MODEL").unwrap_or_else(|_| "deepseek-v4.1-flash".to_string());

        let max_steps = match env::var("AGENT_MAX_STEPS") {
            Ok(v) => v
                .trim()
                .parse::<usize>()
                .with_context(|| format!("AGENT_MAX_STEPS 不是合法整数：{v}"))?,
            Err(_) => 10,
        };
        if max_steps == 0 {
            anyhow::bail!("AGENT_MAX_STEPS 必须大于 0");
        }

        let max_tokens = match env::var("AGENT_MAX_TOKENS") {
            Ok(v) => v
                .trim()
                .parse::<u32>()
                .with_context(|| format!("AGENT_MAX_TOKENS 不是合法整数：{v}"))?,
            Err(_) => 4096,
        };

        let workspace = env::var("AGENT_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("workspace"));

        let system_prompt = env::var("AGENT_SYSTEM_PROMPT")
            .unwrap_or_else(|_| DEFAULT_SYSTEM_PROMPT.to_string());

        let log_enabled = env::var("AGENT_LOG")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                !(v == "0" || v == "false" || v == "off" || v == "no")
            })
            .unwrap_or(true);

        let log_dir = env::var("AGENT_LOG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("logs"));

        let log_file_enabled = env::var("AGENT_LOG_FILE")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                !(v == "0" || v == "false" || v == "off" || v == "no")
            })
            .unwrap_or(true);

        let log_content_limit = match env::var("AGENT_LOG_CONTENT_LIMIT") {
            Ok(v) => v
                .trim()
                .parse::<usize>()
                .with_context(|| format!("AGENT_LOG_CONTENT_LIMIT 不是合法整数：{v}"))?,
            Err(_) => 4000,
        };

        Ok(Self {
            model,
            base_url,
            api_key,
            max_steps,
            max_tokens,
            workspace,
            system_prompt,
            log_enabled,
            log_dir,
            log_file_enabled,
            log_content_limit,
        })
    }

    /// 完整的 chat completions 地址。
    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_url_appends_path() {
        let cfg = Config {
            model: "m".into(),
            base_url: "https://example.com/v1".into(),
            api_key: "k".into(),
            max_steps: 3,
            max_tokens: 100,
            workspace: PathBuf::from("workspace"),
            system_prompt: String::new(),
            log_enabled: true,
            log_dir: PathBuf::from("logs"),
            log_file_enabled: true,
            log_content_limit: 4000,
        };
        assert_eq!(cfg.chat_url(), "https://example.com/v1/chat/completions");
    }

    #[test]
    fn system_prompt_mentions_three_points() {
        assert!(DEFAULT_SYSTEM_PROMPT.contains("角色"));
        assert!(DEFAULT_SYSTEM_PROMPT.contains("何时使用工具"));
        assert!(DEFAULT_SYSTEM_PROMPT.contains("何时直接回答"));
    }
}