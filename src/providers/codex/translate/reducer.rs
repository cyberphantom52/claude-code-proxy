use std::borrow::Cow;

use crate::anthropic::sse::sse_events;
use crate::providers::codex::events::{CodexFailureKind, classify_event_failure};

use super::live_stream::{
    BUFFERED_TOOL_MAX_ARGS_BYTES, CompletedFunctionCallValidationError, function_call_display_name,
    text_output_index, validate_completed_function_call, validate_completed_function_call_fields,
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
    pub codex_cache_write_input_tokens: Option<u64>,
    pub codex_cache_write_tokens_details: Option<serde_json::Value>,
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
pub const STOP_PAUSE_TURN: &str = "pause_turn";
pub const STOP_TOOL_USE: &str = "tool_use";
pub const STOP_MAX_TOKENS: &str = "max_tokens";
pub const STOP_REFUSAL: &str = "refusal";
pub const STOP_STOP_SEQUENCE: &str = "stop_sequence";

// Reconcile the whole output before either translator dispatches terminal-only
// items. Retain a bounded native projection, never arbitrary output for replay.
#[derive(Debug, Clone, Default)]
pub(crate) struct CompletedOutputs {
    items: std::collections::BTreeMap<usize, (serde_json::Value, bool)>,
    annotations: std::collections::BTreeMap<(usize, usize), Vec<serde_json::Value>>,
    logprobs: std::collections::BTreeMap<(usize, usize), Vec<serde_json::Value>>,
    retained_bytes: u64,
    summary_parts: TextParts,
    reserved: std::collections::BTreeMap<usize, &'static str>,
    refused: bool,
}

impl CompletedOutputs {
    pub fn has_refusal(&self) -> bool {
        self.refused
    }

    pub fn is_done(&self, output_index: usize) -> bool {
        self.items.get(&output_index).is_some_and(|(_, done)| *done)
    }

    fn index(&self, payload: &serde_json::Value) -> usize {
        payload["output_index"]
            .as_u64()
            .map(|i| i as usize)
            .or_else(|| {
                let item = &payload["item"];
                let id = payload["item_id"].as_str().or_else(|| item["id"].as_str());
                let call_id = item["call_id"].as_str();
                self.items.iter().find_map(|(oi, (old, _))| {
                    (id.is_some_and(|id| old["id"] == id)
                        || call_id.is_some_and(|id| old["call_id"] == id))
                    .then_some(*oi)
                })
            })
            .or_else(|| {
                if payload["type"] != "response.output_item.done" {
                    return None;
                }
                // A delta-only item has a reserved index but no retained start.
                // Reuse it only when the unfinished same-type item is unique.
                let mut matching = self.reserved.iter().filter_map(|(index, kind)| {
                    (!self.items.contains_key(index) && payload["item"]["type"] == *kind)
                        .then_some(*index)
                });
                let index = matching.next()?;
                matching.next().is_none().then_some(index)
            })
            .unwrap_or_else(|| {
                if matches!(
                    payload["type"].as_str(),
                    Some("response.output_item.added" | "response.output_item.done")
                ) && (self.items.contains_key(&0) || self.reserved.contains_key(&0))
                {
                    self.items
                        .keys()
                        .chain(self.reserved.keys())
                        .max()
                        .map_or(0, |i| i + 1)
                } else {
                    0
                }
            })
    }

    fn retain(&mut self, index: usize, item: serde_json::Value, done: bool) -> Result<(), String> {
        let bytes = crate::providers::codex::request_summary::serialized_json_bytes(&item).max(256);
        let old_bytes = self.items.get(&index).map_or(0, |(item, _)| {
            crate::providers::codex::request_summary::serialized_json_bytes(item).max(256)
        });
        let retained = self
            .retained_bytes
            .saturating_sub(old_bytes)
            .saturating_add(bytes);
        check_output_reconciliation_budget(retained, &self.summary_parts)?;
        self.retained_bytes = retained;
        self.items.insert(index, (item, done));
        Ok(())
    }

    pub fn accept<'a>(
        &mut self,
        payload: Cow<'a, serde_json::Value>,
    ) -> Result<Vec<Cow<'a, serde_json::Value>>, String> {
        let kind = payload["type"].as_str().unwrap_or("");
        if classify_event_failure(&payload).is_some() {
            return Ok(vec![payload]);
        }
        let mut out = Vec::new();
        if matches!(
            kind,
            "response.completed" | "response.done" | "response.incomplete"
        ) {
            let incomplete =
                crate::providers::codex::events::response_is_incomplete_terminal(&payload);
            let interrupted =
                crate::providers::codex::events::response_is_interrupted_terminal(&payload);
            // Interruption may finalize partial message text, never a tool call.
            let allows_incomplete =
                |item: &serde_json::Value| incomplete || (interrupted && item["type"] == "message");
            if self
                .items
                .values()
                .any(|(item, _)| item["status"] == "incomplete" && !allows_incomplete(item))
            {
                return Err("successful response contains an incomplete output item".into());
            }
            if let Some(items) = payload
                .get("response")
                .and_then(|value| value.get("output"))
            {
                let items = items
                    .as_array()
                    .ok_or("terminal response output must be an array")?;
                let unsafe_terminal = self.refused
                    || incomplete
                    || interrupted
                    || items.iter().any(|item| {
                        item["content"]
                            .as_array()
                            .is_some_and(|parts| parts.iter().any(|p| p["type"] == "refusal"))
                    });
                for (index, item) in items.iter().enumerate() {
                    if item["status"] == "incomplete" && !allows_incomplete(item) {
                        return Err("successful response contains an incomplete output item".into());
                    }
                    let already_done = self.items.get(&index).is_some_and(|(_, done)| *done);
                    if unsafe_terminal && item["type"] == "function_call" && !already_done {
                        return Err("capped or refused output cannot finalize a tool call".into());
                    }
                    out.extend(self.accept(Cow::Owned(serde_json::json!({"type":"response.output_item.done","output_index":index,"item":item})))?);
                }
            }
            out.push(payload);
            return Ok(out);
        }
        let index = self.index(&payload);
        let part_type = if kind.starts_with("response.output_text.")
            || kind.starts_with("response.refusal.")
            || kind.starts_with("response.content_part.")
        {
            Some("message")
        } else if kind.starts_with("response.reasoning_summary_") {
            Some("reasoning")
        } else {
            None
        };
        if let Some(part_type) = part_type {
            if self
                .reserved
                .get(&index)
                .is_some_and(|old| *old != part_type)
                || self
                    .items
                    .get(&index)
                    .is_some_and(|(old, _)| old["type"] != part_type)
            {
                return Err("streamed output item changed type".into());
            }
            if self.reserved.insert(index, part_type).is_none() {
                self.retained_bytes = self.retained_bytes.saturating_add(256);
                check_output_reconciliation_budget(self.retained_bytes, &self.summary_parts)?;
            }
        }
        self.refused |= kind.starts_with("response.refusal.")
            || payload
                .get("part")
                .and_then(|value| value.get("type"))
                .is_some_and(|p| p == "refusal")
            || payload
                .get("item")
                .and_then(|value| value.get("content"))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|parts| parts.iter().any(|p| p["type"] == "refusal"));
        if kind.starts_with("response.reasoning_summary_") {
            return self
                .reconcile_summary(&payload, index)
                .map(|events| events.into_iter().map(Cow::Owned).collect());
        }
        for annotation in upstream_annotations(&payload, index) {
            let ci = annotation["content_index"].as_u64().unwrap_or(0) as usize;
            let annotations = self.annotations.entry((index, ci)).or_default();
            let annotation = &annotation["annotation"];
            if !annotations.contains(annotation) {
                self.retained_bytes = self.retained_bytes.saturating_add(
                    crate::providers::codex::request_summary::serialized_json_bytes(annotation)
                        .max(256),
                );
                check_output_reconciliation_budget(self.retained_bytes, &self.summary_parts)?;
                annotations.push(annotation.clone());
            }
        }
        if kind == "response.content_part.done"
            && let Some(logprobs) = payload["part"]["logprobs"].as_array()
        {
            let key = (
                index,
                payload["content_index"].as_u64().unwrap_or(0) as usize,
            );
            let bytes =
                crate::providers::codex::request_summary::serialized_json_bytes(logprobs).max(256);
            let old_bytes = self.logprobs.get(&key).map_or(0, |old| {
                crate::providers::codex::request_summary::serialized_json_bytes(old).max(256)
            });
            self.retained_bytes = self
                .retained_bytes
                .saturating_sub(old_bytes)
                .saturating_add(bytes);
            check_output_reconciliation_budget(self.retained_bytes, &self.summary_parts)?;
            self.logprobs.insert(key, logprobs.clone());
        }
        if matches!(
            kind,
            "response.output_item.added" | "response.output_item.done"
        ) && self
            .reserved
            .get(&index)
            .is_some_and(|kind| payload["item"]["type"] != *kind)
        {
            return Err("completed output item disagrees with streamed output type".into());
        }
        if kind == "response.output_item.added" {
            if payload["item"]["type"] == "function_call"
                && payload["item"].get("namespace").is_some_and(|namespace| {
                    !namespace.is_null()
                        && namespace
                            .as_str()
                            .is_none_or(|namespace| namespace.trim().is_empty())
                })
            {
                return Err("function call namespace must be a nonempty string".into());
            }
            if payload["item"]["type"] == "function_call"
                && payload["item"]["status"] == "incomplete"
            {
                return Err("incomplete output cannot start a tool call".into());
            }
            if let Some((old, _)) = self.items.get(&index) {
                for key in ["type", "id", "call_id", "name", "namespace"] {
                    if old
                        .get(key)
                        .zip(payload["item"].get(key))
                        .is_some_and(|(a, b)| a != b)
                    {
                        return Err("output item identity changed".into());
                    }
                }
                return Err("upstream repeated an output item start".into());
            }
            validate_output_message(&payload["item"])?;
            self.retain(index, payload["item"].clone(), false)?;
        } else if kind == "response.output_item.done" {
            let mut item = payload["item"].clone();
            if !item.is_object() || !item["type"].is_string() {
                return Err("completed output item requires a type".into());
            }
            if item.get("status").is_some_and(|status| {
                status != "completed" && !(item["type"] == "message" && status == "incomplete")
            }) {
                return Err("output item is not completed".into());
            }
            if self.items.iter().any(|(oi, (old, _))| {
                *oi != index
                    && (["id", "call_id"].iter().any(|key| {
                        item.get(key)
                            .zip(old.get(key))
                            .is_some_and(|(a, b)| a.is_string() && a == b)
                    }))
            }) {
                return Err("output item identity moved to another index".into());
            }
            let duplicate = if let Some((old, done)) = self.items.get(&index) {
                if item["type"] == "function_call"
                    && item.get("namespace").is_some()
                    && item["namespace"] != old["namespace"]
                {
                    return Err("completed function call namespace changed".into());
                }
                for key in ["type", "id", "call_id", "name", "namespace"] {
                    if old.get(key).zip(item.get(key)).is_some_and(|(a, b)| a != b) {
                        return Err("completed output item identity changed".into());
                    }
                }
                if *done {
                    merge_completed_output(&mut item, old)?;
                } else {
                    for key in [
                        "id",
                        "namespace",
                        "role",
                        "phase",
                        "encrypted_content",
                        "internal_chat_message_metadata_passthrough",
                    ] {
                        if item.get(key).is_none()
                            && let Some(value) = old.get(key)
                        {
                            item[key] = value.clone();
                        }
                    }
                }
                *done
            } else {
                false
            };
            if item["type"] == "function_call" {
                validate_completed_function_call(&item).map_err(|e| e.to_string())?;
            }
            validate_output_message(&item)?;
            if item["type"] == "reasoning" {
                let mut event = payload.clone().into_owned();
                event["item"] = item.clone();
                out.extend(
                    self.reconcile_summary(&event, index)?
                        .into_iter()
                        .map(Cow::Owned),
                );
                self.summary_parts.close_item(Some(index));
            }
            self.retain(index, item.clone(), item["status"] != "incomplete")?;
            let mut event = payload.into_owned();
            event["item"] = item;
            event["output_index"] = index.into();
            event["_ccp_output_already_done"] = duplicate.into();
            out.push(Cow::Owned(event));
            return Ok(out);
        }
        if (kind == "response.output_item.added" || payload.get("item_id").is_some())
            && payload["output_index"].as_u64() != Some(index as u64)
        {
            let mut event = payload.into_owned();
            event["output_index"] = index.into();
            out.push(Cow::Owned(event));
        } else {
            out.push(payload);
        }
        Ok(out)
    }

    fn reconcile_summary(
        &mut self,
        payload: &serde_json::Value,
        index: usize,
    ) -> Result<Vec<serde_json::Value>, String> {
        let kind = payload["type"].as_str().unwrap_or("");
        let mut event = serde_json::json!({"output_index":index});
        if kind == "response.output_item.done" {
            let Some(summary) = payload["item"].get("summary") else {
                return Ok(Vec::new());
            };
            let summary = summary
                .as_array()
                .ok_or("reasoning summary must be an array")?;
            if self
                .summary_parts
                .parts
                .keys()
                .any(|(oi, ci)| *oi == index && *ci >= summary.len())
            {
                return Err("completed reasoning summary omitted a streamed part".into());
            }
            let mut content = Vec::with_capacity(summary.len());
            for part in summary {
                if part["type"] != "summary_text" || !part["text"].is_string() {
                    return Err("unsupported reasoning summary content".into());
                }
                content.push(serde_json::json!({"type":"output_text","text":part["text"]}));
            }
            event["type"] = "response.output_item.done".into();
            event["item"] = serde_json::json!({"type":"message","content":content});
        } else {
            let delta = kind == "response.reasoning_summary_text.delta";
            let text = if kind.starts_with("response.reasoning_summary_part.") {
                if kind.ends_with(".added")
                    && payload["part"]["text"].as_str().is_none_or(str::is_empty)
                {
                    // Empty part starts are not displayed paragraphs yet.
                    return Ok(Vec::new());
                }
                if payload["part"]["type"] != "summary_text" {
                    return Err("unsupported reasoning summary content".into());
                }
                &payload["part"]["text"]
            } else {
                &payload[if delta { "delta" } else { "text" }]
            };
            if !text.is_string() {
                return Err("reasoning summary requires string content".into());
            }
            if kind.ends_with(".added") {
                event["type"] = "response.content_part.added".into();
                event["part"] = serde_json::json!({"type":"output_text","text":text});
            } else {
                event["type"] = if delta {
                    "response.output_text.delta"
                } else {
                    "response.output_text.done"
                }
                .into();
                event[if delta { "delta" } else { "text" }] = text.clone();
            }
            let ci = payload.get("summary_index").map_or(Ok(0), |v| {
                v.as_u64()
                    .and_then(|v| usize::try_from(v).ok())
                    .ok_or("invalid reasoning summary index")
            })?;
            if ci > crate::providers::codex::client::MAX_PENDING_HTTP_EVENT_BYTES / 256 {
                return Err("reasoning summary index exceeds its buffer limit".into());
            }
            event["content_index"] = ci.into();
        }
        let mut previous = self
            .summary_parts
            .parts
            .range((index, 0)..=(index, usize::MAX))
            .next_back()
            .map(|((_, ci), _)| *ci);
        let mut next = self.summary_parts.parts.len();
        self.summary_parts.ordered = true;
        let deltas = self.summary_parts.accept(&event, &mut next, index)?;
        check_output_reconciliation_budget(self.retained_bytes, &self.summary_parts)?;
        let mut suffix = String::new();
        for delta in deltas {
            let ci = delta.content_index;
            if delta.start && previous.is_none_or(|last| ci > last) {
                suffix.push_str(&"\n\n".repeat(ci - previous.unwrap_or(0)));
                previous = Some(ci);
            }
            suffix.push_str(&delta.text);
        }
        if suffix.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(vec![
                serde_json::json!({"type":"response.reasoning_summary_text.delta","output_index":index,"delta":suffix}),
            ])
        }
    }

    pub fn native_items(
        &self,
        items: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
    ) -> bool {
        let mut complete = self.reserved.iter().all(|(index, kind)| {
            *kind != "reasoning" || self.items.get(index).is_some_and(|(_, done)| *done)
        });
        for (index, (item, done)) in &self.items {
            complete &= item["status"] != "incomplete";
            if validate_output_message(item).is_err() {
                complete = false;
                continue;
            }
            if !done && !matches!(item["type"].as_str(), Some("message" | "reasoning")) {
                complete = false;
                continue;
            }
            let native = match item["type"].as_str() {
                Some("function_call") => {
                    // Keep the validated/sanitized tool projection. Other native
                    // tool lanes cannot be silently represented as ordinary calls.
                    complete &= ["encrypted_content", "encrypted_function_args"]
                        .iter()
                        .all(|key| item.get(key).is_none_or(|v| v.is_null()));
                    continue;
                }
                Some("message") => {
                    let mut message = item.clone();
                    if message.get("role").is_none() {
                        message["role"] = "assistant".into();
                    }
                    if !done {
                        message
                            .as_object_mut()
                            .expect("message item")
                            .remove("status");
                    }
                    let fallback_content = !done || message.get("content").is_none();
                    if fallback_content {
                        message["content"] = items
                            .get(index)
                            .and_then(|i| serde_json::to_value(i).ok())
                            .map(|i| i["content"].clone())
                            .unwrap_or_else(|| serde_json::json!([]));
                    }
                    if !fallback_content {
                        for (ci, part) in message["content"]
                            .as_array_mut()
                            .into_iter()
                            .flatten()
                            .enumerate()
                        {
                            if part["type"] == "output_text"
                                && part.get("logprobs").is_none()
                                && let Some(logprobs) = self.logprobs.get(&(*index, ci))
                            {
                                part["logprobs"] =
                                    serde_json::to_value(logprobs).expect("JSON values serialize");
                            }
                            if part["type"] == "output_text"
                                && let Some(extra) = self.annotations.get(&(*index, ci))
                            {
                                if part.get("annotations").is_none() {
                                    part["annotations"] = serde_json::json!([]);
                                }
                                if let Some(annotations) = part["annotations"].as_array_mut() {
                                    for annotation in extra {
                                        if !annotations.contains(annotation) {
                                            annotations.push(annotation.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    serde_json::from_value(message).ok()
                }
                Some("reasoning") => {
                    complete &= item
                        .get("content")
                        .is_none_or(|v| v.is_null() || v.as_array().is_some_and(Vec::is_empty));
                    let mut reasoning = item.clone();
                    if reasoning.get("summary").is_none() {
                        reasoning["summary"] = serde_json::json!([]);
                    }
                    serde_json::from_value(reasoning).ok()
                }
                Some("compaction" | "web_search_call") => serde_json::from_value(item.clone()).ok(),
                _ => None,
            };
            if let Some(mut native) = native {
                if let ResponsesInputItem::Message {
                    content, metadata, ..
                } = &mut native
                {
                    complete &= metadata.phase.as_deref().is_none_or(|phase| {
                        matches!(phase, "commentary" | "partial_answer" | "final_answer")
                    });
                    if let Some(ResponsesInputItem::Message {
                        content: fallback, ..
                    }) = items.get(index)
                    {
                        let text = |parts: &[super::request::ResponsesContentPart]| {
                            parts
                                .iter()
                                .filter_map(|p| match p {
                                    super::request::ResponsesContentPart::OutputText {
                                        text,
                                        ..
                                    } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<String>()
                        };
                        if text(content) != text(fallback) {
                            complete = false;
                            *content = fallback.clone();
                        }
                    }
                }
                match &native {
                    ResponsesInputItem::Reasoning {
                        encrypted_content,
                        summary,
                        ..
                    } => {
                        complete &= !encrypted_content.is_empty()
                            && summary
                                .iter()
                                .all(|p| p["type"] == "summary_text" && p["text"].is_string())
                            && self
                                .summary_parts
                                .parts
                                .range((*index, 0)..=(*index, usize::MAX))
                                .all(|((_, ci), part)| {
                                    summary.get(*ci).is_some_and(|p| p["text"] == part.text)
                                });
                    }
                    ResponsesInputItem::Compaction { encrypted_content } => {
                        complete &= !encrypted_content.is_empty()
                    }
                    // Replay genuine hosted items like Codex; this does not promise
                    // that a fresh connection restores the hidden retrieved corpus.
                    ResponsesInputItem::WebSearchCall { action, .. } => {
                        complete &= action.is_some();
                    }
                    _ => {}
                }
                items.insert(*index, native);
            } else {
                complete = false;
            }
        }
        complete
    }
}

fn check_output_reconciliation_budget(
    retained_bytes: u64,
    summary_parts: &TextParts,
) -> Result<(), String> {
    if retained_bytes.saturating_add(summary_parts.retained_bytes as u64)
        > crate::providers::codex::client::MAX_PENDING_HTTP_EVENT_BYTES as u64
    {
        return Err("Codex output reconciliation exceeded its 16 MiB buffer limit".into());
    }
    Ok(())
}

fn validate_output_message(item: &serde_json::Value) -> Result<(), String> {
    if item["type"] != "message" {
        return Ok(());
    }
    if item.get("role").is_some_and(|role| role != "assistant") {
        return Err("output message role must be assistant".into());
    }
    if let Some(content) = item.get("content") {
        let parts = content
            .as_array()
            .ok_or("output message content must be an array")?;
        for part in parts {
            match part["type"].as_str() {
                Some("output_text") if part["text"].is_string() => {}
                Some("refusal") if part["refusal"].is_string() => {}
                _ => return Err("unsupported output message content".into()),
            }
        }
    }
    Ok(())
}

fn merge_completed_output(
    new: &mut serde_json::Value,
    old: &serde_json::Value,
) -> Result<(), String> {
    match (new, old) {
        (serde_json::Value::Object(new), serde_json::Value::Object(old)) => {
            for (key, value) in old {
                if let Some(current) = new.get_mut(key) {
                    if key == "arguments" && current != value {
                        let parse =
                            |v: &serde_json::Value| {
                                v.as_str().and_then(|s| {
                                    serde_json::from_str::<serde_json::Value>(
                                        if s.trim().is_empty() { "{}" } else { s },
                                    )
                                    .ok()
                                })
                            };
                        if parse(current).is_none() || parse(current) != parse(value) {
                            return Err("completed function call arguments changed".into());
                        }
                    } else {
                        merge_completed_output(current, value)?;
                    }
                } else {
                    new.insert(key.clone(), value.clone());
                }
            }
        }
        (serde_json::Value::Array(new), serde_json::Value::Array(old))
            if new.len() == old.len() =>
        {
            for (new, old) in new.iter_mut().zip(old) {
                merge_completed_output(new, old)?;
            }
        }
        (new, old) if new == old => {}
        _ => return Err("completed output item changed".into()),
    }
    Ok(())
}

// Share reconciliation between buffered and live output, keyed by content part.
#[derive(Debug, Clone, Default)]
pub(crate) struct TextParts {
    parts: std::collections::BTreeMap<(usize, usize), TextPart>,
    completed_items: std::collections::BTreeSet<usize>,
    retained_bytes: usize,
    ordered: bool,
    linear: bool,
    last_emitted: Option<(usize, usize)>,
}
#[derive(Debug, Clone)]
struct TextPart {
    index: usize,
    text: String,
    refusal: bool,
    finalized: bool,
    closed: bool,
}
pub(crate) struct TextDelta {
    pub index: usize,
    pub content_index: usize,
    pub start: bool,
    pub text: String,
    pub refusal: bool,
}
impl TextParts {
    // A flat Chat stream cannot revise earlier text or reopen a finalized part.
    pub fn linear() -> Self {
        Self {
            linear: true,
            ..Self::default()
        }
    }

    pub fn has_refusal(&self) -> bool {
        self.parts.values().any(|part| part.refusal)
    }
    pub fn has_open_parts(&self) -> bool {
        self.parts.values().any(|part| !part.closed)
    }
    pub fn discard_unemitted_parts(&mut self, first_index: usize) {
        self.parts.retain(|_, part| part.index < first_index);
        self.retained_bytes = self.parts.values().map(|part| 256 + part.text.len()).sum();
    }
    pub fn close_item(&mut self, output_index: Option<usize>) -> Vec<usize> {
        self.parts
            .iter_mut()
            .filter_map(|((oi, _), part)| {
                if !part.closed && output_index.is_none_or(|index| index == *oi) {
                    part.closed = true;
                    part.finalized = true;
                    Some(part.index)
                } else {
                    None
                }
            })
            .collect()
    }
    pub fn output_items(
        &mut self,
        completed: &CompletedOutputs,
    ) -> Vec<(usize, ResponsesInputItem)> {
        let mut messages = std::collections::BTreeMap::<usize, Vec<_>>::new();
        for ((oi, ci), part) in &mut self.parts {
            if !part.refusal && !part.text.is_empty() {
                messages.entry(*oi).or_default().push(
                    super::request::ResponsesContentPart::OutputText {
                        annotations: completed.annotations.get(&(*oi, *ci)).cloned(),
                        logprobs: completed.logprobs.get(&(*oi, *ci)).cloned(),
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
                        metadata: Default::default(),
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
        if matches!(
            kind,
            "response.content_part.added" | "response.content_part.done"
        ) {
            let part = &payload["part"];
            if matches!(part["type"].as_str(), Some("output_text" | "refusal")) {
                let refusal = part["type"] == "refusal";
                let text = part[if refusal { "refusal" } else { "text" }]
                    .as_str()
                    .ok_or("text part requires string content")?;
                if kind == "response.content_part.done" || !text.is_empty() {
                    updates.push((
                        output_index,
                        payload["content_index"].as_u64().unwrap_or(0) as usize,
                        text,
                        false,
                        refusal,
                    ));
                }
            }
        }
        let items: Vec<(usize, &serde_json::Value)> = if kind == "response.output_item.done" {
            vec![(output_index, &payload["item"])]
        } else if matches!(
            kind,
            "response.completed" | "response.done" | "response.incomplete"
        ) {
            payload
                .get("response")
                .and_then(|value| value.get("output"))
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
                .collect()
        } else {
            Vec::new()
        };
        let mut completed_items = Vec::new();
        for (oi, item) in items {
            if item["type"] == "message" {
                if let Some(content) = item["content"].as_array()
                    && self
                        .parts
                        .range((oi, 0)..=(oi, usize::MAX))
                        .any(|((_, ci), _)| *ci >= content.len())
                {
                    return Err("completed output message omitted a streamed text part".into());
                }
                if item["status"] != "incomplete" {
                    completed_items.push(oi);
                }
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
        let mut last_emitted = self.last_emitted;
        let mut validated = Vec::with_capacity(updates.len());
        for (oi, ci, text, delta, refusal) in updates {
            let part = self.parts.get(&(oi, ci));
            if part.is_none() && self.completed_items.contains(&oi) {
                return Err("completed output message gained a new text part".into());
            }
            if part.is_some_and(|part| part.refusal != refusal) {
                return Err("upstream text part changed type".into());
            }
            let suffix = if delta {
                text
            } else {
                text.strip_prefix(part.map_or("", |part| part.text.as_str()))
                    .ok_or("text completion disagrees with streamed text")?
            };
            if part.is_some_and(|part| part.finalized || part.closed) && !suffix.is_empty() {
                return Err("completed text part changed after finalization".into());
            }
            if self.ordered
                && !suffix.is_empty()
                && self
                    .parts
                    .range((oi, ci.saturating_add(1))..=(oi, usize::MAX))
                    .next()
                    .is_some()
            {
                return Err("reasoning summary changed an earlier streamed paragraph".into());
            }
            if self.linear && !refusal && !suffix.is_empty() {
                if last_emitted.is_some_and(|last| (oi, ci) < last) {
                    return Err("output text changed an earlier streamed part".into());
                }
                last_emitted = Some((oi, ci));
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
        self.last_emitted = last_emitted;
        self.completed_items.extend(completed_items);
        let finalize_parts = matches!(
            kind,
            "response.output_text.done" | "response.refusal.done" | "response.content_part.done"
        );
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
                    finalized: false,
                    closed: false,
                }
            });
            part.text.push_str(suffix);
            part.finalized |= finalize_parts;
            part.closed |= self.linear && finalize_parts;
            if start || !suffix.is_empty() {
                out.push(TextDelta {
                    index: part.index,
                    content_index: ci,
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
            .get("response")
            .and_then(|value| value.get("output"))
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
        stop_sequence: Option<Box<str>>,
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
    namespace: Option<String>,
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

fn close_thinking(
    out: &mut Vec<ReducerEvent>,
    active_thinking: &mut Option<ActiveThinking>,
    reasoning_by_output_index: &mut std::collections::HashMap<usize, PendingReasoning>,
    output_items_by_index: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
) {
    if let Some(active) = active_thinking.take()
        && let Some(replay) = reasoning_by_output_index
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
    finish_metadata_from_upstream_with_stops(input, &[])
}

pub fn finish_metadata_from_upstream_with_stops(
    input: &[u8],
    stops: &[String],
) -> Result<Option<FinishMetadata>, UpstreamStreamError> {
    let events = reduce_upstream_bytes_with_stops(input, stops)?;
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

// A sampler interruption can retain authoritative partial messages for the next
// full-input round without certifying a cache/socket continuation. Keep this
// exception message-only: tools, reasoning and opaque snapshots need their own
// replay attestation rather than borrowing the partial-text exception.
pub(crate) fn interrupted_message_follow_up(
    input: &[u8],
    finish: &FinishMetadata,
) -> Option<Vec<ResponsesInputItem>> {
    let response_id = finish.response_id.as_deref()?;
    let text = String::from_utf8_lossy(input);
    let mut completed = CompletedOutputs::default();
    let mut finalized = std::collections::BTreeSet::new();
    let mut interrupted = false;
    for event in sse_events(&text) {
        let payload: serde_json::Value = serde_json::from_str(&event.data).ok()?;
        let kind = payload["type"].as_str()?;
        if classify_event_failure(&payload).is_some() || (interrupted && kind != "ping") {
            return None;
        }
        match kind {
            "response.incomplete"
                if crate::providers::codex::events::response_is_interrupted_terminal(&payload)
                    && payload["response"]["id"] == response_id =>
            {
                interrupted = true;
            }
            "response.output_item.added" | "response.output_item.done"
                if payload["item"]["type"] == "message" => {}
            "response.content_part.added" | "response.content_part.done"
                if payload["part"]["type"] == "output_text" => {}
            "response.output_text.delta"
            | "response.output_text.done"
            | "response.output_text.annotation.added"
            | "response.metadata"
            | "response.created"
            | "response.in_progress"
            | "codex.rate_limits"
            | "keepalive"
            | "ping" => {}
            _ => return None,
        }
        for event in completed.accept(Cow::Owned(payload)).ok()? {
            if event["type"] == "response.output_item.done" {
                finalized.insert(event["output_index"].as_u64()? as usize);
            }
        }
    }
    if !interrupted
        || completed.refused
        || completed.items.is_empty()
        || completed.items.len() != finish.output_items.len()
        || !completed
            .reserved
            .iter()
            .all(|(index, kind)| *kind == "message" && completed.items.contains_key(index))
        || !completed
            .items
            .values()
            .any(|(item, _)| item["status"] == "incomplete")
    {
        return None;
    }
    let mut native = Vec::new();
    for ((index, (item, _)), projected) in completed.items.iter().zip(&finish.output_items) {
        if !finalized.contains(index) || validate_output_message(item).is_err() {
            return None;
        }
        let message: ResponsesInputItem = serde_json::from_value(item.clone()).ok()?;
        let ResponsesInputItem::Message {
            metadata, content, ..
        } = &message
        else {
            return None;
        };
        if !content.iter().any(|part| {
            matches!(part, super::request::ResponsesContentPart::OutputText { text, .. } if !text.is_empty())
        }) || metadata.encrypted_content.is_some()
            || metadata.internal_chat_message_metadata_passthrough.is_some()
            || metadata.phase.as_deref().is_some_and(|phase| {
                !matches!(phase, "commentary" | "partial_answer" | "final_answer")
            })
        {
            return None;
        }
        let mut value = serde_json::to_value(&message).ok()?;
        if value != *item {
            return None;
        }
        if value["status"] == "incomplete" {
            value.as_object_mut()?.remove("status");
        }
        if value != serde_json::to_value(projected).ok()? {
            return None;
        }
        native.push(message);
    }
    Some(native)
}

pub fn reduce_upstream_bytes(input: &[u8]) -> Result<Vec<ReducerEvent>, UpstreamStreamError> {
    reduce_upstream_bytes_with_stops(input, &[])
}

pub fn reduce_upstream_bytes_with_stops(
    input: &[u8],
    stops: &[String],
) -> Result<Vec<ReducerEvent>, UpstreamStreamError> {
    use super::stop_sequences::{StopSequences, text_boundary};
    let mut stops = StopSequences::new(stops);
    let mut stopped_at = None;
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
    let mut needs_follow_up = false;
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
                namespace: state.namespace,
                arguments: state.args_accum,
            },
        );
    }

    let mut completed_outputs = CompletedOutputs::default();
    let payloads = sse_events(&text)
        .filter_map(|evt| serde_json::from_str::<serde_json::Value>(evt.data.trim()).ok())
        .flat_map(
            |payload| match completed_outputs.accept(Cow::Owned(payload)) {
                Ok(events) => events
                    .into_iter()
                    .map(|event| Ok(event.into_owned()))
                    .collect::<Vec<_>>(),
                Err(error) => vec![Err(error)],
            },
        );
    for payload in payloads {
        let p = payload.map_err(|message| UpstreamStreamError {
            kind: UpstreamErrorKind::Failed,
            message,
            retry_after_seconds: None,
            diagnostics: None,
        })?;

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

        let terminal_usage = if matches!(t, TERM_COMPLETED | TERM_DONE | TERM_INCOMPLETE) {
            p.get("response").map(parse_codex_usage).transpose()?
        } else {
            None
        };
        let text_output_index = text_output_index(&p, &item_id_to_output_index);
        if text_boundary(&p)
            && let Some(delta) = stops.flush()
        {
            out.push(ReducerEvent::TextDelta {
                index: delta.index,
                text: delta.text,
            });
        }
        if stops.matched().is_none() {
            out.extend(
                upstream_annotations(&p, text_output_index)
                    .into_iter()
                    .map(ReducerEvent::Annotation),
            );
        }
        for delta in text_parts
            .accept(&p, &mut anthropic_index, text_output_index)
            .map_err(|message| UpstreamStreamError {
                kind: UpstreamErrorKind::Failed,
                message,
                retry_after_seconds: None,
                diagnostics: None,
            })?
        {
            for delta in stops.accept(delta, text_output_index).into_iter().flatten() {
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
            if stops.matched().is_some() {
                stopped_at.get_or_insert(out.len());
                break;
            }
        }

        if t == "response.created" {
            service_tier = p
                .get("response")
                .and_then(|value| value.get("service_tier"))
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
                        namespace: item
                            .get("namespace")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                        args_accum: String::new(),
                        emitted_args: false,
                    },
                );
                out.push(ReducerEvent::ToolStart {
                    index: idx,
                    id: call_id,
                    name: function_call_display_name(
                        item.get("namespace").and_then(serde_json::Value::as_str),
                        &name,
                    )
                    .into_owned(),
                });
                continue;
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
            if p["_ccp_output_already_done"] == true {
                continue;
            }
            let output_index = text_output_index;
            let item = p.get("item");
            if item.is_some_and(|item| item["type"] == "message") {
                if item.is_some_and(|item| item["status"] != "incomplete") {
                    if let Some(delta) = stops.flush_item(Some(output_index)) {
                        out.push(ReducerEvent::TextDelta {
                            index: delta.index,
                            text: delta.text,
                        });
                    }
                    text_parts.close_item(Some(output_index));
                }
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
                            name: function_call_display_name(completed.namespace, completed.name)
                                .into_owned(),
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
                                namespace: completed.namespace.map(str::to_owned),
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
                    namespace,
                    call_id,
                    emitted_args,
                    index,
                } = &mut state;
                let completed = completed.as_ref().ok_or_else(|| {
                    CompletedFunctionCallValidationError::Protocol(
                        "tool output closed without a completed function call".to_string(),
                    )
                })?;
                if completed.namespace != namespace.as_deref() {
                    return Err(CompletedFunctionCallValidationError::Protocol(
                        "completed function call namespace changed".into(),
                    )
                    .into());
                }
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
            final_usage = terminal_usage;
            if let Some(usage) = final_usage.as_mut() {
                usage.service_tier = usage.service_tier.take().or(service_tier.take());
                usage.rate_limits = latest_rate_limits.take();
            }
            output_limited = crate::providers::codex::events::response_is_incomplete_terminal(&p);
            needs_follow_up = crate::providers::codex::events::response_needs_follow_up(&p);
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
            if let Some(delta) = stops.flush() {
                out.push(ReducerEvent::TextDelta {
                    index: delta.index,
                    text: delta.text,
                });
            }
            text_parts.close_item(None);
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

    if stops.matched().is_some() && completed_outputs.has_refusal() {
        return Err(UpstreamStreamError {
            kind: UpstreamErrorKind::Failed,
            message: "Codex refused after a local text stop".into(),
            retry_after_seconds: None,
            diagnostics: None,
        });
    }
    let stop_reason: StopReason = if stops.matched().is_some() {
        STOP_STOP_SEQUENCE
    } else if output_limited {
        STOP_MAX_TOKENS
    } else if text_parts.has_refusal() {
        STOP_REFUSAL
    } else if saw_tool_use {
        STOP_TOOL_USE
    } else if needs_follow_up {
        STOP_PAUSE_TURN
    } else {
        STOP_END_TURN
    };

    output_items_by_index.extend(text_parts.output_items(&completed_outputs));
    continuation_eligible &= completed_outputs.native_items(&mut output_items_by_index);
    let output_items: Vec<ResponsesInputItem> = output_items_by_index.into_values().collect();

    let output_items = if let Some(length) = stopped_at {
        out.truncate(length);
        out.retain(|event| !matches!(event, ReducerEvent::Annotation(_)));
        continuation_eligible = false;
        response_id = None;
        Vec::new()
    } else {
        output_items
    };
    out.push(ReducerEvent::Finish {
        stop_reason,
        stop_sequence: stops.matched().map(Box::from),
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

pub(super) fn parse_codex_usage(
    response: &serde_json::Value,
) -> Result<CodexUsage, UpstreamStreamError> {
    let usage = response.get("usage").unwrap_or(&serde_json::Value::Null);
    let invalid = || UpstreamStreamError {
        kind: UpstreamErrorKind::Failed,
        message: "invalid Codex cache-write usage accounting".into(),
        retry_after_seconds: None,
        diagnostics: None,
    };
    let count = |value: Option<&serde_json::Value>| -> Result<Option<u64>, UpstreamStreamError> {
        value
            .filter(|value| !value.is_null())
            .map(|value| value.as_u64().ok_or_else(invalid))
            .transpose()
    };
    let details = usage.get("input_tokens_details");
    let cache_write = count(details.and_then(|details| details.get("cache_write_tokens")))?;
    let cache_write_details = details
        .and_then(|details| details.get("cache_write_tokens_details"))
        .filter(|value| !value.is_null());
    if cache_write.is_some() || cache_write_details.is_some() {
        let input = count(usage.get("input_tokens"))?.ok_or_else(invalid)?;
        let cached = count(details.and_then(|details| details.get("cached_tokens")))?;
        let output = count(usage.get("output_tokens"))?;
        let total = count(usage.get("total_tokens"))?;
        let uncached = input.checked_sub(cached.unwrap_or(0)).ok_or_else(invalid)?;
        if cache_write.is_some_and(|tokens| tokens > uncached) {
            return Err(invalid());
        }
        if let Some(details) = cache_write_details {
            let details = details.as_object().ok_or_else(invalid)?;
            for value in details.values() {
                if let Some(tokens) = count(Some(value))?
                    && tokens > cache_write.unwrap_or(uncached)
                {
                    return Err(invalid());
                }
            }
        }
        if let Some(total) = total
            && (total < input
                || output.is_some_and(|output| input.checked_add(output) != Some(total)))
        {
            return Err(invalid());
        }
    }
    Ok(CodexUsage {
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
        codex_cache_write_input_tokens: cache_write,
        codex_cache_write_tokens_details: cache_write_details.cloned(),
        output_tokens_details_reasoning: usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64()),
        rate_limits: None,
    })
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
        codex_cache_write_input_tokens: usage.codex_cache_write_input_tokens,
        codex_cache_write_tokens_details: usage.codex_cache_write_tokens_details.clone(),
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
    /// Native write counts have no verified Anthropic pricing/TTL mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    /// Actual Codex cache writes, independent of Anthropic cache creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_cache_write_input_tokens: Option<u64>,
    /// Original native detail keys, without TTL or pricing reinterpretation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_cache_write_tokens_details: Option<serde_json::Value>,
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
