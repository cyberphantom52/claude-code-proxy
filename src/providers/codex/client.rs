use std::borrow::Cow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::anthropic::sse::sse_events;
use crate::config;
use crate::logging::create_logger;
use crate::provider::RequestContext;
use crate::request_identity::ConversationIdentity;
use crate::retry::{compute_backoff_delay, should_retry_status, sleep};
use crate::traffic::TrafficCapture;

use super::auth::constants::{CODEX_API_ENDPOINT, ORIGINATOR, RESPONSES_LITE_ORIGINATOR};
use super::auth::manager::CodexAuthManager;
use super::auth::token_store::{DefaultCodexAuthStore, StoredAuth, file_store};
use super::events::CodexUsageLimit;
use super::models::OperationAuth;
use super::search::{SearchRequest, SearchResponse};
use super::translate::request::ResponsesRequest;

fn has_opaque_native_history(body: &ResponsesRequest) -> bool {
    use super::translate::request::ResponsesInputItem;
    body.input.iter().any(|item| {
        matches!(item, ResponsesInputItem::Compaction { .. } | ResponsesInputItem::CompactionTrigger
                | ResponsesInputItem::Reasoning { .. } | ResponsesInputItem::WebSearchCall { .. })
            || matches!(item, ResponsesInputItem::Message { metadata, .. }
                if metadata.encrypted_content.is_some())
            || matches!(item, ResponsesInputItem::FunctionCall { namespace: Some(namespace), name, .. }
                if namespace == "web" && name == "run")
    })
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct CodexError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
    pub usage_limit: Option<Box<CodexUsageLimit>>,
    pub origin: CodexErrorOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexErrorOrigin {
    Http,
    WebSocket,
    WebSocketHandshake,
    Auth,
    BufferedHttp,
    BufferedWebSocket,
}

impl CodexError {
    pub fn new(status: u16, message: String) -> Self {
        Self {
            status,
            message,
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        }
    }
}

impl std::fmt::Display for CodexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex error {}: {}", self.status, self.message)
    }
}

// ---------------------------------------------------------------------------
// Header builder
// ---------------------------------------------------------------------------

fn default_user_agent(use_responses_lite: bool) -> String {
    if use_responses_lite {
        RESPONSES_LITE_ORIGINATOR.to_string()
    } else {
        format!("claude-code-proxy/{}", env!("CARGO_PKG_VERSION"))
    }
}

pub fn build_codex_headers(
    auth: &StoredAuth,
    ctx: &RequestContext,
    use_responses_lite: bool,
) -> Result<http::HeaderMap, CodexError> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        header_value("content-type", "application/json")?,
    );
    headers.insert(
        http::header::ACCEPT,
        header_value("accept", "text/event-stream")?,
    );
    let bearer = format!("Bearer {}", auth.access);
    headers.insert(
        http::header::AUTHORIZATION,
        header_value("authorization", &bearer)?,
    );
    let originator = if use_responses_lite {
        RESPONSES_LITE_ORIGINATOR.to_string()
    } else {
        config::codex_originator(ORIGINATOR)
    };
    headers.insert("originator", header_value("originator", &originator)?);
    headers.insert(
        "openai-beta",
        header_value("openai-beta", "responses=experimental")?,
    );
    headers.insert(
        "x-codex-beta-features",
        header_value("x-codex-beta-features", "remote_compaction_v2")?,
    );
    if use_responses_lite {
        headers.insert(
            "x-openai-internal-codex-responses-lite",
            header_value("x-openai-internal-codex-responses-lite", "true")?,
        );
    }
    if let Some(ref account_id) = auth.account_id {
        headers.insert(
            "ChatGPT-Account-Id",
            header_value("ChatGPT-Account-Id", account_id)?,
        );
    }
    if let Some(ref session_id) = ctx.session_id {
        // Match the official Codex Responses session header.
        headers.insert("session-id", header_value("session-id", session_id)?);
        headers.insert(
            "x-client-request-id",
            header_value("x-client-request-id", session_id)?,
        );
        let window_id = format!("{session_id}:0");
        headers.insert(
            "x-codex-window-id",
            header_value("x-codex-window-id", &window_id)?,
        );
    }
    let user_agent = config::codex_user_agent(&default_user_agent(use_responses_lite));
    if !user_agent.is_empty() {
        headers.insert(
            http::header::USER_AGENT,
            header_value("user-agent", &user_agent)?,
        );
    }
    Ok(headers)
}

fn build_codex_response_headers(
    auth: &StoredAuth,
    ctx: &RequestContext,
    use_responses_lite: bool,
    codex_auto_review: bool,
    endpoint: &str,
) -> Result<http::HeaderMap, CodexError> {
    // Codex 970111f6 core/src/client.rs and ext/guardian-v2/src/sync_reviewer/mod.rs:
    // reviewer headers are selected-model scoped and restricted to Codex OAuth.
    if codex_auto_review
        && (endpoint.trim_end_matches('/') != CODEX_API_ENDPOINT
            || auth.access.trim().is_empty()
            || auth
                .account_id
                .as_deref()
                .is_none_or(|id| id.trim().is_empty()))
    {
        return Err(CodexError::new(
            400,
            "codex-auto-review requires the official Codex endpoint and a ChatGPT OAuth account"
                .to_string(),
        ));
    }
    let mut headers = build_codex_headers(auth, ctx, use_responses_lite)?;
    if codex_auto_review {
        headers.insert(
            "x-codex-guardian",
            http::HeaderValue::from_static("reviewer"),
        );
    }
    Ok(headers)
}

pub fn build_native_codex_headers(
    auth: &StoredAuth,
    ctx: &RequestContext,
    use_responses_lite: bool,
    stream: bool,
) -> Result<http::HeaderMap, CodexError> {
    let mut headers = build_codex_headers(auth, ctx, use_responses_lite)?;
    headers.insert(
        http::header::ACCEPT,
        header_value(
            "accept",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        )?,
    );
    Ok(headers)
}

pub fn build_codex_search_headers(
    auth: &StoredAuth,
    ctx: &RequestContext,
    search_session_id: &str,
) -> Result<http::HeaderMap, CodexError> {
    let mut headers = build_codex_headers(auth, ctx, false)?;
    headers.insert("session_id", header_value("session_id", search_session_id)?);
    headers.insert(
        "x-client-request-id",
        header_value("x-client-request-id", search_session_id)?,
    );
    let window_id = format!("{search_session_id}:0");
    headers.insert(
        "x-codex-window-id",
        header_value("x-codex-window-id", &window_id)?,
    );
    headers.insert(
        http::header::ACCEPT,
        header_value("accept", "application/json")?,
    );
    let originator = config::codex_originator(RESPONSES_LITE_ORIGINATOR);
    headers.insert("originator", header_value("originator", &originator)?);
    let user_agent = config::codex_user_agent(RESPONSES_LITE_ORIGINATOR);
    if !user_agent.is_empty() {
        headers.insert(
            http::header::USER_AGENT,
            header_value("user-agent", &user_agent)?,
        );
    }
    Ok(headers)
}

pub fn build_codex_image_headers(
    auth: &StoredAuth,
    ctx: &RequestContext,
) -> Result<http::HeaderMap, CodexError> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        header_value("content-type", "application/json")?,
    );
    headers.insert(
        http::header::ACCEPT,
        header_value("accept", "application/json")?,
    );
    headers.insert(
        http::header::AUTHORIZATION,
        header_value("authorization", &format!("Bearer {}", auth.access))?,
    );
    headers.insert(
        "originator",
        header_value("originator", &config::codex_originator(ORIGINATOR))?,
    );
    if let Some(account_id) = auth.account_id.as_deref() {
        headers.insert(
            "ChatGPT-Account-Id",
            header_value("ChatGPT-Account-Id", account_id)?,
        );
    }
    if let Some(session_id) = ctx.session_id.as_deref() {
        headers.insert(
            "x-client-request-id",
            header_value("x-client-request-id", session_id)?,
        );
    }
    let user_agent = config::codex_user_agent(&default_user_agent(false));
    if !user_agent.is_empty() {
        headers.insert(
            http::header::USER_AGENT,
            header_value("user-agent", &user_agent)?,
        );
    }
    Ok(headers)
}

pub fn build_codex_transcription_headers(
    auth: &StoredAuth,
    ctx: &RequestContext,
) -> Result<http::HeaderMap, CodexError> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::ACCEPT,
        header_value("accept", "application/json")?,
    );
    headers.insert(
        http::header::AUTHORIZATION,
        header_value("authorization", &format!("Bearer {}", auth.access))?,
    );
    headers.insert("originator", header_value("originator", "Codex Desktop")?);
    if let Some(account_id) = auth.account_id.as_deref() {
        headers.insert(
            "ChatGPT-Account-Id",
            header_value("ChatGPT-Account-Id", account_id)?,
        );
    }
    if let Some(session_id) = ctx.session_id.as_deref() {
        headers.insert(
            "x-client-request-id",
            header_value("x-client-request-id", session_id)?,
        );
    }
    let user_agent = config::codex_user_agent(&default_user_agent(false));
    if !user_agent.is_empty() {
        headers.insert(
            http::header::USER_AGENT,
            header_value("user-agent", &user_agent)?,
        );
    }
    Ok(headers)
}

fn header_value(name: &str, value: &str) -> Result<http::HeaderValue, CodexError> {
    http::HeaderValue::from_str(value).map_err(|e| CodexError {
        status: 500,
        message: format!("Failed to parse {name} header"),
        detail: Some(e.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::Http,
    })
}

fn search_endpoint(base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    match base_url.strip_suffix("/responses") {
        Some(api_root) => format!("{api_root}/alpha/search"),
        None => format!("{base_url}/alpha/search"),
    }
}

// ---------------------------------------------------------------------------
// WebSocket request shaping
// ---------------------------------------------------------------------------

fn build_websocket_request(
    body: &ResponsesRequest,
    continuation: Option<&super::continuation::ContinuationCandidate>,
) -> serde_json::Value {
    let input = continuation
        .and_then(|candidate| candidate.input_delta.as_deref())
        .unwrap_or(&body.input);
    let mut payload = serde_json::to_value(body.with_input(input)).unwrap_or_default();
    let obj = payload.as_object_mut().expect("request must be an object");

    // Omit the stream field for WebSocket transport
    obj.remove("stream");
    obj.insert("type".to_string(), serde_json::json!("response.create"));

    // Apply continuation if available
    if let Some(candidate) = continuation
        && let Some(ref prev_id) = candidate.previous_response_id
    {
        obj.insert(
            "previous_response_id".to_string(),
            serde_json::json!(prev_id),
        );
    }

    payload
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActualTransport {
    Http,
    WebSocket,
}

pub struct CodexResponse {
    pub body: Vec<u8>,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub transport: ActualTransport,
}

pub type CodexHttpEventReceiver =
    tokio::sync::mpsc::Receiver<Result<serde_json::Value, CodexError>>;

pub(super) const MAX_HTTP_SSE_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_PENDING_HTTP_EVENT_BYTES: usize = 2 * MAX_HTTP_SSE_FRAME_BYTES;
const MIN_PENDING_HTTP_EVENT_BYTES: usize = 256;

#[derive(Default)]
struct HttpSseDecoder {
    frame: Vec<u8>,
    line_start: usize,
    skip_lf: bool,
}

struct DecodedHttpSseEvent {
    event: Option<String>,
    payload: Option<serde_json::Value>,
}

struct HttpEventStreamState {
    provenance_publisher: super::websocket::CodexWebSocketSocketIdPublisher,
    resp: reqwest::Response,
    response_headers: Vec<(String, String)>,
    started_at: Instant,
    body_json: Bytes,
    auth: Arc<OperationAuth>,
    auth_refresh_attempted: bool,
    use_responses_lite: bool,
    codex_auto_review: bool,
    retries: u32,
    model: String,
}

impl HttpSseDecoder {
    fn push(&mut self, input: &[u8]) -> Result<Vec<DecodedHttpSseEvent>, CodexError> {
        let mut events = Vec::new();
        for &byte in input {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(&mut events)?,
                b'\r' => {
                    self.end_line(&mut events)?;
                    self.skip_lf = true;
                }
                _ => self.push_byte(byte)?,
            }
        }
        Ok(events)
    }

    fn finish(&self) -> Result<(), CodexError> {
        if self.frame.is_empty() {
            Ok(())
        } else {
            Err(http_sse_error(
                "Codex SSE stream ended with an incomplete frame",
            ))
        }
    }

    fn push_byte(&mut self, byte: u8) -> Result<(), CodexError> {
        if self.frame.len() >= MAX_HTTP_SSE_FRAME_BYTES {
            return Err(http_sse_error("Codex SSE frame exceeds the size limit"));
        }
        self.frame.push(byte);
        Ok(())
    }

    fn end_line(&mut self, events: &mut Vec<DecodedHttpSseEvent>) -> Result<(), CodexError> {
        if self.frame.len() == self.line_start {
            if !self.frame.is_empty()
                && let Some(event) = decode_http_sse_frame(&self.frame)?
            {
                events.push(event);
            }
            self.frame.clear();
            self.line_start = 0;
            return Ok(());
        }
        self.push_byte(b'\n')?;
        self.line_start = self.frame.len();
        Ok(())
    }
}

fn decode_http_sse_frame(frame: &[u8]) -> Result<Option<DecodedHttpSseEvent>, CodexError> {
    let frame = std::str::from_utf8(frame)
        .map_err(|_| http_sse_error("Codex SSE frame contains invalid UTF-8"))?;
    let mut event = None;
    let mut data: Option<Cow<'_, str>> = None;
    for line in frame.lines() {
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => match &mut data {
                Some(data) => {
                    let data = data.to_mut();
                    data.push('\n');
                    data.push_str(value);
                }
                None => data = Some(Cow::Borrowed(value)),
            },
            _ => {}
        }
    }
    let Some(data) = data else {
        return Ok(None);
    };
    if data == "[DONE]" {
        return Ok(Some(DecodedHttpSseEvent {
            event,
            payload: None,
        }));
    }
    let payload = serde_json::from_str(&data)
        .map_err(|_| http_sse_error("Codex SSE frame contains invalid JSON"))?;
    Ok(Some(DecodedHttpSseEvent {
        event,
        payload: Some(payload),
    }))
}

fn http_sse_error(message: &str) -> CodexError {
    CodexError {
        status: 0,
        message: message.to_string(),
        detail: Some("http_response_sse".to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::Http,
    }
}

fn http_sse_protocol_error(message: &str) -> CodexError {
    CodexError {
        status: 0,
        message: format!("Codex HTTP tool event protocol error: {message}"),
        detail: Some("http_response_sse_protocol".to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::Http,
    }
}

pub(crate) struct OwnerAwareCodexResponse {
    response: CodexResponse,
    pub(crate) socket_id: Option<u64>,
    pub(crate) provenance: Option<super::models::OperationProvenance>,
}

impl OwnerAwareCodexResponse {
    pub(crate) fn new(response: CodexResponse, socket_id: Option<u64>) -> Self {
        Self {
            response,
            socket_id,
            provenance: None,
        }
    }

    fn into_response(self) -> CodexResponse {
        self.response
    }
}

impl std::ops::Deref for OwnerAwareCodexResponse {
    type Target = CodexResponse;

    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

const MAX_BUFFERED_TRANSPORT_RETRIES: u32 = 3;
const MAX_BUFFERED_TRANSPORT_ATTEMPTS: u32 = MAX_BUFFERED_TRANSPORT_RETRIES + 1;
const HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS: u64 = 300_000;
const IMAGE_HEADER_TIMEOUT_MS: u64 = 300_000;
/// Marks the error raised when the wait for the Codex response headers runs
/// out, so the retry classifiers can tell it apart from a transport failure.
const HTTP_RESPONSE_HEADERS_DETAIL: &str = "http_response_headers";

#[derive(Clone)]
struct ProxyEnvironment {
    http_proxy: Option<String>,
    https_proxy: Option<String>,
    all_proxy: Option<String>,
    no_proxy: Option<reqwest::NoProxy>,
    no_proxy_value: Option<String>,
}

impl ProxyEnvironment {
    fn from_env() -> Self {
        if std::env::var_os("REQUEST_METHOD").is_some() {
            return Self {
                http_proxy: None,
                https_proxy: None,
                all_proxy: None,
                no_proxy: None,
                no_proxy_value: None,
            };
        }

        let no_proxy_value = std::env::var("NO_PROXY")
            .or_else(|_| std::env::var("no_proxy"))
            .ok();
        Self {
            http_proxy: proxy_env_value("HTTP_PROXY", "http_proxy")
                .unwrap_or_else(|name| panic!("invalid {name} proxy URL")),
            https_proxy: proxy_env_value("HTTPS_PROXY", "https_proxy")
                .unwrap_or_else(|name| panic!("invalid {name} proxy URL")),
            all_proxy: proxy_env_value("ALL_PROXY", "all_proxy")
                .unwrap_or_else(|name| panic!("invalid {name} proxy URL")),
            no_proxy: no_proxy_value
                .as_deref()
                .and_then(reqwest::NoProxy::from_string),
            no_proxy_value,
        }
    }

    fn websocket_proxy_config(&self) -> super::websocket::WebSocketProxyConfig {
        super::websocket::WebSocketProxyConfig::new(
            self.http_proxy.as_deref(),
            self.https_proxy.as_deref(),
            self.all_proxy.as_deref(),
            self.no_proxy_value.as_deref(),
        )
    }

    fn apply(&self, mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        builder = builder.no_proxy();
        if let Some(proxy) = self.http_proxy.as_deref() {
            builder = builder.proxy(
                reqwest::Proxy::http(proxy)
                    .expect("validated HTTP_PROXY URL")
                    .no_proxy(self.no_proxy.clone()),
            );
        }
        if let Some(proxy) = self.https_proxy.as_deref() {
            builder = builder.proxy(
                reqwest::Proxy::https(proxy)
                    .expect("validated HTTPS_PROXY URL")
                    .no_proxy(self.no_proxy.clone()),
            );
        }
        if let Some(proxy) = self.all_proxy.as_deref() {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy)
                    .expect("validated ALL_PROXY URL")
                    .no_proxy(self.no_proxy.clone()),
            );
        }
        builder
    }
}

fn native_http_client(proxy_environment: &ProxyEnvironment) -> reqwest::Client {
    proxy_environment
        .apply(
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none()),
        )
        .build()
        .expect("failed to create native Responses HTTP client")
}

fn proxy_env_value(
    uppercase: &'static str,
    lowercase: &'static str,
) -> Result<Option<String>, &'static str> {
    let Some(raw) = std::env::var_os(uppercase).or_else(|| std::env::var_os(lowercase)) else {
        return Ok(None);
    };
    let raw = raw.into_string().map_err(|_| uppercase)?;
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    normalize_proxy_url(raw).map(Some).ok_or(uppercase)
}

fn normalize_proxy_url(raw: &str) -> Option<String> {
    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let parsed = url::Url::parse(&candidate).ok()?;
    if !matches!(
        parsed.scheme(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
    ) || parsed.host_str().is_none()
        || matches!(parsed.scheme(), "socks4" | "socks4a")
            && (!parsed.username().is_empty() || parsed.password().is_some())
    {
        return None;
    }
    Some(parsed.to_string())
}

fn websocket_http_client(proxy_environment: &ProxyEnvironment) -> reqwest::Client {
    let tls_config = super::websocket::websocket_tls_config();
    proxy_environment
        .apply(
            reqwest::Client::builder()
                .http1_only()
                .redirect(reqwest::redirect::Policy::none())
                .use_preconfigured_tls((*tls_config).clone()),
        )
        .build()
        .expect("failed to create Codex WebSocket HTTP client")
}

pub struct CodexHttpClient {
    client: reqwest::Client,
    pub(super) native_client: reqwest::Client,
    websocket_client: reqwest::Client,
    websocket_proxy_config: super::websocket::WebSocketProxyConfig,
    pub(super) auth_manager: CodexAuthManager<DefaultCodexAuthStore>,
    pub(super) base_url: String,
    pub(super) model_cache: super::models::ModelCache,
    header_timeout_ms: u64,
    body_idle_timeout_ms: u64,
}

impl Default for CodexHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexHttpClient {
    pub fn new() -> Self {
        let timeout_ms = config::codex_header_timeout_ms();
        let proxy_environment = ProxyEnvironment::from_env();
        Self {
            client: proxy_environment
                .apply(reqwest::Client::builder().connect_timeout(Duration::from_secs(15)))
                .build()
                .expect("failed to create HTTP client"),
            native_client: native_http_client(&proxy_environment),
            websocket_client: websocket_http_client(&proxy_environment),
            websocket_proxy_config: proxy_environment.websocket_proxy_config(),
            model_cache: Default::default(),
            auth_manager: CodexAuthManager::new(file_store()),
            base_url: config::codex_base_url(CODEX_API_ENDPOINT),
            header_timeout_ms: timeout_ms,
            body_idle_timeout_ms: HTTP_RESPONSE_BODY_IDLE_TIMEOUT_MS,
        }
    }

    pub fn body_idle_timeout_ms(&self) -> u64 {
        self.body_idle_timeout_ms
    }

    pub(crate) async fn post_transcription(
        &self,
        base_url: &str,
        input: &super::transcription::PreparedTranscription,
        ctx: &RequestContext,
    ) -> Result<reqwest::Response, CodexError> {
        let url = format!("{}/transcribe", base_url.trim_end_matches('/'));
        let mut auth = self.operation_auth().await.map_err(|error| CodexError {
            status: 401,
            message: "Auth error".to_string(),
            detail: Some(error.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Auth,
        })?;
        let mut refresh_attempted = false;

        loop {
            let headers = build_codex_transcription_headers(&auth, ctx)?;
            let response = self.attempt_transcription(&url, &headers, input).await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refresh_attempted {
                refresh_attempted = true;
                drop(response);
                auth = self
                    .refresh_operation_auth(&auth)
                    .await
                    .map_err(auth_refresh_error)?;
                continue;
            }
            return Ok(response);
        }
    }

    async fn attempt_transcription(
        &self,
        url: &str,
        headers: &http::HeaderMap,
        input: &super::transcription::PreparedTranscription,
    ) -> Result<reqwest::Response, CodexError> {
        let part = reqwest::multipart::Part::stream(input.audio.clone())
            .file_name(input.filename.clone())
            .mime_str(&input.content_type)
            .map_err(|error| CodexError {
                status: 400,
                message: "Invalid audio content type".to_string(),
                detail: Some(error.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            })?;
        let mut form = reqwest::multipart::Form::new().part("file", part);
        if let Some(language) = input.language.as_deref() {
            form = form.text("language", language.to_string());
        }
        let mut request = self.native_client.post(url).multipart(form);
        for (key, value) in headers {
            request = request.header(key.as_str(), value.as_bytes());
        }
        tokio::time::timeout(
            Duration::from_millis(IMAGE_HEADER_TIMEOUT_MS),
            request.send(),
        )
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!(
                "Timed out waiting {}ms for Codex transcription response headers",
                IMAGE_HEADER_TIMEOUT_MS
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })?
        .map_err(|error| CodexError {
            status: 0,
            message: format!("Codex transcription transport error: {error}"),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })
    }

    pub(crate) async fn post_image_json(
        &self,
        base_url: &str,
        operation: super::images::ImageOperation,
        body: &serde_json::Value,
        ctx: &RequestContext,
    ) -> Result<reqwest::Response, CodexError> {
        let body_json = serde_json::to_vec(body)
            .map(|body| Bytes::from(body.into_boxed_slice()))
            .map_err(|error| CodexError {
                status: 500,
                message: "Failed to serialize image request".to_string(),
                detail: Some(error.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            })?;
        let url = format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            operation.upstream_path()
        );
        let mut auth = self.operation_auth().await.map_err(|error| CodexError {
            status: 401,
            message: "Auth error".to_string(),
            detail: Some(error.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Auth,
        })?;
        let mut refresh_attempted = false;

        loop {
            let headers = build_codex_image_headers(&auth, ctx)?;
            let response = self
                .attempt_image_json(&url, &headers, body_json.clone())
                .await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refresh_attempted {
                refresh_attempted = true;
                drop(response);
                auth = self
                    .refresh_operation_auth(&auth)
                    .await
                    .map_err(auth_refresh_error)?;
                continue;
            }
            return Ok(response);
        }
    }

    async fn attempt_image_json(
        &self,
        url: &str,
        headers: &http::HeaderMap,
        body_json: Bytes,
    ) -> Result<reqwest::Response, CodexError> {
        let mut request = self.native_client.post(url);
        for (key, value) in headers {
            request = request.header(key.as_str(), value.as_bytes());
        }
        tokio::time::timeout(
            Duration::from_millis(IMAGE_HEADER_TIMEOUT_MS),
            request.body(body_json).send(),
        )
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!(
                "Timed out waiting {}ms for Codex image response headers",
                IMAGE_HEADER_TIMEOUT_MS
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })?
        .map_err(|error| CodexError {
            status: 0,
            message: format!("Codex image transport error: {error}"),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })
    }

    pub async fn post_native_responses(
        &self,
        body: &serde_json::Value,
        ctx: &RequestContext,
        use_responses_lite: bool,
        stream: bool,
    ) -> Result<reqwest::Response, CodexError> {
        self.post_native_responses_with_turn_state(body, ctx, use_responses_lite, stream, None)
            .await
    }

    pub(crate) async fn post_native_responses_with_turn_state(
        &self,
        body: &serde_json::Value,
        ctx: &RequestContext,
        use_responses_lite: bool,
        stream: bool,
        turn_state: Option<http::HeaderValue>,
    ) -> Result<reqwest::Response, CodexError> {
        let body_json = serde_json::to_vec(body)
            .map(|body| Bytes::from(body.into_boxed_slice()))
            .map_err(|err| CodexError {
                status: 500,
                message: "Failed to serialize native Responses request".to_string(),
                detail: Some(err.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            })?;
        let mut auth = self.operation_auth().await.map_err(|err| CodexError {
            status: 401,
            message: "Auth error".to_string(),
            detail: Some(err.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Auth,
        })?;
        if body
            .get("input")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|input| {
                input.iter().any(|item| {
                    matches!(
                        item.get("type").and_then(serde_json::Value::as_str),
                        Some("compaction" | "compaction_trigger")
                    )
                })
            })
        {
            Arc::make_mut(&mut auth)
                .require_opaque_provenance(&self.base_url)
                .map_err(auth_refresh_error)?;
        }

        let mut refresh_attempted = false;

        loop {
            let started_at = Instant::now();
            let mut headers = build_native_codex_headers(&auth, ctx, use_responses_lite, stream)?;
            if let Some(turn_state) = turn_state.as_ref() {
                headers.insert("x-codex-turn-state", turn_state.clone());
            }
            if let Some(traffic) = ctx.traffic.as_deref() {
                write_codex_http_request_capture(traffic, &self.base_url, &headers, &body_json);
            }

            let response = self
                .attempt_native_responses(&headers, body_json.clone())
                .await?;
            let status = response.status().as_u16();
            if status == 401 && !refresh_attempted {
                refresh_attempted = true;
                drop(response);
                auth = self
                    .refresh_operation_auth(&auth)
                    .await
                    .map_err(auth_refresh_error)?;
                continue;
            }

            if let Some(traffic) = ctx.traffic.as_deref() {
                write_live_upstream_response_headers(traffic, &response, started_at.elapsed());
            }
            return Ok(response);
        }
    }

    async fn attempt_native_responses(
        &self,
        headers: &http::HeaderMap,
        body_json: Bytes,
    ) -> Result<reqwest::Response, CodexError> {
        let mut request = self.native_client.post(&self.base_url);
        for (key, value) in headers {
            request = request.header(key.as_str(), value.as_bytes());
        }

        tokio::time::timeout(
            Duration::from_millis(self.header_timeout_ms),
            request.body(body_json).send(),
        )
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!(
                "Timed out waiting {}ms for Codex response headers",
                self.header_timeout_ms
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })?
        .map_err(|err| CodexError {
            status: 0,
            message: format!("Native Responses transport error: {err}"),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })
    }

    pub async fn post_search(
        &self,
        body: &SearchRequest,
        ctx: &RequestContext,
    ) -> Result<SearchResponse, CodexError> {
        let mut auth = self.operation_auth().await.map_err(|e| CodexError {
            status: 401,
            message: "Auth error".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Auth,
        })?;
        Arc::make_mut(&mut auth)
            .require_opaque_provenance(&self.base_url)
            .map_err(auth_refresh_error)?;
        let body_json = serde_json::to_vec(body)
            .map(|body| Bytes::from(body.into_boxed_slice()))
            .map_err(|e| CodexError {
                status: 500,
                message: "Failed to serialize search request".to_string(),
                detail: Some(e.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            })?;
        let mut auth_refresh_attempted = false;
        let mut retries = 0_u32;

        loop {
            let response = self
                .attempt_post_search(&auth, &body_json, ctx, &body.id)
                .await?;
            if response.status == 401 && !auth_refresh_attempted {
                auth_refresh_attempted = true;
                auth = self
                    .refresh_operation_auth(&auth)
                    .await
                    .map_err(auth_refresh_error)?;
                continue;
            }
            if should_retry_codex_status(response.status)
                && retries < MAX_BUFFERED_TRANSPORT_RETRIES
            {
                let retry_after = response
                    .headers
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
                    .map(|(_, value)| value.as_str());
                let delay = compute_backoff_delay(retries, retry_after);
                if delay.exceeds_budget {
                    return Err(codex_status_error(response));
                }
                retries += 1;
                sleep(delay.wait_ms).await;
                continue;
            }
            if !(200..300).contains(&response.status) {
                return Err(codex_status_error(response));
            }
            return serde_json::from_slice(&response.body).map_err(|e| CodexError {
                status: 502,
                message: "Failed to decode Codex search response".to_string(),
                detail: Some(e.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            });
        }
    }

    async fn stream_codex_http_events_inner(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        provenance_publisher: super::websocket::CodexWebSocketSocketIdPublisher,
    ) -> Result<CodexHttpEventReceiver, CodexError> {
        let mut auth = self.operation_auth().await.map_err(|e| CodexError {
            status: 401,
            message: "Auth error".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Auth,
        })?;
        if has_opaque_native_history(body) {
            Arc::make_mut(&mut auth)
                .require_opaque_provenance(&self.base_url)
                .map_err(auth_refresh_error)?;
        }

        let body_json = serde_json::to_vec(body)
            .map(|body| Bytes::from(body.into_boxed_slice()))
            .map_err(|e| CodexError {
                status: 500,
                message: "Failed to serialize request".to_string(),
                detail: Some(e.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            })?;
        let mut auth_refresh_attempted = false;
        let use_responses_lite = body.client_metadata.is_some();
        let codex_auto_review = body.codex_auto_review && body.model == "codex-auto-review";
        let mut retries = 0_u32;
        let retry_started_at = Instant::now();
        let log_retry = |error: &CodexError, failed_attempt, delay_ms, stop_reason| {
            log_http_stream_retry(
                ctx,
                &body.model,
                "startup",
                retry_started_at,
                failed_attempt,
                error,
                delay_ms,
                stop_reason,
            );
        };
        let (resp, response_headers, started_at) = loop {
            match self
                .start_http_event_attempt(
                    &mut auth,
                    &body_json,
                    ctx,
                    use_responses_lite,
                    codex_auto_review,
                    &mut auth_refresh_attempted,
                )
                .await
            {
                Ok(attempt) => break attempt,
                Err(error) if retryable_http_stream_error(&error) => {
                    if retries >= MAX_BUFFERED_TRANSPORT_RETRIES {
                        log_retry(&error, retries + 1, None, Some("attempts_exhausted"));
                        return Err(error);
                    }
                    let delay = compute_backoff_delay(retries, error.retry_after.as_deref());
                    if delay.exceeds_budget {
                        log_retry(
                            &error,
                            retries + 1,
                            Some(delay.wait_ms),
                            Some("delay_budget_exceeded"),
                        );
                        return Err(error);
                    }
                    log_retry(&error, retries + 1, Some(delay.wait_ms), None);
                    retries += 1;
                    sleep(delay.wait_ms).await;
                }
                Err(error) => {
                    log_retry(&error, retries + 1, None, Some("not_retryable"));
                    return Err(error);
                }
            }
        };

        provenance_publisher.publish_provenance(auth.provenance(&self.base_url));
        Ok(self.spawn_http_event_stream(
            HttpEventStreamState {
                provenance_publisher,
                resp,
                response_headers,
                started_at,
                body_json,
                auth,
                auth_refresh_attempted,
                use_responses_lite,
                codex_auto_review,
                retries,
                model: body.model.clone(),
            },
            ctx.clone(),
        ))
    }

    pub(crate) async fn stream_codex_http_events_for_owner(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
    ) -> Result<super::websocket::CodexWebSocketEventStream, CodexError> {
        let (_tx, receiver) = tokio::sync::mpsc::channel(1);
        let (mut stream, publisher) =
            super::websocket::CodexWebSocketEventStream::pending(receiver);
        let receiver = self
            .stream_codex_http_events_inner(body, ctx, publisher)
            .await?;
        stream.replace_receiver(receiver);
        Ok(stream)
    }

    async fn start_http_event_attempt(
        &self,
        auth: &mut Arc<OperationAuth>,
        body_json: &Bytes,
        ctx: &RequestContext,
        use_responses_lite: bool,
        codex_auto_review: bool,
        auth_refresh_attempted: &mut bool,
    ) -> Result<(reqwest::Response, Vec<(String, String)>, Instant), CodexError> {
        loop {
            let (resp, started_at) = self
                .start_post_http(auth, body_json, ctx, use_responses_lite, codex_auto_review)
                .await?;

            if resp.status().as_u16() == 401 && !*auth_refresh_attempted {
                *auth_refresh_attempted = true;
                *auth = self
                    .refresh_operation_auth(auth)
                    .await
                    .map_err(auth_refresh_error)?;
                continue;
            }

            if !resp.status().is_success() {
                let response = self.collect_http_response(resp, started_at, ctx).await?;
                let mut error = codex_status_error(response);
                error.origin = CodexErrorOrigin::Http;
                return Err(error);
            }

            let status = resp.status().as_u16();
            let headers = response_headers(&resp);
            if let Some(traffic) = ctx.traffic.as_deref() {
                write_upstream_response_headers_capture(
                    traffic,
                    status,
                    started_at.elapsed(),
                    &headers,
                );
            }
            return Ok((resp, headers, started_at));
        }
    }

    pub(crate) async fn stream_codex_auto_events_for_owner(
        self: &Arc<Self>,
        body: Arc<ResponsesRequest>,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
    ) -> Result<super::websocket::CodexWebSocketEventStream, CodexError> {
        let mut websocket = self
            .stream_codex_websocket_events_for_owner(Arc::clone(&body), ctx, continuation)
            .await?;
        match websocket.recv().await {
            Some(Err(err)) if should_fallback_to_http(&err) => {
                self.stream_codex_http_events_for_owner(&body, ctx).await
            }
            Some(item) => {
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                let receiver = websocket.replace_receiver(rx);
                let telemetry = super::rate_limits::current();
                tokio::spawn(super::rate_limits::with_telemetry(telemetry, async move {
                    if tx.send(item).await.is_ok() {
                        forward_codex_events(receiver, tx).await;
                    }
                }));
                Ok(websocket)
            }
            None => Err(CodexError {
                status: 0,
                message: "WebSocket connection closed before the first Codex event".to_string(),
                detail: Some(super::websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::WebSocket,
            }),
        }
    }

    fn spawn_http_event_stream(
        self: &Arc<Self>,
        state: HttpEventStreamState,
        ctx: RequestContext,
    ) -> CodexHttpEventReceiver {
        let HttpEventStreamState {
            provenance_publisher,
            mut resp,
            mut response_headers,
            mut started_at,
            body_json,
            mut auth,
            mut auth_refresh_attempted,
            use_responses_lite,
            codex_auto_review,
            mut retries,
            model,
        } = state;
        let client = self.clone();
        let body_idle_timeout_ms = self.body_idle_timeout_ms;
        let req_id = ctx.req_id.clone();
        let traffic = ctx.traffic.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(1);

        let telemetry = super::rate_limits::current();
        tokio::spawn(super::rate_limits::with_telemetry(telemetry, async move {
            let log = create_logger("codex");
            let mut semantic_output_forwarded = false;
            let mut replay_committed = false;
            let retry_started_at = started_at;
            let log_retry = |error: &CodexError, failed_attempt, delay_ms, stop_reason| {
                log_http_stream_retry(
                    &ctx,
                    &model,
                    "reopen",
                    retry_started_at,
                    failed_attempt,
                    error,
                    delay_ms,
                    stop_reason,
                );
            };

            if tx
                .send(Ok(serde_json::json!({
                    "type": "keepalive",
                    "_ccp_synthetic": true
                })))
                .await
                .is_err()
            {
                return;
            }

            'attempts: loop {
                let mut decoder = HttpSseDecoder::default();
                let mut body_bytes = 0_u64;
                let mut body_chunks = 0_u64;
                let mut event_count = 0_u64;
                let mut pending_events = PendingHttpEvents::default();
                let mut provisional_tools = PendingHttpProvisionalTools::default();

                let mut retry_error = 'read_attempt: loop {
                    let chunk = tokio::select! {
                        _ = tx.closed() => {
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_dropped",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                None,
                            );
                            return;
                        }
                        chunk = tokio::time::timeout(
                            Duration::from_millis(body_idle_timeout_ms),
                            resp.chunk(),
                        ) => chunk
                    };

                    let chunk = match chunk {
                        Ok(Ok(Some(chunk))) => chunk,
                        Ok(Ok(None)) => {
                            let error = match decoder.finish() {
                                Ok(()) => http_sse_error(
                                    "Codex SSE stream ended before a terminal response event",
                                ),
                                Err(error) => error,
                            };
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_failed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                Some(&error.message),
                            );
                            if !replay_committed && retryable_http_stream_error(&error) {
                                break 'read_attempt error;
                            }
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                        Ok(Err(err)) => {
                            let error = CodexError {
                                status: 0,
                                message: format!(
                                    "Transport error reading Codex response body: {err}"
                                ),
                                detail: Some("http_response_body".to_string()),
                                retry_after: None,
                                usage_limit: None,
                                origin: CodexErrorOrigin::Http,
                            };
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_failed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                Some(&error.message),
                            );
                            if !replay_committed {
                                break 'read_attempt error;
                            }
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                        Err(_) => {
                            let error = CodexError {
                                status: 0,
                                message: format!(
                                    "Timed out waiting {body_idle_timeout_ms}ms for the next Codex response body chunk"
                                ),
                                detail: Some("http_response_body".to_string()),
                                retry_after: None,
                                usage_limit: None,
                                origin: CodexErrorOrigin::Http,
                            };
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_failed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                Some(&error.message),
                            );
                            if !replay_committed {
                                break 'read_attempt error;
                            }
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    };

                    body_bytes = body_bytes.saturating_add(chunk.len() as u64);
                    body_chunks = body_chunks.saturating_add(1);
                    let events = match decoder.push(&chunk) {
                        Ok(events) => events,
                        Err(error) => {
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_failed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                Some(&error.message),
                            );
                            if !replay_committed && retryable_http_stream_error(&error) {
                                break 'read_attempt error;
                            }
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    };

                    for event in events {
                        let Some(mut payload) = event.payload else {
                            continue;
                        };
                        event_count = event_count.saturating_add(1);
                        if let Some(traffic) = traffic.as_deref() {
                            write_codex_http_sse_event_capture(
                                traffic,
                                event.event.as_deref(),
                                &payload,
                            );
                        }

                        super::record_upstream_telemetry(&ctx, &payload);
                        let event_kind = super::events::classify_stream_event(&payload);
                        let failure = super::events::classify_event_failure(&payload);
                        if !semantic_output_forwarded
                            && let Some(limit) = super::events::usage_limit_from_event_with_headers(
                                &payload,
                                &response_headers,
                            )
                        {
                            pending_events.clear();
                            let _ = tx
                                .send(Err(codex_usage_limit_error(limit, CodexErrorOrigin::Http)))
                                .await;
                            return;
                        }
                        let provisional_event = match provisional_tools.observe(&mut payload) {
                            Ok(event) => event,
                            Err(error) => {
                                let _ = tx.send(Err(error)).await;
                                return;
                            }
                        };
                        if let Err(error) = ensure_pending_http_buffer_budget(
                            pending_events.event_bytes,
                            provisional_tools.buffered_bytes(),
                        ) {
                            let _ = tx.send(Err(error)).await;
                            return;
                        }

                        // Provisional arguments never establish success, even when a
                        // Read path already looks complete. Wait for authoritative done.
                        if matches!(
                            provisional_event,
                            PendingHttpProvisionalToolEvent::Provisional
                        ) {
                            continue;
                        }

                        let commits_replay = http_event_commits_replay(&payload, event_kind);
                        replay_committed |= commits_replay;

                        if !semantic_output_forwarded
                            && !replay_committed
                            && let Some(failure) = failure.as_ref()
                            && failure.retryable()
                        {
                            pending_events.clear();
                            break 'read_attempt codex_event_failure_error(failure.clone());
                        }
                        if !semantic_output_forwarded
                            && replay_committed
                            && let Some(failure) = failure.as_ref()
                        {
                            let _ = tx
                                .send(Err(codex_event_failure_error(failure.clone())))
                                .await;
                            return;
                        }

                        let terminal = super::events::event_is_terminal(&payload);
                        match event_kind {
                            super::events::CodexStreamEventKind::TerminalFailure => {
                                provisional_tools.clear();
                                if !semantic_output_forwarded {
                                    pending_events.clear();
                                }
                                if tx.send(Ok(payload)).await.is_err() {
                                    return;
                                }
                            }
                            super::events::CodexStreamEventKind::TerminalSuccess => {
                                provisional_tools.clear();
                                for pending in pending_events.take() {
                                    if tx.send(Ok(pending)).await.is_err() {
                                        return;
                                    }
                                }
                                if tx.send(Ok(payload)).await.is_err() {
                                    return;
                                }
                            }
                            super::events::CodexStreamEventKind::Control => {
                                if tx.send(Ok(payload)).await.is_err() {
                                    return;
                                }
                            }
                            super::events::CodexStreamEventKind::Semantic
                            | super::events::CodexStreamEventKind::Structural => {
                                if semantic_output_forwarded {
                                    if tx.send(Ok(payload)).await.is_err() {
                                        return;
                                    }
                                } else {
                                    if let Err(error) = pending_events
                                        .push(payload, provisional_tools.buffered_bytes())
                                    {
                                        let _ = tx.send(Err(error)).await;
                                        return;
                                    }
                                    if commits_replay {
                                        semantic_output_forwarded = true;
                                        for pending in pending_events.take() {
                                            if tx.send(Ok(pending)).await.is_err() {
                                                return;
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if terminal {
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_completed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                None,
                            );
                            return;
                        }
                    }
                };

                loop {
                    if retries >= MAX_BUFFERED_TRANSPORT_RETRIES {
                        log_retry(&retry_error, retries + 1, None, Some("attempts_exhausted"));
                        let _ = tx.send(Err(retry_error)).await;
                        return;
                    }
                    let delay = compute_backoff_delay(retries, retry_error.retry_after.as_deref());
                    if delay.exceeds_budget {
                        log_retry(
                            &retry_error,
                            retries + 1,
                            Some(delay.wait_ms),
                            Some("delay_budget_exceeded"),
                        );
                        let _ = tx.send(Err(retry_error)).await;
                        return;
                    }
                    log_retry(&retry_error, retries + 1, Some(delay.wait_ms), None);
                    retries += 1;
                    tokio::select! {
                        _ = tx.closed() => {
                            log_retry(&retry_error, retries, Some(delay.wait_ms), Some("receiver_closed_during_backoff"));
                            return;
                        },
                        _ = sleep(delay.wait_ms) => {}
                    }

                    let next_attempt = tokio::select! {
                        _ = tx.closed() => {
                            log_retry(&retry_error, retries, Some(delay.wait_ms), Some("receiver_closed_during_reopen"));
                            return;
                        },
                        result = client.start_http_event_attempt(
                            &mut auth,
                            &body_json,
                            &ctx,
                            use_responses_lite,
                            codex_auto_review,
                            &mut auth_refresh_attempted,
                        ) => result
                    };
                    match next_attempt {
                        Ok((next_resp, next_response_headers, next_started_at)) => {
                            provenance_publisher
                                .publish_provenance(auth.provenance(&client.base_url));
                            resp = next_resp;
                            response_headers = next_response_headers;
                            started_at = next_started_at;
                            continue 'attempts;
                        }
                        Err(error) if retryable_http_stream_error(&error) => {
                            retry_error = error;
                        }
                        Err(error) => {
                            log_retry(&error, retries + 1, None, Some("not_retryable"));
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    }
                }
            }
        }));

        rx
    }

    pub(crate) async fn post_codex_for_owner(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
    ) -> Result<OwnerAwareCodexResponse, CodexError> {
        use crate::config::CodexTransport;

        let transport = config::codex_transport();
        let codex_auto_review = body.codex_auto_review && body.model == "codex-auto-review";
        // Reviewer handshake headers must never share an ordinary pooled socket.
        let continuation = continuation.filter(|_| !codex_auto_review);

        let mut auth = self.operation_auth().await.map_err(|e| CodexError {
            status: 401,
            message: "Auth error".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Auth,
        })?;
        if body.input.iter().any(|item| {
            matches!(
                item,
                super::translate::request::ResponsesInputItem::Compaction { .. }
                    | super::translate::request::ResponsesInputItem::CompactionTrigger
            )
        }) {
            Arc::make_mut(&mut auth)
                .require_opaque_provenance(&self.base_url)
                .map_err(auth_refresh_error)?;
        }

        let initial_pool_owner = websocket_pool_owner(continuation).cloned();
        if should_reset_websocket_pool(continuation)
            && let Some(owner) = initial_pool_owner.as_ref()
        {
            super::websocket::invalidate_codex_websocket_pool_turn_for_owner(
                owner,
                continuation.and_then(super::continuation::ContinuationReservation::turn_id),
            );
        }

        let mut active_continuation = continuation.cloned();
        let mut auth_refresh_attempted = false;
        let mut transport_failures = 0u32;
        let mut http_body_json = None;
        let mut serialize_http_body = || -> Result<Bytes, CodexError> {
            if let Some(body_json) = &http_body_json {
                return Ok(Bytes::clone(body_json));
            }
            let body_json = serde_json::to_vec(body)
                .map(|body| Bytes::from(body.into_boxed_slice()))
                .map_err(|e| CodexError {
                    status: 500,
                    message: "Failed to serialize request".to_string(),
                    detail: Some(e.to_string()),
                    retry_after: None,
                    usage_limit: None,
                    origin: CodexErrorOrigin::Http,
                })?;
            http_body_json = Some(body_json.clone());
            Ok(body_json)
        };
        loop {
            let result = match transport {
                CodexTransport::Http => {
                    let body_json = serialize_http_body()?;
                    self.attempt_post_http(
                        &auth,
                        &body_json,
                        ctx,
                        body.client_metadata.is_some(),
                        codex_auto_review,
                    )
                    .await
                    .map(|response| OwnerAwareCodexResponse::new(response, None))
                }
                CodexTransport::WebSocket | CodexTransport::Auto => {
                    let ws_headers = build_codex_response_headers(
                        &auth,
                        ctx,
                        body.client_metadata.is_some(),
                        codex_auto_review,
                        &self.base_url,
                    )?;
                    let ws_headers = super::websocket::codex_websocket_headers(&ws_headers);
                    let ws_body = build_websocket_request(
                        body,
                        active_continuation
                            .as_ref()
                            .map(super::continuation::ContinuationReservation::candidate),
                    );

                    // Try WebSocket first
                    let ws_result = super::websocket::codex_websocket_request(
                        &self.websocket_client,
                        &self.websocket_proxy_config,
                        &self.base_url,
                        &ws_headers,
                        &ws_body,
                        ctx.traffic.as_deref(),
                        super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                        super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                        active_continuation.as_ref(),
                        auth.provenance(&self.base_url),
                    )
                    .await;

                    match ws_result {
                        Ok(response) => Ok(response),
                        Err(err)
                            if should_retry_without_continuation(
                                &err,
                                active_continuation.as_ref(),
                            ) =>
                        {
                            // Drop stale continuation state before considering a
                            // replacement transport or connection.
                            Err(err)
                        }
                        Err(err)
                            if transport == CodexTransport::Auto
                                && should_fallback_to_http(&err) =>
                        {
                            // Fall back to HTTP only if WebSocket failed before sending
                            let body_json = serialize_http_body()?;
                            self.attempt_post_http(
                                &auth,
                                &body_json,
                                ctx,
                                body.client_metadata.is_some(),
                                codex_auto_review,
                            )
                            .await
                            .map(|response| OwnerAwareCodexResponse::new(response, None))
                        }
                        Err(err) => Err(err),
                    }
                }
            };

            if let Ok(response) = &result {
                let text = String::from_utf8_lossy(&response.body);
                for event in sse_events(&text) {
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) {
                        super::record_upstream_telemetry(ctx, &payload);
                    }
                }
            }

            if should_refresh_after_unauthorized(&result, auth_refresh_attempted, transport) {
                auth_refresh_attempted = true;
                match self.refresh_operation_auth(&auth).await {
                    Ok(new_auth) => {
                        auth = new_auth;
                        invalidate_live_continuation_pool(active_continuation.as_ref());
                        active_continuation =
                            full_context_continuation(active_continuation.as_ref());
                        continue;
                    }
                    Err(e) => {
                        return Err(auth_refresh_error(e));
                    }
                }
            }

            if let Ok(response) = &result
                && let Some(limit) =
                    super::events::usage_limit_from_response(&response.body, &response.headers)
            {
                let origin = match response.transport {
                    ActualTransport::Http => CodexErrorOrigin::BufferedHttp,
                    ActualTransport::WebSocket => CodexErrorOrigin::BufferedWebSocket,
                };
                return Err(codex_usage_limit_error(limit, origin));
            }

            if let Ok(response) = &result
                && let Some(failure) = super::events::first_event_failure(&response.body)
            {
                if !failure.retryable() {
                    let mut error = codex_event_failure_error(failure);
                    error.origin = match response.transport {
                        ActualTransport::Http => CodexErrorOrigin::BufferedHttp,
                        ActualTransport::WebSocket => CodexErrorOrigin::BufferedWebSocket,
                    };
                    return Err(error);
                }
                if transport_failures < MAX_BUFFERED_TRANSPORT_RETRIES {
                    let delay =
                        compute_backoff_delay(transport_failures, failure.retry_after.as_deref());
                    if delay.exceeds_budget {
                        return Err(CodexError {
                            status: failure.status,
                            message: failure.message.clone(),
                            detail: Some(failure.message),
                            retry_after: failure.retry_after,
                            usage_limit: None,
                            origin: match response.transport {
                                ActualTransport::Http => CodexErrorOrigin::BufferedHttp,
                                ActualTransport::WebSocket => CodexErrorOrigin::BufferedWebSocket,
                            },
                        });
                    }
                    log_buffered_retry(
                        ctx,
                        transport,
                        transport_failures + 1,
                        delay.wait_ms,
                        failure.status,
                        "upstream_event",
                        &failure.message,
                    );
                    transport_failures += 1;
                    active_continuation = full_context_continuation(active_continuation.as_ref());
                    sleep(delay.wait_ms).await;
                    continue;
                }

                log_buffered_retry_exhausted(
                    ctx,
                    transport,
                    failure.status,
                    "upstream_event",
                    &failure.message,
                );
                return Err(CodexError {
                    status: failure.status,
                    message: failure.message.clone(),
                    detail: Some(failure.message),
                    retry_after: failure.retry_after,
                    usage_limit: None,
                    origin: CodexErrorOrigin::Http,
                });
            }

            match result {
                Ok(response) if response.status == 401 => {
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    return Err(CodexError {
                        status: 401,
                        message: "Unauthorized".to_string(),
                        detail: Some(detail),
                        retry_after: None,
                        usage_limit: None,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if response.status == 403 => {
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    return Err(CodexError {
                        status: 403,
                        message: "Forbidden".to_string(),
                        detail: Some(detail),
                        retry_after: None,
                        usage_limit: None,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if response.status == 429 => {
                    let retry_after = response
                        .headers
                        .iter()
                        .find(|(k, _)| k.to_lowercase() == "retry-after")
                        .map(|(_, v)| v.clone());
                    if transport_failures < MAX_BUFFERED_TRANSPORT_RETRIES {
                        let delay =
                            compute_backoff_delay(transport_failures, retry_after.as_deref());
                        if delay.exceeds_budget {
                            let detail = String::from_utf8_lossy(&response.body).to_string();
                            return Err(CodexError {
                                status: 429,
                                message: "Rate limited".to_string(),
                                detail: Some(detail),
                                retry_after,
                                usage_limit: None,
                                origin: CodexErrorOrigin::Http,
                            });
                        }
                        log_buffered_retry(
                            ctx,
                            transport,
                            transport_failures + 1,
                            delay.wait_ms,
                            response.status,
                            "upstream",
                            "rate limited",
                        );
                        transport_failures += 1;
                        sleep(delay.wait_ms).await;
                        continue;
                    }
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    log_buffered_retry_exhausted(
                        ctx,
                        transport,
                        response.status,
                        "upstream",
                        "rate limited",
                    );
                    return Err(CodexError {
                        status: 429,
                        message: "Rate limited".to_string(),
                        detail: Some(detail),
                        retry_after,
                        usage_limit: None,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if should_retry_codex_status(response.status) => {
                    if transport_failures < MAX_BUFFERED_TRANSPORT_RETRIES {
                        let retry_after = response
                            .headers
                            .iter()
                            .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
                            .map(|(_, value)| value.as_str());
                        let delay = compute_backoff_delay(transport_failures, retry_after);
                        if delay.exceeds_budget {
                            return Err(codex_status_error(response.into_response()));
                        }
                        log_buffered_retry(
                            ctx,
                            transport,
                            transport_failures + 1,
                            delay.wait_ms,
                            response.status,
                            "upstream",
                            "retryable upstream status",
                        );
                        transport_failures += 1;
                        sleep(delay.wait_ms).await;
                        continue;
                    }
                    log_buffered_retry_exhausted(
                        ctx,
                        transport,
                        response.status,
                        "upstream",
                        "retryable upstream status",
                    );
                    return Err(codex_status_error(response.into_response()));
                }
                Ok(response) if !(200..300).contains(&response.status) => {
                    return Err(codex_status_error(response.into_response()));
                }
                Ok(mut response) => {
                    response.provenance = auth.provenance(&self.base_url);
                    return Ok(response);
                }
                Err(err) if is_connection_limit_error(&err) => {
                    if transport_failures >= MAX_BUFFERED_TRANSPORT_RETRIES {
                        return Err(err);
                    }
                    let delay =
                        compute_backoff_delay(transport_failures, err.retry_after.as_deref());
                    if delay.exceeds_budget {
                        return Err(err);
                    }
                    transport_failures += 1;
                    active_continuation = full_context_continuation(active_continuation.as_ref());
                    sleep(delay.wait_ms).await;
                    continue;
                }
                Err(err)
                    if should_retry_without_continuation(&err, active_continuation.as_ref()) =>
                {
                    active_continuation = full_context_continuation(active_continuation.as_ref());
                    continue;
                }
                Err(err) => {
                    // Determine if retryable
                    let retryable = is_retryable_transport_error(&err);
                    if retryable && transport_failures < MAX_BUFFERED_TRANSPORT_RETRIES {
                        let delay =
                            compute_backoff_delay(transport_failures, err.retry_after.as_deref());
                        if delay.exceeds_budget {
                            return Err(err);
                        }
                        log_buffered_retry(
                            ctx,
                            transport,
                            transport_failures + 1,
                            delay.wait_ms,
                            err.status,
                            codex_error_origin_name(err.origin),
                            &err.message,
                        );
                        transport_failures += 1;
                        sleep(delay.wait_ms).await;
                        continue;
                    }
                    if retryable {
                        log_buffered_retry_exhausted(
                            ctx,
                            transport,
                            err.status,
                            codex_error_origin_name(err.origin),
                            &err.message,
                        );
                    }
                    return Err(err);
                }
            }
        }
    }

    pub(crate) async fn stream_codex_websocket_events_for_owner(
        self: &Arc<Self>,
        body: Arc<ResponsesRequest>,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
    ) -> Result<super::websocket::CodexWebSocketEventStream, CodexError> {
        let codex_auto_review = body.codex_auto_review && body.model == "codex-auto-review";
        let continuation = continuation.filter(|_| !codex_auto_review);
        let mut auth = self.operation_auth().await.map_err(|e| CodexError {
            status: 401,
            message: "Auth error".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Auth,
        })?;
        if has_opaque_native_history(&body) {
            Arc::make_mut(&mut auth)
                .require_opaque_provenance(&self.base_url)
                .map_err(auth_refresh_error)?;
        }

        let turn_id = continuation.and_then(super::continuation::ContinuationReservation::turn_id);
        if should_reset_websocket_pool(continuation)
            && let Some(owner) = websocket_pool_owner(continuation)
        {
            super::websocket::invalidate_codex_websocket_pool_turn_for_owner(owner, turn_id);
        }

        let client = self.clone();
        let ctx = ctx.clone();
        let continuation = continuation.cloned();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (rx, socket_id_publisher) = super::websocket::CodexWebSocketEventStream::pending(rx);
        let telemetry = super::rate_limits::current();
        tokio::spawn(super::rate_limits::with_telemetry(telemetry, async move {
            client
                .coordinate_live_websocket_events(
                    body,
                    ctx,
                    continuation,
                    auth,
                    tx,
                    socket_id_publisher,
                )
                .await;
        }));

        Ok(rx)
    }

    #[allow(clippy::too_many_arguments)]
    async fn coordinate_live_websocket_events(
        &self,
        body: Arc<ResponsesRequest>,
        ctx: RequestContext,
        mut continuation: Option<super::continuation::ContinuationReservation>,
        mut auth: Arc<OperationAuth>,
        tx: tokio::sync::mpsc::Sender<Result<serde_json::Value, CodexError>>,
        socket_id_publisher: super::websocket::CodexWebSocketSocketIdPublisher,
    ) {
        let mut auth_refresh_attempted = false;
        let mut continuation_retry_available = continuation
            .as_ref()
            .and_then(|reservation| reservation.candidate().previous_response_id.as_deref())
            .is_some();
        let mut forwarded_any = false;
        let mut connection_limit_retries = 0;

        'attempt: loop {
            socket_id_publisher.publish(None);
            socket_id_publisher.publish_provenance(auth.provenance(&self.base_url));
            let ws_headers = match build_codex_response_headers(
                &auth,
                &ctx,
                body.client_metadata.is_some(),
                body.codex_auto_review && body.model == "codex-auto-review",
                &self.base_url,
            ) {
                Ok(headers) => super::websocket::codex_websocket_headers(&headers),
                Err(err) => {
                    if tx.send(Err(err)).await.is_err() {
                        abort_abandoned_live_continuation(
                            continuation.as_ref(),
                            &socket_id_publisher,
                        );
                    }
                    return;
                }
            };
            let mut stream = {
                let ws_body = build_websocket_request(
                    &body,
                    continuation
                        .as_ref()
                        .map(super::continuation::ContinuationReservation::candidate),
                );
                let start = super::websocket::codex_websocket_event_stream(
                    &self.websocket_client,
                    &self.websocket_proxy_config,
                    &self.base_url,
                    &ws_headers,
                    &ws_body,
                    ctx.traffic.clone(),
                    super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                    super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                    continuation.as_ref(),
                    auth.provenance(&self.base_url),
                );
                tokio::select! {
                    biased;
                    _ = tx.closed() => {
                        abort_abandoned_live_continuation(
                            continuation.as_ref(),
                            &socket_id_publisher,
                        );
                        return;
                    }
                    result = start => match result {
                        Ok(stream) => stream,
                        Err(err) if err.status == 401 && !auth_refresh_attempted && !forwarded_any => {
                            auth_refresh_attempted = true;
                            invalidate_live_continuation_pool(continuation.as_ref());
                            let refresh = self.refresh_operation_auth(&auth);
                            auth = match refresh.await {
                                Ok(auth) => {
                                    if tx.is_closed() {
                                        abort_abandoned_live_continuation(
                                            continuation.as_ref(),
                                            &socket_id_publisher,
                                        );
                                        return;
                                    }
                                    auth
                                },
                                Err(refresh_err) => {
                                    if tx.send(Err(auth_refresh_error(refresh_err))).await.is_err() {
                                        abort_abandoned_live_continuation(
                                            continuation.as_ref(),
                                            &socket_id_publisher,
                                        );
                                    }
                                    return;
                                }
                            };
                            if continuation_retry_available {
                                socket_id_publisher.mark_full_context_retry();
                            }
                            continuation = full_context_continuation(continuation.as_ref());
                            continuation_retry_available = false;
                            continue 'attempt;
                        }
                        Err(err) if continuation_retry_available && is_continuation_retry_error(&err) => {
                            socket_id_publisher.mark_full_context_retry();
                            continuation = full_context_continuation(continuation.as_ref());
                            continuation_retry_available = false;
                            continue 'attempt;
                        }
                        Err(err) => {
                            if tx.send(Err(err)).await.is_err() {
                                abort_abandoned_live_continuation(
                                    continuation.as_ref(),
                                    &socket_id_publisher,
                                );
                            }
                            return;
                        }
                    }
                }
            };

            let mut pending_events = PendingHttpEvents::default();
            loop {
                let item = tokio::select! {
                    biased;
                    _ = tx.closed() => {
                        if let Some(reservation) = continuation.as_ref() {
                            super::websocket::invalidate_codex_websocket_pool_socket(
                                reservation,
                                stream.socket_id(),
                            );
                        }
                        abort_abandoned_live_continuation(
                            continuation.as_ref(),
                            &socket_id_publisher,
                        );
                        return;
                    }
                    item = stream.recv() => item,
                };
                if tx.is_closed() {
                    if let Some(reservation) = continuation.as_ref() {
                        super::websocket::invalidate_codex_websocket_pool_socket(
                            reservation,
                            stream.socket_id(),
                        );
                    }
                    abort_abandoned_live_continuation(continuation.as_ref(), &socket_id_publisher);
                    return;
                }
                let Some(item) = item else {
                    return;
                };
                socket_id_publisher.publish(stream.socket_id());
                if let Ok(payload) = &item {
                    super::record_upstream_telemetry(&ctx, payload);
                }

                let unauthorized = match &item {
                    Err(err) => err.status == 401,
                    Ok(payload) => super::websocket::event_error_status(payload) == Some(401),
                };
                if unauthorized && !auth_refresh_attempted && !forwarded_any {
                    auth_refresh_attempted = true;
                    invalidate_live_continuation_pool(continuation.as_ref());
                    let refresh = self.refresh_operation_auth(&auth);
                    auth = match refresh.await {
                        Ok(auth) => {
                            if tx.is_closed() {
                                if let Some(reservation) = continuation.as_ref() {
                                    super::websocket::invalidate_codex_websocket_pool_socket(
                                        reservation,
                                        stream.socket_id(),
                                    );
                                }
                                abort_abandoned_live_continuation(
                                    continuation.as_ref(),
                                    &socket_id_publisher,
                                );
                                return;
                            }
                            auth
                        }
                        Err(refresh_err) => {
                            if tx.send(Err(auth_refresh_error(refresh_err))).await.is_err() {
                                if let Some(reservation) = continuation.as_ref() {
                                    super::websocket::invalidate_codex_websocket_pool_socket(
                                        reservation,
                                        stream.socket_id(),
                                    );
                                }
                                abort_abandoned_live_continuation(
                                    continuation.as_ref(),
                                    &socket_id_publisher,
                                );
                            }
                            return;
                        }
                    };
                    if continuation_retry_available {
                        socket_id_publisher.mark_full_context_retry();
                    }
                    continuation = full_context_continuation(continuation.as_ref());
                    continuation_retry_available = false;
                    continue 'attempt;
                }

                if let Err(err) = &item
                    && is_connection_limit_error(err)
                    && !forwarded_any
                    && connection_limit_retries < MAX_BUFFERED_TRANSPORT_RETRIES
                {
                    let delay =
                        compute_backoff_delay(connection_limit_retries, err.retry_after.as_deref());
                    if !delay.exceeds_budget {
                        connection_limit_retries += 1;
                        socket_id_publisher.mark_full_context_retry();
                        continuation = full_context_continuation(continuation.as_ref());
                        continuation_retry_available = false;
                        tokio::select! {
                            biased;
                            _ = tx.closed() => {
                                abort_abandoned_live_continuation(continuation.as_ref(), &socket_id_publisher);
                                return;
                            }
                            _ = sleep(delay.wait_ms) => {}
                        }
                        continue 'attempt;
                    }
                }

                if let Err(err) = &item
                    && continuation_retry_available
                    && is_continuation_retry_error(err)
                    && !forwarded_any
                {
                    socket_id_publisher.mark_full_context_retry();
                    continuation = full_context_continuation(continuation.as_ref());
                    continuation_retry_available = false;
                    continue 'attempt;
                }

                let terminal = item.as_ref().is_err()
                    || item.as_ref().is_ok_and(super::events::event_is_terminal);
                match item {
                    Ok(payload) => match super::events::classify_stream_event(&payload) {
                        super::events::CodexStreamEventKind::TerminalFailure => {
                            if !forwarded_any {
                                pending_events.clear();
                            }
                            if tx.send(Ok(payload)).await.is_err() {
                                return;
                            }
                        }
                        super::events::CodexStreamEventKind::TerminalSuccess => {
                            for pending in pending_events.take() {
                                if tx.send(Ok(pending)).await.is_err() {
                                    return;
                                }
                            }
                            if tx.send(Ok(payload)).await.is_err() {
                                return;
                            }
                        }
                        super::events::CodexStreamEventKind::Semantic => {
                            if !forwarded_any {
                                forwarded_any = true;
                                for pending in pending_events.take() {
                                    if tx.send(Ok(pending)).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            if tx.send(Ok(payload)).await.is_err() {
                                return;
                            }
                        }
                        super::events::CodexStreamEventKind::Control => {
                            if tx.send(Ok(payload)).await.is_err() {
                                return;
                            }
                        }
                        super::events::CodexStreamEventKind::Structural => {
                            if forwarded_any {
                                if tx.send(Ok(payload)).await.is_err() {
                                    return;
                                }
                            } else {
                                if let Err(mut error) = pending_events.push(payload, 0) {
                                    error.origin = CodexErrorOrigin::WebSocket;
                                    error.message = error.message.replace("HTTP", "WebSocket");
                                    error.detail =
                                        error.detail.map(|s| s.replace("HTTP", "WebSocket"));
                                    invalidate_live_continuation_pool(continuation.as_ref());
                                    abort_live_continuation(continuation.as_ref());
                                    let _ = tx.send(Err(error)).await;
                                    return;
                                }
                            }
                        }
                    },
                    Err(err) => {
                        if tx.send(Err(err)).await.is_err() {
                            if let Some(reservation) = continuation.as_ref() {
                                super::websocket::invalidate_codex_websocket_pool_socket(
                                    reservation,
                                    stream.socket_id(),
                                );
                            }
                            abort_abandoned_live_continuation(
                                continuation.as_ref(),
                                &socket_id_publisher,
                            );
                            return;
                        }
                    }
                }
                if terminal {
                    return;
                }
            }
        }
    }

    async fn attempt_post_http(
        &self,
        auth: &StoredAuth,
        body_json: &Bytes,
        ctx: &RequestContext,
        use_responses_lite: bool,
        codex_auto_review: bool,
    ) -> Result<CodexResponse, CodexError> {
        let (resp, started_at) = self
            .start_post_http(auth, body_json, ctx, use_responses_lite, codex_auto_review)
            .await?;
        self.collect_http_response(resp, started_at, ctx).await
    }

    async fn start_post_http(
        &self,
        auth: &StoredAuth,
        body_json: &Bytes,
        ctx: &RequestContext,
        use_responses_lite: bool,
        codex_auto_review: bool,
    ) -> Result<(reqwest::Response, Instant), CodexError> {
        let url = &self.base_url;
        let headers =
            build_codex_response_headers(auth, ctx, use_responses_lite, codex_auto_review, url)?;

        if let Some(traffic) = ctx.traffic.as_deref() {
            write_codex_http_request_capture(traffic, url, &headers, body_json);
        }

        // Build headers
        let mut req_builder = self.client.post(url);
        for (key, value) in headers.iter() {
            req_builder = req_builder.header(key.as_str(), value.as_bytes());
        }

        // Apply header timeout
        let started_at = Instant::now();
        super::rate_limits::record(Vec::new());
        let send_fut = req_builder.body(body_json.clone()).send();
        let header_timeout_dur = Duration::from_millis(self.header_timeout_ms);

        let resp = tokio::time::timeout(header_timeout_dur, send_fut)
            .await
            .map_err(|_| CodexError {
                status: 0,
                message: format!(
                    "Timed out waiting {}ms for Codex response headers",
                    self.header_timeout_ms
                ),
                detail: Some(HTTP_RESPONSE_HEADERS_DETAIL.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            })?
            .map_err(|e| {
                if is_retryable_reqwest_error(&e) {
                    CodexError {
                        status: 0,
                        message: format!("Transport error: {e}"),
                        detail: None,
                        retry_after: None,
                        usage_limit: None,
                        origin: CodexErrorOrigin::Http,
                    }
                } else {
                    CodexError {
                        status: 0,
                        message: format!("Network error: {e}"),
                        detail: None,
                        retry_after: None,
                        usage_limit: None,
                        origin: CodexErrorOrigin::Http,
                    }
                }
            })?;

        super::rate_limits::record(super::rate_limits::telemetry_headers(resp.headers()));

        Ok((resp, started_at))
    }

    async fn collect_http_response(
        &self,
        mut resp: reqwest::Response,
        started_at: Instant,
        ctx: &RequestContext,
    ) -> Result<CodexResponse, CodexError> {
        let status = resp.status().as_u16();
        super::rate_limits::record(super::rate_limits::telemetry_headers(resp.headers()));
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        let mut body_bytes = Vec::new();
        let mut response_started = false;
        loop {
            let chunk = tokio::time::timeout(
                Duration::from_millis(self.body_idle_timeout_ms),
                resp.chunk(),
            )
            .await
            .map_err(|_| CodexError {
                status: 0,
                message: format!(
                    "Timed out waiting {}ms for the next Codex response body chunk",
                    self.body_idle_timeout_ms
                ),
                detail: Some("http_response_body".to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            })?
            .map_err(|e| CodexError {
                status: 0,
                message: format!("Transport error reading Codex response body: {e}"),
                detail: Some("http_response_body".to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            })?;

            let Some(chunk) = chunk else {
                break;
            };
            if !response_started {
                if let Some(monitor) = ctx.monitor.as_ref() {
                    monitor.generation_started(&ctx.req_id);
                }
                response_started = true;
            }
            ensure_transport_buffer_budget(
                body_bytes.len(),
                chunk.len(),
                CodexErrorOrigin::Http,
                "HTTP response body",
            )?;
            body_bytes.extend_from_slice(&chunk);
        }

        if let Some(traffic) = ctx.traffic.as_deref() {
            write_upstream_response_capture(
                traffic,
                status,
                started_at.elapsed(),
                &headers,
                &body_bytes,
            );
        }

        Ok(CodexResponse {
            body: body_bytes,
            status,
            headers,
            transport: ActualTransport::Http,
        })
    }

    async fn attempt_post_search(
        &self,
        auth: &StoredAuth,
        body_json: &Bytes,
        ctx: &RequestContext,
        search_session_id: &str,
    ) -> Result<CodexResponse, CodexError> {
        let url = search_endpoint(&self.base_url);
        let headers = build_codex_search_headers(auth, ctx, search_session_id)?;

        if let Some(traffic) = ctx.traffic.as_deref() {
            write_codex_http_request_capture(traffic, &url, &headers, body_json);
        }

        let mut request = self.client.post(&url);
        for (key, value) in headers.iter() {
            request = request.header(key.as_str(), value.as_bytes());
        }
        let started_at = Instant::now();
        let mut response = tokio::time::timeout(
            Duration::from_millis(self.header_timeout_ms),
            request.body(body_json.clone()).send(),
        )
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!(
                "Timed out waiting {}ms for Codex search response headers",
                self.header_timeout_ms
            ),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })?
        .map_err(|e| CodexError {
            status: 0,
            message: format!("Codex search network error: {e}"),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        })?;

        let status = response.status().as_u16();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let mut body = Vec::new();
        let mut response_started = false;
        loop {
            let chunk = tokio::time::timeout(
                Duration::from_millis(self.body_idle_timeout_ms),
                response.chunk(),
            )
            .await
            .map_err(|_| CodexError {
                status: 0,
                message: format!(
                    "Timed out waiting {}ms for the next Codex search response body chunk",
                    self.body_idle_timeout_ms
                ),
                detail: Some("http_response_body".to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            })?
            .map_err(|e| CodexError {
                status: 0,
                message: format!("Transport error reading Codex search response body: {e}"),
                detail: Some("http_response_body".to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            })?;
            let Some(chunk) = chunk else {
                break;
            };
            if !response_started {
                if let Some(monitor) = ctx.monitor.as_ref() {
                    monitor.generation_started(&ctx.req_id);
                }
                response_started = true;
            }
            ensure_transport_buffer_budget(
                body.len(),
                chunk.len(),
                CodexErrorOrigin::Http,
                "HTTP response body",
            )?;
            body.extend_from_slice(&chunk);
        }

        if let Some(traffic) = ctx.traffic.as_deref() {
            write_upstream_response_capture(traffic, status, started_at.elapsed(), &headers, &body);
        }

        Ok(CodexResponse {
            body,
            status,
            headers,
            transport: ActualTransport::Http,
        })
    }
}

pub(super) fn ensure_transport_buffer_budget(
    retained: usize,
    incoming: usize,
    origin: CodexErrorOrigin,
    label: &str,
) -> Result<(), CodexError> {
    if incoming > MAX_PENDING_HTTP_EVENT_BYTES.saturating_sub(retained) {
        return Err(CodexError {
            status: 0,
            message: format!("Codex {label} exceeded its 16 MiB buffer limit"),
            detail: Some("transport_buffer_limit".to_owned()),
            retry_after: None,
            usage_limit: None,
            origin,
        });
    }
    Ok(())
}

async fn forward_codex_events(
    mut source: tokio::sync::mpsc::Receiver<Result<serde_json::Value, CodexError>>,
    tx: tokio::sync::mpsc::Sender<Result<serde_json::Value, CodexError>>,
) {
    while let Some(item) = source.recv().await {
        if tx.send(item).await.is_err() {
            return;
        }
    }
}

fn response_headers(resp: &reqwest::Response) -> Vec<(String, String)> {
    resp.headers()
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_str().unwrap_or("").to_string()))
        .collect()
}

#[derive(Default)]
struct PendingHttpEvents {
    events: Vec<serde_json::Value>,
    event_bytes: usize,
}

impl PendingHttpEvents {
    fn push(
        &mut self,
        payload: serde_json::Value,
        pending_tool_bytes: usize,
    ) -> Result<(), CodexError> {
        let payload_bytes = super::request_summary::serialized_json_bytes(&payload);
        if payload_bytes == u64::MAX {
            return Err(pending_http_event_buffer_error("event encoding"));
        }
        let payload_bytes = usize::try_from(payload_bytes)
            .map_err(|_| pending_http_event_buffer_error("event encoding"))?
            .max(MIN_PENDING_HTTP_EVENT_BYTES);
        let next_event_bytes = self.event_bytes.saturating_add(payload_bytes);
        ensure_pending_http_buffer_budget(next_event_bytes, pending_tool_bytes)?;
        self.event_bytes = next_event_bytes;
        self.events.push(payload);
        Ok(())
    }

    fn clear(&mut self) {
        self.events.clear();
        self.event_bytes = 0;
    }

    fn take(&mut self) -> Vec<serde_json::Value> {
        self.event_bytes = 0;
        std::mem::take(&mut self.events)
    }
}

#[derive(Default)]
struct PendingHttpProvisionalFunctionCall {
    output_index: Option<usize>,
    item_id: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    arguments_done: Option<String>,
}

#[derive(Debug, Default)]
enum PendingHttpProvisionalToolEvent {
    #[default]
    Other,
    Provisional,
    AuthoritativeDone,
}

#[derive(Default)]
struct PendingHttpProvisionalTools {
    calls: Vec<PendingHttpProvisionalFunctionCall>,
}

impl PendingHttpProvisionalTools {
    fn observe(
        &mut self,
        payload: &mut serde_json::Value,
    ) -> Result<PendingHttpProvisionalToolEvent, CodexError> {
        let event_type = payload.get("type").and_then(serde_json::Value::as_str);
        match event_type {
            Some("response.output_item.added")
                if payload
                    .pointer("/item/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("function_call") =>
            {
                let item = &payload["item"];
                self.calls.push(PendingHttpProvisionalFunctionCall {
                    output_index: http_event_output_index(payload),
                    item_id: item
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    call_id: item
                        .get("call_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    name: item
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    arguments: String::new(),
                    arguments_done: None,
                });
                return Ok(PendingHttpProvisionalToolEvent::Provisional);
            }
            Some("response.function_call_arguments.delta") => {
                let Some(call_index) = self.resolve(payload, None) else {
                    return Ok(PendingHttpProvisionalToolEvent::Provisional);
                };
                let delta = payload
                    .get("delta")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let call = &mut self.calls[call_index];
                let next_len = call.arguments.len().saturating_add(delta.len());
                ensure_buffered_tool_args_limit(
                    call.name.as_deref().unwrap_or_default(),
                    next_len,
                )?;
                call.arguments.push_str(delta);
                return Ok(PendingHttpProvisionalToolEvent::Provisional);
            }
            Some("response.function_call_arguments.done") => {
                if let Some(call_index) = self.resolve(payload, None) {
                    let arguments = payload
                        .get("arguments")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            http_sse_protocol_error(
                                "function_call_arguments.done requires string arguments",
                            )
                        })?;
                    let call = &mut self.calls[call_index];
                    ensure_buffered_tool_args_limit(
                        call.name.as_deref().unwrap_or_default(),
                        arguments.len(),
                    )?;
                    call.arguments_done = Some(arguments.to_owned());
                }
                return Ok(PendingHttpProvisionalToolEvent::Provisional);
            }
            Some("response.output_item.done")
                if payload
                    .pointer("/item/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("function_call") =>
            {
                let completed = super::translate::live_stream::validate_completed_function_call(
                    &payload["item"],
                )
                .map_err(provisional_tool_validation_error)?;
                if let Some(call_index) = self.resolve(payload, payload.get("item")) {
                    let call = &self.calls[call_index];
                    super::translate::live_stream::validate_streamed_function_call(
                        &completed,
                        call.call_id.as_deref().unwrap_or_default(),
                        call.name.as_deref().unwrap_or_default(),
                        &call.arguments,
                    )
                    .map_err(provisional_tool_validation_error)?;
                    if let Some(arguments) = &call.arguments_done {
                        let arguments_done =
                            super::translate::live_stream::validate_completed_function_call_fields(
                                Some(completed.call_id),
                                Some(completed.name),
                                Some(arguments),
                            )
                            .map_err(provisional_tool_validation_error)?;
                        super::translate::live_stream::validate_streamed_function_call(
                            &completed,
                            arguments_done.call_id,
                            arguments_done.name,
                            &arguments_done.arguments,
                        )
                        .map_err(provisional_tool_validation_error)?;
                    }
                    self.calls.remove(call_index);
                } else if self.calls.iter().any(|call| {
                    http_event_output_index(payload)
                        .is_some_and(|index| call.output_index == Some(index))
                        || payload["item"]["id"]
                            .as_str()
                            .is_some_and(|id| call.item_id.as_deref() == Some(id))
                        || call.call_id.as_deref() == Some(completed.call_id)
                }) {
                    return Err(http_sse_protocol_error(
                        "completed function call disagrees with provisional identity",
                    ));
                }
                let arguments = completed.arguments.into_owned();
                payload["item"]["arguments"] = serde_json::Value::String(arguments);
                return Ok(PendingHttpProvisionalToolEvent::AuthoritativeDone);
            }
            Some("response.completed") if !self.calls.is_empty() => {
                return Err(http_sse_protocol_error(
                    "response completed before authoritative function-call finalization",
                ));
            }
            _ => {}
        }
        Ok(PendingHttpProvisionalToolEvent::Other)
    }

    fn buffered_bytes(&self) -> usize {
        self.calls.iter().fold(0, |total, call| {
            total
                .saturating_add(std::mem::size_of::<PendingHttpProvisionalFunctionCall>())
                .saturating_add(call.item_id.as_ref().map_or(0, String::capacity))
                .saturating_add(call.call_id.as_ref().map_or(0, String::capacity))
                .saturating_add(call.name.as_ref().map_or(0, String::capacity))
                .saturating_add(call.arguments.capacity())
                .saturating_add(call.arguments_done.as_ref().map_or(0, String::capacity))
        })
    }

    fn clear(&mut self) {
        self.calls.clear();
    }

    fn resolve(
        &self,
        payload: &serde_json::Value,
        item: Option<&serde_json::Value>,
    ) -> Option<usize> {
        let output_index = match payload.get("output_index") {
            Some(_) => Some(http_event_output_index(payload)?),
            None => None,
        };
        let item_id = match item
            .and_then(|item| item.get("id"))
            .or_else(|| payload.get("item_id"))
        {
            Some(value) => Some(value.as_str()?),
            None => None,
        };
        let call_id = match item
            .and_then(|item| item.get("call_id"))
            .or_else(|| payload.get("call_id"))
        {
            Some(value) => Some(value.as_str()?),
            None => None,
        };
        let has_explicit_alias = output_index.is_some() || item_id.is_some() || call_id.is_some();
        let mut matches = self.calls.iter().enumerate().filter_map(|(index, call)| {
            (output_index.is_none_or(|value| call.output_index == Some(value))
                && item_id.is_none_or(|value| call.item_id.as_deref() == Some(value))
                && call_id.is_none_or(|value| call.call_id.as_deref() == Some(value)))
            .then_some(index)
        });
        let first = matches.next();
        if matches.next().is_some() {
            None
        } else if has_explicit_alias {
            first
        } else {
            first.or_else(|| (self.calls.len() == 1).then_some(0))
        }
    }
}

fn provisional_tool_validation_error(
    error: super::translate::live_stream::CompletedFunctionCallValidationError,
) -> CodexError {
    match error {
        super::translate::live_stream::CompletedFunctionCallValidationError::TooLarge {
            ..
        } => CodexError {
            status: 413,
            message: error.to_string(),
            detail: Some(error.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        },
        super::translate::live_stream::CompletedFunctionCallValidationError::Protocol(message) => {
            http_sse_protocol_error(&message)
        }
    }
}

fn http_event_output_index(payload: &serde_json::Value) -> Option<usize> {
    payload
        .get("output_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
}

fn http_event_commits_replay(
    payload: &serde_json::Value,
    event_kind: super::events::CodexStreamEventKind,
) -> bool {
    if event_kind != super::events::CodexStreamEventKind::Semantic {
        return false;
    }
    matches!(
        payload.get("type").and_then(serde_json::Value::as_str),
        Some(
            "response.reasoning_summary_text.delta"
                | "response.output_text.delta"
                | "response.output_text.done"
                | "response.refusal.delta"
                | "response.refusal.done"
                | "response.content_part.done"
                | "response.output_item.done"
        )
    )
}

fn ensure_buffered_tool_args_limit(name: &str, len: usize) -> Result<(), CodexError> {
    if len <= super::translate::live_stream::BUFFERED_TOOL_MAX_ARGS_BYTES {
        return Ok(());
    }
    let tool = if name.is_empty() { "tool" } else { name };
    Err(CodexError {
        status: 413,
        message: format!("Buffered {tool} tool arguments exceeded safe limits"),
        detail: Some(format!(
            "Buffered {tool} tool arguments exceeded safe limits"
        )),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::Http,
    })
}

fn pending_http_event_buffer_error(limit: &str) -> CodexError {
    CodexError {
        status: 413,
        message: format!("Codex pending HTTP event buffer exceeded its {limit} limit"),
        detail: Some(format!(
            "Codex pending HTTP event buffer exceeded its {limit} limit"
        )),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::Http,
    }
}

fn ensure_pending_http_buffer_budget(
    pending_event_bytes: usize,
    provisional_tool_bytes: usize,
) -> Result<(), CodexError> {
    if pending_event_bytes.saturating_add(provisional_tool_bytes) > MAX_PENDING_HTTP_EVENT_BYTES {
        return Err(pending_http_event_buffer_error("aggregate bytes"));
    }
    Ok(())
}

fn codex_event_failure_error(failure: super::events::CodexEventFailure) -> CodexError {
    CodexError {
        status: failure.status,
        message: failure.message.clone(),
        detail: Some(failure.message),
        retry_after: failure.retry_after,
        usage_limit: None,
        origin: CodexErrorOrigin::Http,
    }
}

fn codex_usage_limit_error(limit: CodexUsageLimit, origin: CodexErrorOrigin) -> CodexError {
    CodexError {
        status: 429,
        message: limit.message.clone(),
        detail: Some(limit.message.clone()),
        retry_after: None,
        usage_limit: Some(Box::new(limit)),
        origin,
    }
}

fn retryable_http_stream_error(error: &CodexError) -> bool {
    if error.usage_limit.is_some() {
        return false;
    }
    if should_retry_codex_status(error.status) || is_retryable_transport_error(error) {
        return true;
    }
    if error.status != 0 {
        return false;
    }
    if error.detail.as_deref() == Some("http_response_sse") {
        return error.message != "Codex SSE frame exceeds the size limit";
    }
    let message = error.message.to_ascii_lowercase();
    message.contains("ended before a terminal response event")
        || message.contains("ended with an incomplete frame")
}

#[allow(clippy::too_many_arguments)]
fn log_http_stream_retry(
    ctx: &RequestContext,
    model: &str,
    phase: &'static str,
    started_at: Instant,
    failed_attempt: u32,
    error: &CodexError,
    delay_ms: Option<u64>,
    stop_reason: Option<&'static str>,
) {
    // Classify locally: error messages/details may contain upstream payloads.
    let reason = if error.usage_limit.is_some() {
        "usage_limit"
    } else if error.origin == CodexErrorOrigin::Auth {
        "auth_error"
    } else {
        match error.detail.as_deref() {
            Some(HTTP_RESPONSE_HEADERS_DETAIL) => "http_response_headers",
            Some("http_response_body") => "http_response_body",
            Some("http_response_sse") => "http_response_sse",
            _ if error.status != 0 => "upstream_status",
            Some(_) => "upstream_event_or_stream_error",
            None => "transport_error",
        }
    };
    let mut fields = serde_json::Map::from_iter([
        ("reqId".into(), serde_json::json!(ctx.req_id)),
        ("transport".into(), serde_json::json!("http")),
        ("model".into(), serde_json::json!(model)),
        ("phase".into(), serde_json::json!(phase)),
        ("failedAttempt".into(), serde_json::json!(failed_attempt)),
        (
            "maxAttempts".into(),
            serde_json::json!(MAX_BUFFERED_TRANSPORT_ATTEMPTS),
        ),
        ("status".into(), serde_json::json!(error.status)),
        (
            "origin".into(),
            serde_json::json!(codex_error_origin_name(error.origin)),
        ),
        ("reason".into(), serde_json::json!(reason)),
        (
            "ms".into(),
            serde_json::json!(started_at.elapsed().as_millis()),
        ),
    ]);
    if let Some(delay_ms) = delay_ms {
        fields.insert("delayMs".into(), serde_json::json!(delay_ms));
    }
    let event = if let Some(stop_reason) = stop_reason {
        fields.insert("stopReason".into(), serde_json::json!(stop_reason));
        "codex_http_stream_retry_stopped"
    } else {
        fields.insert("nextAttempt".into(), serde_json::json!(failed_attempt + 1));
        "codex_http_stream_retry"
    };
    let log = create_logger("codex");
    if error.usage_limit.is_some() {
        log.info(event, Some(fields));
    } else {
        log.warn(event, Some(fields));
    }
}

#[allow(clippy::too_many_arguments)]
fn log_http_stream_end(
    log: &crate::logging::Logger,
    message: &str,
    req_id: &str,
    started_at: Instant,
    body_bytes: u64,
    body_chunks: u64,
    event_count: u64,
    error: Option<&str>,
) {
    let mut fields = serde_json::Map::from_iter([
        ("reqId".to_string(), serde_json::json!(req_id)),
        ("transport".to_string(), serde_json::json!("http")),
        ("bodyBytes".to_string(), serde_json::json!(body_bytes)),
        ("bodyChunks".to_string(), serde_json::json!(body_chunks)),
        ("eventCount".to_string(), serde_json::json!(event_count)),
        (
            "ms".to_string(),
            serde_json::json!(started_at.elapsed().as_millis()),
        ),
    ]);
    if let Some(error) = error {
        fields.insert("error".to_string(), serde_json::json!(error));
    }
    match message {
        "codex_http_stream_completed" | "codex_http_stream_local_finish" => {
            log.info(message, Some(fields))
        }
        _ => log.warn(message, Some(fields)),
    }
}

fn write_codex_http_request_capture(
    traffic: &TrafficCapture,
    url: &str,
    headers: &http::HeaderMap,
    body_json: &[u8],
) {
    let body = serde_json::from_slice(body_json).unwrap_or_else(|_| {
        serde_json::json!({
            "unparseable": true,
            "bytes": body_json.len(),
        })
    });
    traffic.write_json("020-upstream-request", &body);
    traffic.write_json(
        "021-upstream-request-metadata",
        &serde_json::json!({
            "provider": "codex",
            "transport": "http",
            "url": url,
            "method": "POST",
            "headers": headers_to_json(headers),
            "size": summarize_json_request_size(&body, body_json),
        }),
    );
}

fn write_live_upstream_response_headers(
    traffic: &TrafficCapture,
    response: &reqwest::Response,
    elapsed: Duration,
) {
    traffic.write_json(
        "030-upstream-response-headers",
        &serde_json::json!({
            "status": response.status().as_u16(),
            "elapsedMs": elapsed.as_millis(),
            "headers": headers_to_json(response.headers()),
        }),
    );
}

fn write_upstream_response_capture(
    traffic: &TrafficCapture,
    status: u16,
    elapsed: Duration,
    headers: &[(String, String)],
    body: &[u8],
) {
    write_upstream_response_headers_capture(traffic, status, elapsed, headers);
    if status >= 400 {
        traffic.write_text("031-upstream-error-body", &String::from_utf8_lossy(body));
    } else {
        traffic.write_bytes("032-upstream-response-body.sse", body);
        write_codex_sse_event_capture(traffic, body);
    }
}

fn write_upstream_response_headers_capture(
    traffic: &TrafficCapture,
    status: u16,
    elapsed: Duration,
    headers: &[(String, String)],
) {
    traffic.write_json(
        "030-upstream-response-headers",
        &serde_json::json!({
            "status": status,
            "elapsedMs": elapsed.as_millis(),
            "headers": headers_to_json_from_pairs(headers),
        }),
    );
}

fn write_codex_http_sse_event_capture(
    traffic: &TrafficCapture,
    event: Option<&str>,
    payload: &serde_json::Value,
) {
    traffic.write_json_event_with_type("040-upstream-event", event, payload);
}

fn write_codex_sse_event_capture(traffic: &TrafficCapture, body: &[u8]) {
    let text = String::from_utf8_lossy(body);
    for event in sse_events(&text) {
        if event.data == "[DONE]" {
            traffic.write_json_event(
                "040-upstream-event",
                &serde_json::json!({
                    "event": event.event,
                    "data": "[DONE]",
                }),
            );
            continue;
        }

        match serde_json::from_str::<serde_json::Value>(&event.data) {
            Ok(value) => {
                traffic.write_json_event_with_type("040-upstream-event", event.event, &value);
            }
            Err(_) => {
                traffic.write_json_event(
                    "040-upstream-event",
                    &serde_json::json!({
                        "event": event.event,
                        "unparseable": true,
                        "data": event.data,
                    }),
                );
            }
        }
    }
}

pub(super) fn headers_to_json(headers: &http::HeaderMap) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (key, value) in headers.iter() {
        out.insert(
            key.to_string(),
            serde_json::Value::String(value.to_str().unwrap_or("").to_string()),
        );
    }
    serde_json::Value::Object(out)
}

fn headers_to_json_from_pairs(headers: &[(String, String)]) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (key, value) in headers {
        out.insert(key.clone(), serde_json::Value::String(value.clone()));
    }
    serde_json::Value::Object(out)
}

pub(super) fn summarize_json_request_size(
    body: &serde_json::Value,
    body_json: impl AsRef<[u8]>,
) -> serde_json::Value {
    serde_json::json!({
        "bytes": body_json.as_ref().len(),
        "inputCount": body
            .get("input")
            .and_then(|v| v.as_array())
            .map(|items| items.len()),
        "toolCount": body
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|items| items.len()),
    })
}

fn auth_refresh_error(err: anyhow::Error) -> CodexError {
    if err.to_string() == "operation_provenance_changed" {
        return CodexError {
            status: 409,
            message: "Codex opaque operation provenance changed; retry portable context".into(),
            detail: Some("operation_provenance_changed".into()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Auth,
        };
    }
    CodexError {
        status: 401,
        message: "Unauthorized".to_string(),
        detail: Some(err.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::Auth,
    }
}

fn codex_status_error(response: CodexResponse) -> CodexError {
    let usage_limit =
        super::events::usage_limit_from_response(&response.body, &response.headers).map(Box::new);
    let retry_after = response
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
        .map(|(_, value)| value.clone());
    let message = codex_status_error_message(&response.body).unwrap_or_else(|| {
        format!(
            "Upstream Codex request failed with status {}",
            response.status
        )
    });
    CodexError {
        status: response.status,
        message: message.clone(),
        detail: Some(message),
        retry_after,
        usage_limit,
        origin: match response.transport {
            ActualTransport::Http => CodexErrorOrigin::BufferedHttp,
            ActualTransport::WebSocket => CodexErrorOrigin::BufferedWebSocket,
        },
    }
}

fn codex_status_error_message(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .or_else(|| value.get("detail"))
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            let text = String::from_utf8_lossy(body);
            sse_events(&text).find_map(|event| {
                let payload = serde_json::from_str::<serde_json::Value>(&event.data).ok()?;
                super::events::classify_event_failure(&payload).map(|failure| failure.message)
            })
        })
}

fn should_retry_codex_status(status: u16) -> bool {
    should_retry_status(status) || status == 529
}

fn codex_error_origin_name(origin: CodexErrorOrigin) -> &'static str {
    match origin {
        CodexErrorOrigin::Http => "http",
        CodexErrorOrigin::WebSocket => "websocket",
        CodexErrorOrigin::WebSocketHandshake => "websocket_handshake",
        CodexErrorOrigin::Auth => "auth",
        CodexErrorOrigin::BufferedHttp => "buffered_http",
        CodexErrorOrigin::BufferedWebSocket => "buffered_websocket",
    }
}

fn log_buffered_retry(
    ctx: &RequestContext,
    transport: crate::config::CodexTransport,
    failed_attempt: u32,
    delay_ms: u64,
    status: u16,
    origin: &str,
    reason: &str,
) {
    let mut fields = serde_json::Map::new();
    fields.insert("reqId".into(), serde_json::json!(ctx.req_id));
    fields.insert("transport".into(), serde_json::json!(transport.as_str()));
    fields.insert("failedAttempt".into(), serde_json::json!(failed_attempt));
    fields.insert("nextAttempt".into(), serde_json::json!(failed_attempt + 1));
    fields.insert(
        "maxAttempts".into(),
        serde_json::json!(MAX_BUFFERED_TRANSPORT_ATTEMPTS),
    );
    fields.insert("delayMs".into(), serde_json::json!(delay_ms));
    fields.insert("status".into(), serde_json::json!(status));
    fields.insert("origin".into(), serde_json::json!(origin));
    fields.insert("reason".into(), serde_json::json!(reason));
    create_logger("codex").warn("buffered_transport_retry", Some(fields));
}

fn log_buffered_retry_exhausted(
    ctx: &RequestContext,
    transport: crate::config::CodexTransport,
    status: u16,
    origin: &str,
    reason: &str,
) {
    let mut fields = serde_json::Map::new();
    fields.insert("reqId".into(), serde_json::json!(ctx.req_id));
    fields.insert("transport".into(), serde_json::json!(transport.as_str()));
    fields.insert(
        "attempts".into(),
        serde_json::json!(MAX_BUFFERED_TRANSPORT_ATTEMPTS),
    );
    fields.insert("status".into(), serde_json::json!(status));
    fields.insert("origin".into(), serde_json::json!(origin));
    fields.insert("reason".into(), serde_json::json!(reason));
    create_logger("codex").warn("buffered_transport_retry_exhausted", Some(fields));
}

fn is_retryable_transport_error(err: &CodexError) -> bool {
    // Running out of patience for the response headers is not a transport
    // failure. The request reached Codex, which is holding the head while the
    // model reasons, so re-sending it starts that reasoning again from scratch
    // and the new attempt runs out of time exactly like the last one. Wait
    // longer with CCP_CODEX_HEADER_TIMEOUT_MS instead.
    if err.detail.as_deref() == Some(HTTP_RESPONSE_HEADERS_DETAIL) {
        return false;
    }
    if err.origin == CodexErrorOrigin::WebSocketHandshake {
        if err.detail.as_deref() == Some(super::websocket::WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL) {
            return false;
        }
        return err.status == 0 || should_retry_codex_status(err.status);
    }
    if err.detail.as_deref() == Some(super::websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL) {
        return true;
    }
    if err.status != 0 {
        return false;
    }

    let message = err.message.to_ascii_lowercase();
    message.contains("timed out waiting")
        || message.contains("transport error")
        || message.contains("connection reset")
        || message.contains("no route to host")
        || message.contains("connection closed")
        || message.contains("timed out")
        || message.contains("econnreset")
        || message.contains("etimedout")
        || message.contains("broken pipe")
        || message.contains("epipe")
}

fn is_retryable_reqwest_error(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() {
        return true;
    }
    let msg = err.to_string().to_lowercase();
    msg.contains("connection reset")
        || msg.contains("connection closed")
        || msg.contains("econnreset")
        || msg.contains("etimedout")
        || msg.contains("epipe")
}

fn should_refresh_after_unauthorized(
    result: &Result<OwnerAwareCodexResponse, CodexError>,
    auth_refresh_attempted: bool,
    transport: crate::config::CodexTransport,
) -> bool {
    if auth_refresh_attempted {
        return false;
    }
    match result {
        Ok(response) => response.status == 401,
        Err(err) => {
            err.status == 401
                && (err.origin != CodexErrorOrigin::WebSocketHandshake
                    || transport == crate::config::CodexTransport::WebSocket)
        }
    }
}

fn should_fallback_to_http(err: &CodexError) -> bool {
    err.origin == CodexErrorOrigin::WebSocketHandshake
        && err.status != http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16()
        && err.detail.as_deref() != Some(super::websocket::WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL)
}

fn is_connection_limit_error(error: &CodexError) -> bool {
    error.origin == CodexErrorOrigin::WebSocket
        && error.detail.as_deref() == Some(super::websocket::WEBSOCKET_CONNECTION_LIMIT_REACHED)
}

fn should_retry_without_continuation(
    err: &CodexError,
    continuation: Option<&super::continuation::ContinuationReservation>,
) -> bool {
    if continuation
        .and_then(|reservation| reservation.candidate().previous_response_id.as_deref())
        .is_none()
    {
        return false;
    }

    is_continuation_retry_error(err)
}

fn full_context_continuation(
    continuation: Option<&super::continuation::ContinuationReservation>,
) -> Option<super::continuation::ContinuationReservation> {
    continuation.map(super::continuation::ContinuationReservation::full_context_retry)
}

fn abort_live_continuation(continuation: Option<&super::continuation::ContinuationReservation>) {
    if let Some(continuation) = continuation {
        super::continuation::abort_continuation_for_owner(continuation);
    }
}

fn abort_abandoned_live_continuation(
    continuation: Option<&super::continuation::ContinuationReservation>,
    socket_id_publisher: &super::websocket::CodexWebSocketSocketIdPublisher,
) {
    if !socket_id_publisher.is_provider_retry_handoff() {
        abort_live_continuation(continuation);
    }
}

fn invalidate_live_continuation_pool(
    continuation: Option<&super::continuation::ContinuationReservation>,
) {
    let Some(continuation) = continuation else {
        return;
    };
    let Some(owner) = websocket_pool_owner(Some(continuation)) else {
        return;
    };
    super::websocket::invalidate_codex_websocket_pool_turn_for_owner(owner, continuation.turn_id());
}

pub(super) fn is_continuation_retry_error(err: &CodexError) -> bool {
    matches!(
        err.detail.as_deref(),
        Some("previous_response_not_found")
            | Some(super::websocket::WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL)
            | Some(super::websocket::WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL)
            | Some(super::websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL)
            | Some(super::websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL)
    )
}

fn websocket_pool_owner(
    continuation: Option<&super::continuation::ContinuationReservation>,
) -> Option<&ConversationIdentity> {
    let continuation = continuation?;
    if continuation.candidate().disabled_reason.as_deref() == Some("disabled") {
        return None;
    }
    continuation.owner()
}

fn should_reset_websocket_pool(
    continuation: Option<&super::continuation::ContinuationReservation>,
) -> bool {
    let Some(reason) =
        continuation.and_then(|continuation| continuation.candidate().disabled_reason.as_deref())
    else {
        return false;
    };
    reason != "disabled"
}
