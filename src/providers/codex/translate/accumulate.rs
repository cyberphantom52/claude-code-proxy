use serde_json::Value;

use crate::traffic::TrafficCapture;

use super::live_stream::{BUFFERED_TOOL_MAX_ARGS_BYTES, CompletedFunctionCallValidationError};
use super::reducer::{
    AnthropicUsage, FinishMetadata, ReducerEvent, UpstreamStreamError,
    map_codex_usage_to_anthropic, reduce_upstream_bytes,
};
use super::web_search_compat::{WebSearchCompatContent, build_web_search_compat_blocks};

pub fn accumulate_response_with_traffic(
    upstream: &[u8],
    message_id: &str,
    model: &str,
    traffic: Option<&TrafficCapture>,
) -> Result<Value, anyhow::Error> {
    accumulate_response_and_metadata_with_traffic(upstream, message_id, model, traffic)
        .map(|(response, _)| response)
}

pub fn accumulate_response_and_metadata_with_traffic(
    upstream: &[u8],
    message_id: &str,
    model: &str,
    traffic: Option<&TrafficCapture>,
) -> Result<(Value, Option<FinishMetadata>), anyhow::Error> {
    let events = match reduce_upstream_bytes(upstream) {
        Ok(events) => events,
        Err(err) => {
            write_reducer_error_capture(traffic, &err);
            return Err(anyhow::anyhow!(
                "upstream error: {} ({:?})",
                err.message,
                err.kind
            ));
        }
    };

    let mut blocks: Vec<AccumulatedBlock> = Vec::new();
    let mut stop_reason = None;
    let mut metadata = None;
    let mut usage: Option<AnthropicUsage> = None;
    let compat_blocks = if events
        .iter()
        .any(|event| matches!(event, ReducerEvent::WebSearch { .. }))
    {
        let mut text = String::new();
        for event in &events {
            if let ReducerEvent::TextDelta { text: delta, .. } = event {
                text.push_str(delta);
            }
        }
        build_web_search_compat_blocks(&events, &text)
    } else {
        Vec::new()
    };

    enum BlockKind {
        Thinking {
            text: String,
            signature: String,
        },
        Text {
            text: String,
        },
        Tool {
            id: String,
            name: String,
            args: String,
        },
    }

    struct AccumulatedBlock {
        index: usize,
        kind: BlockKind,
    }

    for event in events {
        match event {
            ReducerEvent::WebSearch { .. } => {}
            ReducerEvent::ThinkingStart { index } => {
                blocks.push(AccumulatedBlock {
                    index,
                    kind: BlockKind::Thinking {
                        text: String::new(),
                        signature: String::new(),
                    },
                });
            }
            ReducerEvent::ThinkingDelta { index, text } => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| b.index == index)
                    && let BlockKind::Thinking { text: t, .. } = &mut block.kind
                {
                    if t.is_empty() {
                        *t = text;
                    } else {
                        t.push_str(&text);
                    }
                }
            }
            ReducerEvent::ThinkingSignature { index, signature } => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| b.index == index)
                    && let BlockKind::Thinking { signature: s, .. } = &mut block.kind
                {
                    *s = signature;
                }
            }
            ReducerEvent::TextStart { index } => {
                blocks.push(AccumulatedBlock {
                    index,
                    kind: BlockKind::Text {
                        text: String::new(),
                    },
                });
            }
            ReducerEvent::TextDelta { index, text } => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| b.index == index)
                    && let BlockKind::Text { text: t } = &mut block.kind
                {
                    if t.is_empty() {
                        *t = text;
                    } else {
                        t.push_str(&text);
                    }
                }
            }
            ReducerEvent::ToolStart { index, id, name } => {
                blocks.push(AccumulatedBlock {
                    index,
                    kind: BlockKind::Tool {
                        id,
                        name,
                        args: String::new(),
                    },
                });
            }
            ReducerEvent::ToolDelta {
                index,
                partial_json,
            } => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| b.index == index)
                    && let BlockKind::Tool { args, .. } = &mut block.kind
                {
                    if args.is_empty() {
                        *args = partial_json;
                    } else {
                        args.push_str(&partial_json);
                    }
                }
            }
            ReducerEvent::Finish {
                stop_reason: sr,
                usage: u,
                web_search_requests,
                continuation_eligible,
                response_id,
                output_items,
                ..
            } => {
                stop_reason = Some(sr);
                let ws = Some(web_search_requests).filter(|n| *n > 0);
                usage = Some(map_codex_usage_to_anthropic(&u, ws));
                metadata = Some(FinishMetadata {
                    continuation_eligible,
                    response_id,
                    output_items,
                });
            }
        }
    }

    let mut indexed_content: Vec<(usize, Value)> = Vec::new();

    for block in compat_blocks {
        let content = match block.content {
            WebSearchCompatContent::ServerToolUse { id, name, input } => content_block(
                "server_tool_use",
                [
                    ("id", Value::String(id)),
                    ("name", Value::String(name)),
                    ("input", input),
                ],
            ),
            WebSearchCompatContent::WebSearchToolResult {
                tool_use_id,
                content: results,
            } => {
                let results = results
                    .into_iter()
                    .map(|result| {
                        content_block(
                            "web_search_result",
                            [
                                ("title", Value::String(result.title)),
                                ("url", Value::String(result.url)),
                            ],
                        )
                    })
                    .collect();
                content_block(
                    "web_search_tool_result",
                    [
                        ("tool_use_id", Value::String(tool_use_id)),
                        ("content", Value::Array(results)),
                    ],
                )
            }
        };
        indexed_content.push((block.index, content));
    }

    for block in blocks {
        let content = match block.kind {
            BlockKind::Thinking { text, signature } => {
                if text.is_empty() && signature.is_empty() {
                    continue;
                }
                content_block(
                    "thinking",
                    [
                        ("thinking", Value::String(text)),
                        ("signature", Value::String(signature)),
                    ],
                )
            }
            BlockKind::Text { text } => {
                if text.is_empty() {
                    continue;
                }
                content_block("text", [("text", Value::String(text))])
            }
            BlockKind::Tool { id, name, args } => {
                // Read sanitization can expand serialized arguments after the
                // reducer checked their upstream size.
                if args.len() > BUFFERED_TOOL_MAX_ARGS_BYTES {
                    return Err(anyhow::anyhow!(
                        "{}",
                        CompletedFunctionCallValidationError::TooLarge { tool: name }
                    ));
                }
                // The reducer checked identity, object shape and agreement with
                // the authoritative completion (including Read repair).
                let parsed = serde_json::from_str(&args)?;
                content_block(
                    "tool_use",
                    [
                        ("id", Value::String(id)),
                        ("name", Value::String(name)),
                        ("input", parsed),
                    ],
                )
            }
        };
        indexed_content.push((block.index, content));
    }

    indexed_content.sort_by_key(|(index, _)| *index);
    let content: Vec<Value> = indexed_content
        .into_iter()
        .map(|(_, content)| content)
        .collect();

    let mut response = serde_json::json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [],
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage.unwrap_or_default(),
    });

    response["content"] = Value::Array(content);
    Ok((response, metadata))
}

fn content_block<const N: usize>(kind: &str, fields: [(&str, Value); N]) -> Value {
    let mut object = serde_json::Map::from_iter(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value)),
    );
    object.insert("type".to_owned(), Value::String(kind.to_owned()));
    Value::Object(object)
}

fn write_reducer_error_capture(traffic: Option<&TrafficCapture>, err: &UpstreamStreamError) {
    let Some(traffic) = traffic else {
        return;
    };
    traffic.write_json(
        "060-codex-reducer-error",
        &serde_json::json!({
            "kind": format!("{:?}", err.kind),
            "message": err.message,
            "retryAfterSeconds": err.retry_after_seconds,
            "diagnostics": err.diagnostics,
        }),
    );
}
