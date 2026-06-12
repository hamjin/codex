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

/// Spawns a Chat Completions SSE stream parser that converts Chat Completions
/// streaming chunks into the shared `ResponseEvent` enum consumed by codex-core.
pub fn spawn_chat_completions_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
) -> ResponseStream {
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);

    tokio::spawn(async move {
        process_chat_completions_sse(stream_response.bytes, tx_event, idle_timeout).await;
    });

    ResponseStream {
        rx_event,
        upstream_request_id: None,
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletionsChunk {
    id: Option<String>,
    model: Option<String>,
    choices: Option<Vec<ChatChoice>>,
    usage: Option<ChatUsage>,
    error: Option<ChatError>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    delta: Option<ChatDelta>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatDelta {
    content: Option<String>,
    tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatToolCall {
    index: Option<u32>,
    id: Option<String>,
    function: Option<ChatFunctionCall>,
}

#[derive(Debug, Deserialize)]
struct ChatFunctionCall {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatUsage {
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    total_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ChatError {
    message: String,
}

async fn process_chat_completions_sse(
    bytes: ByteStream,
    tx: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
) {
    let mut stream = bytes.eventsource();

    let mut response_id: Option<String> = None;
    let mut token_usage: Option<codex_protocol::protocol::TokenUsage> = None;
    let mut end_turn: Option<bool> = None;
    let mut tool_call_builders: HashMap<u32, ToolCallBuilder> = HashMap::new();

    loop {
        match timeout(idle_timeout, stream.next()).await {
            Ok(Some(Ok(ev))) => {
                let data = ev.data.trim().to_string();
                if data == "[DONE]" {
                    let _ = tx
                        .send(Ok(ResponseEvent::Completed {
                            response_id: response_id.unwrap_or_default(),
                            token_usage: token_usage.take(),
                            end_turn,
                        }))
                        .await;
                    return;
                }

                let chunk: ChatCompletionsChunk = match serde_json::from_str(&data) {
                    Ok(c) => c,
                    Err(e) => {
                        debug!("failed to parse chat completions chunk: {e}");
                        continue;
                    }
                };

                if let Some(error) = chunk.error {
                    let _ = tx
                        .send(Err(ApiError::Stream(format!(
                            "Chat Completions API error: {}",
                            error.message
                        ))))
                        .await;
                    return;
                }

                if response_id.is_none() {
                    response_id = chunk.id.clone();
                }

                if let Some(model) = chunk.model {
                    let _ = tx.send(Ok(ResponseEvent::ServerModel(model))).await;
                }

                for choice in chunk.choices.iter().flatten() {
                    if let Some(delta) = &choice.delta {
                        if let Some(content) = &delta.content {
                            let _ = tx
                                .send(Ok(ResponseEvent::OutputTextDelta(content.clone())))
                                .await;
                        }

                        if let Some(reasoning) = &delta.reasoning_content
                            && !reasoning.is_empty()
                        {
                            let _ = tx
                                .send(Ok(ResponseEvent::ReasoningContentDelta {
                                    delta: reasoning.clone(),
                                    content_index: 0,
                                }))
                                .await;
                        }

                        if let Some(tool_calls) = &delta.tool_calls {
                            for tool_call in tool_calls {
                                let idx = tool_call.index.unwrap_or(0);
                                let builder = tool_call_builders.entry(idx).or_default();

                                if let Some(id) = &tool_call.id {
                                    builder.id = Some(id.clone());
                                }
                                if let Some(func) = &tool_call.function {
                                    if let Some(name) = &func.name {
                                        builder.name = Some(name.clone());
                                        let call_id = builder
                                            .id
                                            .clone()
                                            .unwrap_or_else(|| format!("call_{idx}"));
                                        let function_call =
                                            codex_protocol::models::ResponseItem::FunctionCall {
                                                id: Some(format!("func_{idx}")),
                                                name: name.clone(),
                                                namespace: None,
                                                arguments: String::new(),
                                                call_id,
                                            };
                                        let _ = tx
                                            .send(Ok(ResponseEvent::OutputItemAdded(function_call)))
                                            .await;
                                    }
                                    if let Some(args) = &func.arguments {
                                        let item_id = builder
                                            .id
                                            .clone()
                                            .unwrap_or_else(|| format!("call_{idx}"));
                                        let call_id = builder.id.clone();
                                        let _ = tx
                                            .send(Ok(ResponseEvent::ToolCallInputDelta {
                                                item_id,
                                                call_id,
                                                delta: args.clone(),
                                            }))
                                            .await;
                                    }
                                }
                            }
                        }
                    }

                    if let Some(finish_reason) = &choice.finish_reason {
                        end_turn = Some(finish_reason == "stop" || finish_reason == "end_turn");
                    }
                }

                if let Some(usage) = &chunk.usage {
                    token_usage = Some(codex_protocol::protocol::TokenUsage {
                        input_tokens: usage.prompt_tokens.unwrap_or(0),
                        cached_input_tokens: 0,
                        output_tokens: usage.completion_tokens.unwrap_or(0),
                        reasoning_output_tokens: 0,
                        total_tokens: usage.total_tokens.unwrap_or(0),
                    });
                }
            }
            Ok(Some(Err(e))) => {
                let _ = tx
                    .send(Err(ApiError::Stream(format!("SSE stream error: {e}"))))
                    .await;
                return;
            }
            Ok(None) => {
                if token_usage.is_some() || response_id.is_some() {
                    let _ = tx
                        .send(Ok(ResponseEvent::Completed {
                            response_id: response_id.unwrap_or_default(),
                            token_usage: token_usage.take(),
                            end_turn,
                        }))
                        .await;
                }
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

#[derive(Default)]
struct ToolCallBuilder {
    id: Option<String>,
    name: Option<String>,
}
