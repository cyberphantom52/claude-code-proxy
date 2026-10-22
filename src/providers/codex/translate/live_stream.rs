use std::collections::HashMap;

use crate::anthropic::sse::encode_sse_event;
use crate::providers::codex::client::MAX_PENDING_HTTP_EVENT_BYTES;
use crate::providers::codex::events::classify_event_failure;
use crate::traffic::TrafficCapture;

use super::read_rewrite::{repair_whitespace_stalled_read_args, sanitize_read_args};
use super::reasoning_signature::{PendingReasoning, encode_reasoning_signature};
use super::reducer::{
    CodexRateLimits, CodexUsage, STOP_END_TURN, STOP_TOOL_USE, map_codex_usage_to_anthropic,
    parse_codex_rate_limits, parse_codex_usage,
};
use super::web_search_compat::web_search_query;

pub(crate) const BUFFERED_TOOL_MAX_ARGS_BYTES: usize = 5_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletedFunctionCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompletedFunctionCallValidationError {
    Protocol(String),
    TooLarge { tool: String },
}

impl std::fmt::Display for CompletedFunctionCallValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(message) => formatter.write_str(message),
            Self::TooLarge { tool } => {
                write!(
                    formatter,
                    "Buffered {tool} tool arguments exceeded safe limits"
                )
            }
        }
    }
}

pub(crate) fn validate_completed_function_call(
    item: &serde_json::Value,
) -> Result<CompletedFunctionCall, CompletedFunctionCallValidationError> {
    let call_id = item
        .get("call_id")
        .and_then(serde_json::Value::as_str)
        .filter(|call_id| !call_id.trim().is_empty())
        .ok_or_else(|| {
            CompletedFunctionCallValidationError::Protocol(
                "completed function call is missing call_id".to_string(),
            )
        })?;
    let name = item
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            CompletedFunctionCallValidationError::Protocol(
                "completed function call is missing name".to_string(),
            )
        })?;
    let arguments = item
        .get("arguments")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            CompletedFunctionCallValidationError::Protocol(
                "completed function call is missing string arguments".to_string(),
            )
        })?;

    if arguments.len() > BUFFERED_TOOL_MAX_ARGS_BYTES {
        return Err(CompletedFunctionCallValidationError::TooLarge {
            tool: name.to_string(),
        });
    }
    if arguments.trim().is_empty() {
        return Ok(CompletedFunctionCall {
            call_id: call_id.to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
        });
    }

    let value = serde_json::from_str::<serde_json::Value>(arguments).map_err(|_| {
        CompletedFunctionCallValidationError::Protocol(
            "completed function call arguments are not valid JSON".to_string(),
        )
    })?;
    if !value.is_object() {
        return Err(CompletedFunctionCallValidationError::Protocol(
            "completed function call arguments must be a JSON object".to_string(),
        ));
    }

    Ok(CompletedFunctionCall {
        call_id: call_id.to_string(),
        name: name.to_string(),
        arguments: arguments.to_string(),
    })
}

pub(crate) fn validate_streamed_function_call(
    completed: &CompletedFunctionCall,
    call_id: &str,
    name: &str,
    arguments: &str,
) -> Result<(), CompletedFunctionCallValidationError> {
    let disagreement = || {
        CompletedFunctionCallValidationError::Protocol(
            "completed function call disagrees with streamed tool state".to_string(),
        )
    };
    if completed.call_id != call_id || completed.name != name {
        return Err(disagreement());
    }
    if arguments.is_empty() {
        return Ok(());
    }
    if arguments.trim().is_empty() {
        // Blank authoritative arguments normalize to {}, but whitespace already
        // streamed downstream is not an executable JSON object.
        return Err(CompletedFunctionCallValidationError::Protocol(
            "streamed function call arguments are not valid JSON".to_string(),
        ));
    }
    // Read is buffered: repair only at authoritative finalization, never close
    // the turn from a delta or hide a later upstream failure/usage event.
    let streamed = validate_completed_function_call(&serde_json::json!({
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    }));
    let (streamed_arguments, final_arguments) = match streamed {
        Ok(streamed) => (streamed.arguments, completed.arguments.clone()),
        Err(error @ CompletedFunctionCallValidationError::TooLarge { .. }) => return Err(error),
        Err(error) => match repair_whitespace_stalled_read_args(name, arguments, None) {
            Some(repaired) => (
                repaired,
                sanitize_read_args(name, &completed.arguments, None),
            ),
            None => return Err(error),
        },
    };
    if serde_json::from_str::<serde_json::Value>(&streamed_arguments).ok()
        != serde_json::from_str::<serde_json::Value>(&final_arguments).ok()
    {
        return Err(disagreement());
    }
    Ok(())
}

enum LiveBlock {
    Text {
        index: usize,
        text: String,
        deferred: bool,
    },
    Tool {
        index: usize,
        call_id: String,
        name: String,
        args_accum: String,
        emitted_args: bool,
    },
}

struct LiveWebSearch {
    index: usize,
    result_index: usize,
    id: String,
    query: String,
}

#[derive(Clone)]
struct LiveWebSearchResult {
    title: String,
    url: String,
}

#[derive(Clone, Copy)]
struct LiveThinking {
    output_index: usize,
    anthropic_index: usize,
}

pub struct LiveStreamTranslator {
    message_id: String,
    model: String,
    message_started: bool,
    blocks_by_output_index: HashMap<usize, LiveBlock>,
    item_id_to_output_index: HashMap<String, usize>,
    anthropic_index: usize,
    thinking: Option<LiveThinking>,
    reasoning_by_output_index: HashMap<usize, PendingReasoning>,
    saw_tool_use: bool,
    web_search_requests: usize,
    web_searches: Vec<LiveWebSearch>,
    web_search_results: Vec<LiveWebSearchResult>,
    deferred_text: Vec<(usize, String)>,
    deferred_text_bytes: usize,
    semantic_output_started: bool,
    text_output_started: bool,
    // Seeds Claude Code's live subagent counter until the provider returns
    // authoritative usage in the terminal message_delta.
    estimated_input_tokens: u64,
    finished: bool,
    // Latest `codex.rate_limits` event seen on this stream; handed to the
    // usage mapper at finish so the terminal usage block carries the meter.
    rate_limits: Option<Box<CodexRateLimits>>,
    service_tier: Option<String>,
}

impl LiveStreamTranslator {
    pub fn with_estimated_input_tokens(
        message_id: impl Into<String>,
        model: impl Into<String>,
        estimated_input_tokens: u64,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            model: model.into(),
            message_started: false,
            blocks_by_output_index: HashMap::new(),
            item_id_to_output_index: HashMap::new(),
            anthropic_index: 0,
            thinking: None,
            reasoning_by_output_index: HashMap::new(),
            saw_tool_use: false,
            web_search_requests: 0,
            web_searches: Vec::new(),
            web_search_results: Vec::new(),
            deferred_text: Vec::new(),
            deferred_text_bytes: 0,
            semantic_output_started: false,
            text_output_started: false,
            estimated_input_tokens,
            finished: false,
            rate_limits: None,
            service_tier: None,
        }
    }

    pub fn accept(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
    ) -> Result<Vec<u8>, String> {
        if self.finished {
            return Ok(Vec::new());
        }

        let kind = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let mut out = Vec::new();

        if let Some(failure) = classify_event_failure(payload) {
            return Err(failure.message);
        }

        match kind {
            "codex.rate_limits" => {
                self.rate_limits = parse_codex_rate_limits(payload).or(self.rate_limits.take());
                self.emit_ping(traffic, &mut out);
            }
            "response.created" => {
                self.service_tier = payload
                    .pointer("/response/service_tier")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                self.emit_ping(traffic, &mut out);
            }
            "keepalive" | "response.in_progress" => {
                self.emit_ping(traffic, &mut out);
            }
            "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed" => {}
            "response.output_item.added" => {
                self.output_item_added(payload, traffic, &mut out);
            }
            "response.reasoning_summary_part.added" => {
                let output_index = output_index(payload);
                if let Some(thinking) = self
                    .thinking
                    .filter(|thinking| thinking.output_index == output_index)
                {
                    self.emit(
                        traffic,
                        &mut out,
                        "content_block_delta",
                        &serde_json::json!({
                            "type": "content_block_delta",
                            "index": thinking.anthropic_index,
                            "delta": {"type": "thinking_delta", "thinking": "\n\n"}
                        }),
                    );
                }
            }
            "response.reasoning_summary_text.delta" => {
                self.reasoning_delta(payload, traffic, &mut out);
            }
            "response.output_text.delta" => {
                self.text_delta(payload, traffic, &mut out)?;
            }
            "response.output_text.annotation.added" => {
                self.web_search_annotation(payload);
            }
            "response.function_call_arguments.delta" => {
                self.tool_delta(payload, traffic, &mut out)?;
            }
            "response.function_call_arguments.done" => {
                self.tool_arguments_done(payload)?;
            }
            "response.output_item.done" => {
                self.output_item_done(payload, traffic, &mut out)?;
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                self.finish(payload, traffic, &mut out)?;
            }
            _ => {}
        }

        Ok(out)
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn has_semantic_output(&self) -> bool {
        self.semantic_output_started
    }

    /// Only thinking has been exposed. Text and tool blocks cannot be replayed
    /// safely because the consumer may already have acted on them.
    pub fn can_reconnect(&self) -> bool {
        !self.finished
            && !self.text_output_started
            && !self.saw_tool_use
            && self.web_search_requests == 0
            && self.blocks_by_output_index.is_empty()
    }

    pub fn prepare_reconnect(&mut self, traffic: Option<&TrafficCapture>) -> Vec<u8> {
        let mut out = Vec::new();
        self.close_thinking(traffic, &mut out);
        self.reasoning_by_output_index.clear();
        self.item_id_to_output_index.clear();
        self.service_tier = None;
        // Preserve message_started and anthropic_index: one downstream message,
        // with fresh block indices for the replacement generation.
        out
    }

    pub fn ping_chunk(&mut self, traffic: Option<&TrafficCapture>) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.finished {
            self.emit_ping(traffic, &mut out);
        }
        out
    }

    pub fn finish_after_closed_completed_tool_call(
        &mut self,
        traffic: Option<&TrafficCapture>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished || !self.saw_tool_use || !self.blocks_by_output_index.is_empty() {
            return out;
        }
        self.close_thinking(traffic, &mut out);
        self.ensure_message_start(traffic, &mut out);
        self.emit_finish(STOP_TOOL_USE, None, traffic, &mut out);
        out
    }

    pub fn error_chunk(
        &mut self,
        message: &str,
        error_type: &str,
        traffic: Option<&TrafficCapture>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.close_open_blocks(traffic, &mut out);
        self.deferred_text.clear();
        self.deferred_text_bytes = 0;
        self.ensure_message_start(traffic, &mut out);
        self.emit(
            traffic,
            &mut out,
            "error",
            &serde_json::json!({
                "type": "error",
                "error": {
                    "type": error_type,
                    "message": message,
                }
            }),
        );
        self.finished = true;
        out
    }

    fn ensure_message_start(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        if self.message_started {
            return;
        }
        self.message_started = true;
        self.emit(
            traffic,
            out,
            "message_start",
            &serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": self.estimated_input_tokens,
                        "output_tokens": 0
                    }
                }
            }),
        );
    }

    fn emit_ping(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        self.ensure_message_start(traffic, out);
        self.emit(traffic, out, "ping", &serde_json::json!({"type": "ping"}));
    }

    fn emit(
        &self,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
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

    fn output_item_added(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let Some(item) = payload.get("item") else {
            return;
        };
        let output_index = output_index(payload);
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match item_type {
            "reasoning" => {
                self.reasoning_by_output_index
                    .entry(output_index)
                    .or_default()
                    .capture(item);
            }
            "message" => {
                self.close_thinking(traffic, out);
                let index = self.anthropic_index;
                self.anthropic_index += 1;
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    self.item_id_to_output_index
                        .insert(id.to_string(), output_index);
                }
                let deferred = !self.web_searches.is_empty();
                self.blocks_by_output_index.insert(
                    output_index,
                    LiveBlock::Text {
                        index,
                        text: String::new(),
                        deferred,
                    },
                );
                if !deferred {
                    self.ensure_message_start(traffic, out);
                    self.emit(
                        traffic,
                        out,
                        "content_block_start",
                        &serde_json::json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {"type": "text", "text": ""}
                        }),
                    );
                }
            }
            "function_call" => {
                self.close_thinking(traffic, out);
                self.saw_tool_use = true;
                let index = self.anthropic_index;
                self.anthropic_index += 1;
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
                self.blocks_by_output_index.insert(
                    output_index,
                    LiveBlock::Tool {
                        index,
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args_accum: String::new(),
                        emitted_args: false,
                    },
                );
                self.ensure_message_start(traffic, out);
                self.emit(
                    traffic,
                    out,
                    "content_block_start",
                    &serde_json::json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": call_id,
                            "name": name,
                            "input": {}
                        }
                    }),
                );
            }
            "web_search_call" => {
                self.web_search_requests += 1;
            }
            _ => {}
        }
    }

    fn reasoning_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let output_index = output_index(payload);
        let delta = payload.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if delta.is_empty() {
            return;
        }
        self.semantic_output_started = true;
        if self.thinking.map(|thinking| thinking.output_index) != Some(output_index) {
            self.close_thinking(traffic, out);
            let index = self.anthropic_index;
            self.anthropic_index += 1;
            self.thinking = Some(LiveThinking {
                output_index,
                anthropic_index: index,
            });
            self.ensure_message_start(traffic, out);
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "thinking", "thinking": "", "signature": ""}
                }),
            );
        }
        let index = self
            .thinking
            .expect("thinking block was started")
            .anthropic_index;
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "thinking_delta", "thinking": delta}
            }),
        );
    }

    fn text_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let delta = payload.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if delta.is_empty() {
            self.close_thinking(traffic, out);
            return Ok(());
        }

        let output_index = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .or_else(|| {
                payload
                    .get("item_id")
                    .and_then(|v| v.as_str())
                    .and_then(|id| self.item_id_to_output_index.get(id).copied())
            })
            .unwrap_or(0);

        let deferred = match self.blocks_by_output_index.get(&output_index) {
            Some(LiveBlock::Text { deferred, .. }) => *deferred,
            None => !self.web_searches.is_empty(),
            _ => false,
        };
        let next_bytes = self.deferred_text_bytes.saturating_add(delta.len());
        if deferred && next_bytes > MAX_PENDING_HTTP_EVENT_BYTES {
            return Err("Codex deferred text exceeded its 16 MiB buffer limit".to_string());
        }
        self.close_thinking(traffic, out);
        self.semantic_output_started = true;
        self.text_output_started = true;

        if !self.blocks_by_output_index.contains_key(&output_index) {
            let index = self.anthropic_index;
            self.anthropic_index += 1;
            self.blocks_by_output_index.insert(
                output_index,
                LiveBlock::Text {
                    index,
                    text: String::new(),
                    deferred,
                },
            );
            if !deferred {
                self.ensure_message_start(traffic, out);
                self.emit(
                    traffic,
                    out,
                    "content_block_start",
                    &serde_json::json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {"type": "text", "text": ""}
                    }),
                );
            }
        }

        let Some(LiveBlock::Text {
            index,
            text,
            deferred,
        }) = self.blocks_by_output_index.get_mut(&output_index)
        else {
            return Ok(());
        };
        if *deferred {
            text.push_str(delta);
            self.deferred_text_bytes = next_bytes;
            return Ok(());
        }
        let index = *index;
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": delta}
            }),
        );
        Ok(())
    }

    fn tool_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let Some(output_index) = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
        else {
            return Ok(());
        };
        let delta = payload.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if delta.is_empty() {
            return Ok(());
        }
        self.semantic_output_started = true;
        let Some(LiveBlock::Tool {
            index,
            name,
            args_accum,
            emitted_args,
            ..
        }) = self.blocks_by_output_index.get_mut(&output_index)
        else {
            return Ok(());
        };
        if args_accum.len().saturating_add(delta.len()) > BUFFERED_TOOL_MAX_ARGS_BYTES {
            return Err(format!(
                "Buffered {name} tool arguments exceeded safe limits"
            ));
        }
        args_accum.push_str(delta);
        if name != "Read" {
            *emitted_args = true;
            let index = *index;
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": delta}
                }),
            );
        }
        Ok(())
    }

    fn tool_arguments_done(&mut self, payload: &serde_json::Value) -> Result<(), String> {
        let Some(LiveBlock::Tool {
            args_accum,
            call_id,
            name,
            ..
        }) = self.blocks_by_output_index.get_mut(&output_index(payload))
        else {
            return Ok(());
        };
        let completed = validate_completed_function_call(&serde_json::json!({
            "call_id": call_id,
            "name": name,
            "arguments": payload.get("arguments"),
        }))
        .map_err(|error| error.to_string())?;
        validate_streamed_function_call(&completed, call_id, name, args_accum)
            .map_err(|error| error.to_string())?;
        if args_accum.is_empty() {
            *args_accum = completed.arguments;
        }
        Ok(())
    }

    fn output_item_done(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let mut output_index = output_index(payload);
        if let Some(item) = payload
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(|v| v.as_str())
            .filter(|item_type| *item_type == "reasoning")
            .and_then(|_| payload.get("item"))
        {
            self.reasoning_by_output_index
                .entry(output_index)
                .or_default()
                .capture(item);
            let had_active_summary = self
                .thinking
                .is_some_and(|thinking| thinking.output_index == output_index);
            self.close_thinking(traffic, out);
            if !had_active_summary {
                self.emit_signature_only_reasoning(output_index, traffic, out);
            }
            return Ok(());
        }

        if payload
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(|v| v.as_str())
            == Some("web_search_call")
        {
            self.close_thinking(traffic, out);
            self.semantic_output_started = true;
            let item = &payload["item"];
            let index = self.anthropic_index;
            self.anthropic_index += 1;
            let result_index = self.anthropic_index;
            self.anthropic_index += 1;
            let raw_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            self.web_searches.push(LiveWebSearch {
                index,
                result_index,
                id: super::web_search_compat::server_tool_use_id_from_codex_web_search_id(raw_id),
                query: web_search_query(item),
            });
            return Ok(());
        }

        let completed_function_call = if payload
            .pointer("/item/type")
            .and_then(|value| value.as_str())
            == Some("function_call")
        {
            let completed = validate_completed_function_call(&payload["item"])
                .map_err(|error| error.to_string())?;
            let Some(resolved_output_index) = self.open_tool_output_index_for_done(payload) else {
                self.emit_completed_function_call(&completed, traffic, out);
                return Ok(());
            };
            output_index = resolved_output_index;
            Some(completed)
        } else {
            None
        };

        if let Some(LiveBlock::Tool {
            call_id,
            name,
            args_accum,
            ..
        }) = self.blocks_by_output_index.get(&output_index)
        {
            let completed = completed_function_call
                .as_ref()
                .ok_or("tool output closed without a completed function call")?;
            validate_streamed_function_call(completed, call_id, name, args_accum)
                .map_err(|error| error.to_string())?;
        }
        let Some(mut state) = self.blocks_by_output_index.remove(&output_index) else {
            return Ok(());
        };

        match &mut state {
            LiveBlock::Text {
                index,
                text,
                deferred,
            } => {
                if *deferred {
                    self.deferred_text.push((*index, std::mem::take(text)));
                } else {
                    self.emit(
                        traffic,
                        out,
                        "content_block_stop",
                        &serde_json::json!({
                            "type": "content_block_stop",
                            "index": index,
                        }),
                    );
                }
            }
            LiveBlock::Tool {
                index,
                name,
                call_id,
                args_accum,
                emitted_args,
                ..
            } => {
                let completed = completed_function_call
                    .as_ref()
                    .expect("validated tool completion");
                self.semantic_output_started = true;
                if !*emitted_args {
                    *args_accum =
                        sanitize_read_args(name, &completed.arguments, Some(call_id.as_str()));
                    self.emit(
                        traffic,
                        out,
                        "content_block_delta",
                        &serde_json::json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": {"type": "input_json_delta", "partial_json": args_accum}
                        }),
                    );
                }
                self.emit(
                    traffic,
                    out,
                    "content_block_stop",
                    &serde_json::json!({
                        "type": "content_block_stop",
                        "index": index,
                    }),
                );
            }
        }
        Ok(())
    }

    fn emit_completed_function_call(
        &mut self,
        completed: &CompletedFunctionCall,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let arguments = sanitize_read_args(
            &completed.name,
            &completed.arguments,
            Some(&completed.call_id),
        );

        self.close_thinking(traffic, out);
        self.saw_tool_use = true;
        self.semantic_output_started = true;
        let index = self.anthropic_index;
        self.anthropic_index += 1;
        self.ensure_message_start(traffic, out);
        self.emit(
            traffic,
            out,
            "content_block_start",
            &serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "tool_use",
                    "id": completed.call_id,
                    "name": completed.name,
                    "input": {}
                }
            }),
        );
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": arguments
                }
            }),
        );
        self.emit(
            traffic,
            out,
            "content_block_stop",
            &serde_json::json!({
                "type": "content_block_stop",
                "index": index,
            }),
        );
    }

    fn open_tool_output_index_for_done(&self, payload: &serde_json::Value) -> Option<usize> {
        let call_id = payload
            .pointer("/item/call_id")
            .and_then(serde_json::Value::as_str)?;
        if let Some(output_index) = payload
            .get("output_index")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            && matches!(
                self.blocks_by_output_index.get(&output_index),
                Some(LiveBlock::Tool { .. })
            )
        {
            return Some(output_index);
        }

        let mut matching =
            self.blocks_by_output_index
                .iter()
                .filter_map(|(output_index, block)| {
                    matches!(
                        block,
                        LiveBlock::Tool { call_id: open_call_id, .. } if open_call_id == call_id
                    )
                    .then_some(*output_index)
                });
        let output_index = matching.next()?;
        matching.next().is_none().then_some(output_index)
    }

    fn web_search_annotation(&mut self, payload: &serde_json::Value) {
        let Some(annotation) = payload.get("annotation") else {
            return;
        };
        if annotation.get("type").and_then(|v| v.as_str()) != Some("url_citation") {
            return;
        }
        let Some(url) = annotation.get("url").and_then(|v| v.as_str()) else {
            return;
        };
        if self
            .web_search_results
            .iter()
            .any(|result| result.url == url)
        {
            return;
        }
        let title = annotation
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|title| !title.is_empty())
            .unwrap_or(url);
        self.web_search_results.push(LiveWebSearchResult {
            title: title.to_string(),
            url: url.to_string(),
        });
    }

    fn emit_web_searches(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        let searches = std::mem::take(&mut self.web_searches);
        for search in searches {
            self.ensure_message_start(traffic, out);
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": search.index,
                    "content_block": {
                        "type": "server_tool_use",
                        "id": search.id,
                        "name": "web_search",
                        "input": {}
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": search.index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": serde_json::to_string(&serde_json::json!({"query": search.query})).unwrap_or_default()
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": search.index}),
            );
            let results: Vec<_> = self
                .web_search_results
                .iter()
                .map(|result| {
                    serde_json::json!({
                        "type": "web_search_result",
                        "title": result.title,
                        "url": result.url,
                    })
                })
                .collect();
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": search.result_index,
                    "content_block": {
                        "type": "web_search_tool_result",
                        "tool_use_id": search.id,
                        "content": results
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": search.result_index}),
            );
        }

        for (index, text) in std::mem::take(&mut self.deferred_text) {
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "text", "text": ""}
                }),
            );
            if !text.is_empty() {
                self.emit(
                    traffic,
                    out,
                    "content_block_delta",
                    &serde_json::json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                );
            }
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": index}),
            );
        }
        self.deferred_text_bytes = 0;
    }

    fn finish(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        if self
            .blocks_by_output_index
            .values()
            .any(|block| matches!(block, LiveBlock::Tool { .. }))
        {
            return Err("upstream stream ended with open Codex tool blocks".to_string());
        }
        self.close_thinking(traffic, out);
        self.close_open_blocks(traffic, out);
        self.emit_web_searches(traffic, out);
        self.ensure_message_start(traffic, out);
        let mut usage = payload.get("response").map(parse_codex_usage);
        if let Some(usage) = usage.as_mut() {
            usage.service_tier = usage.service_tier.take().or(self.service_tier.take());
            usage.rate_limits = self.rate_limits.clone();
        }
        let stop_reason = if self.saw_tool_use {
            STOP_TOOL_USE
        } else {
            STOP_END_TURN
        };
        self.emit_finish(stop_reason, usage, traffic, out);
        Ok(())
    }

    fn emit_finish(
        &mut self,
        stop_reason: &str,
        usage: Option<CodexUsage>,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let mapped = map_codex_usage_to_anthropic(&usage, Some(self.web_search_requests));
        self.emit(
            traffic,
            out,
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
        self.emit(
            traffic,
            out,
            "message_stop",
            &serde_json::json!({"type": "message_stop"}),
        );
        self.finished = true;
    }

    fn close_open_blocks(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        self.close_thinking(traffic, out);
        let open: Vec<usize> = self.blocks_by_output_index.keys().copied().collect();
        for output_index in open {
            let Some(state) = self.blocks_by_output_index.remove(&output_index) else {
                continue;
            };
            let index = match state {
                LiveBlock::Text {
                    index,
                    text,
                    deferred: true,
                } => {
                    self.deferred_text.push((index, text));
                    continue;
                }
                LiveBlock::Text { index, .. } => index,
                LiveBlock::Tool { index, .. } => index,
            };
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": index,
                }),
            );
        }
    }

    fn emit_signature_only_reasoning(
        &mut self,
        output_index: usize,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let Some(replay) = self
            .reasoning_by_output_index
            .remove(&output_index)
            .and_then(|pending| pending.replay())
        else {
            return;
        };
        let Some(signature) = encode_reasoning_signature(&replay) else {
            return;
        };
        self.semantic_output_started = true;
        let index = self.anthropic_index;
        self.anthropic_index += 1;
        self.ensure_message_start(traffic, out);
        self.emit(
            traffic,
            out,
            "content_block_start",
            &serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}
            }),
        );
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "signature_delta", "signature": signature}
            }),
        );
        self.emit(
            traffic,
            out,
            "content_block_stop",
            &serde_json::json!({
                "type": "content_block_stop",
                "index": index,
            }),
        );
    }

    fn close_thinking(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        let Some(thinking) = self.thinking.take() else {
            return;
        };
        if let Some(signature) = self
            .reasoning_by_output_index
            .remove(&thinking.output_index)
            .and_then(|pending| pending.replay())
            .and_then(|replay| encode_reasoning_signature(&replay))
        {
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": thinking.anthropic_index,
                    "delta": {"type": "signature_delta", "signature": signature}
                }),
            );
        }
        self.emit(
            traffic,
            out,
            "content_block_stop",
            &serde_json::json!({
                "type": "content_block_stop",
                "index": thinking.anthropic_index,
            }),
        );
    }
}

fn output_index(payload: &serde_json::Value) -> usize {
    payload
        .get("output_index")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize
}
