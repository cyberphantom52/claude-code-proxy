pub mod request;
pub mod response;
pub mod stream;

use std::time::Duration;

use axum::{
    Json,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use http::StatusCode;
use serde_json::{Value, json};

use crate::monitor::TokenUsage;
use crate::provider::RequestContext;

use super::client::{CodexError, CodexHttpClient, HttpBodyDiagnostics};
use request::TranslatedRequest;

pub struct ChatCompletionsBackend {
    client: CodexHttpClient,
}

impl Default for ChatCompletionsBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatCompletionsBackend {
    pub fn new() -> Self {
        Self {
            client: CodexHttpClient::new(),
        }
    }

    pub async fn handle(&self, request: TranslatedRequest, ctx: RequestContext) -> Response {
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &request.model);
            monitor.upstream_started(&ctx.req_id);
        }
        let upstream = match self
            .client
            .post_native_responses(&request.upstream, &ctx, request.use_responses_lite, true)
            .await
        {
            Ok(upstream) => upstream,
            Err(error) => return codex_error_response(error),
        };

        if !upstream.status().is_success() {
            return upstream_error_response(upstream, self.client.body_idle_timeout_ms(), &ctx)
                .await;
        }
        if request.stream {
            return stream::streaming_response(
                upstream,
                ctx,
                request.model,
                request.include_usage,
                self.client.body_idle_timeout_ms(),
            );
        }

        let headers = stream::response_headers(upstream.headers());
        let mut completion = response::CompletionState::new(&request.model, true);
        let (bytes, read_result) = collect_body(
            upstream,
            self.client.body_idle_timeout_ms(),
            Some(&ctx),
            Some(&mut completion),
        )
        .await;
        if read_result.is_ok()
            && let Some(traffic) = ctx.traffic.as_deref()
        {
            traffic.write_bytes("032-upstream-response-body.sse", &bytes);
        }
        let completion = match read_result.and_then(|()| response::finish_completion(completion)) {
            Ok(completion) => completion,
            Err(error) => {
                let mut response = error.response();
                response.headers_mut().extend(headers);
                return response;
            }
        };
        if let Some(traffic) = ctx.traffic.as_deref() {
            traffic.write_json("050-openai-chat-completion-response", &completion);
        }
        let mut downstream = Json(completion).into_response();
        *downstream.headers_mut() = headers;
        downstream.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        downstream
    }
}

async fn collect_body(
    upstream: reqwest::Response,
    idle_timeout_ms: u64,
    ctx: Option<&RequestContext>,
    mut completion: Option<&mut response::CompletionState>,
) -> (Vec<u8>, Result<(), ChatError>) {
    let diagnostics_ctx = ctx;
    // Error bodies need diagnostics, but are not generation progress.
    let ctx = ctx.filter(|_| completion.is_some());
    let mut diagnostics = HttpBodyDiagnostics::new(&upstream, std::time::Instant::now());
    let mut stream = upstream.bytes_stream();
    let mut bytes = Vec::new();
    let mut frame_start = 0;
    let mut scan_start = 0;
    let mut started = false;
    let result = 'read: loop {
        match tokio::time::timeout(Duration::from_millis(idle_timeout_ms), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                diagnostics.record_chunk(chunk.len());
                if !started {
                    if let Some(ctx) = ctx
                        && let Some(monitor) = ctx.monitor.as_ref()
                    {
                        monitor.generation_started(&ctx.req_id);
                    }
                    started = true;
                }
                let remaining = crate::traffic::MAX_SSE_CAPTURE_BYTES.saturating_sub(bytes.len());
                let inspected = remaining.min(chunk.len());
                let exceeded_limit = inspected < chunk.len();
                if exceeded_limit && completion.is_none() {
                    break Err(ChatError::upstream(
                        "Codex response exceeded the response byte limit",
                    ));
                }
                bytes.extend_from_slice(&chunk[..inspected]);
                if let Some(ctx) = ctx
                    && let Some(monitor) = ctx.monitor.as_ref()
                {
                    monitor.stream_progress(
                        &ctx.req_id,
                        chunk.len() as u64,
                        0,
                        TokenUsage::default(),
                    );
                }
                if let Some(completion) = completion.as_deref_mut() {
                    while let Some((end, separator_len)) =
                        super::native::find_sse_boundary_from(&bytes, scan_start)
                    {
                        let frame_end = end + separator_len;
                        for event in
                            crate::anthropic::sse::parse_sse_events(&bytes[frame_start..frame_end])
                        {
                            if event.data == "[DONE]" {
                                continue;
                            }
                            let mut value: Value = match serde_json::from_str(&event.data) {
                                Ok(value) => value,
                                Err(_) => {
                                    break 'read Err(ChatError::upstream(
                                        "Codex returned malformed JSON in its event stream",
                                    ));
                                }
                            };
                            if let Some(event) = event.event
                                && let Some(object) = value.as_object_mut()
                            {
                                object.entry("type").or_insert(Value::String(event));
                            }
                            if let Some(ctx) = ctx {
                                super::record_upstream_telemetry(ctx, &value);
                            }
                            if let Err(error) = completion.observe(&value) {
                                break 'read Err(error);
                            }
                            if completion.completed {
                                bytes.truncate(frame_end);
                                break 'read Ok(());
                            }
                        }
                        frame_start = frame_end;
                        scan_start = frame_end;
                    }
                    scan_start = bytes.len().saturating_sub(3).max(frame_start);
                }
                if exceeded_limit {
                    break Err(ChatError::upstream(
                        "Codex response exceeded the response byte limit",
                    ));
                }
            }
            Ok(Some(Err(error))) => {
                if let Some(ctx) = diagnostics_ctx {
                    diagnostics.log_failure(ctx, "chat_buffered", "read_error", Some(&error));
                }
                break Err(ChatError::upstream(format!(
                    "Codex response body read failed: {error}"
                )));
            }
            Ok(None) => break Ok(()),
            Err(_) => {
                if let Some(ctx) = diagnostics_ctx {
                    diagnostics.log_failure(ctx, "chat_buffered", "idle_timeout", None);
                }
                break Err(ChatError::timeout(format!(
                    "Timed out waiting {idle_timeout_ms}ms for the next Codex response body chunk"
                )));
            }
        }
    };
    (bytes, result)
}

fn codex_error_response(mut error: CodexError) -> Response {
    let retry_after = error.retry_after.take();
    let mut response = ChatError::from_codex(error).response();
    if let Some(retry_after) = retry_after
        && let Ok(value) = http::HeaderValue::from_str(&retry_after)
    {
        response
            .headers_mut()
            .insert(http::header::RETRY_AFTER, value);
    }
    response
}

async fn upstream_error_response(
    upstream: reqwest::Response,
    idle_timeout_ms: u64,
    ctx: &RequestContext,
) -> Response {
    let status = upstream.status();
    let headers = stream::response_headers(upstream.headers());
    let (bytes, read_result) = collect_body(upstream, idle_timeout_ms, Some(ctx), None).await;
    if let Err(error) = read_result {
        let mut response = error.response();
        response.headers_mut().extend(headers);
        return response;
    }
    let message = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .or_else(|| value.get("detail"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| format!("Codex request failed with status {}", status.as_u16()));
    let kind = match status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        _ => "api_error",
    };
    let mut response = ChatError::new(status, kind, message, None, None).response();
    response.headers_mut().extend(headers);
    response
}

#[derive(Debug, Clone)]
pub struct ChatError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: String,
    pub param: Option<String>,
    pub code: Option<String>,
}

impl ChatError {
    pub fn new(
        status: StatusCode,
        kind: &'static str,
        message: impl Into<String>,
        param: Option<&str>,
        code: Option<&str>,
    ) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            param: param.map(str::to_string),
            code: code.map(str::to_string),
        }
    }

    pub fn invalid(message: impl Into<String>, param: Option<&str>, code: Option<&str>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            message,
            param,
            code,
        )
    }

    pub fn unsupported(param: impl Into<String>) -> Self {
        let param = param.into();
        Self::invalid(
            format!("Unsupported parameter: {param}"),
            Some(&param),
            Some("unsupported_parameter"),
        )
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "api_error", message, None, None)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "api_error",
            message,
            None,
            None,
        )
    }

    fn from_codex(error: CodexError) -> Self {
        let status = match error.status {
            401 => StatusCode::UNAUTHORIZED,
            403 => StatusCode::FORBIDDEN,
            429 => StatusCode::TOO_MANY_REQUESTS,
            _ if error.message.contains("Timed out waiting") => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::BAD_GATEWAY,
        };
        let kind = match status {
            StatusCode::UNAUTHORIZED => "authentication_error",
            StatusCode::FORBIDDEN => "permission_error",
            StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
            _ => "api_error",
        };
        Self::new(
            status,
            kind,
            error.detail.unwrap_or(error.message),
            None,
            None,
        )
    }

    pub fn value(&self) -> Value {
        json!({"error":{"message":self.message,"type":self.kind,"param":self.param,"code":self.code}})
    }

    pub fn response(self) -> Response {
        (self.status, Json(self.value())).into_response()
    }
}
