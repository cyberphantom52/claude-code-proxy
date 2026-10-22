use serde_json::Value;

use crate::traffic::TrafficCapture;

use super::reducer::{
    AnthropicUsage, ReducerEvent, UpstreamStreamError, map_codex_usage_to_anthropic,
    reduce_upstream_bytes,
};
use super::web_search_compat::{WebSearchCompatContent, build_web_search_compat_blocks};

pub fn accumulate_response_with_traffic(
    upstream: &[u8],
    message_id: &str,
    model: &str,
    traffic: Option<&TrafficCapture>,
) -> Result<Value, anyhow::Error> {
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
    let mut stop_reason: Option<String> = None;
    let mut usage: Option<AnthropicUsage> = None;
    let mut has_web_search = false;
    let mut text_from_deferred = String::new();

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

    for event in &events {
        match event {
            ReducerEvent::WebSearch { .. } => {
                has_web_search = true;
            }
            ReducerEvent::ThinkingStart { index } => {
                blocks.push(AccumulatedBlock {
                    index: *index,
                    kind: BlockKind::Thinking {
                        text: String::new(),
                        signature: String::new(),
                    },
                });
            }
            ReducerEvent::ThinkingDelta { index, text } => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| b.index == *index)
                    && let BlockKind::Thinking { text: t, .. } = &mut block.kind
                {
                    t.push_str(text);
                }
            }
            ReducerEvent::ThinkingSignature { index, signature } => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| b.index == *index)
                    && let BlockKind::Thinking { signature: s, .. } = &mut block.kind
                {
                    *s = signature.clone();
                }
            }
            ReducerEvent::TextStart { index } => {
                blocks.push(AccumulatedBlock {
                    index: *index,
                    kind: BlockKind::Text {
                        text: String::new(),
                    },
                });
            }
            ReducerEvent::TextDelta { index, text } => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| b.index == *index)
                    && let BlockKind::Text { text: t } = &mut block.kind
                {
                    t.push_str(text);
                }
                text_from_deferred.push_str(text);
            }
            ReducerEvent::ToolStart { index, id, name } => {
                blocks.push(AccumulatedBlock {
                    index: *index,
                    kind: BlockKind::Tool {
                        id: id.clone(),
                        name: name.clone(),
                        args: String::new(),
                    },
                });
            }
            ReducerEvent::ToolDelta {
                index,
                partial_json,
            } => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| b.index == *index)
                    && let BlockKind::Tool { args, .. } = &mut block.kind
                {
                    args.push_str(partial_json);
                }
            }
            ReducerEvent::Finish {
                stop_reason: sr,
                usage: u,
                web_search_requests,
                ..
            } => {
                stop_reason = Some(sr.to_string());
                let ws = Some(*web_search_requests).filter(|n| *n > 0);
                usage = Some(map_codex_usage_to_anthropic(u, ws));
            }
        }
    }

    let mut indexed_content: Vec<(usize, Value)> = Vec::new();

    if has_web_search {
        let compat_blocks = build_web_search_compat_blocks(&events, &text_from_deferred);
        for block in &compat_blocks {
            match &block.content {
                WebSearchCompatContent::ServerToolUse { id, name, input } => {
                    indexed_content.push((
                        block.index,
                        serde_json::json!({
                            "type": "server_tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }),
                    ));
                }
                WebSearchCompatContent::WebSearchToolResult {
                    tool_use_id,
                    content: results,
                } => {
                    let result_content: Vec<Value> = results
                        .iter()
                        .map(|r| {
                            serde_json::json!({
                                "type": "web_search_result",
                                "title": r.title,
                                "url": r.url,
                            })
                        })
                        .collect();
                    indexed_content.push((
                        block.index,
                        serde_json::json!({
                            "type": "web_search_tool_result",
                            "tool_use_id": tool_use_id,
                            "content": result_content,
                        }),
                    ));
                }
            }
        }
    }

    for block in &blocks {
        match &block.kind {
            BlockKind::Thinking { text, signature } => {
                if !text.is_empty() || !signature.is_empty() {
                    indexed_content.push((
                        block.index,
                        serde_json::json!({
                            "type": "thinking",
                            "thinking": text,
                            "signature": signature,
                        }),
                    ));
                }
            }
            BlockKind::Text { text } => {
                if !text.is_empty() {
                    indexed_content.push((
                        block.index,
                        serde_json::json!({
                            "type": "text",
                            "text": text,
                        }),
                    ));
                }
            }
            BlockKind::Tool { id, name, args } => {
                let completed =
                    super::live_stream::validate_completed_function_call(&serde_json::json!({
                        "call_id": id, "name": name, "arguments": args,
                    }))
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                let parsed: Value = serde_json::from_str(&completed.arguments)?;
                indexed_content.push((
                    block.index,
                    serde_json::json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": parsed,
                    }),
                ));
            }
        }
    }

    indexed_content.sort_by_key(|(index, _)| *index);
    let content: Vec<Value> = indexed_content
        .into_iter()
        .map(|(_, content)| content)
        .collect();

    let response = serde_json::json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage.unwrap_or_default(),
    });

    Ok(response)
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
