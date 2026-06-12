use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use codex_client::ByteStream;
use codex_client::StreamResponse;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::debug;

/// Spawns an Anthropic SSE stream parser that converts Anthropic server-sent
/// events into the shared `ResponseEvent` enum consumed by codex-core.
pub fn spawn_anthropic_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
) -> ResponseStream {
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);

    tokio::spawn(async move {
        process_anthropic_sse(stream_response.bytes, tx_event, idle_timeout).await;
    });

    ResponseStream {
        rx_event,
        upstream_request_id: None,
    }
}

/// A single Anthropic SSE event, deserialized from the `data:` line.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicStreamEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: AnthropicMessageMeta },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: u32,
        content_block: AnthropicContentBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u32, delta: AnthropicDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop,
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: AnthropicMessageDelta,
        usage: Option<AnthropicUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "error")]
    Error { error: AnthropicErrorBody },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessageMeta {
    id: String,
    model: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "thinking")]
    Thinking,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
}

#[derive(Debug, Deserialize)]
struct AnthropicMessageDelta {
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorBody {
    message: String,
    #[serde(rename = "type")]
    error_type: String,
}

async fn process_anthropic_sse(
    bytes: ByteStream,
    tx: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
) {
    let mut stream = bytes.eventsource();

    let mut response_id: Option<String> = None;
    let mut token_usage: Option<codex_protocol::protocol::TokenUsage> = None;
    let mut end_turn: Option<bool> = None;
    let mut block_index_to_item_id: HashMap<u32, String> = HashMap::new();

    loop {
        match timeout(idle_timeout, stream.next()).await {
            Ok(Some(Ok(ev))) => {
                let event: AnthropicStreamEvent = match serde_json::from_str(&ev.data) {
                    Ok(e) => e,
                    Err(e) => {
                        debug!("failed to parse anthropic SSE event: {e}");
                        continue;
                    }
                };

                match event {
                    AnthropicStreamEvent::MessageStart { message } => {
                        response_id = Some(message.id.clone());
                        let _ = tx.send(Ok(ResponseEvent::ServerModel(message.model))).await;
                    }
                    AnthropicStreamEvent::ContentBlockStart {
                        index,
                        content_block,
                    } => match content_block {
                        AnthropicContentBlock::Text => {
                            let item_id = format!("msg_{index}");
                            block_index_to_item_id.insert(index, item_id);
                        }
                        AnthropicContentBlock::ToolUse { id, name, input } => {
                            let item_id = format!("tool_{index}");
                            block_index_to_item_id.insert(index, item_id.clone());
                            let function_call =
                                codex_protocol::models::ResponseItem::FunctionCall {
                                    id: Some(item_id),
                                    name,
                                    namespace: None,
                                    arguments: input.to_string(),
                                    call_id: id.clone(),
                                };
                            let _ = tx
                                .send(Ok(ResponseEvent::OutputItemAdded(function_call)))
                                .await;
                        }
                        AnthropicContentBlock::Thinking => {
                            let item_id = format!("reasoning_{index}");
                            block_index_to_item_id.insert(index, item_id.clone());
                            let reasoning = codex_protocol::models::ResponseItem::Reasoning {
                                id: item_id,
                                summary: Vec::new(),
                                content: None,
                                encrypted_content: None,
                            };
                            let _ = tx.send(Ok(ResponseEvent::OutputItemAdded(reasoning))).await;
                        }
                    },
                    AnthropicStreamEvent::ContentBlockDelta { index, delta } => match delta {
                        AnthropicDelta::TextDelta { text } => {
                            let _ = tx.send(Ok(ResponseEvent::OutputTextDelta(text))).await;
                        }
                        AnthropicDelta::InputJsonDelta { partial_json } => {
                            let item_id = block_index_to_item_id
                                .get(&index)
                                .cloned()
                                .unwrap_or_else(|| format!("tool_{index}"));
                            let _ = tx
                                .send(Ok(ResponseEvent::ToolCallInputDelta {
                                    item_id,
                                    call_id: None,
                                    delta: partial_json,
                                }))
                                .await;
                        }
                        AnthropicDelta::ThinkingDelta { thinking } => {
                            let _ = tx
                                .send(Ok(ResponseEvent::ReasoningContentDelta {
                                    delta: thinking,
                                    content_index: index as i64,
                                }))
                                .await;
                        }
                    },
                    AnthropicStreamEvent::ContentBlockStop => {}
                    AnthropicStreamEvent::MessageDelta { delta, usage } => {
                        end_turn = delta
                            .stop_reason
                            .map(|reason| reason == "end_turn" || reason == "stop_sequence");
                        if let Some(usage) = usage {
                            token_usage = Some(codex_protocol::protocol::TokenUsage {
                                input_tokens: usage.input_tokens.unwrap_or(0),
                                cached_input_tokens: 0,
                                output_tokens: usage.output_tokens.unwrap_or(0),
                                reasoning_output_tokens: 0,
                                total_tokens: usage.input_tokens.unwrap_or(0)
                                    + usage.output_tokens.unwrap_or(0),
                            });
                        }
                    }
                    AnthropicStreamEvent::MessageStop => {
                        let _ = tx
                            .send(Ok(ResponseEvent::Completed {
                                response_id: response_id.unwrap_or_default(),
                                token_usage: token_usage.clone(),
                                end_turn,
                            }))
                            .await;
                        return;
                    }
                    AnthropicStreamEvent::Error { error } => {
                        let _ = tx
                            .send(Err(ApiError::Stream(format!(
                                "Anthropic API error: {} ({})",
                                error.message, error.error_type
                            ))))
                            .await;
                        return;
                    }
                    AnthropicStreamEvent::Unknown => {}
                }
            }
            Ok(Some(Err(e))) => {
                let _ = tx
                    .send(Err(ApiError::Stream(format!("SSE stream error: {e}"))))
                    .await;
                return;
            }
            Ok(None) => {
                let _ = tx
                    .send(Err(ApiError::Stream(
                        "stream closed before completion".into(),
                    )))
                    .await;
                return;
            }
            Err(_) => {
                let _ = tx
                    .send(Err(ApiError::Stream("stream idle timeout".into())))
                    .await;
                return;
            }
        }
    }
}
