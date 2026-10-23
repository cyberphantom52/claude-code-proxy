use std::{collections::VecDeque, io, pin::Pin, time::Duration};

use axum::{body::Body, response::Response};
use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt};
use http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

use crate::monitor::TokenUsage;
use crate::providers::codex::native::{NativeResponseOutcome, find_sse_boundary_from};
use crate::{provider::RequestContext, traffic::MAX_SSE_CAPTURE_BYTES};

use super::{ChatError, response::CompletionState};

type UpstreamStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

pub fn streaming_response(
    upstream: reqwest::Response,
    ctx: RequestContext,
    model: String,
    include_usage: bool,
    body_idle_timeout_ms: u64,
) -> Response {
    let outcome = NativeResponseOutcome::default();
    let headers = response_headers(upstream.headers());
    let state = StreamState {
        upstream: Box::pin(upstream.bytes_stream()),
        pending: BytesMut::new(),
        pending_scan: 0,
        received_bytes: 0,
        output: VecDeque::new(),
        completion: CompletionState::new(&model),
        role_sent: false,
        include_usage,
        ended: false,
        generation_started: false,
        ctx,
        outcome: outcome.clone(),
        body_idle_timeout_ms,
        raw: Vec::new(),
    };
    let stream =
        futures_util::stream::unfold(Some(state), |state| async move { next_frame(state).await });
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = StatusCode::OK;
    *response.headers_mut() = headers;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-cache"),
    );
    response.extensions_mut().insert(outcome);
    response
}

async fn next_frame(
    state: Option<StreamState>,
) -> Option<(Result<Bytes, io::Error>, Option<StreamState>)> {
    let mut state = state?;
    loop {
        if let Some(frame) = state.output.pop_front() {
            let next = if state.ended && state.output.is_empty() {
                state.finish_capture("complete");
                None
            } else {
                Some(state)
            };
            return Some((Ok(frame), next));
        }
        if state.ended {
            state.finish_capture("complete");
            return None;
        }
        if state.process_pending_frame() {
            continue;
        }
        match tokio::time::timeout(
            Duration::from_millis(state.body_idle_timeout_ms),
            state.upstream.next(),
        )
        .await
        {
            Ok(Some(Ok(chunk))) => state.observe_chunk(&chunk),
            Ok(Some(Err(error))) => {
                state.fail(ChatError::upstream(format!(
                    "Codex response body read failed: {error}"
                )));
            }
            Ok(None) => {
                if !state.completion.completed {
                    state.fail(ChatError::upstream(
                        "Codex event stream ended before completion",
                    ));
                } else {
                    state.finish_success();
                }
            }
            Err(_) => {
                state.fail(ChatError::timeout(format!(
                    "Timed out waiting {}ms for the next Codex response body chunk",
                    state.body_idle_timeout_ms
                )));
            }
        }
    }
}

struct StreamState {
    upstream: UpstreamStream,
    pending: BytesMut,
    pending_scan: usize,
    received_bytes: usize,
    output: VecDeque<Bytes>,
    completion: CompletionState,
    role_sent: bool,
    include_usage: bool,
    ended: bool,
    generation_started: bool,
    ctx: RequestContext,
    outcome: NativeResponseOutcome,
    body_idle_timeout_ms: u64,
    raw: Vec<u8>,
}

impl StreamState {
    fn observe_chunk(&mut self, chunk: &[u8]) {
        if !chunk.is_empty() && !self.generation_started {
            if let Some(monitor) = self.ctx.monitor.as_ref() {
                monitor.generation_started(&self.ctx.req_id);
            }
            self.generation_started = true;
        }
        self.received_bytes = self.received_bytes.saturating_add(chunk.len());
        if self.received_bytes > MAX_SSE_CAPTURE_BYTES {
            self.fail(ChatError::upstream(
                "Codex response exceeded the response byte limit",
            ));
            return;
        }
        if self.ctx.traffic.is_some() {
            self.raw.extend_from_slice(chunk);
        }
        self.pending.extend_from_slice(chunk);
        if let Some(monitor) = self.ctx.monitor.as_ref() {
            monitor.stream_progress(
                &self.ctx.req_id,
                chunk.len() as u64,
                0,
                TokenUsage::default(),
            );
        }
    }

    // Parse one frame at a time so a large upstream chunk cannot build an
    // amplified queue of downstream JSON chunks before backpressure is applied.
    fn process_pending_frame(&mut self) -> bool {
        let Some((end, separator_len)) = find_sse_boundary_from(&self.pending, self.pending_scan)
        else {
            self.pending_scan = self.pending.len().saturating_sub(3);
            return false;
        };
        let frame = self.pending.split_to(end + separator_len);
        self.pending_scan = 0;
        let mut event_count = 0;
        for event in crate::anthropic::sse::parse_sse_events(&frame) {
            event_count += 1;
            self.observe_event(&event.data);
            if self.ended {
                break;
            }
        }
        if let Some(monitor) = self.ctx.monitor.as_ref() {
            monitor.stream_progress(&self.ctx.req_id, 0, event_count, TokenUsage::default());
        }
        true
    }

    fn observe_event(&mut self, data: &str) {
        if data == "[DONE]" {
            return;
        }
        let event: Value = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(_) => {
                self.fail(ChatError::upstream(
                    "Codex returned malformed JSON in its event stream",
                ));
                return;
            }
        };
        if let Some(traffic) = self.ctx.traffic.as_deref() {
            traffic.write_json_event("040-upstream-event", &event);
        }
        super::super::record_upstream_telemetry(&self.ctx, &event);
        match self.completion.observe(&event) {
            Ok(Some(delta)) => {
                if !self.role_sent {
                    self.completion.freeze_identity();
                    self.role_sent = true;
                    self.output.push_back(sse(self.chunk(json!([
                        {"index":0,"delta":{"role":"assistant"},"finish_reason":null}
                    ]))));
                }
                self.output.push_back(sse(self.chunk(json!([
                    {"index":0,"delta":{"content":delta},"finish_reason":null}
                ]))));
            }
            Ok(None) if self.completion.completed => self.finish_success(),
            Ok(None) => {}
            Err(error) => self.fail(error),
        }
    }

    fn chunk(&self, choices: Value) -> Value {
        let mut chunk = json!({
            "id": self.completion.id,
            "object": "chat.completion.chunk",
            "created": self.completion.created,
            "model": self.completion.model,
            "choices": choices,
        });
        if self.include_usage {
            chunk["usage"] = Value::Null;
        }
        if let Some(tier) = &self.completion.service_tier {
            chunk["service_tier"] = json!(tier);
        }
        chunk
    }

    fn finish_success(&mut self) {
        if self.ended {
            return;
        }
        if self.completion.text.is_empty() {
            self.fail(ChatError::upstream("Codex completed without output text"));
            return;
        }
        let terminal = self.chunk(json!([
            {"index":0,"delta":{},"finish_reason":"stop"}
        ]));
        self.output.push_back(sse(terminal));
        if self.include_usage {
            let mut usage = self.chunk(json!([]));
            usage["usage"] = self.completion.usage.value();
            self.output.push_back(sse(usage));
        }
        self.output
            .push_back(Bytes::from_static(b"data: [DONE]\n\n"));
        self.ended = true;
    }

    fn fail(&mut self, error: ChatError) {
        if self.ended {
            return;
        }
        self.outcome.fail(error.message.clone());
        self.output.push_back(sse(error.value()));
        self.output
            .push_back(Bytes::from_static(b"data: [DONE]\n\n"));
        self.ended = true;
    }

    fn finish_capture(&mut self, capture_outcome: &str) {
        let Some(traffic) = self.ctx.traffic.as_deref() else {
            return;
        };
        if !self.raw.is_empty() {
            traffic.write_bytes("032-upstream-response-body.sse", &self.raw);
        }
        traffic.write_json(
            "033-chat-completions-response-capture",
            &json!({
                "outcome": capture_outcome,
                "capturedBytes": self.raw.len(),
                "truncatedBytes": self.received_bytes.saturating_sub(self.raw.len()),
                "inputTokens": self.completion.usage.prompt_tokens,
                "outputTokens": self.completion.usage.completion_tokens,
            }),
        );
    }
}

impl Drop for StreamState {
    fn drop(&mut self) {
        if !self.ended {
            self.finish_capture("downstream_cancelled");
        }
    }
}

fn sse(value: Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

pub fn response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in [
        "retry-after",
        "x-request-id",
        "openai-processing-ms",
        "openai-version",
    ] {
        if let Some(value) = upstream.get(name) {
            headers.insert(name, value.clone());
        }
    }
    for (name, value) in upstream {
        if name.as_str().starts_with("x-ratelimit-") {
            headers.append(name.clone(), value.clone());
        }
    }
    for (name, value) in crate::providers::codex::rate_limits::telemetry_headers(upstream) {
        headers.insert(name, value);
    }
    headers
}
