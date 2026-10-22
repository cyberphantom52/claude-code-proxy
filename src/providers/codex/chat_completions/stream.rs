use std::{collections::VecDeque, io, pin::Pin, time::Duration};

use axum::{body::Body, response::Response};
use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt};
use http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

use crate::providers::codex::native::NativeResponseOutcome;
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
            monitor.stream_progress(&self.ctx.req_id, chunk.len() as u64, 0, None, None);
        }
    }

    // Parse one frame at a time so a large upstream chunk cannot build an
    // amplified queue of downstream JSON chunks before backpressure is applied.
    fn process_pending_frame(&mut self) -> bool {
        let Some((end, separator_len)) = find_boundary_from(&self.pending, self.pending_scan)
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
            monitor.stream_progress(
                &self.ctx.req_id,
                0,
                event_count,
                Some(self.completion.usage.prompt_tokens),
                Some(self.completion.usage.completion_tokens),
            );
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
            {"index":0,"delta":{},"finish_reason":self.completion.finish_reason}
        ]));
        if let Some(monitor) = self.ctx.monitor.as_ref() {
            monitor.usage_updated(
                &self.ctx.req_id,
                Some(self.completion.usage.prompt_tokens),
                Some(self.completion.usage.completion_tokens),
            );
        }
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
    Bytes::from(format!(
        "data: {}\n\n",
        serde_json::to_string(&value).unwrap()
    ))
}

#[cfg(test)]
fn find_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    find_boundary_from(bytes, 0)
}

fn find_boundary_from(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    for index in start.min(bytes.len())..bytes.len() {
        if bytes[index..].starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        if bytes[index..].starts_with(b"\n\n") || bytes[index..].starts_with(b"\r\r") {
            return Some((index, 2));
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    fn state(include_usage: bool) -> StreamState {
        StreamState {
            upstream: Box::pin(futures_util::stream::empty()),
            pending: BytesMut::new(),
            pending_scan: 0,
            received_bytes: 0,
            output: VecDeque::new(),
            completion: CompletionState::new("requested"),
            role_sent: false,
            include_usage,
            ended: false,
            generation_started: false,
            ctx: RequestContext {
                notification_id: None,
                req_id: "chat-stream-test".into(),
                session_id: None,
                session_seq: None,
                provider: "codex".into(),
                traffic: None,
                monitor: None,
            },
            outcome: NativeResponseOutcome::default(),
            body_idle_timeout_ms: 1_000,
            raw: Vec::new(),
        }
    }

    fn values(state: &mut StreamState) -> Vec<Value> {
        let frames: Vec<_> = state.output.drain(..).collect();
        assert_eq!(frames.last().unwrap().as_ref(), b"data: [DONE]\n\n");
        frames[..frames.len() - 1]
            .iter()
            .map(|frame| {
                assert!(frame.starts_with(b"data: ") && frame.ends_with(b"\n\n"));
                serde_json::from_slice(&frame[6..frame.len() - 2]).unwrap()
            })
            .collect()
    }

    #[test]
    fn exact_frames_freeze_identity_and_separate_full_usage_before_done() {
        for metadata_first in [true, false] {
            let mut state = state(true);
            if metadata_first {
                state.observe_event(r#"{"type":"response.created","response":{"id":"resp_first","model":"first","created_at":7,"service_tier":"default"}}"#);
            }
            state.observe_event(r#"{"type":"response.output_text.delta","delta":"hello"}"#);
            let identity = (
                state.completion.id.clone(),
                state.completion.model.clone(),
                state.completion.created,
            );
            state.observe_event(r#"{"type":"response.completed","response":{"id":"resp_later","model":"later","created_at":99,"status":"completed","service_tier":"priority","usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5,"input_tokens_details":{"cached_tokens":1,"custom":4},"output_tokens_details":{"reasoning_tokens":1}}}}"#);
            let frames = values(&mut state);
            assert_eq!(frames.len(), 4);
            for frame in &frames {
                assert_eq!(frame["id"], identity.0);
                assert_eq!(frame["model"], identity.1);
                assert_eq!(frame["created"], identity.2);
                assert_eq!(frame["object"], "chat.completion.chunk");
            }
            assert_eq!(
                frames[0]["choices"],
                json!([{"index":0,"delta":{"role":"assistant"},"finish_reason":null}])
            );
            assert_eq!(
                frames[1]["choices"],
                json!([{"index":0,"delta":{"content":"hello"},"finish_reason":null}])
            );
            assert_eq!(
                frames[2]["choices"],
                json!([{"index":0,"delta":{},"finish_reason":"stop"}])
            );
            for frame in &frames[..3] {
                assert_eq!(frame["usage"], Value::Null);
            }
            assert_eq!(frames[3]["choices"], json!([]));
            assert_eq!(
                frames[3]["usage"],
                json!({"prompt_tokens":3,"completion_tokens":2,"total_tokens":5,"prompt_tokens_details":{"cached_tokens":1,"custom":4},"completion_tokens_details":{"reasoning_tokens":1}})
            );
            assert_eq!(frames[2]["service_tier"], "priority");
            assert_eq!(frames[3]["service_tier"], "priority");
        }
    }

    #[test]
    fn usage_is_absent_unless_requested() {
        let mut state = state(false);
        state.observe_event(r#"{"type":"response.output_text.delta","delta":"hello"}"#);
        state.observe_event(r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":3,"output_tokens":2}}}"#);
        let frames = values(&mut state);
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|frame| frame.get("usage").is_none()));
    }

    #[test]
    fn endless_structural_or_unframed_streams_stop_at_byte_budget_without_raw_capture() {
        for block in [
            b"data: {\"type\":\"response.in_progress\"}\n\n".repeat(1024),
            vec![b'x'; 16 * 1024],
        ] {
            let mut state = state(false);
            while !state.ended {
                state.observe_chunk(&block);
                while !state.ended && state.process_pending_frame() {}
                assert!(state.pending.len() <= MAX_SSE_CAPTURE_BYTES);
            }
            assert!(state.outcome.failure().unwrap().contains("byte limit"));
            assert!(state.received_bytes <= MAX_SSE_CAPTURE_BYTES + block.len());
            assert_eq!(state.raw.capacity(), 0);
            assert_eq!(state.output.len(), 2);
        }
    }

    #[test]
    fn response_headers_forward_only_local_allowlisted_quotas() {
        let mut source = HeaderMap::new();
        source.insert("x-codex-primary-used-percent", "12".parse().unwrap());
        source.insert("x-codex-turn-state", "private".parse().unwrap());
        let headers = response_headers(&source);
        assert_eq!(headers["x-codex-primary-used-percent"], "12");
        assert!(!headers.contains_key("x-codex-turn-state"));
        assert!(response_headers(&HeaderMap::new()).is_empty());
    }

    #[test]
    fn boundary_supports_split_safe_delimiters() {
        assert_eq!(find_boundary(b"data: {}\n\nnext"), Some((8, 2)));
        assert_eq!(find_boundary(b"data: {}\r\n\r\nnext"), Some((8, 4)));
        assert_eq!(find_boundary(b"data: {}"), None);
    }
}
