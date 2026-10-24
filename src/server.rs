use crate::{
    anthropic::{MAX_ANTHROPIC_REQUEST_BYTES, json_error},
    logging::{Logger, REDACT_KEYS, create_logger},
    monitor::{EndpointKind, MonitorHandle, bounded_error_message},
    openai_compat::{MAX_OPENAI_REQUEST_BYTES, OpenAiError, request::extract_model},
    project,
    provider::RequestContext,
    providers::codex::{
        chat_completions::{ChatCompletionsBackend, request::translate_request},
        client::CodexHttpClient,
        images::{
            CodexImagesBackend, ImageOperation, ImageRequestError, MAX_EDIT_REQUEST_BYTES,
            MAX_GENERATION_REQUEST_BYTES, MultipartEditInput, UploadedImage, image_error_response,
            prepare_json_request, prepare_multipart_edit,
        },
        models,
        native::{
            CodexNativeBackend, NativeResponseOutcome, openai_error, validate_native_request_model,
        },
        transcription::{
            CodexTranscriptionBackend, MAX_TRANSCRIPTION_REQUEST_BYTES, TranscriptionRequestError,
            prepare_transcription, transcription_error_response,
        },
    },
    registry::Registry,
    request_identity::{CLAUDE_AGENT_HEADER, CLAUDE_PARENT_AGENT_HEADER, ConversationIdentity},
    session,
    traffic::{TrafficCaptureOptions, create_traffic_capture, redacted, traffic_capture_enabled},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Query, State},
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use http_body_util::{BodyExt, StreamBody};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::future::{Future, IntoFuture};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use uuid::Uuid;

const CLAUDE_AUTO_REVIEW_SYSTEM_PREFIX: &str =
    "You are a security monitor for autonomous AI coding agents.";
const CODEX_AUTO_REVIEW_MODEL: &str = "gpt-6-luna";
const MAX_ERROR_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_ERROR_CAPTURE_FRAMES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
struct AutoReviewRoute {
    requested_model: String,
    override_model: String,
}

fn is_claude_auto_review_request(body: &crate::anthropic::schema::MessagesRequest) -> bool {
    if body.stream {
        return false;
    }

    let has_tools = body
        .extra
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if has_tools {
        return false;
    }

    body.extra
        .get("system")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.starts_with(CLAUDE_AUTO_REVIEW_SYSTEM_PREFIX))
            })
        })
}

fn apply_auto_review_model(
    body: &mut crate::anthropic::schema::MessagesRequest,
    count_tokens: bool,
    configured_model: Option<&str>,
) -> Option<AutoReviewRoute> {
    if count_tokens || !is_claude_auto_review_request(body) {
        return None;
    }

    let override_model = configured_model
        .filter(|model| !model.is_empty())
        .unwrap_or(CODEX_AUTO_REVIEW_MODEL);
    let requested_model = body.model.clone()?;
    let fast_override = format!(
        "{}-fast",
        override_model
            .strip_suffix("-fast")
            .unwrap_or(override_model)
    );
    let override_model = if requested_model.ends_with("-fast")
        && crate::providers::codex::translate::model_allowlist::is_valid_model_for_codex(
            &fast_override,
        ) {
        fast_override
    } else {
        override_model.to_owned()
    };
    let route = AutoReviewRoute {
        requested_model,
        override_model,
    };
    body.model = Some(route.override_model.clone());
    Some(route)
}

/// Mirrors Codex's own approval reviewer: Low effort (`ext/guardian-reviewer/src/model.rs`)
/// and a reviewer-scoped `guardian:{parent}` prompt cache key (`core/src/guardian/review_session.rs`).
fn apply_codex_auto_review_defaults(
    body: &mut crate::anthropic::schema::MessagesRequest,
    session_id: Option<&str>,
) {
    let output_config = body
        .extra
        .entry("output_config")
        .or_insert_with(|| json!({}));
    if let Some(config) = output_config.as_object_mut() {
        config.entry("effort").or_insert_with(|| json!("low"));
    }
    body.prompt_cache_key_override = session_id.map(|id| format!("guardian:{id}"));
    // The classifier reads only its text verdict; a reasoning summary or encrypted
    // reasoning would be generated and returned for nothing.
    body.omit_reasoning_artifacts = true;
}

pub struct ServerConfig {
    pub bind_address: String,
    pub port: u16,
    pub monitor: Option<MonitorHandle>,
}

pub async fn serve_with_shutdown(
    config: ServerConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let listener = bind_proxy_listener(&config.bind_address, config.port).await?;
    serve_listener(listener, config.monitor, shutdown).await
}

pub async fn bind_proxy_listener(bind_address: &str, port: u16) -> anyhow::Result<TcpListener> {
    let ip = bind_address
        .parse::<std::net::IpAddr>()
        .map_err(|err| anyhow::anyhow!("invalid proxy bind address {bind_address:?}: {err}"))?;
    let addr = std::net::SocketAddr::new(ip, port);
    TcpListener::bind(addr)
        .await
        .map_err(|err| anyhow::anyhow!("failed to bind proxy listener on {addr}: {err}"))
}

pub async fn serve_listener(
    listener: TcpListener,
    monitor: Option<MonitorHandle>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    crate::config::validate_config()?;
    let local_addr = listener.local_addr()?;
    let port = local_addr.port();
    create_logger("server").info(
        "server listening",
        Some(serde_json::Map::from_iter([
            ("port".to_string(), json!(port)),
            (
                "bindAddress".to_string(),
                json!(local_addr.ip().to_string()),
            ),
            (
                "logDir".to_string(),
                json!(
                    crate::paths::log_file()
                        .parent()
                        .map(|path| path.display().to_string())
                ),
            ),
        ])),
    );
    let app = app_with_monitor(Arc::new(Registry::new()), monitor);
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .into_future();
    tokio::pin!(server);
    let mut maintenance = tokio::time::interval(Duration::from_secs(30));
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            result = &mut server => {
                result?;
                break;
            }
            _ = maintenance.tick() => crate::providers::codex::websocket::prune_idle_state(),
        }
    }
    Ok(())
}

fn app_with_monitor(registry: Arc<Registry>, monitor: Option<MonitorHandle>) -> Router {
    let responses_api = crate::config::codex_responses_api();
    let images_api = crate::config::codex_images_api();
    let transcriptions_api = crate::config::codex_transcriptions_api();
    let native_responses = responses_api.then(|| Arc::new(CodexNativeBackend::new()));
    let chat_completions = responses_api.then(|| Arc::new(ChatCompletionsBackend::new()));
    let images = if images_api {
        match CodexImagesBackend::new() {
            Ok(backend) => Some(Arc::new(backend)),
            Err(error) => {
                create_logger("server").warn(
                    "codex images backend disabled by invalid configuration",
                    Some(Map::from_iter([("error".to_string(), json!(error))])),
                );
                None
            }
        }
    } else {
        None
    };
    let transcriptions = transcriptions_api.then(|| Arc::new(CodexTranscriptionBackend::new()));
    let state = Arc::new(AppState {
        discovery: Arc::new(CodexHttpClient::new()),
        registry,
        monitor,
        native_responses,
        chat_completions,
        images,
        transcriptions,
    });
    let router = Router::new()
        .route("/v1/messages", post(handler_messages))
        .route("/v1/messages/count_tokens", post(handler_count_tokens))
        .route("/v1/models", get(handler_models));
    let router = if responses_api {
        router
            .route("/v1/responses", post(handler_responses))
            .route("/v1/chat/completions", post(handler_chat_completions))
    } else {
        router
    };
    let router = router
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            discover_models,
        ))
        .route("/healthz", get(healthz))
        .route("/monitor", get(handler_monitor));
    let router = if images_api {
        router
            .route("/v1/images/generations", post(handler_image_generation))
            .route(
                "/v1/images/edits",
                post(handler_image_edit).layer(DefaultBodyLimit::max(MAX_EDIT_REQUEST_BYTES)),
            )
    } else {
        router
    };
    let router = if transcriptions_api {
        router.route(
            "/v1/audio/transcriptions",
            post(handler_transcription)
                .layer(DefaultBodyLimit::max(MAX_TRANSCRIPTION_REQUEST_BYTES)),
        )
    } else {
        router
    };
    router.fallback(fallback_handler).with_state(state)
}

#[derive(Clone)]
struct AppState {
    discovery: Arc<CodexHttpClient>,
    registry: Arc<Registry>,
    monitor: Option<MonitorHandle>,
    native_responses: Option<Arc<CodexNativeBackend>>,
    chat_completions: Option<Arc<ChatCompletionsBackend>>,
    images: Option<Arc<CodexImagesBackend>>,
    transcriptions: Option<Arc<CodexTranscriptionBackend>>,
}

async fn discover_models(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: axum::middleware::Next,
) -> Response {
    if matches!(
        request.uri().path(),
        "/v1/messages" | "/v1/messages/count_tokens"
    ) {
        return next.run(request).await;
    }
    let context = state.discovery.prepare_model_context().await;
    models::with_model_context(context, next.run(request)).await
}

async fn handler_monitor(
    State(state): State<Arc<AppState>>,
    peer: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
) -> Response {
    let Some(axum::Extension(axum::extract::ConnectInfo(peer))) = peer else {
        return json_error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Monitor access requires a local connection",
        );
    };
    if !peer.ip().to_canonical().is_loopback() {
        return json_error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Monitor access requires a local connection",
        );
    }
    match &state.monitor {
        Some(monitor) => (
            [(http::header::CACHE_CONTROL, "no-store")],
            Json(crate::monitor::snapshot::MonitorResponse::from(
                monitor.snapshot(),
            )),
        )
            .into_response(),
        None => json_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Monitor collection is disabled",
        ),
    }
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

#[derive(serde::Deserialize)]
struct ModelsQuery {
    limit: Option<usize>,
    after_id: Option<String>,
    before_id: Option<String>,
}

/// Anthropic-shaped listing of native Codex IDs and local service-tier variants.
async fn handler_models(
    State(state): State<Arc<AppState>>,
    query: Result<Query<ModelsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(error) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Invalid model listing query: {}", error.body_text()),
            );
        }
    };
    if query
        .limit
        .is_some_and(|limit| !(1..=1000).contains(&limit))
    {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Model listing limit must be between 1 and 1000",
        );
    }
    if query.after_id.is_some() && query.before_id.is_some() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Specify only one of after_id or before_id",
        );
    }

    let mut models = state.registry.all_supported_models();
    models.sort_unstable();
    let mut start = 0;
    let mut end = models.len();
    if let Some((name, cursor)) = query
        .after_id
        .as_ref()
        .map(|id| ("after_id", id))
        .or_else(|| query.before_id.as_ref().map(|id| ("before_id", id)))
    {
        let Ok(index) = models.binary_search_by(|(id, _)| id.cmp(cursor)) else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Invalid {name}: cursor must name a model in the listing"),
            );
        };
        if query.before_id.is_some() {
            end = index;
        } else {
            start = index + 1;
        }
    }
    let limit = query.limit.unwrap_or(end - start);
    let has_more = end - start > limit;
    if query.before_id.is_some() {
        start = end.saturating_sub(limit);
    } else {
        end = end.min(start.saturating_add(limit));
    }
    let data: Vec<Value> = models[start..end]
        .iter()
        .map(|(model, provider)| {
            json!({
                "type": "model",
                "object": "model",
                "id": model,
                "display_name": crate::providers::codex::translate::model_allowlist::model_display_name(model)
                    .unwrap_or_else(|| format!("{model} ({provider})")),
            })
        })
        .collect();
    Json(json!({
        "object": "list",
        "data": data,
        "has_more": has_more,
        "first_id": data.first().and_then(|entry| entry.get("id")).cloned(),
        "last_id": data.last().and_then(|entry| entry.get("id")).cloned(),
    }))
    .into_response()
}

async fn handler_messages(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    crate::providers::codex::translate::request::disclose_messages_compatibility(
        dispatch_request(state, req, false).await,
    )
}

async fn handler_count_tokens(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    crate::providers::codex::translate::request::disclose_messages_compatibility(
        dispatch_request(state, req, true).await,
    )
}

async fn handler_transcription(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let headers = req.headers().clone();
    log.info(
        "request",
        Some(Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!("POST")),
            ("path".to_string(), json!("/v1/audio/transcriptions")),
            ("query".to_string(), json!({})),
        ])),
    );
    let session_id = native_session_id(&headers);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(
            &req_id,
            session_id.clone(),
            None,
            EndpointKind::Transcriptions,
        );
        monitor.provider_selected(&req_id, "codex", "codex-transcribe", None);
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());
    if req.uri().query().is_some() {
        let response = transcription_error_response(TranscriptionRequestError::invalid(
            "Transcription endpoint does not accept query parameters",
            None,
        ));
        log_native_request_completed(
            &log,
            &req_id,
            "transcriptions",
            Some("codex-transcribe"),
            response.status(),
            started_at,
        );
        return monitor_response_body(response, request_guard);
    }
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !content_type.starts_with("multipart/form-data") {
        let response = transcription_error_response(TranscriptionRequestError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: "Transcription request must use multipart/form-data".to_string(),
            param: None,
            code: "unsupported_media_type",
        });
        log_native_request_completed(
            &log,
            &req_id,
            "transcriptions",
            Some("codex-transcribe"),
            response.status(),
            started_at,
        );
        return monitor_response_body(response, request_guard);
    }
    let mut multipart = match Multipart::from_request(req, &()).await {
        Ok(multipart) => multipart,
        Err(error) => {
            let status = axum::response::IntoResponse::into_response(error).status();
            let response = transcription_error_response(TranscriptionRequestError {
                status: if status == StatusCode::PAYLOAD_TOO_LARGE {
                    StatusCode::PAYLOAD_TOO_LARGE
                } else {
                    StatusCode::BAD_REQUEST
                },
                message: if status == StatusCode::PAYLOAD_TOO_LARGE {
                    "Transcription request exceeded the size limit".to_string()
                } else {
                    "Invalid multipart transcription request".to_string()
                },
                param: None,
                code: if status == StatusCode::PAYLOAD_TOO_LARGE {
                    "request_too_large"
                } else {
                    "invalid_multipart"
                },
            });
            log_native_request_completed(
                &log,
                &req_id,
                "transcriptions",
                Some("codex-transcribe"),
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let mut audio = None;
    let mut filename = None;
    let mut audio_content_type = None;
    let mut language = None;
    while let Some(field) = match multipart.next_field().await {
        Ok(field) => field,
        Err(_) => {
            let response = transcription_error_response(TranscriptionRequestError::invalid(
                "Invalid multipart transcription request",
                None,
            ));
            return monitor_response_body(response, request_guard);
        }
    } {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" => {
                if audio.is_some() {
                    let response =
                        transcription_error_response(TranscriptionRequestError::invalid(
                            "Transcription request must contain one 'file' field",
                            Some("file"),
                        ));
                    return monitor_response_body(response, request_guard);
                }
                filename = field.file_name().map(str::to_string);
                audio_content_type = field.content_type().map(str::to_string);
                audio = match field.bytes().await {
                    Ok(bytes) => Some(bytes),
                    Err(_) => {
                        let response =
                            transcription_error_response(TranscriptionRequestError::invalid(
                                "Failed to read uploaded audio",
                                Some("file"),
                            ));
                        return monitor_response_body(response, request_guard);
                    }
                };
            }
            "language" => {
                language = match multipart_text(field, "language").await {
                    Ok(value) => Some(value),
                    Err(error) => {
                        return monitor_response_body(
                            transcription_error_response(TranscriptionRequestError {
                                status: error.status,
                                message: error.message,
                                param: error.param,
                                code: error.code.unwrap_or("invalid_multipart"),
                            }),
                            request_guard,
                        );
                    }
                };
            }
            "model" => {
                if multipart_text(field, "model").await.is_err() {
                    let response =
                        transcription_error_response(TranscriptionRequestError::invalid(
                            "Invalid multipart 'model' field",
                            Some("model"),
                        ));
                    return monitor_response_body(response, request_guard);
                }
            }
            _ => {
                let response = transcription_error_response(TranscriptionRequestError::invalid(
                    format!("Unsupported multipart field '{name}'"),
                    None,
                ));
                return monitor_response_body(response, request_guard);
            }
        }
    }
    let prepared = match prepare_transcription(audio, filename, audio_content_type, language) {
        Ok(prepared) => prepared,
        Err(error) => {
            let response = transcription_error_response(error);
            log_native_request_completed(
                &log,
                &req_id,
                "transcriptions",
                Some("codex-transcribe"),
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let context = RequestContext {
        notification_id: None,
        req_id: req_id.clone(),
        session_id,
        session_seq: None,
        provider: "codex".to_string(),
        traffic: None,
        monitor: state.monitor.clone(),
    };
    let response = match state.transcriptions.as_ref() {
        Some(backend) => backend.handle(prepared, context).await,
        None => transcription_error_response(TranscriptionRequestError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Codex transcription API is unavailable".to_string(),
            param: None,
            code: "transcriptions_api_unavailable",
        }),
    };
    log_native_request_completed(
        &log,
        &req_id,
        "transcriptions",
        Some("codex-transcribe"),
        response.status(),
        started_at,
    );
    monitor_response_body(response, request_guard)
}

async fn handler_image_generation(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    dispatch_image_request(state, req, ImageOperation::Generation).await
}

async fn handler_image_edit(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    dispatch_image_request(state, req, ImageOperation::Edit).await
}

async fn dispatch_image_request(
    state: Arc<AppState>,
    req: Request<Body>,
    operation: ImageOperation,
) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();
    log.info(
        "request",
        Some(Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!("POST")),
            ("path".to_string(), json!(&path)),
            ("query".to_string(), json!(redacted_query(&uri))),
        ])),
    );
    let session_id = native_session_id(&headers);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(&req_id, session_id.clone(), None, EndpointKind::Images);
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());

    if uri.query().is_some() {
        let response = image_error_response(ImageRequestError {
            status: StatusCode::BAD_REQUEST,
            message: "Image endpoints do not accept query parameters".to_string(),
            param: None,
            code: Some("invalid_request"),
        });
        log_native_request_completed(
            &log,
            &req_id,
            operation.label(),
            None,
            response.status(),
            started_at,
        );
        return monitor_response_body(response, request_guard);
    }
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let prepared = if content_type.starts_with("application/json") {
        let limit = match operation {
            ImageOperation::Generation => MAX_GENERATION_REQUEST_BYTES,
            ImageOperation::Edit => MAX_EDIT_REQUEST_BYTES,
        };
        let body = match axum::body::to_bytes(req.into_body(), limit).await {
            Ok(body) => body,
            Err(_) => {
                let response = image_error_response(ImageRequestError {
                    status: StatusCode::PAYLOAD_TOO_LARGE,
                    message: "Image request exceeded the size limit".to_string(),
                    param: None,
                    code: Some("request_too_large"),
                });
                log_native_request_completed(
                    &log,
                    &req_id,
                    operation.label(),
                    None,
                    response.status(),
                    started_at,
                );
                return monitor_response_body(response, request_guard);
            }
        };
        prepare_json_request(operation, &body)
    } else if operation == ImageOperation::Edit && content_type.starts_with("multipart/form-data") {
        let multipart = match Multipart::from_request(req, &()).await {
            Ok(multipart) => multipart,
            Err(_) => {
                let response = image_error_response(ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: "Invalid multipart image edit request".to_string(),
                    param: None,
                    code: Some("invalid_multipart"),
                });
                log_native_request_completed(
                    &log,
                    &req_id,
                    operation.label(),
                    None,
                    response.status(),
                    started_at,
                );
                return monitor_response_body(response, request_guard);
            }
        };
        parse_multipart_image_edit(multipart).await
    } else {
        let response = image_error_response(ImageRequestError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: if operation == ImageOperation::Edit {
                "Image edit request must use application/json or multipart/form-data".to_string()
            } else {
                "Image generation request must use application/json".to_string()
            },
            param: None,
            code: Some("unsupported_media_type"),
        });
        log_native_request_completed(
            &log,
            &req_id,
            operation.label(),
            None,
            response.status(),
            started_at,
        );
        return monitor_response_body(response, request_guard);
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let response = image_error_response(error);
            log_native_request_completed(
                &log,
                &req_id,
                operation.label(),
                None,
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let model = prepared.model.clone();
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.provider_selected(&req_id, "codex", &model, None);
    }
    let context = RequestContext {
        notification_id: None,
        req_id: req_id.clone(),
        session_id,
        session_seq: None,
        provider: "codex".to_string(),
        traffic: None,
        monitor: state.monitor.clone(),
    };
    let response = match state.images.as_ref() {
        Some(backend) => backend.handle(operation, prepared, context).await,
        None => image_error_response(ImageRequestError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Codex Images API is unavailable".to_string(),
            param: None,
            code: Some("images_api_unavailable"),
        }),
    };
    log_native_request_completed(
        &log,
        &req_id,
        operation.label(),
        Some(&model),
        response.status(),
        started_at,
    );
    monitor_response_body(response, request_guard)
}

async fn parse_multipart_image_edit(
    mut multipart: Multipart,
) -> Result<crate::providers::codex::images::PreparedImageRequest, ImageRequestError> {
    let mut input = MultipartEditInput::default();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| ImageRequestError {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid multipart image edit request".to_string(),
            param: None,
            code: Some("invalid_multipart"),
        })?
    {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "image" | "image[]" => {
                let bytes = field.bytes().await.map_err(|_| ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: "Failed to read uploaded image".to_string(),
                    param: Some("image"),
                    code: Some("invalid_image"),
                })?;
                input.images.push(UploadedImage {
                    bytes, // EFFICIENCY: field.bytes() already returns owned Bytes, avoid to_vec() copy
                });
            }
            "prompt" => {
                input.prompt = Some(multipart_text(field, "prompt").await?);
            }
            "model" => {
                input.model = Some(multipart_text(field, "model").await?);
            }
            "background" => {
                input.background = Some(multipart_text(field, "background").await?);
            }
            "quality" => {
                input.quality = Some(multipart_text(field, "quality").await?);
            }
            "size" => {
                input.size = Some(multipart_text(field, "size").await?);
            }
            "n" => {
                let raw = multipart_text(field, "n").await?;
                input.n = Some(raw.parse::<u8>().map_err(|_| ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: "'n' must be an integer between 1 and 10".to_string(),
                    param: Some("n"),
                    code: Some("invalid_request"),
                })?);
            }
            "mask" => {
                return Err(ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: "Image masks are not supported by the Codex image backend".to_string(),
                    param: Some("mask"),
                    code: Some("unsupported_parameter"),
                });
            }
            _ => {
                return Err(ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: format!("Unsupported multipart field '{name}'"),
                    param: None,
                    code: Some("unsupported_parameter"),
                });
            }
        }
    }
    prepare_multipart_edit(input)
}

async fn multipart_text(
    field: axum::extract::multipart::Field<'_>,
    param: &'static str,
) -> Result<String, ImageRequestError> {
    field.text().await.map_err(|_| ImageRequestError {
        status: StatusCode::BAD_REQUEST,
        message: format!("Invalid multipart '{param}' field"),
        param: Some(param),
        code: Some("invalid_multipart"),
    })
}

async fn handler_responses(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();
    let query = redacted_query(&uri);
    log.info(
        "request",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!(method.as_str())),
            ("path".to_string(), json!(&path)),
            ("query".to_string(), json!(&query)),
        ])),
    );

    let session_id = native_session_id(&headers);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(&req_id, session_id.clone(), None, EndpointKind::Responses);
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());
    let body = {
        let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_OPENAI_REQUEST_BYTES).await
        {
            Ok(bytes) => bytes,
            Err(_) => {
                let response = openai_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "invalid_request_error",
                    "Request body exceeded the size limit".to_string(),
                    None,
                    Some("request_too_large"),
                );
                log_native_request_completed(
                    &log,
                    &req_id,
                    "responses",
                    None,
                    response.status(),
                    started_at,
                );
                return monitor_response_body(response, request_guard);
            }
        };
        match parse_native_json_body(&body_bytes) {
            Ok(body) => body,
            Err(response) => {
                log_native_request_completed(
                    &log,
                    &req_id,
                    "responses",
                    None,
                    response.status(),
                    started_at,
                );
                return monitor_response_body(response, request_guard);
            }
        }
    };
    let model = match extract_model(&body) {
        Ok(model) => model,
        Err(error) => return monitor_response_body(error.response(), request_guard),
    };
    let now = current_millis();
    let session_state = session::existing_session(session_id.as_deref(), now);
    let Some(provider) = state.registry.provider_for_model(&model) else {
        let response = OpenAiError::invalid(
            format!(
                "Invalid model ID \"{model}\". {}",
                state.registry.unknown_model_message()
            ),
            Some("model"),
        )
        .response();
        return monitor_response_body(response, request_guard);
    };
    if let Err(response) = validate_native_request_model(&body) {
        return monitor_response_body(response, request_guard);
    }
    let current =
        session::record_session_request(session_id.as_deref(), session_state.as_ref(), now);
    let effort = body
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(monitor) = state.monitor.as_ref() {
        if let Some(current) = current.as_ref() {
            monitor.session_sequence_resolved(&req_id, current.seq);
        }
        monitor.provider_selected(&req_id, provider.name(), &model, effort);
    }
    let traffic = create_traffic_capture(TrafficCaptureOptions {
        req_id: req_id.clone(),
        session_id: session_id.clone(),
        session_seq: current.as_ref().map(|session| session.seq),
        provider: Some(provider.name().to_string()),
    })
    .map(Arc::new);
    if let Some(capture) = traffic.as_ref() {
        if let Some(monitor) = state.monitor.as_ref() {
            monitor.traffic_capture_path(&req_id, capture.root().to_path_buf());
        }
        capture.write_json(
            "000-metadata",
            &json!({
                "reqId": &req_id,
                "sessionId": &session_id,
                "sessionSeq": current.as_ref().map(|session| session.seq),
                "kind": "responses",
                "provider": provider.name(),
                "model": &model,
                "requestedModel": &model,
                "method": method.as_str(),
                "path": &path,
                "query": &query,
                "headers": headers_to_record(&headers),
            }),
        );
        capture.write_json("010-openai-responses-request", &body);
    }
    let context = RequestContext {
        notification_id: None,
        req_id: req_id.clone(),
        session_id,
        session_seq: current.map(|session| session.seq),
        provider: provider.name().to_string(),
        traffic: traffic.clone(),
        monitor: state.monitor.clone(),
    };
    let response = match state.native_responses.as_ref() {
        Some(backend) => backend.handle(body, context).await,
        None => openai_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Native Responses API is disabled",
            None,
            None,
        ),
    };
    log_routed_openai_request_completed(
        &log,
        &req_id,
        "responses",
        provider.name(),
        Some(&model),
        response.status(),
        started_at,
    );
    monitor_response_body(response, request_guard)
}

async fn handler_chat_completions(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();
    let query = redacted_query(&uri);
    log.info(
        "request",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!(method.as_str())),
            ("path".to_string(), json!(&path)),
            ("query".to_string(), json!(&query)),
        ])),
    );

    let session_id = native_session_id(&headers);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(
            &req_id,
            session_id.clone(),
            None,
            EndpointKind::ChatCompletions,
        );
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());
    let body = {
        let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_OPENAI_REQUEST_BYTES).await
        {
            Ok(bytes) => bytes,
            Err(_) => {
                let response = openai_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "invalid_request_error",
                    "Request body exceeded the size limit".to_string(),
                    None,
                    Some("request_too_large"),
                );
                log_native_request_completed(
                    &log,
                    &req_id,
                    "chat_completions",
                    None,
                    response.status(),
                    started_at,
                );
                return monitor_response_body(response, request_guard);
            }
        };
        match parse_native_json_body(&body_bytes) {
            Ok(body) => body,
            Err(response) => {
                log_native_request_completed(
                    &log,
                    &req_id,
                    "chat_completions",
                    None,
                    response.status(),
                    started_at,
                );
                return monitor_response_body(response, request_guard);
            }
        }
    };
    let model = match extract_model(&body) {
        Ok(model) => model,
        Err(error) => return monitor_response_body(error.response(), request_guard),
    };
    let now = current_millis();
    let session_state = session::existing_session(session_id.as_deref(), now);
    let Some(provider) = state.registry.provider_for_model(&model) else {
        let response = OpenAiError::invalid(
            format!(
                "Invalid model ID \"{model}\". {}",
                state.registry.unknown_model_message()
            ),
            Some("model"),
        )
        .response();
        return monitor_response_body(response, request_guard);
    };
    let request_capture = traffic_capture_enabled()
        .then(|| serde_json::to_vec_pretty(&redacted(&body)).unwrap_or_else(|_| b"{}".to_vec()));
    let translated = match translate_request(body) {
        Ok(translated) => translated,
        Err(error) => return monitor_response_body(error.response(), request_guard),
    };
    let current =
        session::record_session_request(session_id.as_deref(), session_state.as_ref(), now);
    let effort = translated.effort.clone();
    if let Some(monitor) = state.monitor.as_ref() {
        if let Some(current) = current.as_ref() {
            monitor.session_sequence_resolved(&req_id, current.seq);
        }
        monitor.provider_selected(&req_id, provider.name(), &model, effort.clone());
    }
    let traffic = create_traffic_capture(TrafficCaptureOptions {
        req_id: req_id.clone(),
        session_id: session_id.clone(),
        session_seq: current.as_ref().map(|session| session.seq),
        provider: Some(provider.name().to_string()),
    })
    .map(Arc::new);
    if let Some(capture) = traffic.as_ref() {
        if let Some(monitor) = state.monitor.as_ref() {
            monitor.traffic_capture_path(&req_id, capture.root().to_path_buf());
        }
        capture.write_json(
            "000-metadata",
            &json!({
                "reqId": &req_id,
                "sessionId": &session_id,
                "sessionSeq": current.as_ref().map(|session| session.seq),
                "kind": "chat_completions",
                "provider": provider.name(),
                "model": &model,
                "requestedModel": &model,
                "effort": &effort,
                "method": method.as_str(),
                "path": &path,
                "query": &query,
                "headers": headers_to_record(&headers),
            }),
        );
        if let Some(request_capture) = request_capture {
            capture.write_bytes("010-openai-chat-completions-request.json", &request_capture);
        }
        capture.write_json("020-upstream-request", &translated.upstream);
    }
    let context = RequestContext {
        notification_id: None,
        req_id: req_id.clone(),
        session_id,
        session_seq: current.map(|session| session.seq),
        provider: provider.name().to_string(),
        traffic: traffic.clone(),
        monitor: state.monitor.clone(),
    };
    let response = match state.chat_completions.as_ref() {
        Some(backend) => backend.handle(translated, context).await,
        None => openai_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Chat Completions API is disabled",
            None,
            None,
        ),
    };
    log_routed_openai_request_completed(
        &log,
        &req_id,
        "chat_completions",
        provider.name(),
        Some(&model),
        response.status(),
        started_at,
    );
    monitor_response_body(response, request_guard)
}

fn native_session_id(headers: &http::HeaderMap) -> Option<String> {
    [
        "x-claude-code-session-id",
        "session_id",
        "x-client-request-id",
    ]
    .into_iter()
    .find_map(|name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

#[allow(clippy::result_large_err)]
fn parse_native_json_body(body: &[u8]) -> Result<Value, Response> {
    if body.is_empty() {
        return Err(openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Invalid JSON: empty body",
            None,
            Some("invalid_json"),
        ));
    }
    serde_json::from_slice(body).map_err(|error| {
        openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("Invalid JSON: {error}"),
            None,
            Some("invalid_json"),
        )
    })
}

fn log_routed_openai_request_completed(
    log: &Logger,
    req_id: &str,
    endpoint: &str,
    provider: &str,
    model: Option<&str>,
    status: StatusCode,
    started_at: Instant,
) {
    log.info(
        "request_completed",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(req_id)),
            ("endpoint".to_string(), json!(endpoint)),
            ("provider".to_string(), json!(provider)),
            ("model".to_string(), json!(model)),
            ("countTokens".to_string(), json!(false)),
            ("status".to_string(), json!(status.as_u16())),
            ("ms".to_string(), json!(started_at.elapsed().as_millis())),
        ])),
    );
}

fn log_native_request_completed(
    log: &Logger,
    req_id: &str,
    endpoint: &str,
    model: Option<&str>,
    status: StatusCode,
    started_at: Instant,
) {
    log_routed_openai_request_completed(log, req_id, endpoint, "codex", model, status, started_at);
}

async fn dispatch_request(
    state: Arc<AppState>,
    req: Request<Body>,
    count_tokens: bool,
) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let conversation_identity = (!count_tokens)
        .then(|| ConversationIdentity::from_headers(&headers))
        .flatten();
    let path = uri.path().to_string();
    let query = redacted_query(&uri);
    let endpoint = if count_tokens {
        EndpointKind::CountTokens
    } else {
        EndpointKind::Messages
    };
    log.info(
        "request",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!(method.as_str())),
            ("path".to_string(), json!(&path)),
            ("query".to_string(), json!(&query)),
        ])),
    );
    let session_id = req
        .headers()
        .get("x-claude-code-session-id")
        .and_then(|value| value.to_str().ok())
        .map(std::string::ToString::to_string);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(&req_id, session_id.clone(), None, endpoint);
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());
    let now = current_millis();
    let mut body: crate::anthropic::schema::MessagesRequest = {
        let body_bytes =
            match axum::body::to_bytes(req.into_body(), MAX_ANTHROPIC_REQUEST_BYTES).await {
                Ok(bytes) => bytes,
                Err(_) => {
                    let response = json_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request_too_large",
                        "Request body exceeded the size limit",
                    );
                    log_request_completed(
                        &log,
                        RequestLogContext {
                            req_id: &req_id,
                            provider: None,
                            model: None,
                            count_tokens,
                            status: response.status(),
                            started_at,
                        },
                    );
                    let (response, details) = record_failed_response(
                        &log,
                        FailedResponseLogContext {
                            req_id: &req_id,
                            provider: None,
                            model: None,
                            count_tokens,
                            started_at,
                        },
                        response,
                    )
                    .await;
                    monitor_failed(
                        state.monitor.as_ref(),
                        &req_id,
                        Some(response.status()),
                        details
                            .as_ref()
                            .map(|details| details.message.as_str())
                            .unwrap_or("Invalid JSON"),
                    );
                    return with_request_id(response, &req_id);
                }
            };

        match parse_json_body(&body_bytes) {
            Ok(body) => body,
            Err(response) => {
                let status = response.status();
                log_request_completed(
                    &log,
                    RequestLogContext {
                        req_id: &req_id,
                        provider: None,
                        model: None,
                        count_tokens,
                        status: response.status(),
                        started_at,
                    },
                );
                let (response, details) = record_failed_response(
                    &log,
                    FailedResponseLogContext {
                        req_id: &req_id,
                        provider: None,
                        model: None,
                        count_tokens,
                        started_at,
                    },
                    *response,
                )
                .await;
                monitor_failed(
                    state.monitor.as_ref(),
                    &req_id,
                    Some(status),
                    details
                        .as_ref()
                        .map(|details| details.message.as_str())
                        .unwrap_or("Invalid JSON"),
                );
                return with_request_id(response, &req_id);
            }
        }
    };

    if let Err(error) =
        crate::providers::codex::translate::request::validate_messages_capabilities(&body)
    {
        return failed_request_response(
            &log,
            FailedResponseLogContext {
                req_id: &req_id,
                provider: None,
                model: body.model.as_deref(),
                count_tokens,
                started_at,
            },
            json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            ),
            request_guard,
        )
        .await;
    }
    let context = state.discovery.prepare_model_context().await;
    models::with_model_context(context, async {
        let thread_attempt = match crate::providers::codex::threads::prepare(
            &mut body,
            conversation_identity.as_ref(),
            models::current_operation_provenance(),
            count_tokens,
        ) {
            Ok(attempt) => attempt,
            Err(error) => {
                return failed_request_response(
                    &log,
                    FailedResponseLogContext {
                        req_id: &req_id,
                        provider: None,
                        model: body.model.as_deref(),
                        count_tokens,
                        started_at,
                    },
                    error.response(),
                    request_guard,
                )
                .await;
            }
        };
        crate::providers::codex::threads::scope(thread_attempt.clone(), async {
            if let Some(project) = project::name_from_request(
                body.extra.get("system"),
                body.messages.iter().rev().map(|message| &message.content),
            ) && let Some(monitor) = state.monitor.as_ref()
            {
                monitor.project_resolved(&req_id, project);
            }

            let model = match body.model.as_deref() {
                Some(model) => model,
                None => {
                    let response = json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        format!(
                            "Missing \"model\" in request body. {}",
                            state.registry.unknown_model_message()
                        ),
                    );
                    log_request_completed(
                        &log,
                        RequestLogContext {
                            req_id: &req_id,
                            provider: None,
                            model: None,
                            count_tokens,
                            status: response.status(),
                            started_at,
                        },
                    );
                    let (response, details) = record_failed_response(
                        &log,
                        FailedResponseLogContext {
                            req_id: &req_id,
                            provider: None,
                            model: None,
                            count_tokens,
                            started_at,
                        },
                        response,
                    )
                    .await;
                    monitor_failed(
                        state.monitor.as_ref(),
                        &req_id,
                        Some(response.status()),
                        details
                            .as_ref()
                            .map(|details| details.message.as_str())
                            .unwrap_or("Missing model"),
                    );
                    return with_request_id(response, &req_id);
                }
            };

            let mut model = model.to_owned();
            let session_state = session::existing_session(session_id.as_deref(), now);
            let original_provider = state.registry.provider_for_model(&model);
            let configured_auto_review_model = crate::config::auto_review_model();
            let auto_review_route = original_provider.as_ref().and_then(|_| {
                apply_auto_review_model(
                    &mut body,
                    count_tokens,
                    configured_auto_review_model.as_deref(),
                )
            });
            if auto_review_route.is_some() {
                model = body.model.clone().expect("override model");
            }

            let provider = if auto_review_route.is_some() {
                state.registry.provider_for_model(&model)
            } else {
                original_provider
            };

            let provider = match provider {
                Some(provider) => provider,
                None => {
                    log.warn(
                        "unknown model",
                        Some(serde_json::Map::from_iter([
                            ("reqId".to_string(), json!(&req_id)),
                            ("model".to_string(), json!(&model)),
                        ])),
                    );
                    let response = json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        format!(
                            "Invalid model ID \"{model}\". {}",
                            state.registry.unknown_model_message()
                        ),
                    );
                    log_request_completed(
                        &log,
                        RequestLogContext {
                            req_id: &req_id,
                            provider: None,
                            model: Some(&model),
                            count_tokens,
                            status: response.status(),
                            started_at,
                        },
                    );
                    let (response, details) = record_failed_response(
                        &log,
                        FailedResponseLogContext {
                            req_id: &req_id,
                            provider: None,
                            model: Some(&model),
                            count_tokens,
                            started_at,
                        },
                        response,
                    )
                    .await;
                    monitor_failed(
                        state.monitor.as_ref(),
                        &req_id,
                        Some(response.status()),
                        details
                            .as_ref()
                            .map(|details| details.message.as_str())
                            .unwrap_or("Unknown model"),
                    );
                    return with_request_id(response, &req_id);
                }
            };

            body.bypass_provider_model_override = auto_review_route.is_some();
            if body.bypass_provider_model_override {
                apply_codex_auto_review_defaults(&mut body, session_id.as_deref());
            }

            if let Some(route) = auto_review_route.as_ref() {
                log.info(
                    "auto-review route selected",
                    Some(Map::from_iter([
                        ("reqId".to_string(), json!(&req_id)),
                        ("requestedModel".to_string(), json!(&route.requested_model)),
                        ("overrideModel".to_string(), json!(&route.override_model)),
                        ("provider".to_string(), json!(provider.name())),
                    ])),
                );
            }

            let effort = body
                .extra
                .get("output_config")
                .and_then(|config| config.get("effort"))
                .and_then(Value::as_str)
                .filter(|effort| {
                    !effort.is_empty()
                        && effort.len() <= 256
                        && !effort.chars().any(|c| c.is_whitespace() || c.is_control())
                })
                .map(str::to_string);
            let current =
                session::record_session_request(session_id.as_deref(), session_state.as_ref(), now);
            if let Some(monitor) = state.monitor.as_ref() {
                if let Some(current) = current.as_ref() {
                    monitor.session_sequence_resolved(&req_id, current.seq);
                }
                monitor.provider_selected(&req_id, provider.name(), &model, effort);
            }

            let traffic = create_traffic_capture(TrafficCaptureOptions {
                req_id: req_id.clone(),
                session_id: session_id.clone(),
                session_seq: current.as_ref().map(|s| s.seq),
                provider: Some(provider.name().to_string()),
            })
            .map(Arc::new);

            if let Some(capture) = traffic.as_ref() {
                if let Some(monitor) = state.monitor.as_ref() {
                    monitor.traffic_capture_path(&req_id, capture.root().to_path_buf());
                }
                capture.write_json(
                    "000-metadata",
                    &json!({
                        "reqId": &req_id,
                        "sessionId": &session_id,
                        "sessionSeq": current.as_ref().map(|s| s.seq),
                        "kind": if count_tokens { "count_tokens" } else { "messages" },
                        "provider": provider.name(),
                        "model": &model,
                        "method": method.as_str(),
                        "path": &path,
                        "query": &query,
                        "headers": headers_to_record(&headers),
                    }),
                );
                capture.write_json(
                    "010-anthropic-request",
                    &serde_json::to_value(&body).unwrap_or_else(|_| json!({})),
                );
            }

            let context = RequestContext {
                notification_id: headers
                    .get("x-clodex-notification-id")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
                req_id: req_id.clone(),
                session_id,
                session_seq: current.map(|s| s.seq),
                provider: provider.name().to_string(),
                traffic,
                monitor: state.monitor.clone(),
            };

            let response = if count_tokens {
                provider.handle_count_tokens(body, context).await
            } else {
                provider
                    .handle_messages_with_conversation_identity(
                        body,
                        context,
                        if auto_review_route.is_some() {
                            None
                        } else {
                            conversation_identity
                        },
                    )
                    .await
            };
            let response =
                crate::providers::codex::threads::response_body(response, thread_attempt);
            log_request_completed(
                &log,
                RequestLogContext {
                    req_id: &req_id,
                    provider: Some(provider.name()),
                    model: Some(&model),
                    count_tokens,
                    status: response.status(),
                    started_at,
                },
            );
            let status = response.status();
            if status.is_success() {
                return monitor_response_body(response, request_guard);
            }

            let (response, details) = record_failed_response(
                &log,
                FailedResponseLogContext {
                    req_id: &req_id,
                    provider: Some(provider.name()),
                    model: Some(&model),
                    count_tokens,
                    started_at,
                },
                response,
            )
            .await;
            if let Some(details) = details.as_ref() {
                monitor_failed(
                    state.monitor.as_ref(),
                    &req_id,
                    Some(status),
                    details.message.as_str(),
                );
            } else {
                monitor_failed(
                    state.monitor.as_ref(),
                    &req_id,
                    Some(status),
                    format!("HTTP {}", status.as_u16()),
                );
            }
            with_request_id(response, &req_id)
        })
        .await
    })
    .await
}

/// Header Claude Code reads to populate the `requestId` field it writes into
/// every transcript record.
const REQUEST_ID_HEADER: &str = "request-id";

/// Publish the per-request id this proxy mints. Anthropic returns `request-id`,
/// and Claude Code records it as `requestId`; without it every downstream
/// consumer that de-duplicates transcript records by request id (its own
/// parser, usage dashboards) counts each request twice, because a transcript
/// legitimately repeats a record and the id is what resolves it.
///
/// An upstream-supplied id is preserved: relabelling a real provider id with a
/// local uuid would lose the more useful value.
fn stamp_request_id(headers: &mut http::HeaderMap, req_id: &str) {
    if !headers.contains_key(REQUEST_ID_HEADER)
        && let Ok(value) = http::HeaderValue::from_str(req_id)
    {
        headers.insert(REQUEST_ID_HEADER, value);
    }
}

fn with_request_id(mut response: Response, req_id: &str) -> Response {
    stamp_request_id(response.headers_mut(), req_id);
    response
}

fn monitor_response_body(response: Response, guard: RequestMonitorGuard) -> Response {
    let status = response.status();
    let outcome = response
        .extensions()
        .get::<NativeResponseOutcome>()
        .cloned();
    let (mut parts, body) = response.into_parts();
    // Stamped on the parts before the body is streamed, so a streaming SSE
    // response carries the header too.
    stamp_request_id(&mut parts.headers, &guard.req_id);
    let stream = futures_util::stream::unfold(
        (body, guard, outcome),
        move |(mut body, mut guard, outcome)| async move {
            match body.frame().await {
                Some(Ok(frame)) => Some((Ok(frame), (body, guard, outcome))),
                Some(Err(err)) => {
                    guard.failed(status, err.to_string());
                    Some((Err(err), (body, guard, outcome)))
                }
                None => {
                    if let Some(message) = outcome.as_ref().and_then(NativeResponseOutcome::failure)
                    {
                        create_logger("server").error(
                            "request_stream_failed",
                            Some(Map::from_iter([
                                ("reqId".to_string(), json!(&guard.req_id)),
                                ("actualHTTPstatus".to_string(), json!(status.as_u16())),
                                ("message".to_string(), json!(&message)),
                            ])),
                        );
                        guard.failed(status, message);
                    } else if status.is_success() {
                        guard.completed(status);
                    } else {
                        guard.failed(status, format!("HTTP {}", status.as_u16()));
                    }
                    None
                }
            }
        },
    );
    Response::from_parts(parts, Body::new(StreamBody::new(stream)))
}

struct RequestLogContext<'a> {
    req_id: &'a str,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    count_tokens: bool,
    status: StatusCode,
    started_at: Instant,
}

fn log_request_completed(log: &Logger, ctx: RequestLogContext<'_>) {
    log.info(
        "request_completed",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(ctx.req_id)),
            ("provider".to_string(), json!(ctx.provider)),
            ("model".to_string(), json!(ctx.model)),
            ("countTokens".to_string(), json!(ctx.count_tokens)),
            ("status".to_string(), json!(ctx.status.as_u16())),
            (
                "ms".to_string(),
                json!(ctx.started_at.elapsed().as_millis()),
            ),
        ])),
    );
}

struct FailedResponseLogContext<'a> {
    req_id: &'a str,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    count_tokens: bool,
    started_at: Instant,
}

struct FailedResponseDetails {
    message: String,
}

async fn failed_request_response(
    log: &Logger,
    ctx: FailedResponseLogContext<'_>,
    response: Response,
    mut guard: RequestMonitorGuard,
) -> Response {
    let status = response.status();
    log_request_completed(
        log,
        RequestLogContext {
            req_id: ctx.req_id,
            provider: ctx.provider,
            model: ctx.model,
            count_tokens: ctx.count_tokens,
            status,
            started_at: ctx.started_at,
        },
    );
    let (response, details) = record_failed_response(log, ctx, response).await;
    guard.failed(
        status,
        details.map_or_else(
            || format!("HTTP {}", status.as_u16()),
            |details| details.message,
        ),
    );
    with_request_id(response, &guard.req_id)
}

async fn record_failed_response(
    log: &Logger,
    ctx: FailedResponseLogContext<'_>,
    response: Response,
) -> (Response, Option<FailedResponseDetails>) {
    if response.status().is_success() {
        return (response, None);
    }

    let status = response.status();
    let (parts, mut body) = response.into_parts();
    let mut frames = VecDeque::new();
    let mut bytes = Vec::new();
    let mut complete = false;
    let mut read_error = None;
    while bytes.len() < MAX_ERROR_CAPTURE_BYTES && frames.len() < MAX_ERROR_CAPTURE_FRAMES {
        let Some(frame) = body.frame().await else {
            complete = true;
            break;
        };
        match &frame {
            Ok(frame) => {
                if let Some(data) = frame.data_ref() {
                    let remaining = MAX_ERROR_CAPTURE_BYTES - bytes.len();
                    bytes.extend_from_slice(&data[..data.len().min(remaining)]);
                }
            }
            Err(error) => read_error = Some(bounded_error_message(&error.to_string()).to_owned()),
        }
        frames.push_back(frame);
        if read_error.is_some() {
            break;
        }
    }
    // A truncated JSON fragment cannot be safely redacted as structured data.
    let response_body = if complete {
        response_body_value(&bytes)
    } else {
        json!({ "captureTruncated": true, "capturedBytes": bytes.len() })
    };
    let message = error_message_from_response(&response_body)
        .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
    let mut document = json!({
        "reqId": ctx.req_id,
        "provider": ctx.provider,
        "model": ctx.model,
        "countTokens": ctx.count_tokens,
        "status": status.as_u16(),
        "elapsedMs": ctx.started_at.elapsed().as_millis(),
        "message": &message,
        "bodyReadError": read_error,
    });
    document
        .as_object_mut()
        .expect("capture document is an object")
        .insert("response".to_string(), response_body);
    let error_file = write_error_capture(ctx.req_id, &redact_error_value(document));

    let mut fields = serde_json::Map::from_iter([
        ("reqId".to_string(), json!(ctx.req_id)),
        ("provider".to_string(), json!(ctx.provider)),
        ("model".to_string(), json!(ctx.model)),
        ("countTokens".to_string(), json!(ctx.count_tokens)),
        ("status".to_string(), json!(status.as_u16())),
        (
            "ms".to_string(),
            json!(ctx.started_at.elapsed().as_millis()),
        ),
        ("message".to_string(), json!(message)),
    ]);
    if let Some(path) = error_file.as_ref() {
        fields.insert("errorFile".to_string(), json!(path.display().to_string()));
    }
    log.error("request_failed", Some(fields));

    let stream = futures_util::stream::unfold((frames, body), |(mut frames, mut body)| async {
        let frame = match frames.pop_front() {
            Some(frame) => Some(frame),
            None => body.frame().await,
        };
        frame.map(|frame| (frame, (frames, body)))
    });
    (
        Response::from_parts(parts, Body::new(StreamBody::new(stream))),
        Some(FailedResponseDetails { message }),
    )
}

fn response_body_value(bytes: &[u8]) -> Value {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) => json!({ "json": value }),
        Err(_) => json!({ "text": String::from_utf8_lossy(bytes) }),
    }
}

fn error_message_from_response(response_body: &Value) -> Option<String> {
    response_body
        .get("json")
        .and_then(|body| body.get("error"))
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| {
            response_body
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .map(|message| bounded_error_message(message).to_owned())
}

fn write_error_capture(req_id: &str, document: &Value) -> Option<PathBuf> {
    let dir = crate::paths::state_dir().join("errors");
    fs::create_dir_all(&dir).ok()?;
    set_mode(&dir, 0o700);
    let path = dir.join(format!(
        "{}-{}.json",
        current_millis(),
        sanitize_path_part(req_id)
    ));
    let file = File::create(&path).ok()?;
    set_mode(&path, 0o600);
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, document).ok()?;
    writer.write_all(b"\n").ok()?;
    writer.flush().ok()?;
    Some(path)
}

fn sanitize_path_part(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn redact_error_value(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(redact_error_value).collect()),
        Value::Object(fields) => {
            let mut out = Map::new();
            for (key, value) in fields {
                if REDACT_KEYS.contains(&key.to_lowercase().as_str()) {
                    out.insert(key, redact_error_key(value));
                } else {
                    out.insert(key, redact_error_value(value));
                }
            }
            Value::Object(out)
        }
        value => value,
    }
}

fn redact_error_key(value: Value) -> Value {
    match value {
        Value::String(value) => Value::String(format!("[redacted len={}]", value.len())),
        _ => Value::String("[redacted]".to_string()),
    }
}

struct RequestMonitorGuard {
    monitor: Option<MonitorHandle>,
    req_id: String,
}

impl RequestMonitorGuard {
    fn new(monitor: Option<MonitorHandle>, req_id: String) -> Self {
        Self { monitor, req_id }
    }

    fn completed(&mut self, status: StatusCode) {
        if let Some(monitor) = self.monitor.take() {
            monitor.request_completed(&self.req_id, status.as_u16());
        }
    }

    fn failed(&mut self, status: StatusCode, error: String) {
        if let Some(monitor) = self.monitor.take() {
            monitor.request_failed(&self.req_id, Some(status.as_u16()), error);
        }
    }
}

impl Drop for RequestMonitorGuard {
    fn drop(&mut self) {
        if let Some(monitor) = self.monitor.as_ref() {
            monitor.request_abandoned(&self.req_id, "Request future ended before completion");
        }
    }
}

fn monitor_failed(
    monitor: Option<&MonitorHandle>,
    req_id: &str,
    status: Option<StatusCode>,
    error: impl Into<String>,
) {
    if let Some(monitor) = monitor {
        monitor.request_failed(req_id, status.map(|status| status.as_u16()), error);
    }
}

fn headers_to_record(headers: &http::HeaderMap) -> Value {
    let mut out = Map::new();
    for (key, value) in headers {
        if let Ok(raw) = value.to_str() {
            let value = if matches!(
                key.as_str(),
                CLAUDE_AGENT_HEADER | CLAUDE_PARENT_AGENT_HEADER
            ) {
                format!("[redacted len={}]", raw.len())
            } else {
                raw.to_string()
            };
            out.insert(key.as_str().to_string(), Value::String(value));
        }
    }
    Value::Object(out)
}

fn redacted_query(uri: &http::Uri) -> Value {
    let mut out = Map::new();
    let Some(query) = uri.query() else {
        return Value::Object(out);
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let key = key.into_owned();
        let lower = key.to_lowercase();
        let value = if REDACT_KEYS.contains(&lower.as_str()) {
            Value::String(format!("[redacted len={}]", value.len()))
        } else {
            Value::String(value.into_owned())
        };
        out.insert(key, value);
    }
    Value::Object(out)
}

fn parse_json_body<T>(body: &[u8]) -> Result<T, Box<Response>>
where
    T: DeserializeOwned,
{
    if body.is_empty() {
        return Err(Box::new(json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Invalid JSON: empty body",
        )));
    }

    serde_json::from_slice::<T>(body).map_err(|err| {
        Box::new(json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("Invalid JSON: {err}"),
        ))
    })
}

async fn fallback_handler(method: axum::http::Method, uri: axum::http::Uri) -> Response {
    json_error(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("No route for {method} {}", uri.path()),
    )
}

fn current_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(mode);
            let _ = fs::set_permissions(path, perm);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}
