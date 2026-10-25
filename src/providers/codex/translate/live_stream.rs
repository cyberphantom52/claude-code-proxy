use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::anthropic::sse::append_json_sse_event;
use crate::providers::codex::events::classify_event_failure;
use crate::traffic::TrafficCapture;

use super::read_rewrite::{repair_whitespace_stalled_read_args, sanitize_parsed_read_args};
use super::reasoning_signature::{PendingReasoning, encode_reasoning_signature};
use super::reducer::{
    CodexRateLimits, CodexUsage, CompletedOutputs, STOP_END_TURN, STOP_MAX_TOKENS, STOP_PAUSE_TURN,
    STOP_REFUSAL, STOP_TOOL_USE, TextDelta, TextParts, map_codex_usage_to_anthropic,
    parse_codex_rate_limits, parse_codex_usage, upstream_annotations,
};
use super::stop_sequences::{StopSequences, text_boundary};
use super::web_search_compat::web_search_query;

pub(crate) const BUFFERED_TOOL_MAX_ARGS_BYTES: usize = 5_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletedFunctionCall<'a> {
    pub call_id: &'a str,
    pub name: &'a str,
    pub namespace: Option<&'a str>,
    pub arguments: Cow<'a, str>,
    pub parsed_arguments: serde_json::Value,
}

pub(crate) fn function_call_display_name<'a>(
    namespace: Option<&str>,
    name: &'a str,
) -> Cow<'a, str> {
    match namespace {
        None | Some("functions") => Cow::Borrowed(name),
        Some(namespace) => Cow::Owned(format!("{namespace}.{name}")),
    }
}

impl CompletedFunctionCall<'_> {
    pub(crate) fn sanitized_arguments(&self, call_id: Option<&str>) -> Cow<'_, str> {
        if self
            .namespace
            .is_some_and(|namespace| namespace != "functions")
        {
            return Cow::Borrowed(&self.arguments);
        }
        sanitize_parsed_read_args(
            self.name,
            &self.arguments,
            Cow::Borrowed(&self.parsed_arguments),
            call_id,
        )
    }
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
) -> Result<CompletedFunctionCall<'_>, CompletedFunctionCallValidationError> {
    if item
        .get("status")
        .is_some_and(|status| status != "completed")
    {
        return Err(CompletedFunctionCallValidationError::Protocol(
            "function call is not completed".into(),
        ));
    }
    let mut completed = validate_completed_function_call_fields(
        item.get("call_id").and_then(serde_json::Value::as_str),
        item.get("name").and_then(serde_json::Value::as_str),
        item.get("arguments").and_then(serde_json::Value::as_str),
    )?;
    completed.namespace = match item.get("namespace") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(namespace)) if !namespace.trim().is_empty() => {
            Some(namespace)
        }
        Some(_) => {
            return Err(CompletedFunctionCallValidationError::Protocol(
                "function call namespace must be a nonempty string".into(),
            ));
        }
    };
    Ok(completed)
}

pub(crate) fn validate_completed_function_call_fields<'a>(
    call_id: Option<&'a str>,
    name: Option<&'a str>,
    arguments: Option<&'a str>,
) -> Result<CompletedFunctionCall<'a>, CompletedFunctionCallValidationError> {
    let call_id = call_id
        .filter(|call_id| !call_id.trim().is_empty())
        .ok_or_else(|| {
            CompletedFunctionCallValidationError::Protocol(
                "completed function call is missing call_id".to_string(),
            )
        })?;
    let name = name.filter(|name| !name.trim().is_empty()).ok_or_else(|| {
        CompletedFunctionCallValidationError::Protocol(
            "completed function call is missing name".to_string(),
        )
    })?;
    let arguments = arguments.ok_or_else(|| {
        CompletedFunctionCallValidationError::Protocol(
            "completed function call is missing string arguments".to_string(),
        )
    })?;
    if arguments.len() > BUFFERED_TOOL_MAX_ARGS_BYTES {
        return Err(CompletedFunctionCallValidationError::TooLarge {
            tool: name.to_string(),
        });
    }
    let arguments = if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    };
    let parsed_arguments = serde_json::from_str::<serde_json::Value>(arguments).map_err(|_| {
        CompletedFunctionCallValidationError::Protocol(
            "completed function call arguments are not valid JSON".to_string(),
        )
    })?;
    if !parsed_arguments.is_object() {
        return Err(CompletedFunctionCallValidationError::Protocol(
            "completed function call arguments must be a JSON object".to_string(),
        ));
    }
    Ok(CompletedFunctionCall {
        call_id,
        name,
        namespace: None,
        arguments: Cow::Borrowed(arguments),
        parsed_arguments,
    })
}

pub(crate) fn validate_streamed_function_call(
    completed: &CompletedFunctionCall<'_>,
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
    if arguments == completed.arguments.as_ref() {
        return Ok(());
    }
    // Read is buffered: repair only at authoritative finalization, never close
    // the turn from a delta or hide a later upstream failure/usage event.
    match validate_completed_function_call_fields(Some(call_id), Some(name), Some(arguments)) {
        Ok(streamed) => {
            if streamed.parsed_arguments != completed.parsed_arguments {
                return Err(disagreement());
            }
        }
        Err(error @ CompletedFunctionCallValidationError::TooLarge { .. }) => return Err(error),
        Err(error) => {
            if completed
                .namespace
                .is_some_and(|namespace| namespace != "functions")
            {
                return Err(error);
            }
            let Some(repaired) = repair_whitespace_stalled_read_args(name, arguments, None) else {
                return Err(error);
            };
            let final_arguments = completed.sanitized_arguments(None);
            let repaired = serde_json::from_str::<serde_json::Value>(&repaired).ok();
            let rewritten = match final_arguments {
                Cow::Owned(arguments) => serde_json::from_str::<serde_json::Value>(&arguments).ok(),
                Cow::Borrowed(_) => None,
            };
            let final_value = rewritten.as_ref().or(Some(&completed.parsed_arguments));
            if repaired.as_ref() != final_value {
                return Err(disagreement());
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
struct LiveTool {
    index: usize,
    call_id: String,
    name: String,
    namespace: Option<String>,
    args_accum: String,
    emitted_args: bool,
    exposed: bool,
}

#[derive(Clone)]
struct LiveWebSearch {
    index: usize,
    result_index: usize,
    id: String,
    query: String,
}

#[derive(Clone, Copy)]
struct LiveThinking {
    output_index: usize,
    anthropic_index: usize,
}

#[derive(Clone)]
pub struct LiveStreamTranslator {
    message_id: String,
    model: String,
    message_started: bool,
    blocks_by_output_index: HashMap<usize, LiveTool>,
    item_id_to_output_index: HashMap<String, usize>,
    anthropic_index: usize,
    thinking: Option<LiveThinking>,
    reasoning_by_output_index: HashMap<usize, PendingReasoning>,
    saw_tool_use: bool,
    text_parts: TextParts,
    stops: StopSequences,
    exposed_text: BTreeSet<usize>,
    completed_outputs: CompletedOutputs,
    web_search_requests: usize,
    web_searches: Vec<LiveWebSearch>,
    annotations: Vec<serde_json::Value>,
    annotation_bytes: u64,
    deferred_text: BTreeMap<usize, String>,
    semantic_output_started: bool,
    current_generation_semantic_output_started: bool,
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
            text_parts: TextParts::default(),
            stops: StopSequences::default(),
            exposed_text: BTreeSet::new(),
            completed_outputs: CompletedOutputs::default(),
            web_search_requests: 0,
            web_searches: Vec::new(),
            annotations: Vec::new(),
            annotation_bytes: 0,
            deferred_text: BTreeMap::new(),
            semantic_output_started: false,
            current_generation_semantic_output_started: false,
            text_output_started: false,
            estimated_input_tokens,
            finished: false,
            rate_limits: None,
            service_tier: None,
        }
    }

    pub fn with_stop_sequences(mut self, stops: &[String]) -> Self {
        self.stops = StopSequences::new(stops);
        self
    }

    pub fn matched_stop_sequence(&self) -> Option<&str> {
        self.stops.matched()
    }

    pub fn accept(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
    ) -> Result<Vec<u8>, String> {
        if self.finished {
            return Ok(Vec::new());
        }

        // Terminal batches may recover several blocks. Roll back on failure so
        // error_chunk only closes starts actually delivered to the consumer.
        let terminal = matches!(
            payload["type"].as_str(),
            Some("response.completed" | "response.done" | "response.incomplete")
        );
        let saved = terminal.then(|| self.clone());
        let result = (|| {
            let events = self.completed_outputs.accept(Cow::Borrowed(payload))?;
            let mut out = Vec::new();
            for event in events {
                out.extend(self.accept_event(&event, traffic)?);
            }
            Ok(out)
        })();
        if result.is_err()
            && let Some(saved) = saved
        {
            *self = saved;
        }
        result
    }

    fn accept_event(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
    ) -> Result<Vec<u8>, String> {
        let kind = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let mut out = Vec::new();

        if let Some(failure) = classify_event_failure(payload) {
            return Err(failure.message);
        }
        if self.stops.matched().is_some() && self.completed_outputs.has_refusal() {
            return Err("Codex refused after a local text stop".into());
        }
        if matches!(
            kind,
            "response.completed" | "response.incomplete" | "response.done"
        ) && !self.blocks_by_output_index.is_empty()
        {
            return Err("upstream stream ended with open Codex tool blocks".to_string());
        }

        let terminal_usage = if matches!(
            kind,
            "response.completed" | "response.incomplete" | "response.done"
        ) {
            payload
                .get("response")
                .map(parse_codex_usage)
                .transpose()
                .map_err(|error| error.message)?
        } else {
            None
        };
        let text_output_index = text_output_index(payload, &self.item_id_to_output_index);
        if text_boundary(payload)
            && let Some(delta) = self.stops.flush()
        {
            self.text_delta(delta, traffic, &mut out);
        }
        for annotation in upstream_annotations(payload, text_output_index) {
            if !self.annotations.contains(&annotation) {
                let bytes =
                    crate::providers::codex::request_summary::serialized_json_bytes(&annotation)
                        .max(256);
                self.annotation_bytes = self.annotation_bytes.saturating_add(bytes);
                if self.annotation_bytes
                    > crate::providers::codex::client::MAX_PENDING_HTTP_EVENT_BYTES as u64
                {
                    return Err("Codex annotations exceeded their 16 MiB buffer limit".into());
                }
                self.annotations.push(annotation);
            }
        }
        let next_index = self.anthropic_index;
        let text_deltas =
            self.text_parts
                .accept(payload, &mut self.anthropic_index, text_output_index)?;
        // Reconcile final-only text before deciding, but reject an empty replacement
        // before emitting starts or marking the translator finished.
        if crate::providers::codex::events::event_is_success_terminal(payload)
            && !crate::providers::codex::events::response_is_incomplete_terminal(payload)
            && self.stops.matched().is_none()
            && self.has_semantic_output()
            && !self.has_current_generation_semantic_output()
            && !text_deltas
                .iter()
                .any(|delta| !delta.text.is_empty() || delta.refusal)
        {
            self.text_parts.discard_unemitted_parts(next_index);
            return Err(
                "Codex replacement completed without producing output after reconnect".to_string(),
            );
        }
        for delta in text_deltas {
            for delta in self
                .stops
                .accept(delta, text_output_index)
                .into_iter()
                .flatten()
            {
                self.text_delta(delta, traffic, &mut out);
            }
            if self.stops.matched().is_some() {
                break;
            }
        }
        if self.stops.matched().is_some() {
            if self.completed_outputs.has_refusal() {
                return Err("Codex refused after a local text stop".into());
            }
            self.annotations.clear();
            self.annotation_bytes = 0;
        }
        match kind {
            "codex.rate_limits" => {
                self.rate_limits = parse_codex_rate_limits(payload).or(self.rate_limits.take());
                self.emit_ping(traffic, &mut out);
            }
            "response.created" => {
                self.service_tier = payload
                    .get("response")
                    .and_then(|value| value.get("service_tier"))
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
            "response.reasoning_summary_text.delta" => {
                self.reasoning_delta(payload, traffic, &mut out);
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
                self.finish(payload, terminal_usage, traffic, &mut out);
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

    pub fn has_current_generation_semantic_output(&self) -> bool {
        self.current_generation_semantic_output_started
    }

    /// Only thinking has been exposed. Text and tool blocks cannot be replayed
    /// safely because the consumer may already have acted on them.
    pub fn can_reconnect(&self) -> bool {
        !self.finished
            && self.stops.matched().is_none()
            && !self.stops.has_pending()
            && !self.text_output_started
            && !self.saw_tool_use
            && self.web_search_requests == 0
            && self.blocks_by_output_index.is_empty()
    }

    pub fn prepare_reconnect(&mut self, traffic: Option<&TrafficCapture>) -> Vec<u8> {
        let mut out = Vec::new();
        if self.stops.matched().is_some() || self.stops.has_pending() {
            return out;
        }
        self.close_thinking(traffic, &mut out);
        self.reasoning_by_output_index.clear();
        self.completed_outputs = CompletedOutputs::default();
        self.item_id_to_output_index.clear();
        self.annotations.clear();
        self.annotation_bytes = 0;
        self.service_tier = None;
        self.current_generation_semantic_output_started = false;
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
        if self.finished
            || self.stops.matched().is_some()
            || self.stops.has_pending()
            || !self.saw_tool_use
            || !self.blocks_by_output_index.is_empty()
            || self.text_parts.has_open_parts()
            || self.text_parts.has_refusal()
            || !self.deferred_text.is_empty()
        {
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
            traffic.write_json_event_envelope("050-downstream-event", event, data);
        }
        append_json_sse_event(out, event, data);
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
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    self.item_id_to_output_index
                        .insert(id.to_string(), output_index);
                }
            }
            "function_call" => {
                self.close_thinking(traffic, out);
                self.saw_tool_use = true;
                let index = self.anthropic_index;
                self.anthropic_index += 1;
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                self.blocks_by_output_index.insert(
                    output_index,
                    LiveTool {
                        index,
                        call_id: call_id.to_owned(),
                        name: name.to_owned(),
                        namespace: item
                            .get("namespace")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                        args_accum: String::new(),
                        emitted_args: false,
                        exposed: self.stops.matched().is_none(),
                    },
                );
                if self.stops.matched().is_some() {
                    return;
                }
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
                            "name": function_call_display_name(item.get("namespace").and_then(serde_json::Value::as_str), name),
                            "input": {}
                        }
                    }),
                );
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
        if delta.is_empty() || self.stops.matched().is_some() {
            return;
        }
        self.semantic_output_started = true;
        self.current_generation_semantic_output_started = true;
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
        delta: TextDelta,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        self.close_thinking(traffic, out);
        if !delta.text.is_empty() || delta.refusal {
            self.semantic_output_started = true;
            self.current_generation_semantic_output_started = true;
        }
        // Even an empty started block must not be replayed under the same index.
        self.text_output_started = true;
        if self.deferred_text.contains_key(&delta.index)
            || (delta.start && !self.web_searches.is_empty())
        {
            self.deferred_text
                .entry(delta.index)
                .or_default()
                .push_str(&delta.text);
            return;
        }
        self.ensure_message_start(traffic, out);
        if delta.start {
            self.exposed_text.insert(delta.index);
            self.emit(traffic, out, "content_block_start", &serde_json::json!({
                "type":"content_block_start", "index":delta.index, "content_block":{"type":"text","text":""}
            }));
        }
        if !delta.text.is_empty() {
            self.emit(traffic, out, "content_block_delta", &serde_json::json!({
                "type":"content_block_delta", "index":delta.index, "delta":{"type":"text_delta","text":delta.text}
            }));
        }
    }

    fn close_text_parts(
        &mut self,
        output_index: Option<usize>,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        if let Some(delta) = self.stops.flush_item(output_index) {
            self.text_delta(delta, traffic, out);
        }
        for index in self.text_parts.close_item(output_index) {
            if self.exposed_text.remove(&index) {
                self.emit(
                    traffic,
                    out,
                    "content_block_stop",
                    &serde_json::json!({"type":"content_block_stop","index":index}),
                );
            }
        }
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
        self.current_generation_semantic_output_started = true;
        let Some(LiveTool {
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
        if name != "Read" && self.stops.matched().is_none() {
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
        let Some(LiveTool {
            args_accum,
            call_id,
            name,
            ..
        }) = self.blocks_by_output_index.get_mut(&output_index(payload))
        else {
            return Ok(());
        };
        let completed = validate_completed_function_call_fields(
            Some(call_id),
            Some(name),
            payload.get("arguments").and_then(serde_json::Value::as_str),
        )
        .map_err(|error| error.to_string())?;
        validate_streamed_function_call(&completed, call_id, name, args_accum)
            .map_err(|error| error.to_string())?;
        if args_accum.is_empty() {
            *args_accum = completed.arguments.into_owned();
        }
        Ok(())
    }

    fn output_item_done(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let mut output_index = text_output_index(payload, &self.item_id_to_output_index);
        if payload["_ccp_output_already_done"] == true {
            return Ok(());
        }
        if payload["item"]["type"] == "message" {
            if payload["item"]["status"] != "incomplete" {
                self.close_text_parts(Some(output_index), traffic, out);
            }
            return Ok(());
        }
        if let Some(item) = payload
            .get("item")
            .filter(|item| item["type"] == "reasoning")
        {
            if self.stops.matched().is_some() {
                return Ok(());
            }
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

        if payload["item"]["type"] == "web_search_call" {
            self.web_search_requests += 1;
            if self.stops.matched().is_some() {
                return Ok(());
            }
            self.close_thinking(traffic, out);
            self.semantic_output_started = true;
            self.current_generation_semantic_output_started = true;
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

        let completed_function_call = if payload["item"]["type"] == "function_call" {
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

        if let Some(LiveTool {
            call_id,
            name,
            namespace,
            args_accum,
            ..
        }) = self.blocks_by_output_index.get(&output_index)
        {
            let completed = completed_function_call
                .as_ref()
                .ok_or("tool output closed without a completed function call")?;
            if completed.namespace != namespace.as_deref() {
                return Err("completed function call namespace changed".into());
            }
            validate_streamed_function_call(completed, call_id, name, args_accum)
                .map_err(|error| error.to_string())?;
        }
        let Some(state) = self.blocks_by_output_index.remove(&output_index) else {
            return Ok(());
        };
        if self.stops.matched().is_some() {
            if state.exposed {
                self.emit(
                    traffic,
                    out,
                    "content_block_stop",
                    &serde_json::json!({"type":"content_block_stop", "index":state.index}),
                );
            }
            return Ok(());
        }

        let LiveTool {
            index,
            call_id,
            emitted_args,
            ..
        } = state;
        let completed = completed_function_call
            .as_ref()
            .expect("validated tool completion");
        self.semantic_output_started = true;
        self.current_generation_semantic_output_started = true;
        if !emitted_args {
            let arguments = completed.sanitized_arguments(Some(call_id.as_str()));
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type":"content_block_delta", "index":index,
                    "delta":{"type":"input_json_delta", "partial_json":arguments}
                }),
            );
        }
        self.emit(
            traffic,
            out,
            "content_block_stop",
            &serde_json::json!({"type":"content_block_stop", "index":index}),
        );
        Ok(())
    }

    fn emit_completed_function_call(
        &mut self,
        completed: &CompletedFunctionCall<'_>,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        if self.stops.matched().is_some() {
            return;
        }
        let arguments = completed.sanitized_arguments(Some(completed.call_id));

        self.close_thinking(traffic, out);
        self.saw_tool_use = true;
        self.semantic_output_started = true;
        self.current_generation_semantic_output_started = true;
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
                    "name": function_call_display_name(completed.namespace, completed.name),
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
            .get("item")
            .and_then(|value| value.get("call_id"))
            .and_then(serde_json::Value::as_str)?;
        if let Some(output_index) = payload
            .get("output_index")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            && matches!(
                self.blocks_by_output_index.get(&output_index),
                Some(LiveTool { .. })
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
                        LiveTool { call_id: open_call_id, .. } if open_call_id == call_id
                    )
                    .then_some(*output_index)
                });
        let output_index = matching.next()?;
        matching.next().is_none().then_some(output_index)
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
                        "content": []
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
    }

    fn finish(
        &mut self,
        payload: &serde_json::Value,
        mut usage: Option<CodexUsage>,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        self.close_thinking(traffic, out);
        self.close_open_blocks(traffic, out);
        self.emit_web_searches(traffic, out);
        self.ensure_message_start(traffic, out);
        if let Some(usage) = usage.as_mut() {
            usage.service_tier = usage.service_tier.take().or(self.service_tier.take());
            usage.rate_limits = self.rate_limits.take();
        }
        let stop_reason = if self.stops.matched().is_some() {
            super::reducer::STOP_STOP_SEQUENCE
        } else if crate::providers::codex::events::response_is_incomplete_terminal(payload) {
            STOP_MAX_TOKENS
        } else if self.text_parts.has_refusal() {
            STOP_REFUSAL
        } else if self.saw_tool_use {
            STOP_TOOL_USE
        } else if crate::providers::codex::events::response_needs_follow_up(payload) {
            STOP_PAUSE_TURN
        } else {
            STOP_END_TURN
        };
        self.emit_finish(stop_reason, usage, traffic, out);
    }

    fn emit_finish(
        &mut self,
        stop_reason: &str,
        usage: Option<CodexUsage>,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let mapped = map_codex_usage_to_anthropic(&usage, Some(self.web_search_requests));
        let mut message_delta = serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason, "stop_sequence": self.stops.matched() },
            "usage": mapped,
        });
        if !self.annotations.is_empty() {
            message_delta["codex_annotations"] = std::mem::take(&mut self.annotations).into();
        }
        self.emit(traffic, out, "message_delta", &message_delta);
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
        self.close_text_parts(None, traffic, out);
        let open: Vec<usize> = self.blocks_by_output_index.keys().copied().collect();
        for output_index in open {
            let Some(state) = self.blocks_by_output_index.remove(&output_index) else {
                continue;
            };
            if !state.exposed {
                continue;
            }
            let index = state.index;
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
        self.current_generation_semantic_output_started = true;
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

pub(super) fn text_output_index(
    payload: &serde_json::Value,
    item_id_to_output_index: &HashMap<String, usize>,
) -> usize {
    payload
        .get("output_index")
        .and_then(serde_json::Value::as_u64)
        .map(|index| index as usize)
        .or_else(|| {
            payload["item_id"]
                .as_str()
                .or_else(|| {
                    payload
                        .get("item")
                        .and_then(|value| value.get("id"))
                        .and_then(serde_json::Value::as_str)
                })
                .and_then(|id| item_id_to_output_index.get(id).copied())
        })
        .unwrap_or(0)
}

fn output_index(payload: &serde_json::Value) -> usize {
    payload
        .get("output_index")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize
}
