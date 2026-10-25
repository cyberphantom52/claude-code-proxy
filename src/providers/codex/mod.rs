pub mod auth;
pub mod chat_completions;
pub mod client;
pub mod compaction;
pub mod continuation;
pub mod count_tokens;
pub(crate) mod events;
pub mod images;
pub mod models;
pub mod native;
pub mod rate_limits;
mod recovery;
pub mod request_summary;
pub mod search;
mod search_loop;
pub(crate) mod threads;
pub mod transcription;
pub mod translate;
pub mod websocket;

use async_trait::async_trait;
use axum::Json;
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::anthropic::error::json_error;
use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::anthropic::sse::parse_sse_events;
use crate::config;
use crate::logging::create_logger;
use crate::monitor::TokenUsage;
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::registry;
use crate::request_identity::ConversationIdentity;
use crate::retry::{compute_backoff_delay, sleep};

use self::auth::browser_login::run_browser_login;
use self::auth::device::DeviceAuthClient;
use self::auth::manager::CodexAuthManager;
use self::auth::token_store::file_store;
use self::client::CodexHttpClient;
use self::compaction::{
    CompactionAttempt, CompactionError, CompactionScope,
    abort_compaction_attempt_scoped as abort_compaction_attempt,
    activate_compaction_scoped as activate_compaction,
    apply_compaction_replay_scoped as apply_compaction_replay,
    begin_compaction_scoped as begin_compaction, request_compaction,
    store_compaction_scoped as store_compaction,
};
use self::continuation::{
    ContinuationReservation, abort_continuation_for_owner, continuation_candidate_for_owner,
    record_continuation_for_owner,
};
use self::count_tokens::count_translated_tokens;
use self::native::NativeResponseOutcome;
use self::translate::accumulate::accumulate_response_and_metadata_with_traffic;
use self::translate::live_stream::LiveStreamTranslator;
use self::translate::model_allowlist::{
    assert_allowed_model, resolve_model_request, uses_responses_lite, validate_hosted_web_search,
};
use self::translate::reducer::{
    ReducerEvent, finish_metadata_from_upstream_with_stops, reduce_upstream_bytes_with_stops,
};
use self::translate::request::{
    TranslateOptions, disclose_messages_compatibility, has_hosted_web_search,
    is_compact_messages_request, translate_request, validate_messages_capabilities,
};

const MAX_RETRYABLE_LIVE_STREAM_RETRIES: u32 = 3;
const MAX_EMPTY_COMPLETION_RETRIES: u32 = 2;
const EMPTY_COMPLETION_RETRY_BUDGET: Duration = Duration::from_secs(60);
const EMPTY_CODEX_COMPLETION_DETAIL: &str = "empty_codex_completion";
const LIVE_STREAM_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct CodexProvider {
    client: Arc<CodexHttpClient>,
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexProvider {
    pub fn new() -> Self {
        Self {
            client: Arc::new(CodexHttpClient::new()),
        }
    }
}

fn transport_for_request(
    configured: config::CodexTransport,
    compact_boundary: bool,
) -> config::CodexTransport {
    if compact_boundary {
        config::CodexTransport::Http
    } else {
        configured
    }
}

impl CodexProvider {
    async fn handle_messages_inner(
        &self,
        incoming: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> Response {
        if let Err(error) = validate_messages_capabilities(&incoming) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            );
        }
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        threads::set_message_id(&message_id);
        let want_stream = incoming.stream;
        let lite_search =
            search::SearchPolicy::from_request(&incoming, conversation_identity.as_ref());
        let (resolved, mut translated, compact_boundary, use_responses_lite, model) = {
            let mut body = incoming;
            let model = body.model.as_deref().unwrap_or("gpt-6-sol");
            if body.model.is_some()
                && let Err(error) = self::translate::model_allowlist::validate_model_configuration(
                    model,
                    !body.bypass_provider_model_override,
                )
            {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    error.to_string(),
                );
            }

            let resolved = resolve_model_request(model);
            if let Err(e) = assert_allowed_model(&resolved.model) {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    e.to_string(),
                );
            }
            let use_responses_lite = match apply_model_lane_for_request(&resolved.model, &body) {
                Ok(lite) => lite,
                Err(error) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        error.to_string(),
                    );
                }
            };
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.model_resolved(&ctx.req_id, &resolved.model);
            }

            let compact_boundary = is_compact_messages_request(&body);
            let model = body.model.take().unwrap_or_else(|| "gpt-6-sol".to_string());
            let options = TranslateOptions {
                session_id: ctx.session_id.clone(),
                service_tier: resolved.service_tier.clone(),
                model: resolved.model.clone(),
                use_responses_lite,
            };
            let thread_body = threads::current().map(|_| body.clone());
            let mut translated = match translate_request(body, options.clone()) {
                Ok(t) => t,
                Err(e) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        e.to_string(),
                    );
                }
            };

            if let Some(body) = thread_body.as_ref()
                && let Err(error) = threads::restore_native(body, options, &mut translated)
            {
                return error.response();
            }
            (
                resolved,
                translated,
                compact_boundary,
                use_responses_lite,
                model,
            )
        };

        let mut portable_request = None;
        let server_compaction_enabled = config::codex_server_compaction();
        let scope = conversation_identity
            .as_ref()
            .zip(models::current_operation_provenance())
            .map(|(owner, provenance)| CompactionScope {
                owner: owner.clone(),
                provenance,
                branch: threads::branch(),
            });
        let mut compaction_attempt = None;
        if !server_compaction_enabled && let Some(scope) = scope.as_ref() {
            compaction::clear_compaction_scoped(scope);
        }
        if server_compaction_enabled
            && compact_boundary
            && let Some(scope) = scope.as_ref()
        {
            let attempt = begin_compaction(scope, &translated.model);
            compaction_attempt = Some(attempt);
            let mut preparing_cleanup = LiveRequestStateCleanup::new(
                ContinuationReservation::for_owner_turn(conversation_identity.as_ref(), None),
                Some(attempt),
            );
            log_compaction_event(
                "server_compaction_triggered",
                &ctx,
                translated.input.len(),
                None,
            );
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.compaction_started(&ctx.req_id);
            }
            let mut compaction_ctx = ctx.clone();
            compaction_ctx.monitor = None;
            match request_compaction(self.client.as_ref(), &translated, &compaction_ctx).await {
                Ok(native_history) => {
                    if store_compaction(scope, attempt, native_history) {
                        log_compaction_event(
                            "server_compaction_completed",
                            &ctx,
                            translated.input.len(),
                            None,
                        );
                    } else {
                        compaction_attempt = None;
                        log_compaction_event(
                            "server_compaction_failed",
                            &ctx,
                            translated.input.len(),
                            Some("compaction state was superseded or exceeded the in-memory limit"),
                        );
                    }
                }
                Err(CompactionError::Upstream(error)) if error.usage_limit.is_some() => {
                    abort_compaction_attempt(Some(scope), Some(attempt));
                    log_compaction_event(
                        "server_compaction_failed",
                        &ctx,
                        translated.input.len(),
                        Some(&error.to_string()),
                    );
                    return map_codex_error_to_response(&error);
                }
                Err(error) => {
                    abort_compaction_attempt(Some(scope), Some(attempt));
                    compaction_attempt = None;
                    log_compaction_event(
                        "server_compaction_failed",
                        &ctx,
                        translated.input.len(),
                        Some(&error.to_string()),
                    );
                }
            }
            preparing_cleanup.disarm();
        } else if server_compaction_enabled
            && !compact_boundary
            && let Some(replay) = apply_compaction_replay(scope.as_ref(), &translated)
        {
            portable_request = Some(translated);
            translated = replay.request;
            compaction_attempt = Some(replay.attempt);
        }

        // Check continuation
        let previous_response_id_enabled = config::codex_previous_response_id();
        let continuation = continuation_candidate_for_owner(
            conversation_identity.as_ref(),
            &translated,
            previous_response_id_enabled,
        );
        let turn_id = continuation.turn_id();
        let mut request_cleanup =
            LiveRequestStateCleanup::new(continuation.clone(), compaction_attempt);
        let configured_transport =
            transport_for_request(config::codex_transport(), compact_boundary);
        let transport = configured_transport.as_str();
        let upstream_started_at = Instant::now();
        let log = create_logger("codex");
        let req_id = ctx.req_id.clone();
        log.info(
            "codex_upstream_request_started",
            Some(serde_json::Map::from_iter([
                ("reqId".to_string(), serde_json::json!(&req_id)),
                ("transport".to_string(), serde_json::json!(transport)),
                ("model".to_string(), serde_json::json!(&resolved.model)),
                ("stream".to_string(), serde_json::json!(want_stream)),
                (
                    "responsesLite".to_string(),
                    serde_json::json!(use_responses_lite),
                ),
                (
                    "previousResponseIdEnabled".to_string(),
                    serde_json::json!(previous_response_id_enabled),
                ),
                (
                    "hasPreviousResponseId".to_string(),
                    serde_json::json!(continuation.candidate().previous_response_id.is_some()),
                ),
                (
                    "inputDeltaCount".to_string(),
                    serde_json::json!(continuation.candidate().input_delta.as_ref().map(Vec::len)),
                ),
                ("turnId".to_string(), serde_json::json!(turn_id)),
                (
                    "continuationDisabledReason".to_string(),
                    serde_json::json!(continuation.candidate().disabled_reason),
                ),
            ])),
        );

        // Post to upstream with continuation
        let client = self.client.clone();
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }
        if use_responses_lite && let Some(policy) = lite_search {
            let response = search_loop::response(
                client,
                message_id,
                model,
                ctx,
                translated,
                continuation,
                LiveStreamCompaction {
                    compact_boundary,
                    attempt: compaction_attempt,
                    portable_request: None,
                },
                configured_transport,
                policy,
                want_stream,
            )
            .await;
            request_cleanup.disarm();
            return response;
        }
        if want_stream {
            let response = live_stream_response(
                client,
                message_id,
                &model,
                ctx,
                Arc::new(translated),
                continuation,
                LiveStreamCompaction {
                    compact_boundary,
                    attempt: compaction_attempt,
                    portable_request: portable_request.map(Arc::new),
                },
                configured_transport,
            )
            .await;
            log.info(
                "codex_upstream_response_ready",
                Some(serde_json::Map::from_iter([
                    ("reqId".to_string(), serde_json::json!(&req_id)),
                    ("transport".to_string(), serde_json::json!(transport)),
                    (
                        "status".to_string(),
                        serde_json::json!(response.status().as_u16()),
                    ),
                    (
                        "ms".to_string(),
                        serde_json::json!(upstream_started_at.elapsed().as_millis()),
                    ),
                ])),
            );
            request_cleanup.disarm();
            return response;
        }

        let mut request_continuation = continuation.clone();
        let mut continuation = Some(continuation);
        let mut attempt = 0_u32;
        let (upstream, reduction) = loop {
            let result = match await_empty_retry(
                client.post_codex_for_owner(&translated, &ctx, continuation.as_ref()),
                attempt > 0,
                upstream_started_at,
            )
            .await
            {
                Ok(result) => result,
                Err(()) => {
                    log_empty_completion_retry(&ctx, attempt, upstream_started_at.elapsed(), None);
                    abort_compaction_attempt(scope.as_ref(), compaction_attempt);
                    abort_continuation_for_owner(&request_continuation);
                    return map_codex_error_to_response(&empty_buffered_completion_error());
                }
            };
            let response = match result {
                Ok(r) => r,
                Err(e)
                    if e.detail.as_deref() == Some("operation_provenance_changed")
                        && portable_request.is_some()
                        && !threads::replaying() =>
                {
                    abort_compaction_attempt(scope.as_ref(), compaction_attempt.take());
                    abort_continuation_for_owner(&request_continuation);
                    translated = portable_request.take().expect("portable fallback checked");
                    request_continuation = ContinuationReservation::for_owner_turn(None, None);
                    continuation = None;
                    continue;
                }
                Err(e) => {
                    log.warn(
                        "codex_upstream_request_failed",
                        Some(serde_json::Map::from_iter([
                            ("reqId".to_string(), serde_json::json!(&req_id)),
                            ("transport".to_string(), serde_json::json!(transport)),
                            ("status".to_string(), serde_json::json!(e.status)),
                            (
                                "origin".to_string(),
                                serde_json::json!(format!("{:?}", e.origin)),
                            ),
                            ("error".to_string(), serde_json::json!(&e.message)),
                            (
                                "ms".to_string(),
                                serde_json::json!(upstream_started_at.elapsed().as_millis()),
                            ),
                        ])),
                    );
                    abort_compaction_attempt(scope.as_ref(), compaction_attempt);
                    abort_continuation_for_owner(&request_continuation);
                    return map_codex_error_to_response(&e);
                }
            };
            match reduce_upstream_bytes_with_stops(&response.body, &translated.stop_sequences) {
                Ok(events) if is_empty_codex_success_completion(&events) => {}
                reduction => break (response, reduction),
            }
            // A successful terminal event with no output would translate into
            // an empty end_turn; retry with full context instead.
            let error = empty_buffered_completion_error();
            drop_live_continuation_for_retry(&mut continuation);
            let delay = compute_backoff_delay(attempt, None);
            if !allow_empty_completion_retry(attempt, upstream_started_at.elapsed(), delay.wait_ms)
            {
                log_empty_completion_retry(&ctx, attempt, upstream_started_at.elapsed(), None);
                abort_compaction_attempt(scope.as_ref(), compaction_attempt);
                abort_continuation_for_owner(&request_continuation);
                return map_codex_error_to_response(&error);
            }
            if delay.exceeds_budget {
                abort_compaction_attempt(scope.as_ref(), compaction_attempt);
                abort_continuation_for_owner(&request_continuation);
                return map_codex_error_to_response(&error);
            }
            log_empty_completion_retry(
                &ctx,
                attempt,
                upstream_started_at.elapsed(),
                Some(delay.wait_ms),
            );
            attempt += 1;
            sleep(delay.wait_ms).await;
        };
        if let Some(failure) = events::first_event_failure(&upstream.body) {
            abort_compaction_attempt(scope.as_ref(), compaction_attempt);
            abort_continuation_for_owner(&request_continuation);
            return map_codex_event_failure_to_response(&failure);
        }
        log.info(
            "codex_upstream_response_received",
            Some(serde_json::Map::from_iter([
                ("reqId".to_string(), serde_json::json!(&req_id)),
                (
                    "transport".to_string(),
                    serde_json::json!(format!("{:?}", upstream.transport)),
                ),
                (
                    "hasReusableSocket".to_string(),
                    serde_json::json!(upstream.socket_id.is_some()),
                ),
                ("status".to_string(), serde_json::json!(upstream.status)),
                (
                    "bodyBytes".to_string(),
                    serde_json::json!(upstream.body.len()),
                ),
                (
                    "ms".to_string(),
                    serde_json::json!(upstream_started_at.elapsed().as_millis()),
                ),
            ])),
        );

        match accumulate_response_and_metadata_with_traffic(
            reduction,
            &message_id,
            &model,
            ctx.traffic.as_deref(),
        ) {
            Ok((json, finish)) => {
                threads::settle(
                    &json,
                    finish.as_ref(),
                    &translated,
                    upstream.provenance.as_ref(),
                );
                update_continuation_from_finish(
                    compaction_scope(&request_continuation).as_ref(),
                    &request_continuation,
                    compaction_attempt,
                    &translated,
                    finish.as_ref(),
                    upstream.socket_id,
                    upstream.provenance.as_ref(),
                    compact_boundary,
                );
                request_cleanup.disarm();
                (StatusCode::OK, Json(json)).into_response()
            }
            Err(e) => {
                abort_compaction_attempt(scope.as_ref(), compaction_attempt);
                abort_continuation_for_owner(&request_continuation);
                map_codex_failure_to_response(&format!("Accumulation error: {e}"))
            }
        }
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn supported_models(&self) -> Vec<String> {
        registry::expand_codex_models()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &CODEX_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        self.handle_messages_with_conversation_identity(body, ctx, None)
            .await
    }

    async fn handle_messages_with_conversation_identity(
        &self,
        mut body: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> Response {
        let response = rate_limits::with_response_telemetry(async {
            let context = self.client.prepare_model_context().await;
            models::with_model_context(context, async {
                let attempt = if threads::current().is_none() {
                    match threads::prepare(
                        &mut body,
                        conversation_identity.as_ref(),
                        models::current_operation_provenance(),
                        false,
                    ) {
                        Ok(attempt) => attempt,
                        Err(error) => return error.response(),
                    }
                } else {
                    None
                };
                let response = threads::scope(
                    attempt.clone().or_else(threads::current),
                    Box::pin(self.handle_messages_inner(body, ctx, conversation_identity)),
                )
                .await;
                threads::response_body(response, attempt)
            })
            .await
        })
        .await;
        disclose_messages_compatibility(response)
    }

    async fn handle_count_tokens(
        &self,
        mut body: MessagesRequest,
        ctx: RequestContext,
    ) -> Response {
        if let Err(error) = threads::prepare(&mut body, None, None, true) {
            return error.response();
        }
        if let Err(error) = validate_messages_capabilities(&body) {
            return disclose_messages_compatibility(json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            ));
        }
        let context = self.client.prepare_model_context().await;
        disclose_messages_compatibility(
            models::with_model_context(context, self.handle_count_tokens_inner(body, ctx)).await,
        )
    }
}

impl CodexProvider {
    async fn handle_count_tokens_inner(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
    ) -> Response {
        let model = body.model.as_deref().unwrap_or("gpt-6-sol");
        if body.model.is_some()
            && let Err(error) = self::translate::model_allowlist::validate_model_configuration(
                model,
                !body.bypass_provider_model_override,
            )
        {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            );
        }
        let resolved = resolve_model_request(model);
        if let Err(e) = assert_allowed_model(&resolved.model) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                e.to_string(),
            );
        }
        let use_responses_lite = match apply_model_lane_for_request(&resolved.model, &body) {
            Ok(lite) => lite,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    error.to_string(),
                );
            }
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved.model);
        }

        let translated = match translate_request(
            body,
            TranslateOptions {
                session_id: None,
                service_tier: resolved.service_tier.clone(),
                model: resolved.model.clone(),
                use_responses_lite,
            },
        ) {
            Ok(t) => t,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    e.to_string(),
                );
            }
        };

        let tokens = count_translated_tokens(&translated);
        (
            StatusCode::OK,
            Json(CountTokensResponse {
                input_tokens: tokens,
            }),
        )
            .into_response()
    }
}

/// Preserve the model-selected lane; Lite search is orchestrated locally.
fn apply_model_lane_for_request(model: &str, body: &MessagesRequest) -> anyhow::Result<bool> {
    let lite = uses_responses_lite(model);
    if has_hosted_web_search(body) && !lite {
        validate_hosted_web_search(model)?;
    }
    Ok(lite)
}

fn count_sse_events(bytes: &[u8]) -> u64 {
    String::from_utf8_lossy(bytes).matches("event:").count() as u64
}

fn log_compaction_event(
    event: &str,
    ctx: &RequestContext,
    input_items: usize,
    error: Option<&str>,
) {
    let mut fields = serde_json::Map::new();
    fields.insert("reqId".into(), serde_json::json!(ctx.req_id));
    fields.insert("inputItems".into(), serde_json::json!(input_items));
    if let Some(error) = error {
        fields.insert("error".into(), serde_json::json!(error));
        create_logger("codex").warn(event, Some(fields));
    } else {
        create_logger("codex").info(event, Some(fields));
    }
}

fn compaction_scope(continuation: &ContinuationReservation) -> Option<CompactionScope> {
    Some(CompactionScope {
        owner: continuation.owner()?.clone(),
        provenance: continuation.provenance()?.clone(),
        branch: threads::branch(),
    })
}

fn abort_request_state(
    scope: Option<&CompactionScope>,
    continuation: &ContinuationReservation,
    compaction_attempt: Option<CompactionAttempt>,
) {
    abort_compaction_attempt(scope, compaction_attempt);
    abort_continuation_for_owner(continuation);
}

struct LiveRequestStateCleanup {
    continuation: ContinuationReservation,
    scope: Option<CompactionScope>,
    compaction_attempt: Option<CompactionAttempt>,
    armed: bool,
}

impl LiveRequestStateCleanup {
    fn new(
        continuation: ContinuationReservation,
        compaction_attempt: Option<CompactionAttempt>,
    ) -> Self {
        Self {
            scope: compaction_scope(&continuation),
            continuation,
            compaction_attempt,
            armed: true,
        }
    }

    fn abort(&mut self) {
        if self.armed {
            abort_request_state(
                self.scope.as_ref(),
                &self.continuation,
                self.compaction_attempt,
            );
            self.armed = false;
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LiveRequestStateCleanup {
    fn drop(&mut self) {
        self.abort();
    }
}

enum LiveStreamStart {
    Response(Response),
    Retry {
        error: client::CodexError,
        full_context_retry_attempted: bool,
    },
}

#[derive(Clone)]
struct LiveStreamCompaction {
    compact_boundary: bool,
    attempt: Option<CompactionAttempt>,
    portable_request: Option<Arc<translate::request::ResponsesRequest>>,
}

#[allow(clippy::too_many_arguments)]
async fn live_stream_response(
    client: Arc<CodexHttpClient>,
    message_id: String,
    model: &str,
    ctx: RequestContext,
    mut request_body: Arc<translate::request::ResponsesRequest>,
    continuation: ContinuationReservation,
    mut compaction: LiveStreamCompaction,
    transport: config::CodexTransport,
) -> Response {
    let model = model.to_string();
    let mut request_continuation = continuation.clone();
    let mut cleanup =
        LiveRequestStateCleanup::new(request_continuation.clone(), compaction.attempt);
    let mut attempt = 0_u32;
    let started_at = Instant::now();
    let mut empty_retry = false;
    let mut retry_started = None;
    let mut continuation = Some(continuation);

    loop {
        rate_limits::record(Vec::new());
        let open_stream = async {
            match transport {
                config::CodexTransport::Http => {
                    client
                        .stream_codex_http_events_for_owner(&request_body, &ctx)
                        .await
                }
                config::CodexTransport::WebSocket => {
                    client
                        .stream_codex_websocket_events_for_owner(
                            request_body.clone(),
                            &ctx,
                            continuation.as_ref(),
                        )
                        .await
                }
                config::CodexTransport::Auto => {
                    client
                        .stream_codex_auto_events_for_owner(
                            request_body.clone(),
                            &ctx,
                            continuation.as_ref(),
                        )
                        .await
                }
            }
        };
        let upstream_events = match await_empty_retry(
            open_stream,
            empty_retry || retry_started.is_some(),
            if empty_retry {
                started_at
            } else {
                retry_started.unwrap_or(started_at)
            },
        )
        .await
        {
            Ok(events) => events,
            Err(()) => {
                if empty_retry {
                    log_empty_completion_retry(&ctx, attempt, started_at.elapsed(), None);
                }
                cleanup.abort();
                return if empty_retry {
                    map_codex_error_to_response(&empty_live_completion_error())
                } else {
                    exhausted_recovery_response(
                        &ctx,
                        attempt,
                        &connection_reset_error(
                            "Connection recovery exceeded its 60-second budget",
                        ),
                    )
                };
            }
        };
        let upstream_events = match upstream_events {
            Ok(events) => events,
            Err(err)
                if err.detail.as_deref() == Some("operation_provenance_changed")
                    && compaction.portable_request.is_some()
                    && !threads::replaying() =>
            {
                cleanup.abort();
                request_body = compaction
                    .portable_request
                    .take()
                    .expect("portable fallback checked");
                compaction.attempt = None;
                request_continuation = ContinuationReservation::for_owner_turn(None, None);
                continuation = None;
                continue;
            }
            Err(err) if err.origin == client::CodexErrorOrigin::Http => {
                cleanup.abort();
                return map_codex_error_to_response(&err);
            }
            Err(err) if retryable_live_start_codex_error(&err) => {
                let dropped = drop_live_continuation_for_retry(&mut continuation);
                if dropped && is_missing_previous_response_error(&err) {
                    attempt += 1;
                    continue;
                }
                let delay = compute_backoff_delay(attempt, err.retry_after.as_deref());
                if attempt >= MAX_RETRYABLE_LIVE_STREAM_RETRIES
                    || (empty_retry
                        && !allow_empty_completion_retry(
                            attempt,
                            started_at.elapsed(),
                            delay.wait_ms,
                        ))
                {
                    cleanup.abort();
                    if empty_retry {
                        log_empty_completion_retry(&ctx, attempt, started_at.elapsed(), None);
                        return map_codex_error_to_response(&empty_live_completion_error());
                    }
                    return exhausted_recovery_response(&ctx, attempt, &err);
                }
                if delay.exceeds_budget {
                    cleanup.abort();
                    return exhausted_recovery_response(&ctx, attempt, &err);
                }
                recovery::notify(
                    &ctx,
                    "reconnecting",
                    attempt + 1,
                    "Connection interrupted; reconnecting",
                );
                retry_started.get_or_insert_with(Instant::now);
                attempt += 1;
                let remaining =
                    EMPTY_COMPLETION_RETRY_BUDGET.saturating_sub(retry_started.unwrap().elapsed());
                let _ = tokio::time::timeout(remaining, sleep(delay.wait_ms)).await;
                continue;
            }
            Err(err) => {
                cleanup.abort();
                return map_codex_error_to_response(&err);
            }
        };

        let start_stream = live_stream_response_once(
            LiveRecovery {
                client: client.clone(),
                attempts: attempt,
                deadline: retry_started.map(|at| at + EMPTY_COMPLETION_RETRY_BUDGET),
                websocket: transport != config::CodexTransport::Http,
            },
            upstream_events,
            message_id.clone(),
            &model,
            ctx.clone(),
            request_continuation.clone(),
            request_body.clone(),
            compaction.clone(),
        );
        let started = match await_empty_retry(
            start_stream,
            empty_retry || retry_started.is_some(),
            if empty_retry {
                started_at
            } else {
                retry_started.unwrap_or(started_at)
            },
        )
        .await
        {
            Ok(started) => started,
            Err(()) => {
                if empty_retry {
                    log_empty_completion_retry(&ctx, attempt, started_at.elapsed(), None);
                }
                cleanup.abort();
                return if empty_retry {
                    map_codex_error_to_response(&empty_live_completion_error())
                } else {
                    exhausted_recovery_response(
                        &ctx,
                        attempt,
                        &connection_reset_error(
                            "Connection recovery exceeded its 60-second budget",
                        ),
                    )
                };
            }
        };
        match started {
            LiveStreamStart::Response(response) => {
                if attempt > 0 {
                    recovery::notify(
                        &ctx,
                        if response.status().is_success() {
                            "restored"
                        } else {
                            "failed"
                        },
                        attempt,
                        if response.status().is_success() {
                            "Connection restored; continuing"
                        } else {
                            "Recovery failed"
                        },
                    );
                }
                cleanup.disarm();
                return response;
            }
            LiveStreamStart::Retry {
                error,
                full_context_retry_attempted,
            } => {
                if error.detail.as_deref() == Some("operation_provenance_changed")
                    && compaction.portable_request.is_some()
                    && !threads::replaying()
                {
                    cleanup.abort();
                    request_body = compaction
                        .portable_request
                        .take()
                        .expect("portable fallback checked");
                    compaction.attempt = None;
                    request_continuation = ContinuationReservation::for_owner_turn(None, None);
                    continuation = None;
                    continue;
                }
                // The incremental HTTP reader performs its own bounded
                // pre-semantic retries so it can stop immediately when the
                // consumer disappears. Do not multiply that exhausted retry
                // loop by the provider-level WebSocket retry policy.
                if error.origin == client::CodexErrorOrigin::Http {
                    cleanup.abort();
                    return exhausted_recovery_response(&ctx, attempt, &error);
                }
                let dropped = drop_live_continuation_for_retry(&mut continuation);
                if full_context_retry_attempted && client::is_continuation_retry_error(&error) {
                    cleanup.abort();
                    return exhausted_recovery_response(&ctx, attempt, &error);
                }
                if dropped && is_missing_previous_response_error(&error) {
                    attempt += 1;
                    continue;
                }
                let empty = error.detail.as_deref() == Some(EMPTY_CODEX_COMPLETION_DETAIL);
                empty_retry |= empty;
                let delay = compute_backoff_delay(attempt, error.retry_after.as_deref());
                if attempt >= MAX_RETRYABLE_LIVE_STREAM_RETRIES
                    || (empty_retry
                        && !allow_empty_completion_retry(
                            attempt,
                            started_at.elapsed(),
                            delay.wait_ms,
                        ))
                {
                    if empty_retry {
                        log_empty_completion_retry(&ctx, attempt, started_at.elapsed(), None);
                        cleanup.abort();
                        return map_codex_error_to_response(&empty_live_completion_error());
                    }
                    cleanup.abort();
                    return exhausted_recovery_response(&ctx, attempt, &error);
                }
                if delay.exceeds_budget {
                    cleanup.abort();
                    return exhausted_recovery_response(&ctx, attempt, &error);
                }
                if empty {
                    log_empty_completion_retry(
                        &ctx,
                        attempt,
                        started_at.elapsed(),
                        Some(delay.wait_ms),
                    );
                }
                recovery::notify(
                    &ctx,
                    "reconnecting",
                    attempt + 1,
                    "Response interrupted; retrying before output",
                );
                retry_started.get_or_insert_with(Instant::now);
                attempt += 1;
                let remaining =
                    EMPTY_COMPLETION_RETRY_BUDGET.saturating_sub(retry_started.unwrap().elapsed());
                let _ = tokio::time::timeout(remaining, sleep(delay.wait_ms)).await;
            }
        }
    }
}

// Bound replay work only. Once real output is forwarded, this wrapper is gone
// and the response stream can run normally without replaying tools.
async fn await_empty_retry<T>(
    future: impl std::future::Future<Output = T>,
    retry: bool,
    started_at: Instant,
) -> Result<T, ()> {
    if !retry {
        return Ok(future.await);
    }
    let remaining = EMPTY_COMPLETION_RETRY_BUDGET
        .checked_sub(started_at.elapsed())
        .ok_or(())?;
    tokio::time::timeout(remaining, future)
        .await
        .map_err(|_| ())
}

// Do not restart a long generation after its retry allowance is already spent.
// The initial attempt is not timed out: legitimate reasoning may take time.
fn allow_empty_completion_retry(attempt: u32, elapsed: Duration, wait_ms: u64) -> bool {
    attempt < MAX_EMPTY_COMPLETION_RETRIES
        && elapsed + Duration::from_millis(wait_ms) < EMPTY_COMPLETION_RETRY_BUDGET
}

fn log_empty_completion_retry(
    ctx: &RequestContext,
    attempt: u32,
    elapsed: Duration,
    delay: Option<u64>,
) {
    create_logger("codex").warn(
        if delay.is_some() {
            "empty_completion_retry"
        } else {
            "empty_completion_retry_exhausted"
        },
        Some(serde_json::Map::from_iter([
            ("reqId".into(), serde_json::json!(ctx.req_id)),
            ("attempts".into(), serde_json::json!(attempt + 1)),
            (
                "maxAttempts".into(),
                serde_json::json!(MAX_EMPTY_COMPLETION_RETRIES + 1),
            ),
            ("elapsedMs".into(), serde_json::json!(elapsed.as_millis())),
            ("delayMs".into(), serde_json::json!(delay)),
        ])),
    );
}

fn provider_retry(
    upstream_events: &websocket::CodexWebSocketEventStream,
    error: client::CodexError,
) -> LiveStreamStart {
    let full_context_retry_attempted = upstream_events.used_full_context_retry();
    upstream_events.mark_provider_retry_handoff();
    LiveStreamStart::Retry {
        error,
        full_context_retry_attempted,
    }
}

struct LiveRecovery {
    client: Arc<CodexHttpClient>,
    attempts: u32,
    deadline: Option<Instant>,
    websocket: bool,
}

#[allow(clippy::too_many_arguments)]
async fn live_stream_response_once(
    reconnect: LiveRecovery,
    mut upstream_events: websocket::CodexWebSocketEventStream,
    message_id: String,
    model: &str,
    ctx: RequestContext,
    request_continuation: ContinuationReservation,
    request_body: Arc<translate::request::ResponsesRequest>,
    compaction: LiveStreamCompaction,
) -> LiveStreamStart {
    let estimated_input_tokens = count_translated_tokens(&request_body);
    let mut translator = LiveStreamTranslator::with_estimated_input_tokens(
        message_id,
        model.to_string(),
        estimated_input_tokens,
    )
    .with_stop_sequences(&request_body.stop_sequences);
    let mut upstream_sse_body = (request_continuation.turn_id().is_some()
        || compaction.attempt.is_some()
        || threads::current().is_some())
    .then(Vec::new);
    // Keep protocol framing private until real output makes a transparent retry unsafe.
    // Every branch that consumes pending_chunk returns, so it is never flushed twice.
    let mut pending_chunk = Vec::new();
    let mut generation_started = false;

    while let Some(item) = upstream_events.recv().await {
        let payload = match item {
            Ok(payload) => payload,
            Err(err) => {
                if err.detail.as_deref() == Some("operation_provenance_changed") {
                    return provider_retry(&upstream_events, err);
                }
                if err.origin == client::CodexErrorOrigin::Http {
                    abort_request_state(
                        compaction_scope(&request_continuation).as_ref(),
                        &request_continuation,
                        compaction.attempt,
                    );
                    return LiveStreamStart::Response(map_codex_error_to_response(&err));
                }
                if retryable_live_start_codex_error(&err) {
                    return provider_retry(&upstream_events, err);
                }
                abort_request_state(
                    compaction_scope(&request_continuation).as_ref(),
                    &request_continuation,
                    compaction.attempt,
                );
                return LiveStreamStart::Response(map_codex_error_to_response(&err));
            }
        };
        if !generation_started && codex_generation_event(&payload) {
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.generation_started(&ctx.req_id);
            }
            generation_started = true;
        }
        append_upstream_sse_payload(
            &mut upstream_sse_body,
            &payload,
            &ctx,
            &request_continuation,
            compaction.attempt,
        );
        let (chunk, terminal) = match translate_live_stream_payload(&mut translator, &payload, None)
        {
            Ok(result) => result,
            Err(message) => {
                // A spent subscription window reopens hours from now, so the
                // retry budget can only burn the request down to the same 429.
                // Report it once, with the reset clock upstream supplied.
                if let Some(limit) = events::usage_limit_from_event(&payload) {
                    abort_request_state(
                        compaction_scope(&request_continuation).as_ref(),
                        &request_continuation,
                        compaction.attempt,
                    );
                    return LiveStreamStart::Response(usage_limit_response(&limit));
                }
                if let Some(failure) = events::classify_event_failure(&payload) {
                    if failure.retryable() {
                        return provider_retry(
                            &upstream_events,
                            codex_event_failure_error(
                                &failure,
                                client::CodexErrorOrigin::WebSocket,
                            ),
                        );
                    }
                    abort_request_state(
                        compaction_scope(&request_continuation).as_ref(),
                        &request_continuation,
                        compaction.attempt,
                    );
                    return LiveStreamStart::Response(map_codex_event_failure_to_response(
                        &failure,
                    ));
                }
                abort_request_state(
                    compaction_scope(&request_continuation).as_ref(),
                    &request_continuation,
                    compaction.attempt,
                );
                return LiveStreamStart::Response(map_codex_failure_to_response(&message));
            }
        };
        pending_chunk.extend_from_slice(&chunk);
        if terminal
            && events::event_is_success_terminal(&payload)
            && !events::response_is_incomplete_terminal(&payload)
            && !translator.has_semantic_output()
            && translator.matched_stop_sequence().is_none()
        {
            // Record shape only; never log prompt or generated content by default.
            create_logger("codex").warn(
                "empty_completion_received",
                Some(serde_json::Map::from_iter([
                    ("reqId".into(), serde_json::json!(ctx.req_id)),
                    ("model".into(), serde_json::json!(model)),
                    ("eventType".into(), serde_json::json!(payload.get("type"))),
                    (
                        "status".into(),
                        serde_json::json!(payload.pointer("/response/status")),
                    ),
                    (
                        "outputItems".into(),
                        serde_json::json!(
                            payload
                                .pointer("/response/output")
                                .and_then(|v| v.as_array())
                                .map(Vec::len)
                        ),
                    ),
                    (
                        "outputTokens".into(),
                        serde_json::json!(payload.pointer("/response/usage/output_tokens")),
                    ),
                ])),
            );
            return provider_retry(&upstream_events, empty_live_completion_error());
        }
        if translator.has_semantic_output() && !pending_chunk.is_empty() {
            record_live_stream_downstream_capture(&ctx, &pending_chunk);
            record_live_stream_progress(&ctx, &pending_chunk);
            if terminal {
                log_live_stream_completion(&ctx, &upstream_events, &request_continuation);
                update_continuation_from_upstream(
                    compaction_scope(&request_continuation).as_ref(),
                    &request_continuation,
                    compaction.attempt,
                    &request_body,
                    upstream_sse_body.as_deref().unwrap_or_default(),
                    upstream_events.socket_id(),
                    upstream_events.provenance().as_ref(),
                    compaction.compact_boundary,
                );
                return LiveStreamStart::Response(single_live_stream_response(pending_chunk));
            }
            return LiveStreamStart::Response(remaining_live_stream_response(
                reconnect,
                upstream_events,
                translator,
                pending_chunk,
                ctx,
                request_continuation,
                request_body,
                upstream_sse_body,
                compaction,
            ));
        }
        if terminal {
            log_live_stream_completion(&ctx, &upstream_events, &request_continuation);
            update_continuation_from_upstream(
                compaction_scope(&request_continuation).as_ref(),
                &request_continuation,
                compaction.attempt,
                &request_body,
                upstream_sse_body.as_deref().unwrap_or_default(),
                upstream_events.socket_id(),
                upstream_events.provenance().as_ref(),
                compaction.compact_boundary,
            );
            if pending_chunk.is_empty() {
                return LiveStreamStart::Response(empty_live_stream_response());
            }
            record_live_stream_downstream_capture(&ctx, &pending_chunk);
            record_live_stream_progress(&ctx, &pending_chunk);
            return LiveStreamStart::Response(single_live_stream_response(pending_chunk));
        }
    }

    provider_retry(
        &upstream_events,
        client::CodexError {
            status: 0,
            message: "WebSocket connection closed before terminal Codex response event".to_string(),
            detail: Some(websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: client::CodexErrorOrigin::WebSocket,
        },
    )
}

fn empty_live_completion_error() -> client::CodexError {
    client::CodexError {
        status: 503,
        message: "Codex completed without producing output".to_string(),
        detail: Some(EMPTY_CODEX_COMPLETION_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: client::CodexErrorOrigin::WebSocket,
    }
}

fn codex_generation_event(payload: &serde_json::Value) -> bool {
    !matches!(
        payload.get("type").and_then(|value| value.as_str()),
        Some("codex.rate_limits" | "keepalive") | None
    )
}

fn translate_live_stream_payload(
    translator: &mut LiveStreamTranslator,
    payload: &serde_json::Value,
    traffic: Option<&crate::traffic::TrafficCapture>,
) -> Result<(Vec<u8>, bool), String> {
    let chunk = translator.accept(payload, traffic)?;
    let terminal = events::event_is_terminal(payload) || translator.is_finished();
    Ok((chunk, terminal))
}

fn log_live_stream_completion(
    ctx: &RequestContext,
    events: &websocket::CodexWebSocketEventStream,
    continuation: &ContinuationReservation,
) {
    create_logger("codex").info(
        "codex_upstream_response_received",
        Some(serde_json::Map::from_iter([
            ("reqId".into(), serde_json::json!(ctx.req_id)),
            (
                "hasReusableSocket".into(),
                serde_json::json!(events.socket_id().is_some()),
            ),
            (
                "continuationSocketReused".into(),
                serde_json::json!(
                    continuation.origin_socket_id().is_some()
                        && continuation.origin_socket_id() == events.socket_id()
                ),
            ),
            (
                "fullContextRetry".into(),
                serde_json::json!(events.used_full_context_retry()),
            ),
        ])),
    );
}

fn record_live_stream_downstream_capture(ctx: &RequestContext, chunk: &[u8]) {
    let Some(traffic) = ctx.traffic.as_ref() else {
        return;
    };
    for event in parse_sse_events(chunk) {
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&event.data) else {
            continue;
        };
        traffic.write_json_event(
            "050-downstream-event",
            &serde_json::json!({
                "event": event.event.as_deref().unwrap_or("message"),
                "data": data,
            }),
        );
    }
}

fn record_live_stream_progress(ctx: &RequestContext, chunk: &[u8]) {
    if let Some(monitor) = ctx.monitor.as_ref() {
        monitor.stream_progress(
            &ctx.req_id,
            chunk.len() as u64,
            count_sse_events(chunk),
            TokenUsage::default(),
        );
    }
}

fn single_live_stream_response(chunk: Vec<u8>) -> Response {
    event_stream_response(
        futures_util::stream::once(async move { Ok::<Bytes, std::io::Error>(Bytes::from(chunk)) }),
        NativeResponseOutcome::default(),
    )
}

fn empty_live_stream_response() -> Response {
    event_stream_response(
        futures_util::stream::empty::<Result<Bytes, std::io::Error>>(),
        NativeResponseOutcome::default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn remaining_live_stream_response(
    reconnect: LiveRecovery,
    mut upstream_events: websocket::CodexWebSocketEventStream,
    mut translator: LiveStreamTranslator,
    first_chunk: Vec<u8>,
    ctx: RequestContext,
    request_continuation: ContinuationReservation,
    request_body: Arc<translate::request::ResponsesRequest>,
    mut upstream_sse_body: Option<Vec<u8>>,
    compaction: LiveStreamCompaction,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let model_context = models::current_model_context();
    let thread_attempt = threads::current();
    let telemetry = rate_limits::current();
    let outcome = NativeResponseOutcome::default();
    let response_outcome = outcome.clone();
    let worker = async move {
        let mut cleanup =
            LiveRequestStateCleanup::new(request_continuation.clone(), compaction.attempt);
        if tx.send(Ok(Bytes::from(first_chunk))).await.is_err() {
            abort_request_state(
                compaction_scope(&request_continuation).as_ref(),
                &request_continuation,
                compaction.attempt,
            );
            return;
        }
        let mut heartbeat = tokio::time::interval(LIVE_STREAM_HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat.tick().await;
        let mut reconnects = reconnect.attempts;
        let mut recovery_deadline = reconnect.deadline;
        let mut restored_pending = false;
        'events: loop {
            let receive = async {
                if let Some(deadline) = recovery_deadline {
                    match tokio::time::timeout_at(
                        tokio::time::Instant::from_std(deadline),
                        upstream_events.recv(),
                    )
                    .await
                    {
                        Ok(item) => item,
                        Err(_) => {
                            Some(Err(connection_reset_error("Connection recovery timed out")))
                        }
                    }
                } else {
                    upstream_events.recv().await
                }
            };
            let item = tokio::select! {
                biased;
                _ = tx.closed() => {
                    abort_request_state(
                        compaction_scope(&request_continuation).as_ref(),
                        &request_continuation,
                        compaction.attempt,
                    );
                    return;
                }
                item = receive => item,
                _ = heartbeat.tick() => {
                    let chunk = translator.ping_chunk(ctx.traffic.as_deref());
                    if !translator.can_reconnect() {
                        recovery_deadline = None;
                    }
                    if !chunk.is_empty() {
                        record_live_stream_progress(&ctx, &chunk);
                        if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                            abort_request_state(
                                compaction_scope(&request_continuation).as_ref(),
                                &request_continuation,
                                compaction.attempt,
                            );
                            return;
                        }
                    }
                    continue;
                }
            };
            let item = item.unwrap_or_else(|| {
                Err(connection_reset_error(
                    "WebSocket connection closed before terminal response",
                ))
            });
            let item = match item {
                Ok(payload) if reconnect.websocket && translator.can_reconnect() => {
                    match events::classify_event_failure(&payload)
                        .filter(|failure| failure.retryable())
                    {
                        Some(failure) => Err(codex_event_failure_error(
                            &failure,
                            client::CodexErrorOrigin::WebSocket,
                        )),
                        None => Ok(payload),
                    }
                }
                other => other,
            };
            match item {
                Ok(payload) => {
                    if restored_pending {
                        recovery::notify(
                            &ctx,
                            "restored",
                            reconnects,
                            "Connection restored; continuing",
                        );
                        restored_pending = false;
                    }
                    append_upstream_sse_payload(
                        &mut upstream_sse_body,
                        &payload,
                        &ctx,
                        &request_continuation,
                        compaction.attempt,
                    );
                    let (chunk, terminal) = match translate_live_stream_payload(
                        &mut translator,
                        &payload,
                        ctx.traffic.as_deref(),
                    ) {
                        Ok(result) => result,
                        Err(message) => {
                            abort_request_state(
                                compaction_scope(&request_continuation).as_ref(),
                                &request_continuation,
                                compaction.attempt,
                            );
                            let failure = events::classify_event_failure(&payload);
                            let error_type = failure
                                .as_ref()
                                .map_or("api_error", events::CodexEventFailure::client_error_type);
                            let error_message = failure
                                .as_ref()
                                .map_or(message.as_str(), |failure| failure.message.as_str());
                            outcome.fail(error_message.to_string());
                            let chunk = translator.error_chunk(
                                error_message,
                                error_type,
                                ctx.traffic.as_deref(),
                            );
                            if !chunk.is_empty() {
                                record_live_stream_progress(&ctx, &chunk);
                                let _ = tx.send(Ok(Bytes::from(chunk))).await;
                            }
                            return;
                        }
                    };
                    if !chunk.is_empty() {
                        record_live_stream_progress(&ctx, &chunk);
                        if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                            abort_request_state(
                                compaction_scope(&request_continuation).as_ref(),
                                &request_continuation,
                                compaction.attempt,
                            );
                            return;
                        }
                    }
                    if terminal {
                        log_live_stream_completion(&ctx, &upstream_events, &request_continuation);
                        update_continuation_from_upstream(
                            compaction_scope(&request_continuation).as_ref(),
                            &request_continuation,
                            compaction.attempt,
                            &request_body,
                            upstream_sse_body.as_deref().unwrap_or_default(),
                            upstream_events.socket_id(),
                            upstream_events.provenance().as_ref(),
                            compaction.compact_boundary,
                        );
                        cleanup.disarm();
                        return;
                    }
                }
                Err(mut err) => {
                    if reconnect.websocket
                        && err.origin == client::CodexErrorOrigin::WebSocket
                        && retryable_live_start_codex_error(&err)
                        && translator.can_reconnect()
                    {
                        let deadline = *recovery_deadline
                            .get_or_insert_with(|| Instant::now() + EMPTY_COMPLETION_RETRY_BUDGET);
                        upstream_events.mark_provider_retry_handoff();
                        let chunk = translator.prepare_reconnect(ctx.traffic.as_deref());
                        if !chunk.is_empty() && tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                            abort_request_state(
                                compaction_scope(&request_continuation).as_ref(),
                                &request_continuation,
                                compaction.attempt,
                            );
                            return;
                        }
                        while reconnects < MAX_RETRYABLE_LIVE_STREAM_RETRIES
                            && Instant::now() < deadline
                        {
                            let delay =
                                compute_backoff_delay(reconnects, err.retry_after.as_deref());
                            if delay.exceeds_budget
                                || Duration::from_millis(delay.wait_ms)
                                    >= deadline.saturating_duration_since(Instant::now())
                            {
                                break;
                            }
                            reconnects += 1;
                            recovery::notify(
                                &ctx,
                                "reconnecting",
                                reconnects,
                                "Connection interrupted during thinking; reconnecting",
                            );
                            let retry_continuation = request_continuation.full_context_retry();
                            rate_limits::record(Vec::new());
                            let open_connection = async {
                                sleep(delay.wait_ms).await;
                                reconnect
                                    .client
                                    .stream_codex_websocket_events_for_owner(
                                        request_body.clone(),
                                        &ctx,
                                        Some(&retry_continuation),
                                    )
                                    .await
                            };
                            let result = tokio::select! {
                                biased;
                                _ = tx.closed() => {
                                    abort_request_state(compaction_scope(&request_continuation).as_ref(), &request_continuation, compaction.attempt);
                                    return;
                                }
                                result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), open_connection) => result,
                            };
                            match result {
                                Ok(Ok(events)) => {
                                    upstream_events = events;
                                    if let Some(body) = upstream_sse_body.as_mut() {
                                        body.clear();
                                    }
                                    restored_pending = true;
                                    continue 'events;
                                }
                                Ok(Err(error)) => {
                                    let retryable = retryable_live_start_codex_error(&error);
                                    err = error;
                                    if !retryable {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    abort_request_state(
                        compaction_scope(&request_continuation).as_ref(),
                        &request_continuation,
                        compaction.attempt,
                    );
                    if err.origin == client::CodexErrorOrigin::WebSocket {
                        let chunk = translator
                            .finish_after_closed_completed_tool_call(ctx.traffic.as_deref());
                        if !chunk.is_empty() {
                            recovery::notify(
                                &ctx,
                                "restored",
                                reconnects,
                                "Connection interrupted; preserving completed tool calls and continuing",
                            );
                            record_live_stream_progress(&ctx, &chunk);
                            let _ = tx.send(Ok(Bytes::from(chunk))).await;
                            return;
                        }
                    }
                    let explanation = if translator.can_reconnect() {
                        "Connection recovery failed. Retry this turn."
                    } else {
                        "Connection interrupted after output began. Automatic replay stopped to avoid duplicate text or tool actions; retry this turn."
                    };
                    recovery::notify(&ctx, "failed", reconnects, explanation);
                    let error_message = format!("{} — {explanation}", codex_error_message(&err));
                    let error_type = codex_stream_error_type(&err);
                    outcome.fail(error_message.clone());
                    let chunk =
                        translator.error_chunk(&error_message, error_type, ctx.traffic.as_deref());
                    if !chunk.is_empty() {
                        record_live_stream_progress(&ctx, &chunk);
                        let _ = tx.send(Ok(Bytes::from(chunk))).await;
                    }
                    return;
                }
            }
        }
    };
    tokio::spawn(rate_limits::with_telemetry(
        telemetry,
        threads::scope(thread_attempt, async move {
            if let Some(context) = model_context {
                models::with_model_context(context, worker).await;
            } else {
                worker.await;
            }
        }),
    ));

    let stream = futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|item| (item, rx))
    });
    event_stream_response(stream, response_outcome)
}

fn exhausted_recovery_response(
    ctx: &RequestContext,
    attempt: u32,
    error: &client::CodexError,
) -> Response {
    recovery::notify(
        ctx,
        "failed",
        attempt,
        "Connection recovery stopped; retry this turn",
    );
    let mut response = map_codex_error_to_response(error);
    response
        .headers_mut()
        .insert("x-should-retry", http::HeaderValue::from_static("false"));
    response
}

fn connection_reset_error(message: &str) -> client::CodexError {
    client::CodexError {
        status: 0,
        message: message.to_owned(),
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: client::CodexErrorOrigin::WebSocket,
    }
}

fn append_upstream_sse_payload(
    buffer: &mut Option<Vec<u8>>,
    payload: &serde_json::Value,
    ctx: &RequestContext,
    continuation: &ContinuationReservation,
    attempt: Option<CompactionAttempt>,
) {
    let Some(body) = buffer.as_mut() else {
        return;
    };
    if matches!(
        payload.get("type").and_then(|value| value.as_str()),
        Some("keepalive" | "codex.rate_limits")
    ) {
        return;
    }
    let bytes = request_summary::serialized_json_bytes(payload);
    // Native metadata is optional. Never let its retention limit cut the answer.
    if bytes.saturating_add(8)
        > continuation::MAX_OWNER_TRANSCRIPT_BYTES.saturating_sub(body.len() as u64)
    {
        *buffer = None;
        abort_request_state(
            compaction_scope(continuation).as_ref(),
            continuation,
            attempt,
        );
        create_logger("codex").warn(
            "codex_native_metadata_disabled",
            Some(serde_json::Map::from_iter([
                ("reqId".into(), serde_json::json!(ctx.req_id)),
                ("reason".into(), serde_json::json!("transcript_overflow")),
            ])),
        );
        return;
    }
    body.extend_from_slice(b"data: ");
    serde_json::to_writer(&mut *body, payload).expect("JSON value serializes to a byte buffer");
    body.extend_from_slice(b"\n\n");
}

pub(super) fn record_upstream_telemetry(ctx: &RequestContext, payload: &serde_json::Value) {
    if payload.get("type").and_then(|value| value.as_str()) == Some("codex.rate_limits") {
        rate_limits::record(rate_limits::event_headers(payload));
    }
    if events::event_is_terminal(payload)
        && let Some(usage) = payload.pointer("/response/usage")
        && let Some(monitor) = ctx.monitor.as_ref()
    {
        monitor.usage_updated(&ctx.req_id, TokenUsage::from_codex(usage));
    }
}

fn event_stream_response<S>(stream: S, outcome: NativeResponseOutcome) -> Response
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let headers = [
        (http::header::CONTENT_TYPE, "text/event-stream"),
        (http::header::CACHE_CONTROL, "no-cache"),
        (http::header::CONNECTION, "keep-alive"),
    ];
    let mut response = (headers, Body::from_stream(stream)).into_response();
    response.extensions_mut().insert(outcome);
    response
}

fn empty_buffered_completion_error() -> client::CodexError {
    client::CodexError {
        status: 503,
        message: "Codex completed without producing output".to_string(),
        detail: Some(EMPTY_CODEX_COMPLETION_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: match config::codex_transport() {
            config::CodexTransport::Http => client::CodexErrorOrigin::BufferedHttp,
            _ => client::CodexErrorOrigin::BufferedWebSocket,
        },
    }
}

/// True when the buffered upstream body ended in a successful terminal event
/// without ever producing semantic output (text, thinking, tool, web search).
fn is_empty_codex_success_completion(events: &[ReducerEvent]) -> bool {
    use self::translate::reducer::{TERM_COMPLETED, TERM_DONE};

    let mut saw_success_terminal = false;
    for event in events {
        match event {
            ReducerEvent::TextDelta { text, .. } if !text.is_empty() => return false,
            ReducerEvent::ThinkingStart { .. }
            | ReducerEvent::ToolStart { .. }
            | ReducerEvent::WebSearch { .. } => return false,
            ReducerEvent::Finish {
                terminal_type,
                stop_reason,
                ..
            } if *stop_reason == translate::reducer::STOP_END_TURN
                && (terminal_type == TERM_COMPLETED || terminal_type == TERM_DONE) =>
            {
                saw_success_terminal = true;
            }
            _ => {}
        }
    }
    saw_success_terminal
}

fn retryable_live_start_codex_error(err: &client::CodexError) -> bool {
    if err.origin == client::CodexErrorOrigin::WebSocketHandshake {
        if err.detail.as_deref() == Some(websocket::WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL) {
            return false;
        }
        return err.status == 0 || matches!(err.status, 429 | 500 | 502 | 503 | 504 | 529);
    }
    if err.detail.as_deref() == Some(websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL) {
        return true;
    }
    matches!(err.status, 429 | 500 | 502 | 503 | 504 | 529)
        || (err.status == 0 && retryable_live_message(codex_error_message(err)))
}

fn is_missing_previous_response_error(err: &client::CodexError) -> bool {
    matches!(
        err.detail.as_deref(),
        Some("previous_response_not_found")
            | Some(websocket::WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL)
    )
}

fn drop_live_continuation_for_retry(continuation: &mut Option<ContinuationReservation>) -> bool {
    if continuation
        .as_ref()
        .and_then(|reservation| reservation.candidate().previous_response_id.as_deref())
        .is_none()
    {
        return false;
    }

    if let Some(reservation) = continuation.as_ref() {
        *continuation = Some(reservation.full_context_retry());
    }
    true
}

fn retryable_live_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "overloaded",
        "rate limit",
        "you can retry your request",
        "temporarily unavailable",
        "timed out",
        "connection closed",
        "connection reset",
        "no route to host",
        "broken pipe",
        "epipe",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn codex_event_failure_error(
    failure: &events::CodexEventFailure,
    origin: client::CodexErrorOrigin,
) -> client::CodexError {
    client::CodexError {
        status: failure.status,
        message: failure.message.clone(),
        detail: Some(failure.message.clone()),
        retry_after: failure.retry_after.clone(),
        usage_limit: None,
        origin,
    }
}

fn codex_stream_error_type(err: &client::CodexError) -> &'static str {
    match err.status {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ if codex_error_message(err)
            .to_lowercase()
            .contains("overloaded") =>
        {
            "overloaded_error"
        }
        _ => "api_error",
    }
}

#[allow(clippy::too_many_arguments)]
fn update_continuation_from_upstream(
    scope: Option<&CompactionScope>,
    continuation: &ContinuationReservation,
    compaction_attempt: Option<CompactionAttempt>,
    request_body: &translate::request::ResponsesRequest,
    upstream_body: &[u8],
    socket_id: Option<u64>,
    actual_provenance: Option<&models::OperationProvenance>,
    compact_boundary: bool,
) {
    let finish = if threads::current().is_some() {
        accumulate_response_and_metadata_with_traffic(
            reduce_upstream_bytes_with_stops(upstream_body, &request_body.stop_sequences),
            "",
            &request_body.model,
            None,
        )
        .ok()
        .and_then(|(response, finish)| {
            threads::settle(&response, finish.as_ref(), request_body, actual_provenance);
            finish
        })
    } else {
        finish_metadata_from_upstream_with_stops(upstream_body, &request_body.stop_sequences)
            .ok()
            .flatten()
    };
    update_continuation_from_finish(
        scope,
        continuation,
        compaction_attempt,
        request_body,
        finish.as_ref(),
        socket_id,
        actual_provenance,
        compact_boundary,
    );
}

#[allow(clippy::too_many_arguments)]
fn update_continuation_from_finish(
    scope: Option<&CompactionScope>,
    continuation: &ContinuationReservation,
    compaction_attempt: Option<CompactionAttempt>,
    request_body: &translate::request::ResponsesRequest,
    finish: Option<&translate::reducer::FinishMetadata>,
    socket_id: Option<u64>,
    actual_provenance: Option<&models::OperationProvenance>,
    compact_boundary: bool,
) {
    if actual_provenance.is_none() || actual_provenance != continuation.provenance() {
        abort_request_state(scope, continuation, compaction_attempt);
        return;
    }
    match finish {
        Some(finish) if finish.continuation_eligible => {
            if compact_boundary {
                activate_compaction(
                    scope,
                    compaction_attempt,
                    &request_body.model,
                    &finish.output_items,
                );
            }
            record_continuation_for_owner(
                continuation,
                request_body,
                finish.response_id.as_deref(),
                socket_id,
                &finish.output_items,
            );
        }
        _ => {
            abort_compaction_attempt(scope, compaction_attempt);
            abort_continuation_for_owner(continuation);
        }
    }
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Answer a spent subscription window with the rate limit headers the client
/// reads, so it can name the exhausted window and show when it reopens.
///
/// `Retry-After` is deliberately absent: clients sleep for its full value, and
/// here that is hours. `x-should-retry: false` stops the retry loop instead,
/// which is the honest signal — a spent window does not reopen on a backoff.
fn usage_limit_response(limit: &events::CodexUsageLimit) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-should-retry"),
        HeaderValue::from_static("false"),
    );
    headers.insert(
        HeaderName::from_static("anthropic-ratelimit-unified-status"),
        HeaderValue::from_static("rejected"),
    );
    if let Some(resets_at) = limit.resets_at
        && let Ok(value) = HeaderValue::from_str(&resets_at.to_string())
    {
        headers.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-reset"),
            value,
        );
    }
    if let Some(window) = limit.window {
        headers.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-representative-claim"),
            HeaderValue::from_static(window.claim()),
        );
    }

    (
        headers,
        json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            &limit.message,
        ),
    )
        .into_response()
}

fn map_codex_error_to_response(err: &client::CodexError) -> Response {
    if let Some(limit) = err.usage_limit.as_ref() {
        return usage_limit_response(limit);
    }
    let message = codex_error_message(err);
    if is_context_window_overflow(message) {
        return map_codex_failure_to_response(message);
    }
    if err.detail.as_deref() == Some(EMPTY_CODEX_COMPLETION_DETAIL) {
        // The proxy has already spent its own retry budget on this request, so
        // a client retry only multiplies it: Claude Code resends a 503 up to
        // ten times, each one repeating the proxy's full retry loop.
        return (
            [(
                HeaderName::from_static("x-should-retry"),
                HeaderValue::from_static("false"),
            )],
            json_error(StatusCode::SERVICE_UNAVAILABLE, "api_error", &err.message),
        )
            .into_response();
    }

    match err.status {
        401 => json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            err.detail.as_deref().unwrap_or("Authentication failed"),
        ),
        403 => json_error(
            StatusCode::FORBIDDEN,
            "permission_error",
            err.detail.as_deref().unwrap_or("Permission denied"),
        ),
        400 | 422 => {
            let response = json_error(
                StatusCode::from_u16(err.status).unwrap_or(StatusCode::BAD_REQUEST),
                "invalid_request_error",
                codex_error_message(err),
            );
            if let Some(retry_after) = err.retry_after.as_deref() {
                ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
            } else {
                response
            }
        }
        413 => {
            let response = json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                codex_error_message(err),
            );
            if let Some(retry_after) = err.retry_after.as_deref() {
                ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
            } else {
                response
            }
        }
        429 => {
            let response = json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                &err.message,
            );
            if let Some(retry_after) = err.retry_after.as_deref() {
                ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
            } else {
                response
            }
        }
        status @ (400..=599) => {
            let response = json_error(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                if status == 529 {
                    "overloaded_error"
                } else {
                    "api_error"
                },
                codex_error_message(err),
            );
            if let Some(retry_after) = err.retry_after.as_deref() {
                ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
            } else {
                response
            }
        }
        _ => json_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            codex_error_message(err),
        ),
    }
}

fn map_codex_failure_to_response(message: &str) -> Response {
    if is_context_window_overflow(message) {
        json_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", message)
    } else {
        json_error(StatusCode::BAD_GATEWAY, "api_error", message)
    }
}

fn map_codex_event_failure_to_response(failure: &events::CodexEventFailure) -> Response {
    let status = StatusCode::from_u16(failure.client_status()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = json_error(status, failure.client_error_type(), &failure.message);
    if !failure.retryable() {
        response
            .headers_mut()
            .insert("x-should-retry", HeaderValue::from_static("false"));
    }
    if let Some(retry_after) = failure.retry_after.as_deref() {
        ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
    } else {
        response
    }
}

fn is_context_window_overflow(message: &str) -> bool {
    message.to_ascii_lowercase().contains("context window")
}

fn codex_error_message(err: &client::CodexError) -> &str {
    if err.status == 0 {
        err.message.as_str()
    } else {
        err.detail.as_deref().unwrap_or("Upstream error")
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub(crate) struct CodexCli;

impl CliHandlers for CodexCli {
    fn login(&self) -> Result<(), anyhow::Error> {
        let tokens = run_browser_login()?;
        let store = file_store();
        let manager = CodexAuthManager::new(store);
        let saved = manager.persist_initial_tokens(&tokens)?;
        print!(
            "{}",
            format_auth_saved_output(&manager.store.auth_path(), saved.account_id.as_deref())
        );
        Ok(())
    }

    fn device(&self) -> Result<(), anyhow::Error> {
        let tokens = DeviceAuthClient::new().run()?;
        let store = file_store();
        let manager = CodexAuthManager::new(store);
        let saved = manager.persist_initial_tokens(&tokens)?;
        print!(
            "{}",
            format_auth_saved_output(&manager.store.auth_path(), saved.account_id.as_deref())
        );
        Ok(())
    }

    fn status(&self) -> Result<(), anyhow::Error> {
        let store = file_store();
        let stored = store.load_auth()?;
        match stored {
            Some(auth) => {
                println!(
                    "Account: {}",
                    auth.account_id.as_deref().unwrap_or("(none)")
                );
                println!("{}", format_expiry(auth.expires, now_ms()));
                println!("Storage: {}", store.auth_path());
                Ok(())
            }
            None => {
                anyhow::bail!("Not authenticated");
            }
        }
    }

    fn logout(&self) -> Result<(), anyhow::Error> {
        let store = file_store();
        store.clear_auth()?;
        println!("Logged out");
        Ok(())
    }
}

pub(crate) static CODEX_CLI: CodexCli = CodexCli;

// ---------------------------------------------------------------------------
// CLI helpers
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn format_expiry(expires: u64, now: u64) -> String {
    let remaining = (i128::from(expires) - i128::from(now)).div_euclid(1000);
    let iso = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(expires) * 1_000_000)
        .ok()
        .and_then(|dt| {
            let fmt = time::format_description::parse_borrowed::<2>(
                "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
            )
            .ok()?;
            dt.format(&fmt).ok()
        })
        .unwrap_or_else(|| "invalid".to_string());
    format!("Expires: {iso} (in {remaining}s)")
}

fn format_auth_saved_output(auth_path: &str, account_id: Option<&str>) -> String {
    let mut out = format!("Auth saved in {auth_path}\n");
    if let Some(account_id) = account_id {
        out.push_str(&format!("Account: {account_id}\n"));
    }
    out
}
