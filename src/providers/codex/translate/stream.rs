use crate::anthropic::sse::encode_sse_event;
use crate::traffic::TrafficCapture;

use super::reducer::{
    ReducerEvent, UpstreamStreamError, map_codex_usage_to_anthropic, reduce_upstream_bytes,
};
use super::web_search_compat::build_web_search_compat_blocks;

struct MessageMetadata<'a> {
    id: &'a str,
    model: &'a str,
    estimated_input_tokens: u64,
}

fn emit(
    out: &mut Vec<u8>,
    traffic: Option<&TrafficCapture>,
    event: &str,
    data: &serde_json::Value,
) {
    if let Some(traffic) = traffic {
        traffic.write_json_event(
            "050-downstream-event",
            &serde_json::json!({
                "event": event,
                "data": data,
            }),
        );
    }
    out.extend_from_slice(&encode_sse_event(Some(event), &data.to_string()));
}

fn ensure_message_start(
    out: &mut Vec<u8>,
    traffic: Option<&TrafficCapture>,
    message_started: &mut bool,
    message: &MessageMetadata<'_>,
) {
    if !*message_started {
        *message_started = true;
        let data = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": message.id,
                "type": "message",
                "role": "assistant",
                "model": message.model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": message.estimated_input_tokens,
                    "output_tokens": 0,
                }
            }
        });
        emit(out, traffic, "message_start", &data);
    }
}

fn emit_content_event(
    out: &mut Vec<u8>,
    traffic: Option<&TrafficCapture>,
    message_started: &mut bool,
    message: &MessageMetadata<'_>,
    event: &ReducerEvent,
) -> bool {
    match event {
        ReducerEvent::ThinkingStart { index } => {
            ensure_message_start(out, traffic, message_started, message);
            emit(
                out,
                traffic,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "thinking", "thinking": "", "signature": ""}
                }),
            );
            true
        }
        ReducerEvent::ThinkingDelta { index, text } => {
            emit(
                out,
                traffic,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "thinking_delta", "thinking": text}
                }),
            );
            true
        }
        ReducerEvent::ThinkingSignature { index, signature } => {
            emit(
                out,
                traffic,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "signature_delta", "signature": signature}
                }),
            );
            true
        }
        ReducerEvent::ThinkingStop { index } => {
            emit(
                out,
                traffic,
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": index,
                }),
            );
            true
        }
        ReducerEvent::TextStart { index } => {
            ensure_message_start(out, traffic, message_started, message);
            emit(
                out,
                traffic,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "text", "text": ""}
                }),
            );
            true
        }
        ReducerEvent::TextDelta { index, text } => {
            emit(
                out,
                traffic,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "text_delta", "text": text}
                }),
            );
            true
        }
        ReducerEvent::TextStop { index } => {
            emit(
                out,
                traffic,
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": index,
                }),
            );
            true
        }
        ReducerEvent::ToolStart { index, id, name } => {
            ensure_message_start(out, traffic, message_started, message);
            emit(
                out,
                traffic,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": {}
                    }
                }),
            );
            true
        }
        ReducerEvent::ToolDelta {
            index,
            partial_json,
        } => {
            emit(
                out,
                traffic,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": partial_json
                    }
                }),
            );
            true
        }
        ReducerEvent::ToolStop { index } => {
            emit(
                out,
                traffic,
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": index,
                }),
            );
            true
        }
        _ => false,
    }
}

fn is_content_event(event: &ReducerEvent) -> bool {
    matches!(
        event,
        ReducerEvent::ThinkingStart { .. }
            | ReducerEvent::ThinkingDelta { .. }
            | ReducerEvent::ThinkingSignature { .. }
            | ReducerEvent::ThinkingStop { .. }
            | ReducerEvent::TextStart { .. }
            | ReducerEvent::TextDelta { .. }
            | ReducerEvent::TextStop { .. }
            | ReducerEvent::ToolStart { .. }
            | ReducerEvent::ToolDelta { .. }
            | ReducerEvent::ToolStop { .. }
    )
}

pub fn translate_stream_bytes(
    upstream: &[u8],
    message_id: &str,
    model: &str,
) -> Result<Vec<u8>, anyhow::Error> {
    translate_stream_bytes_with_traffic(upstream, message_id, model, 0, None)
}

pub fn translate_stream_bytes_with_traffic(
    upstream: &[u8],
    message_id: &str,
    model: &str,
    estimated_input_tokens: u64,
    traffic: Option<&TrafficCapture>,
) -> Result<Vec<u8>, anyhow::Error> {
    let events = match reduce_upstream_bytes(upstream) {
        Ok(events) => events,
        Err(err) => {
            write_reducer_error_capture(traffic, &err);
            return Err(anyhow::anyhow!(
                "upstream stream error: {} ({:?})",
                err.message,
                err.kind
            ));
        }
    };

    let mut out = Vec::new();
    let mut message_started = false;
    let mut web_search_events: Vec<ReducerEvent> = Vec::new();
    let mut deferred_content_events: Vec<ReducerEvent> = Vec::new();
    let message = MessageMetadata {
        id: message_id,
        model,
        estimated_input_tokens,
    };

    for event in &events {
        if matches!(event, ReducerEvent::WebSearch { .. }) {
            web_search_events.push(event.clone());
            continue;
        }
        if !web_search_events.is_empty() && is_content_event(event) {
            deferred_content_events.push(event.clone());
            continue;
        }

        if emit_content_event(&mut out, traffic, &mut message_started, &message, event) {
            continue;
        }

        match event {
            ReducerEvent::ToolProgress { .. } | ReducerEvent::Progress => {}
            ReducerEvent::Finish {
                stop_reason,
                usage,
                web_search_requests,
                ..
            } => {
                // Emit web search compat blocks
                if !web_search_events.is_empty() {
                    let text_from_deferred: String = deferred_content_events
                        .iter()
                        .filter_map(|e| match e {
                            ReducerEvent::TextDelta { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    let compat_blocks =
                        build_web_search_compat_blocks(&web_search_events, &text_from_deferred);
                    for block in &compat_blocks {
                        use super::web_search_compat::WebSearchCompatContent;
                        match &block.content {
                            WebSearchCompatContent::ServerToolUse { id, name, input } => {
                                ensure_message_start(
                                    &mut out,
                                    traffic,
                                    &mut message_started,
                                    &message,
                                );
                                emit(
                                    &mut out,
                                    traffic,
                                    "content_block_start",
                                    &serde_json::json!({
                                        "type": "content_block_start",
                                        "index": block.index,
                                        "content_block": {
                                            "type": "server_tool_use",
                                            "id": id,
                                            "name": name,
                                            "input": {}
                                        }
                                    }),
                                );
                                emit(
                                    &mut out,
                                    traffic,
                                    "content_block_delta",
                                    &serde_json::json!({
                                        "type": "content_block_delta",
                                        "index": block.index,
                                        "delta": {
                                            "type": "input_json_delta",
                                            "partial_json": serde_json::to_string(input).unwrap_or_default()
                                        }
                                    }),
                                );
                                emit(
                                    &mut out,
                                    traffic,
                                    "content_block_stop",
                                    &serde_json::json!({
                                        "type": "content_block_stop",
                                        "index": block.index,
                                    }),
                                );
                            }
                            WebSearchCompatContent::WebSearchToolResult {
                                tool_use_id,
                                content: results,
                            } => {
                                let result_content: Vec<serde_json::Value> = results
                                    .iter()
                                    .map(|r| {
                                        serde_json::json!({
                                            "type": "web_search_result",
                                            "title": r.title,
                                            "url": r.url,
                                        })
                                    })
                                    .collect();
                                emit(
                                    &mut out,
                                    traffic,
                                    "content_block_start",
                                    &serde_json::json!({
                                        "type": "content_block_start",
                                        "index": block.index,
                                        "content_block": {
                                            "type": "web_search_tool_result",
                                            "tool_use_id": tool_use_id,
                                            "content": result_content,
                                        }
                                    }),
                                );
                                emit(
                                    &mut out,
                                    traffic,
                                    "content_block_stop",
                                    &serde_json::json!({
                                        "type": "content_block_stop",
                                        "index": block.index,
                                    }),
                                );
                            }
                        }
                    }
                }

                // Emit deferred content
                for deferred in &deferred_content_events {
                    emit_content_event(&mut out, traffic, &mut message_started, &message, deferred);
                }

                ensure_message_start(&mut out, traffic, &mut message_started, &message);

                let mapped = map_codex_usage_to_anthropic(usage, Some(*web_search_requests));
                emit(
                    &mut out,
                    traffic,
                    "message_delta",
                    &serde_json::json!({
                        "type": "message_delta",
                        "delta": {
                            "stop_reason": stop_reason,
                            "stop_sequence": null
                        },
                        "usage": mapped,
                    }),
                );
                emit(
                    &mut out,
                    traffic,
                    "message_stop",
                    &serde_json::json!({"type": "message_stop"}),
                );
            }
            _ => {}
        }
    }

    Ok(out)
}

fn write_reducer_error_capture(traffic: Option<&TrafficCapture>, err: &UpstreamStreamError) {
    let Some(traffic) = traffic else {
        return;
    };
    traffic.write_json(
        "060-codex-stream-reducer-error",
        &serde_json::json!({
            "kind": format!("{:?}", err.kind),
            "message": err.message,
            "retryAfterSeconds": err.retry_after_seconds,
            "diagnostics": err.diagnostics,
        }),
    );
}
