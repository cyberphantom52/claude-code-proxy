use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::anthropic::sse::parse_sse_events;
use crate::monitor::TokenUsage;
use crate::provider::RequestContext;
use crate::traffic::{
    MAX_SSE_CAPTURE_BYTES, MAX_STREAM_CAPTURE_EVENT_BYTES, MAX_STREAM_CAPTURE_EVENTS,
    MAX_STREAM_CAPTURE_FRAME_BYTES,
};

use super::client::{CodexError, CodexHttpClient, HttpBodyDiagnostics};
use super::translate::model_allowlist::{
    assert_allowed_model, model_parallel_tool_calls, model_reasoning_summary, model_verbosity,
    resolve_model_effort, resolve_model_request, resolve_service_tier_value, uses_responses_lite,
    validate_hosted_web_search, validate_input_modalities,
};
use super::translate::request::{Effort, ServiceTier, effort_wire_value};

pub struct CodexNativeBackend {
    client: CodexHttpClient,
}

impl Default for CodexNativeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexNativeBackend {
    pub fn new() -> Self {
        Self {
            client: CodexHttpClient::new(),
        }
    }

    pub async fn handle(&self, body: Value, ctx: RequestContext) -> Response {
        self.handle_with_turn_state(body, ctx, None).await
    }

    pub(crate) async fn handle_with_turn_state(
        &self,
        mut body: Value,
        ctx: RequestContext,
        turn_state: Option<HeaderValue>,
    ) -> Response {
        let resolved = match shape_native_request(&mut body) {
            Ok(resolved) => resolved,
            Err(response) => return response,
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved.model);
            monitor.upstream_started(&ctx.req_id);
        }

        let upstream = match self
            .client
            .post_native_responses_with_turn_state(
                &body,
                &ctx,
                resolved.use_responses_lite,
                true,
                turn_state,
            )
            .await
        {
            Ok(response) => response,
            Err(error) => return local_codex_error(error),
        };

        if !resolved.stream && upstream.status().is_success() {
            return buffered_response(upstream, ctx, self.client.body_idle_timeout_ms()).await;
        }
        passthrough_response(upstream, ctx, self.client.body_idle_timeout_ms(), true)
    }
}

struct NativeResolved {
    model: String,
    use_responses_lite: bool,
    stream: bool,
}

#[allow(clippy::result_large_err)]
pub fn validate_native_request_model(body: &Value) -> Result<&str, Response> {
    let object = body.as_object().ok_or_else(|| {
        openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Request body must be a JSON object",
            None,
            None,
        )
    })?;
    let requested = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| {
            openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Missing or invalid 'model' in request body",
                Some("model"),
                None,
            )
        })?;
    let resolved = resolve_model_request(requested);
    if let Err(error) = assert_allowed_model(&resolved.model) {
        return Err(openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            error.to_string(),
            Some("model"),
            Some("invalid_value"),
        ));
    }
    Ok(requested)
}

#[allow(clippy::result_large_err)]
fn shape_native_request(body: &mut Value) -> Result<NativeResolved, Response> {
    let requested = validate_native_request_model(body)?;
    let resolved = resolve_model_request(requested);
    let object = body
        .as_object_mut()
        .expect("validated native Responses body must be an object");

    let model = resolved.model;

    let hosted_web_search = has_native_hosted_web_search(object);
    if hosted_web_search {
        validate_hosted_web_search(&model).map_err(|error| invalid_capability("tools", error))?;
    }
    let use_responses_lite = uses_responses_lite(&model) && !hosted_web_search;
    object.insert("model".to_string(), Value::String(model.clone()));
    if let Some(tier) = resolved.service_tier
        && !object.contains_key("service_tier")
    {
        object.insert("service_tier".to_string(), json!(tier));
    }
    apply_native_capabilities(object, &model, use_responses_lite)?;

    let stream = match object.get("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(stream)) => *stream,
        Some(_) => return Err(invalid_capability("stream", "stream must be a boolean")),
    };
    match object.get("store") {
        None | Some(Value::Null | Value::Bool(false)) => {}
        Some(Value::Bool(true)) => {
            return Err(invalid_capability("store", "Codex requires store=false"));
        }
        Some(_) => return Err(invalid_capability("store", "store must be a boolean")),
    }
    // Codex only accepts streamed, unstored upstream responses. Buffering is a
    // downstream presentation mode, not a different upstream request mode.
    object.insert("stream".to_string(), Value::Bool(true));
    object.insert("store".to_string(), Value::Bool(false));
    Ok(NativeResolved {
        use_responses_lite,
        model,
        stream,
    })
}

fn invalid_capability(param: &str, error: impl std::fmt::Display) -> Response {
    openai_error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        error.to_string(),
        Some(param),
        Some("unsupported_value"),
    )
}

#[allow(clippy::result_large_err)]
fn native_optional_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    param: &str,
) -> Result<Option<&'a str>, Response> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(invalid_capability(
            param,
            format!("'{param}' must be a string"),
        )),
    }
}

#[allow(clippy::result_large_err)]
fn apply_native_capabilities(
    object: &mut Map<String, Value>,
    model: &str,
    lite: bool,
) -> Result<(), Response> {
    let requested_parallel = match object.get("parallel_tool_calls") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => {
            return Err(invalid_capability(
                "parallel_tool_calls",
                "parallel_tool_calls must be a boolean",
            ));
        }
    };
    let parallel = model_parallel_tool_calls(model, lite, requested_parallel)
        .map_err(|error| invalid_capability("parallel_tool_calls", error))?;
    object
        .entry("parallel_tool_calls")
        .or_insert(json!(parallel));
    if let Some(input) = object.get("input") {
        validate_input_modalities(model, input)
            .map_err(|error| invalid_capability("input", error))?;
    }
    if let Some(tier) = native_optional_string(object, "service_tier", "service_tier")? {
        let tier = resolve_service_tier_value(model, tier)
            .map_err(|error| invalid_capability("service_tier", error))?;
        if matches!(&tier, ServiceTier::Other(value) if value == "default") {
            object.remove("service_tier");
        } else {
            object.insert("service_tier".to_string(), json!(tier));
        }
    }

    // Explicit null cannot satisfy Lite's context contract; do not replace it.
    if lite
        && (object.get("reasoning") == Some(&Value::Null)
            || object
                .get("reasoning")
                .and_then(|reasoning| reasoning.get("context"))
                .is_some_and(|context| context.as_str() != Some("all_turns")))
    {
        return Err(invalid_capability(
            "reasoning.context",
            "Responses Lite requires reasoning.context='all_turns'",
        ));
    }
    let mut reasoning = match object.get_mut("reasoning") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(reasoning)) => std::mem::take(reasoning),
        Some(_) => {
            return Err(invalid_capability(
                "reasoning",
                "reasoning must be an object",
            ));
        }
    };
    if lite {
        reasoning.entry("context").or_insert(json!("all_turns"));
    }
    let explicit_effort = match reasoning.get("effort") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            Effort::deserialize(value)
                .map_err(|error| invalid_capability("reasoning.effort", error))?
                .to_string(),
        ),
    };
    let effort = if reasoning.get("effort") == Some(&Value::Null) {
        None
    } else {
        resolve_model_effort(model, explicit_effort.as_deref())
            .map_err(|error| invalid_capability("reasoning.effort", error))?
    };
    if let Some(effort) = &effort {
        reasoning.insert(
            "effort".to_string(),
            effort_wire_value(effort)
                .map_err(|error| invalid_capability("reasoning.effort", error))?,
        );
    }
    let explicit_summary = native_optional_string(&reasoning, "summary", "reasoning.summary")?;
    let wants_reasoning = effort
        .as_deref()
        .is_some_and(|effort| !matches!(effort, "none" | "disabled"));
    let summary = model_reasoning_summary(model, explicit_summary, None)
        .map_err(|error| invalid_capability("reasoning.summary", error))?;
    match summary {
        Some(summary)
            if (wants_reasoning || explicit_summary.is_some())
                && reasoning.get("summary") != Some(&Value::Null) =>
        {
            reasoning.insert("summary".to_string(), json!(summary));
        }
        None => {
            reasoning.remove("summary");
        }
        _ => {}
    }
    if !reasoning.is_empty() && object.get("reasoning") != Some(&Value::Null) {
        object.insert("reasoning".to_string(), Value::Object(reasoning));
    }
    let mut text = match object.get_mut("text") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(text)) => std::mem::take(text),
        Some(_) => return Err(invalid_capability("text", "text must be an object")),
    };
    let requested_verbosity = native_optional_string(&text, "verbosity", "text.verbosity")?;
    let verbosity = if requested_verbosity.is_some()
        || super::translate::model_catalog::catalog_model(model)
            .and_then(|model| model.supports_verbosity)
            == Some(true)
    {
        model_verbosity(model, requested_verbosity)
            .map_err(|error| invalid_capability("text.verbosity", error))?
    } else {
        None
    };
    if let Some(verbosity) = verbosity {
        text.entry("verbosity").or_insert(json!(verbosity));
    }
    if !text.is_empty() && object.get("text") != Some(&Value::Null) {
        object.insert("text".to_string(), Value::Object(text));
    }
    Ok(())
}

fn has_native_hosted_web_search(object: &Map<String, Value>) -> bool {
    object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                matches!(
                    tool.get("type").and_then(Value::as_str),
                    Some("web_search" | "web_search_preview")
                )
            })
        })
}

fn local_codex_error(error: CodexError) -> Response {
    let status = match error.status {
        400..=599 => StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_GATEWAY),
        _ => StatusCode::BAD_GATEWAY,
    };
    let kind = match status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        _ => "api_error",
    };
    let message = error.detail.as_deref().unwrap_or(&error.message);
    let mut response = openai_error(status, kind, message, None, None);
    if let Some(retry_after) = error.retry_after
        && let Ok(value) = http::HeaderValue::from_str(&retry_after)
    {
        response
            .headers_mut()
            .insert(http::header::RETRY_AFTER, value);
    }
    response
}

pub fn openai_error(
    status: StatusCode,
    kind: &str,
    message: impl Into<String>,
    param: Option<&str>,
    code: Option<&str>,
) -> Response {
    (
        status,
        axum::Json(json!({
            "error": {
                "message": message.into(),
                "type": kind,
                "param": param,
                "code": code,
            }
        })),
    )
        .into_response()
}

#[derive(Clone, Default)]
pub struct NativeResponseOutcome {
    failure: Arc<Mutex<Option<String>>>,
}

impl NativeResponseOutcome {
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().ok().and_then(|failure| failure.clone())
    }

    pub(crate) fn fail(&self, message: String) {
        if let Ok(mut failure) = self.failure.lock()
            && failure.is_none()
        {
            *failure = Some(message);
        }
    }
}

async fn buffered_response(
    upstream: reqwest::Response,
    ctx: RequestContext,
    body_idle_timeout_ms: u64,
) -> Response {
    let status = upstream.status();
    let headers = passthrough_headers(upstream.headers());
    let outcome = NativeResponseOutcome::default();
    let mut observer = NativeResponseObserver::new(ctx, false, true, outcome.clone());
    observer.buffer_terminal = true;
    let mut diagnostics = HttpBodyDiagnostics::new(&upstream, std::time::Instant::now());
    let mut stream = upstream.bytes_stream();
    let mut received_bytes = 0_usize;
    let mut response = loop {
        match tokio::time::timeout(Duration::from_millis(body_idle_timeout_ms), stream.next()).await
        {
            Ok(Some(Ok(chunk))) => {
                diagnostics.record_chunk(chunk.len());
                let remaining = MAX_SSE_CAPTURE_BYTES.saturating_sub(received_bytes);
                let inspected = remaining.min(chunk.len());
                received_bytes += inspected;
                observer.observe(&chunk[..inspected]);
                if let Some(terminal) = observer.buffered_terminal.take() {
                    observer.finish(if terminal.is_ok() {
                        "complete"
                    } else {
                        "validation_error"
                    });
                    break match terminal {
                        Ok(event) => buffered_terminal_response(event, status),
                        Err(message) => {
                            openai_error(StatusCode::BAD_GATEWAY, "api_error", message, None, None)
                        }
                    };
                }
                if inspected < chunk.len() {
                    observer.finish("response_byte_limit");
                    break openai_error(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        "Native Responses body exceeded the response byte limit",
                        None,
                        None,
                    );
                }
            }
            Ok(Some(Err(error))) => {
                diagnostics.log_failure(
                    &observer.ctx,
                    "native_buffered",
                    "read_error",
                    Some(&error),
                );
                observer.finish("read_error");
                break openai_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    format!("Native Responses body read failed: {error}"),
                    None,
                    None,
                );
            }
            Ok(None) => {
                observer.finish("complete");
                break openai_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Native Responses stream ended before a terminal event",
                    None,
                    None,
                );
            }
            Err(_) => {
                diagnostics.log_failure(&observer.ctx, "native_buffered", "idle_timeout", None);
                observer.finish("idle_timeout");
                break openai_error(
                    StatusCode::GATEWAY_TIMEOUT,
                    "api_error",
                    format!(
                        "Timed out waiting {body_idle_timeout_ms}ms for the next Codex response body chunk"
                    ),
                    None,
                    None,
                );
            }
        }
    };
    if !response.status().is_success() {
        outcome.fail(format!(
            "Native Responses buffering failed: HTTP {}",
            response.status()
        ));
    }
    response.headers_mut().extend(headers);
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response.extensions_mut().insert(outcome);
    response
}

fn buffered_terminal_response(mut event: Value, status: StatusCode) -> Response {
    // Validate the real envelope before treating any payload as a final Response.
    let failure = super::events::classify_event_failure(&event);
    let response = event.get_mut("response").filter(|response| {
        response.is_object() && response.get("id").is_some_and(Value::is_string)
    });
    if let Some(response) = response
        && (response.get("output").is_some_and(Value::is_array)
            || (failure.is_some()
                && response.get("status").and_then(Value::as_str) == Some("failed")))
    {
        // Keep the native object, including unknown/opaque output and usage
        // fields. Do not project it through Anthropic/Chat output types.
        return (status, axum::Json(response.take())).into_response();
    }
    if let Some(failure) = failure {
        let mut response = openai_error(
            StatusCode::from_u16(failure.client_status()).unwrap_or(StatusCode::BAD_GATEWAY),
            failure.client_error_type(),
            failure.message,
            None,
            None,
        );
        if let Some(retry_after) = failure.retry_after
            && let Ok(value) = HeaderValue::from_str(&retry_after)
        {
            response
                .headers_mut()
                .insert(http::header::RETRY_AFTER, value);
        }
        return response;
    }
    openai_error(
        StatusCode::BAD_GATEWAY,
        "api_error",
        "Native Responses terminal response did not contain a complete output array",
        None,
        None,
    )
}

fn passthrough_response(
    upstream: reqwest::Response,
    ctx: RequestContext,
    body_idle_timeout_ms: u64,
    expect_sse: bool,
) -> Response {
    let status = upstream.status();
    let headers = passthrough_headers(upstream.headers());
    let content_type = upstream.headers().get(http::header::CONTENT_TYPE);
    let is_sse = content_type
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
        });
    let inspect_sse = expect_sse && status.is_success() && content_type.is_none();
    let outcome = NativeResponseOutcome::default();
    let observer = NativeResponseObserver::new(ctx, is_sse, inspect_sse, outcome.clone());
    let state = Some(NativeBodyState {
        diagnostics: HttpBodyDiagnostics::new(&upstream, std::time::Instant::now()),
        stream: Box::pin(upstream.bytes_stream()),
        observer,
        body_idle_timeout_ms,
    });
    let stream = futures_util::stream::unfold(state, |state| async move {
        let mut state = state?;
        match tokio::time::timeout(
            Duration::from_millis(state.body_idle_timeout_ms),
            state.stream.next(),
        )
        .await
        {
            Ok(Some(Ok(chunk))) => {
                state.diagnostics.record_chunk(chunk.len());
                state.observer.observe(&chunk);
                Some((Ok::<Bytes, io::Error>(chunk), Some(state)))
            }
            Ok(Some(Err(error))) => {
                state.diagnostics.log_failure(
                    &state.observer.ctx,
                    "native_stream",
                    "read_error",
                    Some(&error),
                );
                let message = format!("Native Responses body read failed: {error}");
                state.observer.finish("read_error");
                Some((Err(io::Error::other(message)), None))
            }
            Ok(None) => {
                state.observer.finish("complete");
                None
            }
            Err(_) => {
                state.diagnostics.log_failure(
                    &state.observer.ctx,
                    "native_stream",
                    "idle_timeout",
                    None,
                );
                let message = format!(
                    "Timed out waiting {}ms for the next Codex response body chunk",
                    state.body_idle_timeout_ms
                );
                state.observer.finish("idle_timeout");
                Some((Err(io::Error::new(io::ErrorKind::TimedOut, message)), None))
            }
        }
    });

    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response.extensions_mut().insert(outcome);
    response
}

fn passthrough_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in upstream {
        if native_response_header_allowed(name) {
            headers.append(name.clone(), value.clone());
        }
    }
    for (name, value) in super::rate_limits::telemetry_headers(upstream) {
        headers.insert(name, value);
    }
    headers
}

fn native_response_header_allowed(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "content-type"
            | "cache-control"
            | "retry-after"
            | "x-request-id"
            | "x-codex-turn-state"
            | "openai-processing-ms"
            | "openai-version"
            | "openai-model"
            | "x-models-etag"
            | "x-reasoning-included"
    ) || name.as_str().starts_with("x-ratelimit-")
}

type UpstreamByteStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static>>;

struct NativeBodyState {
    diagnostics: HttpBodyDiagnostics,
    stream: UpstreamByteStream,
    observer: NativeResponseObserver,
    body_idle_timeout_ms: u64,
}

#[derive(Default)]
struct NativeBufferedOutputs {
    response_id: Option<String>,
    sequence_number: Option<u64>,
    items: BTreeMap<usize, (Value, bool)>,
    item_ids: BTreeMap<String, usize>,
    retained_bytes: u64,
}

impl NativeBufferedOutputs {
    fn observe(&mut self, event: &Value) -> Result<(), String> {
        if let Some(sequence) = event.get("sequence_number") {
            let sequence = sequence
                .as_u64()
                .ok_or("invalid native event sequence number")?;
            if self
                .sequence_number
                .is_some_and(|previous| sequence <= previous)
            {
                return Err("native event sequence moved backwards or repeated".into());
            }
            self.sequence_number = Some(sequence);
        }
        for id in [event.get("response_id"), event.pointer("/response/id")]
            .into_iter()
            .flatten()
        {
            let id = id
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or("invalid native response identity")?;
            if self
                .response_id
                .as_deref()
                .is_some_and(|previous| previous != id)
            {
                return Err("native response identity changed within the event stream".into());
            }
            self.response_id.get_or_insert_with(|| id.to_string());
        }
        if super::events::event_is_terminal(event) {
            return Ok(());
        }
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if !kind.starts_with("response.") {
            return Ok(());
        }
        let item_event = matches!(
            kind,
            "response.output_item.added" | "response.output_item.done"
        );
        if !item_event && event.get("output_index").is_none() {
            return Ok(());
        }
        if self.response_id.is_none() {
            return Err("native output arrived without a response identity".into());
        }
        let index = event
            .get("output_index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| *index < MAX_SSE_CAPTURE_BYTES / 256)
            .ok_or("native output event requires a bounded output index")?;
        let (old, done) = self
            .items
            .get(&index)
            .map(|(item, done)| (item, *done))
            .unwrap_or((&Value::Null, false));
        if done && kind.ends_with(".delta") {
            return Err("native output changed after its finalized snapshot".into());
        }
        let (item, finalized) = if item_event {
            let item = event
                .get("item")
                .ok_or("native output event did not contain an item")?;
            // The done event is authoritative data, not permission to execute
            // a tool. Check its actual status against the eventual terminal.
            validate_native_buffered_item(item, false, false)?;
            native_buffered_item_identity(old, item)?;
            if done && old != item {
                return Err("native finalized output item changed".into());
            }
            if kind.ends_with(".added") && old.get("type").is_some() {
                return Err("native output item start repeated".into());
            }
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                if self
                    .item_ids
                    .get(id)
                    .is_some_and(|previous| *previous != index)
                {
                    return Err("native output item identity moved to another index".into());
                }
                self.item_ids.insert(id.to_string(), index);
            }
            (item.clone(), kind.ends_with(".done"))
        } else {
            let id = event
                .get("item_id")
                .map(|id| {
                    id.as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or("invalid native output item identity")
                })
                .transpose()?;
            if let Some(id) = id {
                if old
                    .get("id")
                    .filter(|id| !id.is_null())
                    .is_some_and(|previous| previous != id)
                    || self
                        .item_ids
                        .get(id)
                        .is_some_and(|previous| *previous != index)
                {
                    return Err("native output event changed item identity".into());
                }
                if !self.item_ids.contains_key(id) {
                    self.item_ids.insert(id.to_string(), index);
                }
            }
            if self.items.contains_key(&index)
                && id.is_none_or(|id| old.get("id").and_then(Value::as_str) == Some(id))
            {
                return Ok(());
            }
            if done {
                return Err("native finalized output event changed item identity".into());
            }
            // Reserve an observed index/identity, not invented output. Only a
            // real done snapshot or nonempty terminal output can finalize it.
            let item = if let Some(id) = id {
                let mut item = if old.is_null() {
                    json!({})
                } else {
                    old.clone()
                };
                item["id"] = Value::String(id.to_string());
                item
            } else {
                Value::Null
            };
            (item, false)
        };
        let old_bytes = self.items.get(&index).map_or(0, |(item, _)| {
            super::request_summary::serialized_json_bytes(item).max(256)
        });
        let retained_bytes = self
            .retained_bytes
            .saturating_sub(old_bytes)
            .saturating_add(super::request_summary::serialized_json_bytes(&item).max(256));
        if retained_bytes > MAX_SSE_CAPTURE_BYTES as u64 {
            return Err("native finalized output exceeded the response byte limit".into());
        }
        self.retained_bytes = retained_bytes;
        self.items.insert(index, (item, finalized));
        Ok(())
    }

    fn complete(&self, event: &mut Value) -> Result<(), String> {
        // Failures keep their actual native error/Response payload, never an
        // assembled successful output. The shared classifier runs first.
        if super::events::classify_event_failure(event).is_some() {
            return Ok(());
        }
        let allow_incomplete = super::events::response_is_incomplete_terminal(event)
            || super::events::response_is_interrupted_terminal(event);
        let response = event
            .get_mut("response")
            .ok_or("native terminal event omitted its response")?;
        let output = match response.get("output") {
            None => None,
            Some(Value::Array(output)) => Some(output),
            Some(_) => return Err("native terminal response output must be an array".into()),
        };
        if let Some(output) = output.filter(|output| !output.is_empty()) {
            let mut ids = BTreeSet::new();
            for (index, item) in output.iter().enumerate() {
                validate_native_buffered_item(item, true, allow_incomplete)?;
                if let Some(id) = item.get("id").and_then(Value::as_str)
                    && (!ids.insert(id)
                        || self
                            .item_ids
                            .get(id)
                            .is_some_and(|previous| *previous != index))
                {
                    return Err(
                        "native terminal output duplicated or moved an item identity".into(),
                    );
                }
            }
            for (index, (item, done)) in &self.items {
                let terminal_item = output
                    .get(*index)
                    .ok_or("native terminal output omitted a streamed item")?;
                native_buffered_item_identity(item, terminal_item)?;
                if *done && !native_buffered_snapshot_matches(item, terminal_item) {
                    return Err(
                        "native terminal output disagrees with its finalized snapshot".into(),
                    );
                }
            }
        } else if !self.items.is_empty() {
            let mut output = Vec::with_capacity(self.items.len());
            for (expected, (index, (item, done))) in self.items.iter().enumerate() {
                if expected != *index || !done {
                    return Err("native terminal output cannot be assembled from incomplete or gapped item snapshots".into());
                }
                validate_native_buffered_item(item, true, allow_incomplete)?;
                output.push(item.clone());
            }
            response["output"] = Value::Array(output);
        }
        Ok(())
    }
}

fn validate_native_buffered_item(
    item: &Value,
    finalized: bool,
    allow_incomplete: bool,
) -> Result<(), String> {
    if !item.is_object()
        || item
            .get("type")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err("native output snapshot requires an object with a type".into());
    }
    if item
        .get("id")
        .is_some_and(|id| !id.is_null() && id.as_str().is_none_or(str::is_empty))
    {
        return Err("native output snapshot has an invalid item identity".into());
    }
    if finalized
        && item
            .get("status")
            .filter(|status| !status.is_null())
            .is_some_and(|status| {
                status == "in_progress" || (status == "incomplete" && !allow_incomplete)
            })
    {
        return Err("native output snapshot is not finalized".into());
    }
    Ok(())
}

fn native_buffered_item_identity(previous: &Value, item: &Value) -> Result<(), String> {
    for key in ["type", "id", "call_id", "name", "namespace"] {
        if previous
            .get(key)
            .filter(|value| !value.is_null())
            .is_some_and(|value| item.get(key) != Some(value))
        {
            return Err("native output snapshot changed item identity".into());
        }
    }
    Ok(())
}

fn native_buffered_snapshot_matches(snapshot: &Value, authoritative: &Value) -> bool {
    match (snapshot, authoritative) {
        (Value::Object(snapshot), Value::Object(authoritative)) => {
            snapshot.iter().all(|(key, value)| {
                authoritative
                    .get(key)
                    .is_some_and(|current| native_buffered_snapshot_matches(value, current))
            })
        }
        (Value::Array(snapshot), Value::Array(authoritative)) => {
            snapshot.len() == authoritative.len()
                && snapshot
                    .iter()
                    .zip(authoritative)
                    .all(|(previous, current)| native_buffered_snapshot_matches(previous, current))
        }
        _ => snapshot == authoritative,
    }
}

struct NativeResponseObserver {
    ctx: RequestContext,
    is_sse: bool,
    inspect_sse: bool,
    validate_json_on_eof: bool,
    terminal_seen: bool,
    buffer_terminal: bool,
    buffered_terminal: Option<Result<Value, String>>,
    buffered_outputs: NativeBufferedOutputs,
    generation_started: bool,
    raw: Vec<u8>,
    raw_truncated: u64,
    pending: Vec<u8>,
    pending_scan: usize,
    discarding_oversized_frame: bool,
    pending_truncated: bool,
    captured_events: Vec<Value>,
    captured_event_bytes: usize,
    captured_events_truncated: u64,
    usage: TokenUsage,
    outcome: NativeResponseOutcome,
    finished: bool,
}

impl NativeResponseObserver {
    fn new(
        ctx: RequestContext,
        is_sse: bool,
        inspect_sse: bool,
        outcome: NativeResponseOutcome,
    ) -> Self {
        Self {
            ctx,
            is_sse,
            inspect_sse,
            validate_json_on_eof: inspect_sse,
            terminal_seen: false,
            buffer_terminal: false,
            buffered_terminal: None,
            buffered_outputs: NativeBufferedOutputs::default(),
            generation_started: false,
            raw: Vec::new(),
            raw_truncated: 0,
            pending: Vec::new(),
            pending_scan: 0,
            discarding_oversized_frame: false,
            pending_truncated: false,
            captured_events: Vec::new(),
            captured_event_bytes: 0,
            captured_events_truncated: 0,
            usage: TokenUsage::default(),
            outcome,
            finished: false,
        }
    }

    fn observe(&mut self, chunk: &[u8]) {
        if !chunk.is_empty() && !self.generation_started {
            if let Some(monitor) = self.ctx.monitor.as_ref() {
                monitor.generation_started(&self.ctx.req_id);
            }
            self.generation_started = true;
        }
        self.capture_raw(chunk);

        if self.is_sse || self.inspect_sse {
            self.pending.extend_from_slice(chunk);
        }
        if self.inspect_sse {
            // Inspect the prefix, not embedded strings, so JSON/error bodies
            // keep their normal inspection rather than being mistaken for SSE.
            let bom = b"\xef\xbb\xbf";
            let partial_bom = !self.pending.is_empty() && bom.starts_with(&self.pending);
            if self.pending.starts_with(bom) {
                self.pending.drain(..bom.len());
            }
            let prefix = self.pending.trim_ascii_start();
            let fields: [&[u8]; 5] = [b"data:", b"event:", b":", b"id:", b"retry:"];
            if fields.iter().any(|field| prefix.starts_with(field)) {
                self.is_sse = true;
                self.inspect_sse = false;
            } else if (!partial_bom && !fields.iter().any(|field| field.starts_with(prefix)))
                || self.pending.len() > MAX_STREAM_CAPTURE_FRAME_BYTES
            {
                self.inspect_sse = false;
                self.pending.clear();
            }
        }
        let events = if self.is_sse {
            self.drain_sse_events()
        } else {
            0
        };
        if let Some(monitor) = self.ctx.monitor.as_ref() {
            monitor.stream_progress(&self.ctx.req_id, chunk.len() as u64, events, self.usage);
        }
    }

    fn capture_raw(&mut self, chunk: &[u8]) {
        // Non-SSE JSON still needs a bounded body for outcome/usage inspection.
        if self.is_sse && self.ctx.traffic.is_none() {
            return;
        }
        let remaining = MAX_SSE_CAPTURE_BYTES.saturating_sub(self.raw.len());
        let captured = remaining.min(chunk.len());
        self.raw.extend_from_slice(&chunk[..captured]);
        if captured < chunk.len() {
            self.raw_truncated = self
                .raw_truncated
                .saturating_add((chunk.len() - captured) as u64);
        }
    }

    fn drain_sse_events(&mut self) -> u64 {
        if self.discarding_oversized_frame {
            let Some((end, separator_len)) = find_sse_boundary_from(&self.pending, 0) else {
                retain_boundary_prefix(&mut self.pending);
                return 0;
            };
            self.pending.drain(..end + separator_len);
            self.discarding_oversized_frame = false;
            self.pending_scan = 0;
        }

        let mut consumed = 0;
        let mut parsed = Vec::new();
        while let Some((relative_end, separator_len)) =
            find_sse_boundary_from(&self.pending, self.pending_scan)
        {
            let end = relative_end + separator_len;
            parsed.extend(parse_sse_events(&self.pending[consumed..end]));
            consumed = end;
            self.pending_scan = consumed;
        }
        if consumed > 0 {
            self.pending.drain(..consumed);
            self.pending_scan = 0;
        } else {
            self.pending_scan = self.pending.len().saturating_sub(3);
        }

        let mut count = 0_u64;
        for event in parsed {
            count += 1;
            if event.data == "[DONE]" {
                continue;
            }
            match serde_json::from_str::<Value>(&event.data) {
                Ok(value) => self.record_event(event.event.as_deref(), value),
                Err(_) => {
                    if self.buffer_terminal {
                        self.buffered_terminal = Some(Err(
                            "Native Responses returned malformed JSON in its event stream"
                                .to_string(),
                        ));
                    }
                    self.capture_event(json!({
                        "event": event.event,
                        "unparseable": true,
                        "bytes": event.data.len(),
                    }));
                }
            }
            if self.buffered_terminal.is_some() {
                break;
            }
        }

        if !self.buffer_terminal && self.pending.len() > MAX_STREAM_CAPTURE_FRAME_BYTES {
            self.pending_truncated = true;
            self.discarding_oversized_frame = true;
            retain_boundary_prefix(&mut self.pending);
            self.pending_scan = 0;
        }
        count
    }

    fn record_event(&mut self, event: Option<&str>, mut value: Value) {
        if let Some(event) = event
            && let Some(object) = value.as_object_mut()
        {
            object
                .entry("type")
                .or_insert_with(|| Value::String(event.to_string()));
        }
        self.update_usage(&value);
        value = self.update_outcome(value);
        self.terminal_seen |= super::events::event_is_terminal(&value);
        if self.buffer_terminal {
            match self.buffered_outputs.observe(&value) {
                Err(message) => {
                    self.outcome.fail(message.clone());
                    self.buffered_terminal = Some(Err(message));
                }
                Ok(()) if self.terminal_seen => {
                    let mut terminal = value.clone();
                    let result = self
                        .buffered_outputs
                        .complete(&mut terminal)
                        .map(|()| terminal);
                    if let Err(message) = &result {
                        self.outcome.fail(message.clone());
                    }
                    self.buffered_terminal = Some(result);
                }
                Ok(()) => {}
            }
        }
        if self.ctx.traffic.is_none() {
            return;
        }
        let mut captured = value;
        if let Some(event) = event
            && let Some(object) = captured.as_object_mut()
        {
            object
                .entry("_sse_event")
                .or_insert_with(|| Value::String(event.to_string()));
        }
        self.capture_event(captured);
    }

    fn capture_event(&mut self, value: Value) {
        if self.ctx.traffic.is_none() {
            return;
        }
        let bytes = serde_json::to_vec(&value).map_or(0, |value| value.len());
        if self.captured_events.len() < MAX_STREAM_CAPTURE_EVENTS
            && self.captured_event_bytes.saturating_add(bytes) <= MAX_STREAM_CAPTURE_EVENT_BYTES
        {
            self.captured_event_bytes += bytes;
            self.captured_events.push(value);
        } else {
            self.captured_events_truncated = self.captured_events_truncated.saturating_add(1);
        }
    }

    fn update_outcome(&self, mut value: Value) -> Value {
        // Move non-SSE responses through the terminal envelope, then recover the
        // original body for capture without copying its output payload.
        let failure = if !self.is_sse && value.get("type").is_none() {
            let mut envelope = Value::Object(Map::from_iter([
                (
                    "type".to_string(),
                    Value::String("response.completed".to_string()),
                ),
                ("response".to_string(), value),
            ]));
            let failure = super::events::classify_event_failure(&envelope);
            value = envelope["response"].take();
            failure
        } else {
            super::events::classify_event_failure(&value)
        };
        if let Some(failure) = failure {
            self.outcome.fail(failure.message);
            return value;
        }
        let event_type = value.get("type").and_then(Value::as_str);
        let has_error = value.get("error").is_some_and(|error| !error.is_null());
        let failed_status = value.get("status").and_then(Value::as_str) == Some("failed");
        if matches!(
            event_type,
            Some("response.failed" | "response.error" | "error")
        ) || has_error
            || failed_status
        {
            let message = value
                .pointer("/response/error/message")
                .or_else(|| value.pointer("/error/message"))
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Native Responses stream failed");
            self.outcome.fail(message.to_string());
        }
        value
    }

    fn update_usage(&mut self, value: &Value) {
        if self.is_sse && !super::events::event_is_terminal(value) {
            return;
        }
        if let Some(usage) = value
            .pointer("/response/usage")
            .or_else(|| value.get("usage"))
        {
            self.usage = TokenUsage::from_codex(usage);
        }
    }

    fn finish(&mut self, outcome: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        if outcome == "complete" && self.is_sse && !self.terminal_seen {
            let message = if self.pending_truncated || self.discarding_oversized_frame {
                "Native Responses terminal outcome could not be validated within the SSE frame inspection limit"
            } else {
                "Native Responses stream ended before a terminal event"
            };
            self.outcome.fail(message.to_string());
        }
        if !self.is_sse {
            match serde_json::from_slice::<Value>(&self.raw) {
                Ok(value) if self.raw_truncated == 0 => {
                    self.update_usage(&value);
                    let value = self.update_outcome(value);
                    self.capture_event(value);
                    if let Some(monitor) = self.ctx.monitor.as_ref() {
                        monitor.usage_updated(&self.ctx.req_id, self.usage);
                    }
                }
                _ if outcome == "complete" && self.validate_json_on_eof => {
                    // A rejected/unfinished SSE prefix is not proof of JSON.
                    // A bounded capture cannot prove the complete body either.
                    let message = if self.raw_truncated > 0 {
                        "Native Responses body could not be validated within the inspection byte limit"
                    } else {
                        "Native Responses body ended without a terminal SSE event or complete JSON"
                    };
                    self.outcome.fail(message.to_string());
                }
                _ => {}
            }
        }
        self.write_capture(outcome);
    }

    fn write_capture(&self, outcome: &str) {
        let Some(traffic) = self.ctx.traffic.as_deref() else {
            return;
        };
        if !self.raw.is_empty() {
            traffic.write_bytes(
                if self.is_sse {
                    "032-upstream-response-body.sse"
                } else {
                    "032-upstream-response-body.json"
                },
                &self.raw,
            );
        }
        for event in &self.captured_events {
            traffic.write_json_event("040-upstream-event", event);
        }
        traffic.write_json(
            "033-native-response-capture",
            &json!({
                "outcome": outcome,
                "capturedBytes": self.raw.len(),
                "truncatedBytes": self.raw_truncated,
                "pendingFrameTruncated": self.pending_truncated,
                "capturedEvents": self.captured_events.len(),
                "capturedEventBytes": self.captured_event_bytes,
                "truncatedEvents": self.captured_events_truncated,
                "inputTokens": self.usage.input_tokens,
                "cachedInputTokens": self.usage.cached_input_tokens,
                "outputTokens": self.usage.output_tokens,
            }),
        );
    }
}

impl Drop for NativeResponseObserver {
    fn drop(&mut self) {
        if !self.finished {
            self.finish("downstream_cancelled");
        }
    }
}

pub(super) fn find_sse_boundary_from(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
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

fn retain_boundary_prefix(bytes: &mut Vec<u8>) {
    let keep = bytes.len().min(3);
    if bytes.len() > keep {
        bytes.drain(..bytes.len() - keep);
    }
}
