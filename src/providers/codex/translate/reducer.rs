use crate::anthropic::sse::sse_events;
use crate::providers::codex::events::{CodexFailureKind, classify_event_failure};

use super::live_stream::{
    BUFFERED_TOOL_MAX_ARGS_BYTES, CompletedFunctionCallValidationError,
    validate_completed_function_call, validate_completed_function_call_fields,
    validate_streamed_function_call,
};
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
pub const STOP_MAX_TOKENS: &str = "max_tokens";
pub const STOP_REFUSAL: &str = "refusal";

// Share reconciliation between buffered and live output, keyed by content part.
#[derive(Default)]
pub(super) struct TextParts {
    parts: std::collections::BTreeMap<(usize, usize), TextPart>,
    retained_bytes: usize,
}
struct TextPart {
    index: usize,
    text: String,
    refusal: bool,
    closed: bool,
}
pub(super) struct TextDelta {
    pub index: usize,
    pub start: bool,
    pub text: String,
    pub refusal: bool,
}
impl TextParts {
    pub fn has_refusal(&self) -> bool {
        self.parts.values().any(|part| part.refusal)
    }
    pub fn has_open_parts(&self) -> bool {
        self.parts.values().any(|part| !part.closed)
    }
    pub fn close_item(&mut self, output_index: Option<usize>) -> Vec<usize> {
        self.parts
            .iter_mut()
            .filter_map(|((oi, _), part)| {
                if !part.closed && output_index.is_none_or(|index| index == *oi) {
                    part.closed = true;
                    Some(part.index)
                } else {
                    None
                }
            })
            .collect()
    }
    pub fn output_items(&mut self) -> Vec<(usize, ResponsesInputItem)> {
        let mut messages = std::collections::BTreeMap::<usize, Vec<_>>::new();
        for ((oi, _), part) in &mut self.parts {
            if !part.refusal && !part.text.is_empty() {
                messages.entry(*oi).or_default().push(
                    super::request::ResponsesContentPart::OutputText {
                        text: std::mem::take(&mut part.text),
                    },
                );
            }
        }
        messages
            .into_iter()
            .map(|(oi, content)| {
                (
                    oi,
                    ResponsesInputItem::Message {
                        role: "assistant".into(),
                        content,
                    },
                )
            })
            .collect()
    }
    pub fn accept(
        &mut self,
        payload: &serde_json::Value,
        next: &mut usize,
        output_index: usize,
    ) -> Result<Vec<TextDelta>, String> {
        let mut updates = Vec::new();
        let kind = payload["type"].as_str().unwrap_or("");
        if matches!(
            kind,
            "response.output_text.delta"
                | "response.output_text.done"
                | "response.refusal.delta"
                | "response.refusal.done"
        ) {
            let ci = payload["content_index"].as_u64().unwrap_or(0) as usize;
            let refusal = kind.starts_with("response.refusal.");
            let delta = kind.ends_with(".delta");
            let field = if delta {
                "delta"
            } else if refusal {
                "refusal"
            } else {
                "text"
            };
            let text = payload[field]
                .as_str()
                .ok_or("text event requires string content")?;
            if !delta || !text.is_empty() {
                updates.push((output_index, ci, text, delta, refusal));
            }
        }
        if kind == "response.content_part.done" {
            let part = &payload["part"];
            if matches!(part["type"].as_str(), Some("output_text" | "refusal")) {
                let refusal = part["type"] == "refusal";
                let text = part[if refusal { "refusal" } else { "text" }]
                    .as_str()
                    .ok_or("text part requires string content")?;
                updates.push((
                    output_index,
                    payload["content_index"].as_u64().unwrap_or(0) as usize,
                    text,
                    false,
                    refusal,
                ));
            }
        }
        let items: Vec<(usize, &serde_json::Value)> = if kind == "response.output_item.done" {
            vec![(output_index, &payload["item"])]
        } else if matches!(
            kind,
            "response.completed" | "response.done" | "response.incomplete"
        ) {
            payload
                .pointer("/response/output")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
                .collect()
        } else {
            Vec::new()
        };
        for (oi, item) in items {
            if item["type"] == "message" {
                for (ci, part) in item["content"].as_array().into_iter().flatten().enumerate() {
                    if matches!(part["type"].as_str(), Some("output_text" | "refusal")) {
                        let refusal = part["type"] == "refusal";
                        updates.push((
                            oi,
                            ci,
                            part[if refusal { "refusal" } else { "text" }]
                                .as_str()
                                .ok_or("text part requires string content")?,
                            false,
                            refusal,
                        ));
                    }
                }
            }
        }
        // Validate the entire event before changing block state: an error must
        // not leave unseen starts for the live error path to close.
        let mut retained = self.retained_bytes;
        let mut validated = Vec::with_capacity(updates.len());
        for (oi, ci, text, delta, refusal) in updates {
            let part = self.parts.get(&(oi, ci));
            if part.is_some_and(|part| part.refusal != refusal) {
                return Err("upstream text part changed type".into());
            }
            let suffix = if delta {
                text
            } else {
                text.strip_prefix(part.map_or("", |part| part.text.as_str()))
                    .ok_or("text completion disagrees with streamed text")?
            };
            if part.is_some_and(|part| part.closed) && !suffix.is_empty() {
                return Err("completed text part changed after output item closed".into());
            }
            let overhead = if part.is_none() { 256 } else { 0 };
            retained = retained
                .saturating_add(overhead)
                .saturating_add(suffix.len());
            if retained > crate::providers::codex::client::MAX_PENDING_HTTP_EVENT_BYTES {
                return Err("Codex text reconciliation exceeded its 16 MiB buffer limit".into());
            }
            validated.push((oi, ci, suffix, refusal));
        }
        self.retained_bytes = retained;
        let mut out = Vec::with_capacity(validated.len());
        for (oi, ci, suffix, refusal) in validated {
            let start = !self.parts.contains_key(&(oi, ci));
            let part = self.parts.entry((oi, ci)).or_insert_with(|| {
                let index = *next;
                *next += 1;
                TextPart {
                    index,
                    text: String::new(),
                    refusal,
                    closed: false,
                }
            });
            part.text.push_str(suffix);
            if start || !suffix.is_empty() {
                out.push(TextDelta {
                    index: part.index,
                    start,
                    text: suffix.to_owned(),
                    refusal,
                });
            }
        }
        Ok(out)
    }
}

pub type TerminalType = &'static str;
pub const TERM_COMPLETED: &str = "response.completed";
pub const TERM_INCOMPLETE: &str = "response.incomplete";
pub const TERM_DONE: &str = "response.done";

impl From<CompletedFunctionCallValidationError> for UpstreamStreamError {
    fn from(error: CompletedFunctionCallValidationError) -> Self {
        Self {
            kind: UpstreamErrorKind::Failed,
            message: error.to_string(),
            retry_after_seconds: None,
            diagnostics: None,
        }
    }
}

pub(super) fn upstream_annotations(
    payload: &serde_json::Value,
    output_index: usize,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let kind = payload["type"].as_str().unwrap_or("");
    let oi = output_index;
    if kind == "response.output_text.annotation.added"
        && let Some(annotation) = payload.get("annotation")
    {
        out.push(serde_json::json!({"output_index":oi,"content_index":payload["content_index"],"annotation":annotation}));
    }
    if kind == "response.content_part.done" {
        for annotation in payload["part"]["annotations"]
            .as_array()
            .into_iter()
            .flatten()
        {
            out.push(serde_json::json!({"output_index":oi,"content_index":payload["content_index"],"annotation":annotation}));
        }
    }
    let items: Vec<(usize, &serde_json::Value)> = if kind == "response.output_item.done" {
        vec![(oi, &payload["item"])]
    } else if matches!(
        kind,
        "response.completed" | "response.done" | "response.incomplete"
    ) {
        payload
            .pointer("/response/output")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .collect()
    } else {
        Vec::new()
    };
    for (oi, item) in items {
        for (ci, part) in item["content"].as_array().into_iter().flatten().enumerate() {
            for annotation in part["annotations"].as_array().into_iter().flatten() {
                out.push(serde_json::json!({"output_index":oi,"content_index":ci,"annotation":annotation}));
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
pub enum ReducerEvent {
    Annotation(serde_json::Value),
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

struct ToolState {
    index: usize,
    call_id: String,
    name: String,
    args_accum: String,
    emitted_args: bool,
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
    let text = String::from_utf8_lossy(input);
    let mut out = Vec::new();

    let mut blocks_by_output_index: std::collections::HashMap<usize, ToolState> =
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
    let mut text_parts = TextParts::default();
    let mut output_limited = false;
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
        state: ToolState,
        items: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
    ) {
        items.insert(
            output_index,
            ResponsesInputItem::FunctionCall {
                call_id: state.call_id,
                name: state.name,
                arguments: state.args_accum,
            },
        );
    }

    for evt in sse_events(&text) {
        let data = evt.data.trim();
        if data.is_empty() {
            continue;
        }

        let p: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let t = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
        event_count += 1;
        last_event_type = Some(t.to_owned());

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

        let text_output_index = p
            .get("output_index")
            .and_then(serde_json::Value::as_u64)
            .map(|index| index as usize)
            .or_else(|| {
                p["item_id"]
                    .as_str()
                    .or_else(|| p.pointer("/item/id").and_then(serde_json::Value::as_str))
                    .and_then(|id| item_id_to_output_index.get(id).copied())
            })
            .unwrap_or(0);
        out.extend(
            upstream_annotations(&p, text_output_index)
                .into_iter()
                .map(ReducerEvent::Annotation),
        );
        for delta in text_parts
            .accept(&p, &mut anthropic_index, text_output_index)
            .map_err(|message| UpstreamStreamError {
                kind: UpstreamErrorKind::Failed,
                message,
                retry_after_seconds: None,
                diagnostics: None,
            })?
        {
            close_thinking(
                &mut out,
                &mut active_thinking,
                &mut reasoning_by_output_index,
                &mut output_items_by_index,
            );
            if delta.start {
                out.push(ReducerEvent::TextStart { index: delta.index });
            }
            if !delta.text.is_empty() {
                out.push(ReducerEvent::TextDelta {
                    index: delta.index,
                    text: delta.text,
                });
            }
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
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    item_id_to_output_index.insert(id.to_string(), output_index);
                }
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
                    ToolState {
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
            let ToolState {
                args_accum,
                emitted_args,
                name,
                index,
                ..
            } = state;
            if args_accum.len().saturating_add(delta.len()) > BUFFERED_TOOL_MAX_ARGS_BYTES {
                return Err(
                    CompletedFunctionCallValidationError::TooLarge { tool: name.clone() }.into(),
                );
            }
            args_accum.push_str(delta);
            if name != "Read" {
                *emitted_args = true;
                out.push(ReducerEvent::ToolDelta {
                    index: *index,
                    partial_json: delta.to_string(),
                });
            }
            continue;
        }

        if t == "response.function_call_arguments.done" {
            let output_index = match p.get("output_index").and_then(|v| v.as_u64()) {
                Some(v) => v as usize,
                None => continue,
            };
            if let Some(ToolState {
                args_accum,
                call_id,
                name,
                ..
            }) = blocks_by_output_index.get_mut(&output_index)
            {
                let completed = validate_completed_function_call_fields(
                    Some(call_id),
                    Some(name),
                    p.get("arguments").and_then(serde_json::Value::as_str),
                )?;
                validate_streamed_function_call(&completed, call_id, name, args_accum)?;
                if args_accum.is_empty() {
                    *args_accum = completed.arguments.into_owned();
                }
            }
            continue;
        }

        if t == "response.output_item.done" {
            let output_index = text_output_index;
            let item = p.get("item");
            if item.is_some_and(|item| item["type"] == "message") {
                text_parts.close_item(Some(output_index));
                continue;
            }

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
                        Some(ToolState { .. })
                    );
                if !explicit_tool {
                    let mut matching = blocks_by_output_index.iter().filter_map(|(output_index, state)| {
                        matches!(state, ToolState { call_id, .. } if call_id == completed.call_id)
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
                        let arguments = completed
                            .sanitized_arguments(Some(completed.call_id))
                            .into_owned();
                        out.push(ReducerEvent::ToolStart {
                            index,
                            id: completed.call_id.to_owned(),
                            name: completed.name.to_owned(),
                        });
                        out.push(ReducerEvent::ToolDelta {
                            index,
                            partial_json: arguments.clone(),
                        });
                        if p.get("output_index").is_none()
                            || blocks_by_output_index.contains_key(&output_index)
                            || output_items_by_index.contains_key(&output_index)
                            || text_parts.parts.keys().any(|(oi, _)| *oi == output_index)
                        {
                            output_index = blocks_by_output_index
                                .keys()
                                .chain(output_items_by_index.keys())
                                .chain(text_parts.parts.keys().map(|(oi, _)| oi))
                                .copied()
                                .max()
                                .map_or(0, |index| index + 1);
                        }
                        output_items_by_index.insert(
                            output_index,
                            ResponsesInputItem::FunctionCall {
                                call_id: completed.call_id.to_owned(),
                                name: completed.name.to_owned(),
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

            {
                let ToolState {
                    args_accum,
                    name,
                    call_id,
                    emitted_args,
                    index,
                } = &mut state;
                let completed = completed.as_ref().ok_or_else(|| {
                    CompletedFunctionCallValidationError::Protocol(
                        "tool output closed without a completed function call".to_string(),
                    )
                })?;
                validate_streamed_function_call(completed, call_id, name, args_accum)?;
                if !*emitted_args {
                    *args_accum = completed
                        .sanitized_arguments(Some(call_id.as_str()))
                        .into_owned();
                    out.push(ReducerEvent::ToolDelta {
                        index: *index,
                        partial_json: args_accum.clone(),
                    });
                }
            }

            capture_output_item(output_index, state, &mut output_items_by_index);

            continue;
        }

        if t == "response.completed" || t == "response.incomplete" || t == "response.done" {
            _saw_terminal = true;
            terminal_type = Some(t.to_owned());
            response_id = p
                .get("response")
                .and_then(|r| r.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            final_usage = p.get("response").map(parse_codex_usage);
            if let Some(usage) = final_usage.as_mut() {
                usage.service_tier = usage.service_tier.take().or(service_tier.take());
                usage.rate_limits = latest_rate_limits.take();
            }
            output_limited = crate::providers::codex::events::response_is_incomplete_terminal(&p);
            continuation_eligible = !output_limited && !text_parts.has_refusal();
            if output_limited || text_parts.has_refusal() {
                // A capped/refusal text part may not receive output_item.done. Retain
                // real text, but never finalize an unvalidated tool call.
                if !blocks_by_output_index.is_empty() {
                    return Err(UpstreamStreamError {
                        kind: UpstreamErrorKind::Failed,
                        message: "output limit truncated an unfinished tool call".into(),
                        retry_after_seconds: None,
                        diagnostics: None,
                    });
                }
            }
            text_parts.close_item(None);
            output_items_by_index.extend(text_parts.output_items());
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

    let stop_reason: StopReason = if output_limited {
        STOP_MAX_TOKENS
    } else if text_parts.has_refusal() {
        STOP_REFUSAL
    } else if saw_tool_use {
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
    blocks: &std::collections::HashMap<usize, ToolState>,
) -> Vec<OpenBlockDiagnostic> {
    let mut out: Vec<_> = blocks
        .iter()
        .map(|(output_index, state)| OpenBlockDiagnostic {
            output_index: *output_index,
            anthropic_index: state.index,
            kind: "tool".to_string(),
            name: Some(state.name.clone()),
            call_id: Some(state.call_id.clone()),
            text_bytes: None,
            argument_bytes: Some(state.args_accum.len()),
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
        cache_creation_input_tokens: None,
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
    /// The authenticated lane does not report cache-write accounting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
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
