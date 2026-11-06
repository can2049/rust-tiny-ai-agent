//! 会话消息的中立数据结构。
//!
//! 这里的类型只表达"一次会话里发生过什么"，与具体 HTTP API 的线上格式无关；
//! 线上格式的转换集中在 `llm.rs`，便于以后换模型提供方。

use serde::Serialize;
use serde_json::Value;

/// 一条消息的发出方。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// 用户（任务或工具结果回传）。
    User,
    /// 模型。
    Assistant,
    /// 工具结果（在线上格式中会展开成多条 tool 消息）。
    Tool,
}

/// 消息中的一个内容片段。
#[derive(Clone, Debug)]
pub enum Part {
    /// 纯文本。
    Text(String),
    /// 模型要求调用某个工具。
    ToolUse {
        /// 本次调用的唯一 id，回传结果时必须带上。
        id: String,
        /// 工具名。
        name: String,
        /// 工具参数（已解析为 JSON 对象）。
        input: Value,
    },
    /// 工具的执行结果。
    ToolResult {
        /// 对应 `Part::ToolUse` 的 id。
        tool_use_id: String,
        /// 结果文本（失败时是错误文本）。
        content: String,
        /// 该结果是否是错误。
        is_error: bool,
    },
}

/// 一条会话消息。
#[derive(Clone, Debug)]
pub struct Message {
    /// 发出方。
    pub role: Role,
    /// 内容片段，顺序即展示顺序。
    pub parts: Vec<Part>,
}

impl Message {
    /// 构造一条用户文本消息。
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            parts: vec![Part::Text(text.into())],
        }
    }

    /// 构造一条模型消息（可含文字与工具调用）。
    pub fn assistant(parts: Vec<Part>) -> Self {
        Self {
            role: Role::Assistant,
            parts,
        }
    }

    /// 构造一条承载工具结果的消息（可含多个结果）。
    pub fn tool_results(results: Vec<Part>) -> Self {
        Self {
            role: Role::Tool,
            parts: results,
        }
    }

    /// 把消息里所有文本片段拼起来（用于取出最终回答）。
    pub fn text(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            if let Part::Text(t) = part {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
        out
    }
}

/// 提供给模型的工具定义（名称、说明、参数 JSON Schema）。
#[derive(Clone, Debug, Serialize)]
pub struct ToolDefinition {
    /// 工具名，模型用它来发起调用。
    pub name: String,
    /// 面向模型的说明：什么时候该用、参数格式示例。
    pub description: String,
    /// 参数的 JSON Schema。
    pub input_schema: Value,
}

/// 模型一次回复的结果。
#[derive(Clone, Debug)]
pub struct AssistantTurn {
    /// 回复的内容片段（文字 + 可能的工具调用）。
    pub parts: Vec<Part>,
    /// 提供方给出的结束原因，例如 `tool_calls` / `stop`。
    pub stop_reason: String,
}

impl AssistantTurn {
    /// 取出本次回复里的所有工具调用（顺序即模型给出的顺序）。
    pub fn tool_calls(&self) -> Vec<(String, String, Value)> {
        self.parts
            .iter()
            .filter_map(|part| match part {
                Part::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_joins_text_parts() {
        let msg = Message::assistant(vec![
            Part::Text("甲".into()),
            Part::ToolUse {
                id: "1".into(),
                name: "calculator".into(),
                input: Value::Null,
            },
            Part::Text("乙".into()),
        ]);
        assert_eq!(msg.text(), "甲\n乙");
    }

    #[test]
    fn tool_calls_extracts_in_order() {
        let turn = AssistantTurn {
            parts: vec![
                Part::Text("我先查一下".into()),
                Part::ToolUse {
                    id: "a".into(),
                    name: "get_weather".into(),
                    input: serde_json::json!({"city": "香港"}),
                },
            ],
            stop_reason: "tool_calls".into(),
        };
        let calls = turn.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "a");
        assert_eq!(calls[0].1, "get_weather");
    }
}