//! 模型客户端：请求/响应结构、线上格式转换与重试逻辑。
//!
//! 目标端点兼容 OpenAI 的 `POST {base_url}/chat/completions`（tool use 走
//! `tools` + `tool_calls` 字段）。所有"中立类型 ↔ 线上格式"的转换都集中在这里。

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::Config;
use crate::logger::Logger;
use crate::types::{AssistantTurn, Message, Part, Role, ToolDefinition};

/// 失败后的最大重试次数（不含首次请求）。
const MAX_RETRIES: u32 = 2;

/// 错误信息里最多保留的响应体字符数。
const ERROR_BODY_LIMIT: usize = 400;

/// 一次 API 调用的客户端。
pub struct Llm {
    client: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl Llm {
    /// 用配置构造客户端，并设置整体超时。
    pub fn new(cfg: &Config) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .context("创建 HTTP 客户端失败")?;
        Ok(Self {
            client,
            url: cfg.chat_url(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
        })
    }

    /// 发送一次对话请求；网络错误、429、5xx 最多重试 2 次。
    pub async fn send(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        logger: Option<&Logger>,
    ) -> Result<AssistantTurn> {
        let body = ChatRequest {
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            messages: to_wire_messages(system, messages),
            tools: tools
                .iter()
                .map(|t| WireTool {
                    kind: "function",
                    function: t.clone(),
                })
                .collect(),
        };

        if let Some(log) = logger {
            // 完整请求体单独放在 request 字段，便于复盘"模型到底看到了什么"。
            let serialized = serde_json::to_string(&body).unwrap_or_default();
            log.emit(
                None,
                "llm_request",
                json!({
                    "url": self.url,
                    "model": self.model,
                    "max_tokens": self.max_tokens,
                    "message_count": body.messages.len(),
                    "tool_names": tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
                    "system_prompt": log.clip(system),
                    "request_bytes": serialized.len(),
                    "request": serialized,
                }),
            );
        }

        let mut last_error = String::new();
        for attempt in 0..=MAX_RETRIES {
            let started = std::time::Instant::now();
            match self.post_once(&body, logger).await {
                Ok(turn) => {
                    if let Some(log) = logger {
                        log.emit(
                            None,
                            "llm_response",
                            json!({
                                "attempt": attempt + 1,
                                "elapsed_ms": started.elapsed().as_millis() as u64,
                                "stop_reason": turn.stop_reason,
                                "tool_call_count": turn.tool_calls().len(),
                                "text": log.clip(&turn.text()),
                            }),
                        );
                    }
                    return Ok(turn);
                }
                Err(err) if err.retryable && attempt < MAX_RETRIES => {
                    last_error = err.message;
                    let wait = std::time::Duration::from_millis(800 * u64::pow(2, attempt));
                    eprintln!(
                        "[llm] 第 {} 次请求失败（{}），{:?} 后重试…",
                        attempt + 1,
                        last_error,
                        wait
                    );
                    if let Some(log) = logger {
                        log.emit(
                            None,
                            "llm_retry",
                            json!({
                                "attempt": attempt + 1,
                                "retryable": true,
                                "error": last_error,
                                "wait_ms": wait.as_millis() as u64,
                                "elapsed_ms": started.elapsed().as_millis() as u64,
                            }),
                        );
                    }
                    tokio::time::sleep(wait).await;
                }
                Err(err) => {
                    last_error = err.message;
                    if let Some(log) = logger {
                        log.emit(
                            None,
                            "llm_error",
                            json!({
                                "attempt": attempt + 1,
                                "retryable": err.retryable,
                                "error": last_error,
                                "elapsed_ms": started.elapsed().as_millis() as u64,
                            }),
                        );
                    }
                    break;
                }
            }
        }
        bail!("模型 API 调用失败：{last_error}")
    }

    /// 发一次请求，把失败归类为"可重试/不可重试"。
    async fn post_once(
        &self,
        body: &ChatRequest,
        logger: Option<&Logger>,
    ) -> std::result::Result<AssistantTurn, ApiError> {
        let resp = self
            .client
            .post(&self.url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| ApiError::retryable(format!("网络错误：{e}")))?;

        let status = resp.status();
        let text = resp.text().await.map_err(|e| ApiError {
            message: format!("读取响应体失败：{e}"),
            retryable: false,
        })?;

        if !status.is_success() {
            let retryable = is_retryable_status(status) || looks_like_transient_rejection(&text);
            if let Some(log) = logger {
                log.emit(
                    None,
                    "llm_http_error",
                    json!({
                        "status": status.as_u16(),
                        "retryable": retryable,
                        "body": truncate(&text, ERROR_BODY_LIMIT),
                        "body_bytes": text.len(),
                    }),
                );
            }
            return Err(ApiError {
                message: format!("HTTP {status}：{}", truncate(&text, ERROR_BODY_LIMIT)),
                retryable,
            });
        }

        if let Some(log) = logger {
            log.emit(
                None,
                "llm_http_response",
                json!({
                    "status": status.as_u16(),
                    "body_bytes": text.len(),
                    "usage": parse_usage(&text),
                }),
            );
        }

        let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| ApiError {
            message: format!(
                "响应 JSON 解析失败：{e}；原文：{}",
                truncate(&text, ERROR_BODY_LIMIT)
            ),
            retryable: false,
        })?;

        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::retryable("响应中没有 choices".to_string()))?;

        let mut parts = Vec::new();
        if let Some(content) = choice.message.content
            && !content.trim().is_empty()
        {
            parts.push(Part::Text(content));
        }
        for call in choice.message.tool_calls.unwrap_or_default() {
            parts.push(Part::ToolUse {
                id: call.id,
                name: call.function.name,
                input: parse_arguments(&call.function.arguments),
            });
        }

        if parts.is_empty() {
            bail_or_err("模型返回了空消息")?;
        }

        Ok(AssistantTurn {
            parts,
            stop_reason: choice.finish_reason.unwrap_or_else(|| "unknown".into()),
        })
    }
}

/// 把 `bail!` 的宏语义包装成 `ApiError`，便于在响应体异常时统一报错。
fn bail_or_err(msg: &str) -> std::result::Result<(), ApiError> {
    Err(ApiError::retryable(msg.to_string()))
}

/// 从响应体里尽力提取 token 用量（不同服务商字段可能不同，取不到就返回 null）。
fn parse_usage(raw_body: &str) -> Value {
    let Ok(value) = serde_json::from_str::<Value>(raw_body) else {
        return Value::Null;
    };
    let Some(usage) = value.get("usage") else {
        return Value::Null;
    };
    json!({
        "prompt_tokens": usage.get("prompt_tokens").cloned().unwrap_or(Value::Null),
        "completion_tokens": usage.get("completion_tokens").cloned().unwrap_or(Value::Null),
        "total_tokens": usage.get("total_tokens").cloned().unwrap_or(Value::Null),
    })
}

/// 判断 HTTP 状态码本身是否代表"过一会儿重试可能成功"。
fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || status.is_server_error()
}

/// 识别"网关/中间层瞬时拒绝"。
///
/// 有些兼容端点会在内部组件抖动时返回 400，但请求本身没问题（同样的请求
/// 稍后就能成功）。这类响应带有明显的内部错误特征，值得重试；反之，真正的
/// 参数错误（例如 `max_tokens` 非法）重试也没用，不该浪费 2 次请求。
fn looks_like_transient_rejection(body: &str) -> bool {
    const HINTS: [&str; 5] = [
        "internal maas component",
        "rejected by an internal",
        "service unavailable",
        "temporarily",
        "bad gateway",
    ];
    let lower = body.to_ascii_lowercase();
    HINTS.iter().any(|hint| lower.contains(hint))
}

/// 解析 `arguments` 字符串；失败时保留原文，让工具层报参数不合法。
fn parse_arguments(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => v,
        Ok(other) => serde_json::json!({ "__invalid_arguments": other }),
        Err(_) => serde_json::json!({ "__invalid_arguments": raw }),
    }
}

/// 截断字符串到 `limit` 个字符（按字符而非字节，避免切坏中文）。
pub fn truncate(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let head: String = s.chars().take(limit).collect();
    let total = s.chars().count();
    format!("{head}…[共 {total} 字符，已截断]")
}

/// 可重试标记的 API 错误。
struct ApiError {
    message: String,
    retryable: bool,
}

impl ApiError {
    fn retryable(message: String) -> Self {
        Self {
            message,
            retryable: true,
        }
    }
}

/// 把中立消息列表转成线上格式（含 system 消息，工具结果展开为多条 tool 消息）。
fn to_wire_messages(system: &str, messages: &[Message]) -> Vec<WireMessage> {
    let mut out = Vec::new();
    if !system.trim().is_empty() {
        out.push(WireMessage {
            role: "system".into(),
            content: Some(system.to_string()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }
    for msg in messages {
        let text = msg.text();
        match msg.role {
            Role::User => out.push(WireMessage {
                role: "user".into(),
                content: Some(text),
                tool_calls: Vec::new(),
                tool_call_id: None,
            }),
            Role::Assistant => {
                let calls: Vec<WireToolCall> = msg
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::ToolUse { id, name, input } => Some(WireToolCall {
                            id: id.clone(),
                            kind: "function",
                            function: WireCallFunction {
                                name: name.clone(),
                                arguments: input.to_string(),
                            },
                        }),
                        _ => None,
                    })
                    .collect();
                if text.trim().is_empty() && calls.is_empty() {
                    continue;
                }
                out.push(WireMessage {
                    role: "assistant".into(),
                    content: if text.trim().is_empty() {
                        None
                    } else {
                        Some(text)
                    },
                    tool_calls: calls,
                    tool_call_id: None,
                });
            }
            Role::Tool => {
                // 兼容 OpenAI 的服务要求每条工具结果单独一条 role=tool 消息。
                for part in &msg.parts {
                    if let Part::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = part
                    {
                        let content = if *is_error {
                            format!("[工具执行出错] {content}")
                        } else {
                            content.clone()
                        };
                        out.push(WireMessage {
                            role: "tool".into(),
                            content: Some(content),
                            tool_calls: Vec::new(),
                            tool_call_id: Some(tool_use_id.clone()),
                        });
                    }
                }
            }
        }
    }
    out
}

// ---------- 线上格式（OpenAI 兼容） ----------

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
}

#[derive(Serialize, Clone)]
struct WireMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize, Clone)]
struct WireTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ToolDefinition,
}

#[derive(Serialize, Clone)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireCallFunction,
}

#[derive(Serialize, Clone)]
struct WireCallFunction {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ResponseToolCall>>,
}

#[derive(Deserialize)]
struct ResponseToolCall {
    id: String,
    function: ResponseFunction,
}

#[derive(Deserialize)]
struct ResponseFunction {
    name: String,
    #[serde(default)]
    arguments: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_chars() {
        let s = "一二三四五";
        assert_eq!(truncate(s, 3), "一二三…[共 5 字符，已截断]");
        assert_eq!(truncate(s, 10), s);
    }

    #[test]
    fn tool_results_become_separate_messages() {
        let messages = vec![
            Message::user_text("hi"),
            Message::assistant(vec![
                Part::ToolUse {
                    id: "c1".into(),
                    name: "calculator".into(),
                    input: serde_json::json!({"expression": "1+1"}),
                },
                Part::ToolUse {
                    id: "c2".into(),
                    name: "calculator".into(),
                    input: serde_json::json!({"expression": "2+2"}),
                },
            ]),
            Message::tool_results(vec![
                Part::ToolResult {
                    tool_use_id: "c1".into(),
                    content: "2".into(),
                    is_error: false,
                },
                Part::ToolResult {
                    tool_use_id: "c2".into(),
                    content: "boom".into(),
                    is_error: true,
                },
            ]),
        ];
        let wire = to_wire_messages("sys", &messages);
        assert_eq!(wire.len(), 5); // system + user + assistant + 2 tool
        assert_eq!(wire[0].role, "system");
        assert_eq!(wire[2].tool_calls.len(), 2);
        assert_eq!(wire[4].content.as_deref(), Some("[工具执行出错] boom"));
        assert_eq!(wire[4].tool_call_id.as_deref(), Some("c2"));
    }

    #[test]
    fn invalid_arguments_are_marked() {
        let v = parse_arguments("not json");
        assert!(v.get("__invalid_arguments").is_some());
    }

    #[test]
    fn retryable_statuses() {
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::REQUEST_TIMEOUT));
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        // 参数真的错时不该重试
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn transient_rejection_is_recognised() {
        // 实测遇到过的真实响应体：400 但属于中间层瞬时拒绝
        let real = r#"{"error":{"message":"The request is invalid: the request was rejected by an internal MaaS component. Please check the request body, required fields, and request format. (type: invalid_request_error)"}}"#;
        assert!(looks_like_transient_rejection(real));

        let plain_param_error = r#"{"error":{"message":"max_tokens: must be greater than 0"}}"#;
        assert!(!looks_like_transient_rejection(plain_param_error));

        assert!(!looks_like_transient_rejection(""));
    }

    #[test]
    fn usage_is_extracted_when_present() {
        let body = r#"{"usage":{"prompt_tokens":12,"completion_tokens":7,"total_tokens":19}}"#;
        let usage = parse_usage(body);
        assert_eq!(usage["prompt_tokens"], 12);
        assert_eq!(usage["total_tokens"], 19);
        // 没有 usage 字段时不应 panic
        assert!(parse_usage("{}").is_null());
        assert!(parse_usage("not json").is_null());
    }
}