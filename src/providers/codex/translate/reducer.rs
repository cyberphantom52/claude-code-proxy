use crate::anthropic::sse::parse_sse_events;
use crate::providers::codex::events::{CodexFailureKind, classify_event_failure};

use super::live_stream::{
    BUFFERED_TOOL_MAX_ARGS_BYTES, CompletedFunctionCallValidationError,
    validate_completed_function_call, validate_streamed_function_call,
};
use super::read_rewrite::sanitize_read_args;
use super::reasoning_signature::{PendingReasoning, ReasoningReplay, encode_reasoning_signature};
use super::request::ResponsesInputItem;
use super::web_search_compat::{server_tool_use_id_from_codex_web_search_id, web_search_query};

#[derive(Debug, Clone)]
pub struct UpstreamStreamError {
    pub kind: UpstreamErrorKind,
    pub message: String,
    pub retry_after_seconds: Option<u64>,
    pub diagnostics: Option<UpstreamStreamDiagnostics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamErrorKind {
    RateLimit,
    Overloaded,
    Transient,
    Failed,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UpstreamStreamDiagnostics {
    pub event_count: usize,
    pub last_event_type: Option<String>,
    pub saw_terminal_event: bool,
    pub open_blocks: Vec<OpenBlockDiagnostic>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OpenBlockDiagnostic {
    pub output_index: usize,
    pub anthropic_index: usize,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_bytes: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct CodexUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub input_tokens_details_cached: Option<u64>,
    pub output_tokens_details_reasoning: Option<u64>,
    /// Actual tier reported by the response, not the requested tier.
    pub service_tier: Option<String>,
    /// Latest `codex.rate_limits` snapshot seen on the stream that produced
    /// this usage. Codex sends it as its own event before any usage exists,
    /// so the translator carries it here for the mapper.
    pub rate_limits: Option<Box<CodexRateLimits>>,
}

/// Subscription meter summary from a `codex.rate_limits` stream event.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CodexRateLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_reached: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<CodexRateLimitWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<CodexRateLimitWindow>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CodexRateLimitWindow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<u64>,
}

/// Parse a `codex.rate_limits` event payload. Returns `None` when the event
/// carries no `rate_limits` object.
pub fn parse_codex_rate_limits(payload: &serde_json::Value) -> Option<Box<CodexRateLimits>> {
    let limits = payload.get("rate_limits")?.as_object()?;
    let window = |name: &str| -> Option<CodexRateLimitWindow> {
        let w = limits.get(name)?.as_object()?;
        Some(CodexRateLimitWindow {
            used_percent: w.get("used_percent").and_then(|v| v.as_f64()),
            window_minutes: w.get("window_minutes").and_then(|v| v.as_u64()),
            reset_at: w.get("reset_at").and_then(|v| v.as_u64()),
        })
    };
    Some(Box::new(CodexRateLimits {
        plan_type: payload
            .get("plan_type")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        limit_reached: limits.get("limit_reached").and_then(|v| v.as_bool()),
        primary: window("primary"),
        secondary: window("secondary"),
    }))
}

pub type StopReason = &'static str;
pub const STOP_END_TURN: &str = "end_turn";
pub const STOP_TOOL_USE: &str = "tool_use";

pub type TerminalType = &'static str;
pub const TERM_COMPLETED: &str = "response.completed";
pub const TERM_INCOMPLETE: &str = "response.incomplete";
pub const TERM_DONE: &str = "response.done";

impl From<CompletedFunctionCallValidationError> for UpstreamStreamError {
    fn from(error: CompletedFunctionCallValidationError) -> Self {
        Self {
            kind: match error {
                CompletedFunctionCallValidationError::TooLarge { .. } => UpstreamErrorKind::Failed,
                CompletedFunctionCallValidationError::Protocol(_) => UpstreamErrorKind::Transient,
            },
            message: error.to_string(),
            retry_after_seconds: None,
            diagnostics: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ReducerEvent {
    ThinkingStart {
        index: usize,
    },
    ThinkingDelta {
        index: usize,
        text: String,
    },
    ThinkingSignature {
        index: usize,
        signature: String,
    },
    TextStart {
        index: usize,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    ToolStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolDelta {
        index: usize,
        partial_json: String,
    },
    WebSearch {
        index: usize,
        result_index: usize,
        id: String,
        query: String,
    },
    Finish {
        stop_reason: StopReason,
        terminal_type: String,
        continuation_eligible: bool,
        usage: Option<CodexUsage>,
        web_search_requests: usize,
        response_id: Option<String>,
        output_items: Vec<ResponsesInputItem>,
    },
}

#[derive(Debug, Clone)]
pub struct FinishMetadata {
    pub continuation_eligible: bool,
    pub response_id: Option<String>,
    pub output_items: Vec<ResponsesInputItem>,
}

enum BlockState {
    Text {
        index: usize,
        text_accum: String,
    },
    Tool {
        index: usize,
        call_id: String,
        name: String,
        args_accum: String,
        emitted_args: bool,
    },
}

#[derive(Clone, Copy)]
struct ActiveThinking {
    output_index: usize,
    anthropic_index: usize,
}

fn reasoning_input_item(replay: ReasoningReplay) -> ResponsesInputItem {
    ResponsesInputItem::Reasoning {
        id: replay.id,
        summary: Vec::new(),
        encrypted_content: replay.encrypted_content,
    }
}

fn finalize_active_thinking(
    active: ActiveThinking,
    out: &mut Vec<ReducerEvent>,
    reasoning_by_output_index: &mut std::collections::HashMap<usize, PendingReasoning>,
    output_items_by_index: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
) {
    if let Some(replay) = reasoning_by_output_index
        .remove(&active.output_index)
        .and_then(|pending| pending.replay())
        && let Some(signature) = encode_reasoning_signature(&replay)
    {
        out.push(ReducerEvent::ThinkingSignature {
            index: active.anthropic_index,
            signature,
        });
        output_items_by_index.insert(active.output_index, reasoning_input_item(replay));
    }
}

fn close_thinking(
    out: &mut Vec<ReducerEvent>,
    active_thinking: &mut Option<ActiveThinking>,
    reasoning_by_output_index: &mut std::collections::HashMap<usize, PendingReasoning>,
    output_items_by_index: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
) {
    if let Some(active) = active_thinking.take() {
        finalize_active_thinking(
            active,
            out,
            reasoning_by_output_index,
            output_items_by_index,
        );
    }
}

fn emit_signature_only_reasoning(
    output_index: usize,
    anthropic_index: &mut usize,
    out: &mut Vec<ReducerEvent>,
    reasoning_by_output_index: &mut std::collections::HashMap<usize, PendingReasoning>,
    output_items_by_index: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
) {
    let Some(replay) = reasoning_by_output_index
        .remove(&output_index)
        .and_then(|pending| pending.replay())
    else {
        return;
    };
    let Some(signature) = encode_reasoning_signature(&replay) else {
        return;
    };
    let index = *anthropic_index;
    *anthropic_index += 1;
    out.push(ReducerEvent::ThinkingStart { index });
    out.push(ReducerEvent::ThinkingSignature { index, signature });
    output_items_by_index.insert(output_index, reasoning_input_item(replay));
}

pub fn finish_metadata_from_upstream(
    input: &[u8],
) -> Result<Option<FinishMetadata>, UpstreamStreamError> {
    let events = reduce_upstream_bytes(input)?;
    Ok(events.into_iter().rev().find_map(|event| match event {
        ReducerEvent::Finish {
            continuation_eligible,
            response_id,
            output_items,
            ..
        } => Some(FinishMetadata {
            continuation_eligible,
            response_id,
            output_items,
        }),
        _ => None,
    }))
}

pub fn reduce_upstream_bytes(input: &[u8]) -> Result<Vec<ReducerEvent>, UpstreamStreamError> {
    let sse_events = parse_sse_events(input);
    let mut out = Vec::new();

    let mut blocks_by_output_index: std::collections::HashMap<usize, BlockState> =
        std::collections::HashMap::new();
    let mut output_items_by_index: std::collections::BTreeMap<usize, ResponsesInputItem> =
        std::collections::BTreeMap::new();
    let mut item_id_to_output_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut reasoning_by_output_index: std::collections::HashMap<usize, PendingReasoning> =
        std::collections::HashMap::new();
    let mut anthropic_index = 0usize;
    let mut active_thinking: Option<ActiveThinking> = None;
    let mut saw_tool_use = false;
    let mut final_usage: Option<CodexUsage> = None;
    let mut service_tier: Option<String> = None;
    let mut latest_rate_limits: Option<Box<CodexRateLimits>> = None;
    let mut response_id: Option<String> = None;
    let mut terminal_type: Option<String> = None;
    let mut continuation_eligible = false;
    let mut web_search_requests = 0usize;
    let mut _saw_terminal = false;
    let mut event_count = 0usize;
    let mut last_event_type: Option<String> = None;

    fn capture_output_item(
        output_index: usize,
        state: &BlockState,
        items: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
    ) {
        match state {
            BlockState::Text {
                index: _,
                text_accum,
            } => {
                if text_accum.is_empty() {
                    return;
                }
                items.insert(
                    output_index,
                    ResponsesInputItem::Message {
                        role: "assistant".to_string(),
                        content: vec![super::request::ResponsesContentPart::OutputText {
                            text: text_accum.clone(),
                        }],
                    },
                );
            }
            BlockState::Tool {
                args_accum,
                name,
                call_id,
                ..
            } => {
                items.insert(
                    output_index,
                    ResponsesInputItem::FunctionCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: args_accum.clone(),
                    },
                );
            }
        }
    }

    for evt in &sse_events {
        let data = evt.data.trim();
        if data.is_empty() {
            continue;
        }

        let p: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let t = p
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        event_count += 1;
        last_event_type = Some(t.clone());

        // A trailing billing/keepalive ping carries no model output. All other
        // JSON events after a terminal remain invalid.
        if _saw_terminal && t == "ping" {
            continue;
        }

        if _saw_terminal {
            let message = if t == "response.completed"
                || t == "response.incomplete"
                || t == "response.done"
            {
                "upstream stream contained multiple terminal Codex response events"
            } else {
                "upstream stream contained a JSON event after the terminal Codex response event"
            };
            return Err(UpstreamStreamError {
                kind: UpstreamErrorKind::Transient,
                message: message.to_string(),
                retry_after_seconds: None,
                diagnostics: Some(UpstreamStreamDiagnostics {
                    event_count,
                    last_event_type,
                    saw_terminal_event: true,
                    open_blocks: describe_open_blocks(&blocks_by_output_index),
                }),
            });
        }

        if let Some(failure) = classify_event_failure(&p) {
            let kind = match failure.kind {
                CodexFailureKind::RateLimit => UpstreamErrorKind::RateLimit,
                CodexFailureKind::Overloaded => UpstreamErrorKind::Overloaded,
                CodexFailureKind::Transient => UpstreamErrorKind::Transient,
                CodexFailureKind::Permanent => UpstreamErrorKind::Failed,
            };
            let retry_after_seconds = failure
                .retry_after
                .as_deref()
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value.ceil() as u64);
            return Err(UpstreamStreamError {
                kind,
                message: failure.message,
                retry_after_seconds,
                diagnostics: None,
            });
        }

        if t == "response.created" {
            service_tier = p
                .pointer("/response/service_tier")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            continue;
        }

        if t == "codex.rate_limits" {
            latest_rate_limits = parse_codex_rate_limits(&p).or(latest_rate_limits);
            continue;
        }

        if t == "response.output_item.added" {
            let item = match p.get("item") {
                Some(v) => v,
                None => continue,
            };
            let output_index: usize =
                p.get("output_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if item_type == "reasoning" {
                reasoning_by_output_index
                    .entry(output_index)
                    .or_default()
                    .capture(item);
                continue;
            }
            if item_type == "message" {
                close_thinking(
                    &mut out,
                    &mut active_thinking,
                    &mut reasoning_by_output_index,
                    &mut output_items_by_index,
                );
                let idx = anthropic_index;
                anthropic_index += 1;
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    item_id_to_output_index.insert(id.to_string(), output_index);
                }
                blocks_by_output_index.insert(
                    output_index,
                    BlockState::Text {
                        index: idx,
                        text_accum: String::new(),
                    },
                );
                out.push(ReducerEvent::TextStart { index: idx });
                continue;
            }

            if item_type == "function_call" {
                close_thinking(
                    &mut out,
                    &mut active_thinking,
                    &mut reasoning_by_output_index,
                    &mut output_items_by_index,
                );
                saw_tool_use = true;
                let idx = anthropic_index;
                anthropic_index += 1;
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                blocks_by_output_index.insert(
                    output_index,
                    BlockState::Tool {
                        index: idx,
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args_accum: String::new(),
                        emitted_args: false,
                    },
                );
                out.push(ReducerEvent::ToolStart {
                    index: idx,
                    id: call_id,
                    name,
                });
                continue;
            }

            continue;
        }

        if t == "response.reasoning_summary_part.added" {
            let output_index = p.get("output_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(active) =
                active_thinking.filter(|active| active.output_index == output_index)
            {
                out.push(ReducerEvent::ThinkingDelta {
                    index: active.anthropic_index,
                    text: "\n\n".to_string(),
                });
            }
            continue;
        }

        if t == "response.reasoning_summary_text.delta" {
            let output_index = p.get("output_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let delta = p.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if delta.is_empty() {
                continue;
            }
            if active_thinking.map(|active| active.output_index) != Some(output_index) {
                close_thinking(
                    &mut out,
                    &mut active_thinking,
                    &mut reasoning_by_output_index,
                    &mut output_items_by_index,
                );
                let index = anthropic_index;
                anthropic_index += 1;
                active_thinking = Some(ActiveThinking {
                    output_index,
                    anthropic_index: index,
                });
                out.push(ReducerEvent::ThinkingStart { index });
            }
            out.push(ReducerEvent::ThinkingDelta {
                index: active_thinking
                    .expect("thinking block was started")
                    .anthropic_index,
                text: delta.to_string(),
            });
            continue;
        }

        if t == "response.output_text.delta" {
            close_thinking(
                &mut out,
                &mut active_thinking,
                &mut reasoning_by_output_index,
                &mut output_items_by_index,
            );
            let output_index = p
                .get("output_index")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            let item_id = p.get("item_id").and_then(|v| v.as_str());
            let state = if let Some(oi) = output_index {
                blocks_by_output_index.get_mut(&oi)
            } else if let Some(id) = item_id {
                item_id_to_output_index
                    .get(id)
                    .and_then(|oi| blocks_by_output_index.get_mut(oi))
            } else {
                None
            };
            let delta = p.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if delta.is_empty() {
                continue;
            }
            match state {
                Some(BlockState::Text { index, text_accum }) => {
                    text_accum.push_str(delta);
                    out.push(ReducerEvent::TextDelta {
                        index: *index,
                        text: delta.to_string(),
                    });
                }
                _ => continue,
            }
            continue;
        }

        if t == "response.function_call_arguments.delta" {
            let output_index = match p.get("output_index").and_then(|v| v.as_u64()) {
                Some(v) => v as usize,
                None => continue,
            };
            let delta = p.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if delta.is_empty() {
                continue;
            }
            let state = match blocks_by_output_index.get_mut(&output_index) {
                Some(s) => s,
                None => continue,
            };
            match state {
                BlockState::Tool {
                    args_accum,
                    emitted_args,
                    name,
                    index,
                    ..
                } => {
                    if args_accum.len().saturating_add(delta.len()) > BUFFERED_TOOL_MAX_ARGS_BYTES {
                        return Err(CompletedFunctionCallValidationError::TooLarge {
                            tool: name.clone(),
                        }
                        .into());
                    }
                    args_accum.push_str(delta);
                    if name != "Read" {
                        *emitted_args = true;
                        out.push(ReducerEvent::ToolDelta {
                            index: *index,
                            partial_json: delta.to_string(),
                        });
                    }
                }
                _ => continue,
            }
            continue;
        }

        if t == "response.function_call_arguments.done" {
            let output_index = match p.get("output_index").and_then(|v| v.as_u64()) {
                Some(v) => v as usize,
                None => continue,
            };
            if let Some(BlockState::Tool {
                args_accum,
                call_id,
                name,
                ..
            }) = blocks_by_output_index.get_mut(&output_index)
            {
                let completed = validate_completed_function_call(&serde_json::json!({
                    "call_id": call_id,
                    "name": name,
                    "arguments": p.get("arguments"),
                }))?;
                validate_streamed_function_call(&completed, call_id, name, args_accum)?;
                if args_accum.is_empty() {
                    *args_accum = completed.arguments;
                }
            }
            continue;
        }

        if t == "response.output_item.done" {
            let output_index: usize =
                p.get("output_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let item = p.get("item");

            if let Some(item_val) = item
                && item_val.get("type").and_then(|v| v.as_str()) == Some("reasoning")
            {
                reasoning_by_output_index
                    .entry(output_index)
                    .or_default()
                    .capture(item_val);
                let had_active_summary =
                    active_thinking.is_some_and(|active| active.output_index == output_index);
                close_thinking(
                    &mut out,
                    &mut active_thinking,
                    &mut reasoning_by_output_index,
                    &mut output_items_by_index,
                );
                if !had_active_summary {
                    emit_signature_only_reasoning(
                        output_index,
                        &mut anthropic_index,
                        &mut out,
                        &mut reasoning_by_output_index,
                        &mut output_items_by_index,
                    );
                }
                continue;
            }

            if let Some(item_val) = item
                && item_val.get("type").and_then(|v| v.as_str()) == Some("web_search_call")
            {
                close_thinking(
                    &mut out,
                    &mut active_thinking,
                    &mut reasoning_by_output_index,
                    &mut output_items_by_index,
                );
                let idx = anthropic_index;
                anthropic_index += 1;
                let result_index = anthropic_index;
                anthropic_index += 1;
                web_search_requests += 1;
                let id_val = item_val.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let id = server_tool_use_id_from_codex_web_search_id(id_val);
                let query = web_search_query(item_val);
                out.push(ReducerEvent::WebSearch {
                    index: idx,
                    result_index,
                    id,
                    query,
                });
                continue;
            }

            let completed = if item
                .and_then(|item| item.get("type"))
                .and_then(|v| v.as_str())
                == Some("function_call")
            {
                Some(validate_completed_function_call(
                    item.expect("function call item"),
                )?)
            } else {
                None
            };
            let mut output_index = output_index;
            if let Some(completed) = completed.as_ref() {
                let explicit_tool = p.get("output_index").is_some()
                    && matches!(
                        blocks_by_output_index.get(&output_index),
                        Some(BlockState::Tool { .. })
                    );
                if !explicit_tool {
                    let mut matching = blocks_by_output_index.iter().filter_map(|(output_index, state)| {
                        matches!(state, BlockState::Tool { call_id, .. } if call_id == &completed.call_id)
                            .then_some(*output_index)
                    });
                    let matching_index = matching.next().filter(|_| matching.next().is_none());
                    if let Some(matching_index) = matching_index {
                        output_index = matching_index;
                    } else {
                        // Done-only calls have no open block to close or replay.
                        close_thinking(
                            &mut out,
                            &mut active_thinking,
                            &mut reasoning_by_output_index,
                            &mut output_items_by_index,
                        );
                        saw_tool_use = true;
                        let index = anthropic_index;
                        anthropic_index += 1;
                        let arguments = sanitize_read_args(
                            &completed.name,
                            &completed.arguments,
                            Some(&completed.call_id),
                        );
                        out.push(ReducerEvent::ToolStart {
                            index,
                            id: completed.call_id.clone(),
                            name: completed.name.clone(),
                        });
                        out.push(ReducerEvent::ToolDelta {
                            index,
                            partial_json: arguments.clone(),
                        });
                        if p.get("output_index").is_none()
                            || blocks_by_output_index.contains_key(&output_index)
                            || output_items_by_index.contains_key(&output_index)
                        {
                            output_index = blocks_by_output_index
                                .keys()
                                .chain(output_items_by_index.keys())
                                .copied()
                                .max()
                                .map_or(0, |index| index + 1);
                        }
                        output_items_by_index.insert(
                            output_index,
                            ResponsesInputItem::FunctionCall {
                                call_id: completed.call_id.clone(),
                                name: completed.name.clone(),
                                arguments,
                            },
                        );
                        continue;
                    }
                }
            }
            let Some(mut state) = blocks_by_output_index.remove(&output_index) else {
                continue;
            };

            if let BlockState::Tool {
                args_accum,
                name,
                call_id,
                emitted_args,
                index,
                ..
            } = &mut state
            {
                let completed = completed.as_ref().ok_or_else(|| {
                    CompletedFunctionCallValidationError::Protocol(
                        "tool output closed without a completed function call".to_string(),
                    )
                })?;
                validate_streamed_function_call(completed, call_id, name, args_accum)?;
                if !*emitted_args {
                    *args_accum =
                        sanitize_read_args(name, &completed.arguments, Some(call_id.as_str()));
                    out.push(ReducerEvent::ToolDelta {
                        index: *index,
                        partial_json: args_accum.clone(),
                    });
                }
            }

            capture_output_item(output_index, &state, &mut output_items_by_index);

            continue;
        }

        if t == "response.completed" || t == "response.incomplete" || t == "response.done" {
            _saw_terminal = true;
            terminal_type = Some(t.clone());
            response_id = p
                .get("response")
                .and_then(|r| r.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            final_usage = p.get("response").map(parse_codex_usage);
            if let Some(usage) = final_usage.as_mut() {
                usage.service_tier = usage.service_tier.take().or(service_tier.take());
                usage.rate_limits = latest_rate_limits.clone();
            }
            continuation_eligible = t == "response.completed" || t == "response.done";
            continue;
        }
    }

    let open_blocks = describe_open_blocks(&blocks_by_output_index);
    if !_saw_terminal || !open_blocks.is_empty() {
        let diagnostics = UpstreamStreamDiagnostics {
            event_count,
            last_event_type,
            saw_terminal_event: _saw_terminal,
            open_blocks,
        };
        return Err(UpstreamStreamError {
            kind: UpstreamErrorKind::Transient,
            message: if diagnostics.saw_terminal_event {
                "upstream stream ended with open Codex output blocks".to_string()
            } else {
                "upstream stream ended before terminal Codex response event".to_string()
            },
            retry_after_seconds: None,
            diagnostics: Some(diagnostics),
        });
    }

    close_thinking(
        &mut out,
        &mut active_thinking,
        &mut reasoning_by_output_index,
        &mut output_items_by_index,
    );

    let stop_reason: StopReason = if saw_tool_use {
        STOP_TOOL_USE
    } else {
        STOP_END_TURN
    };

    let output_items: Vec<ResponsesInputItem> = output_items_by_index.into_values().collect();

    out.push(ReducerEvent::Finish {
        stop_reason,
        terminal_type: terminal_type.unwrap_or_else(|| TERM_INCOMPLETE.to_string()),
        continuation_eligible,
        usage: final_usage,
        web_search_requests,
        response_id,
        output_items,
    });

    Ok(out)
}

fn describe_open_blocks(
    blocks: &std::collections::HashMap<usize, BlockState>,
) -> Vec<OpenBlockDiagnostic> {
    let mut out: Vec<_> = blocks
        .iter()
        .map(|(output_index, state)| match state {
            BlockState::Text { index, text_accum } => OpenBlockDiagnostic {
                output_index: *output_index,
                anthropic_index: *index,
                kind: "text".to_string(),
                name: None,
                call_id: None,
                text_bytes: Some(text_accum.len()),
                argument_bytes: None,
            },
            BlockState::Tool {
                index,
                call_id,
                name,
                args_accum,
                ..
            } => OpenBlockDiagnostic {
                output_index: *output_index,
                anthropic_index: *index,
                kind: "tool".to_string(),
                name: Some(name.clone()),
                call_id: Some(call_id.clone()),
                text_bytes: None,
                argument_bytes: Some(args_accum.len()),
            },
        })
        .collect();
    out.sort_by_key(|block| block.output_index);
    out
}

pub(super) fn parse_codex_usage(response: &serde_json::Value) -> CodexUsage {
    let usage = response.get("usage").unwrap_or(&serde_json::Value::Null);
    CodexUsage {
        service_tier: response
            .get("service_tier")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
        input_tokens_details_cached: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64()),
        output_tokens_details_reasoning: usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64()),
        rate_limits: None,
    }
}

pub fn map_codex_usage_to_anthropic(
    u: &Option<CodexUsage>,
    web_search_requests: Option<usize>,
) -> AnthropicUsage {
    let usage = match u {
        Some(u) => u,
        None => return AnthropicUsage::default(),
    };
    let cached = usage.input_tokens_details_cached.unwrap_or(0);
    let total_input = usage.input_tokens.unwrap_or(0);
    let input_tokens = total_input.saturating_sub(cached);

    let mut result = AnthropicUsage {
        speed: match usage.service_tier.as_deref() {
            Some("priority" | "ultrafast") => Some("fast".to_string()),
            Some("default" | "standard" | "flex") => Some("standard".to_string()),
            _ => None,
        },
        input_tokens,
        output_tokens: usage.output_tokens.unwrap_or(0),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
        server_tool_use: None,
        output_tokens_details: usage
            .output_tokens_details_reasoning
            .map(|reasoning_tokens| OutputTokensDetails {
                reasoning_tokens,
                thinking_tokens: reasoning_tokens,
            }),
        codex_rate_limits: usage.rate_limits.clone(),
    };

    if let Some(requests) = web_search_requests
        && requests > 0
    {
        result.server_tool_use = Some(WebSearchUsage {
            web_search_requests: requests,
        });
    }

    result
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AnthropicUsage {
    /// Only report speed when an upstream response confirms a known tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<WebSearchUsage>,
    /// Reasoning tokens as reported by Codex. `thinking_tokens` mirrors the
    /// field name Claude Code records for Anthropic models so per-turn
    /// accounting in transcripts stays comparable across providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<OutputTokensDetails>,
    /// Subscription meter as of this response, from the `codex.rate_limits`
    /// event, so a downstream proxy can account per turn. Claude Code ignores
    /// the unknown key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_rate_limits: Option<Box<CodexRateLimits>>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OutputTokensDetails {
    pub reasoning_tokens: u64,
    pub thinking_tokens: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WebSearchUsage {
    pub web_search_requests: usize,
}
