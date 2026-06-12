use serde::Serialize;

/// Serialized request body for the Anthropic Messages API (`POST /v1/messages`).
///
/// Maps from codex-rs internal types (ResponseItem, tools) into Anthropic's
/// wire format. Tool use content blocks and tool_result content blocks are
/// embedded in the `messages` array as `content` items.
#[derive(Debug, Serialize, Clone)]
pub struct AnthropicMessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<AnthropicSystem>,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    pub stream: bool,
}

/// System prompt for the Anthropic Messages API.
///
/// Can be a plain string or an array of content blocks for structured system prompts.
#[derive(Debug, Serialize, Clone)]
#[serde(untagged)]
pub enum AnthropicSystem {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

/// A single message in the Anthropic conversation history.
#[derive(Debug, Serialize, Clone)]
pub struct AnthropicMessage {
    pub role: AnthropicRole,
    pub content: AnthropicMessageContent,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum AnthropicRole {
    User,
    Assistant,
}

#[derive(Debug, Serialize, Clone)]
#[serde(untagged)]
pub enum AnthropicMessageContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

/// A content block within a message (text, tool_use, tool_result, etc.).
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type")]
pub enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
}

/// Tool definition for the Anthropic Messages API.
#[derive(Debug, Serialize, Clone)]
pub struct AnthropicTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}
