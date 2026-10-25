use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use http::HeaderMap;
use hyper_util::client::proxy::matcher::Matcher as ProxyMatcher;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        handshake::{client::generate_key, derive_accept_key},
        protocol::Role,
    },
};

use crate::logging::create_logger;
use crate::request_identity::ConversationIdentity;
use crate::retry::sleep as retry_sleep;
use crate::traffic::TrafficCapture;

use super::client::{
    ActualTransport, CodexError, CodexErrorOrigin, CodexResponse, OwnerAwareCodexResponse,
    headers_to_json, summarize_json_request_size,
};
use super::continuation::ContinuationReservation;
use super::events::event_is_terminal;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const WEBSOCKET_PROTOCOL_HEADER: &str = "responses_websockets=2026-02-06";
pub const WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
pub const WEBSOCKET_IDLE_TIMEOUT_MS: u64 = 300_000;
pub const WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL: &str = "websocket_response_start_timeout";
pub const WEBSOCKET_MISSING_TERMINAL_DETAIL: &str = "websocket_missing_terminal";
pub const WEBSOCKET_KEEPALIVE_FAILURE_DETAIL: &str = "websocket_keepalive_failure";
pub const WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL: &str =
    "websocket_continuation_socket_missing";
pub(super) const WEBSOCKET_CONNECTION_LIMIT_REACHED: &str = "websocket_connection_limit_reached";
pub(super) const WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL: &str = "websocket_proxy_tunnel_rejected";

// Match HTTP's idle connection retention; response/read timeouts are independent.
const POOL_IDLE_TTL_MS: u64 = 90_000;
const MAX_POOL_ENTRIES: usize = 10_000;
const POOL_CONNECT_CLEANUP_THRESHOLD: usize = 50;
const POOL_CONNECT_CLEANUP_TARGET: usize = 40;
const MAX_CONNECT_RESPONSE_HEADER_BYTES: usize = 8 * 1024;
// Premier palier appliqué dès qu'une origine refuse un upgrade, puis doublement.
const WEBSOCKET_CONNECT_BACKOFF_STEP: Duration = Duration::from_secs(1);
const WEBSOCKET_CONNECT_MAX_SPACING: Duration = Duration::from_secs(8);
// Connexions réussies consécutives avant de réduire l'espacement de moitié.
const WEBSOCKET_CONNECT_RELAX_AFTER: u32 = 20;
const WEBSOCKET_CONNECT_FORBIDDEN_COOLDOWN: Duration = Duration::from_secs(3);
const WEBSOCKET_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const WEBSOCKET_KEEPALIVE_SEND_TIMEOUT: Duration = Duration::from_secs(10);

pub type CodexWebSocketEventReceiver =
    tokio::sync::mpsc::Receiver<Result<serde_json::Value, CodexError>>;

pub(crate) struct CodexWebSocketEventStream {
    receiver: CodexWebSocketEventReceiver,
    provenance: Arc<Mutex<Option<super::models::OperationProvenance>>>,
    socket_id: Arc<AtomicU64>,
    full_context_retry: Arc<AtomicBool>,
    provider_retry_handoff: Arc<AtomicBool>,
}

#[derive(Clone)]
pub(crate) struct CodexWebSocketSocketIdPublisher {
    provenance: Arc<Mutex<Option<super::models::OperationProvenance>>>,
    socket_id: Arc<AtomicU64>,
    full_context_retry: Arc<AtomicBool>,
    provider_retry_handoff: Arc<AtomicBool>,
}

impl CodexWebSocketEventStream {
    pub(crate) fn pending(
        receiver: CodexWebSocketEventReceiver,
    ) -> (Self, CodexWebSocketSocketIdPublisher) {
        let provenance = Arc::new(Mutex::new(None));
        let socket_id = Arc::new(AtomicU64::new(0));
        let full_context_retry = Arc::new(AtomicBool::new(false));
        let provider_retry_handoff = Arc::new(AtomicBool::new(false));
        (
            Self {
                provenance: provenance.clone(),
                receiver,
                socket_id: socket_id.clone(),
                full_context_retry: full_context_retry.clone(),
                provider_retry_handoff: provider_retry_handoff.clone(),
            },
            CodexWebSocketSocketIdPublisher {
                provenance,
                socket_id,
                full_context_retry,
                provider_retry_handoff,
            },
        )
    }

    pub(crate) async fn recv(&mut self) -> Option<Result<serde_json::Value, CodexError>> {
        self.receiver.recv().await
    }

    pub(crate) fn provenance(&self) -> Option<super::models::OperationProvenance> {
        self.provenance
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn socket_id(&self) -> Option<u64> {
        match self.socket_id.load(Ordering::Acquire) {
            0 => None,
            socket_id => Some(socket_id),
        }
    }

    pub(crate) fn used_full_context_retry(&self) -> bool {
        self.full_context_retry.load(Ordering::Acquire)
    }

    pub(crate) fn mark_provider_retry_handoff(&self) {
        self.provider_retry_handoff.store(true, Ordering::Release);
    }

    pub(crate) fn replace_receiver(
        &mut self,
        receiver: CodexWebSocketEventReceiver,
    ) -> CodexWebSocketEventReceiver {
        std::mem::replace(&mut self.receiver, receiver)
    }
}

impl CodexWebSocketSocketIdPublisher {
    pub(super) fn publish_provenance(
        &self,
        provenance: Option<super::models::OperationProvenance>,
    ) {
        *self.provenance.lock().unwrap_or_else(|e| e.into_inner()) = provenance;
    }

    pub(super) fn publish(&self, socket_id: Option<u64>) {
        self.socket_id
            .store(socket_id.unwrap_or(0), Ordering::Release);
    }

    pub(super) fn mark_full_context_retry(&self) {
        self.full_context_retry.store(true, Ordering::Release);
    }

    pub(super) fn is_provider_retry_handoff(&self) -> bool {
        self.provider_retry_handoff.load(Ordering::Acquire)
    }
}

trait WebSocketIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> WebSocketIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

type BoxedWebSocketIo = Box<dyn WebSocketIo>;
type CodexWebSocketStream = WebSocketStream<BoxedWebSocketIo>;

pub(super) struct WebSocketProxyConfig {
    matcher: ProxyMatcher,
    tls_config: Arc<rustls::ClientConfig>,
}

#[derive(Clone)]
struct WebSocketProxyRoute {
    uri: http::Uri,
    basic_auth: Option<http::HeaderValue>,
}

impl WebSocketProxyConfig {
    pub(super) fn new(
        http_proxy: Option<&str>,
        https_proxy: Option<&str>,
        all_proxy: Option<&str>,
        no_proxy: Option<&str>,
    ) -> Self {
        let mut builder = ProxyMatcher::builder();
        if let Some(proxy) = all_proxy {
            builder = builder.all(proxy.to_string());
        }
        if let Some(proxy) = http_proxy {
            builder = builder.http(proxy.to_string());
        }
        if let Some(proxy) = https_proxy {
            builder = builder.https(proxy.to_string());
        }
        if let Some(no_proxy) = no_proxy {
            builder = builder.no(no_proxy.to_string());
        }
        Self {
            matcher: builder.build(),
            tls_config: websocket_tls_config(),
        }
    }

    fn http_connect_route(
        &self,
        websocket_url: &str,
    ) -> Result<Option<WebSocketProxyRoute>, CodexError> {
        let http_url = to_http_upgrade_url(websocket_url).map_err(|error| CodexError {
            status: 0,
            message: error.message,
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        })?;
        if !http_url.starts_with("https://") {
            return Ok(None);
        }
        let destination = http_url.parse::<http::Uri>().map_err(|_| {
            websocket_protocol_error("WebSocket destination URL could not be routed")
        })?;
        let Some(proxy) = self.matcher.intercept(&destination) else {
            return Ok(None);
        };
        if !matches!(proxy.uri().scheme_str(), Some("http" | "https")) {
            return Ok(None);
        }
        Ok(Some(WebSocketProxyRoute {
            uri: proxy.uri().clone(),
            basic_auth: proxy.basic_auth().cloned(),
        }))
    }
}

static WEBSOCKET_TLS_CONFIG: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    let load_error_count = native.errors.len();
    let (_, parse_error_count) = roots.add_parsable_certificates(native.certs);
    if load_error_count > 0 || parse_error_count > 0 {
        let mut fields = serde_json::Map::new();
        fields.insert("loadErrorCount".into(), serde_json::json!(load_error_count));
        fields.insert(
            "parseErrorCount".into(),
            serde_json::json!(parse_error_count),
        );
        create_logger("codex").warn("native_certificate_load_errors", Some(fields));
    }
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
});

pub(super) fn websocket_tls_config() -> Arc<rustls::ClientConfig> {
    WEBSOCKET_TLS_CONFIG.clone()
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct CodexWebSocketError {
    pub message: String,
}

impl CodexWebSocketError {
    pub fn new(message: String) -> Self {
        Self { message }
    }
}

impl std::fmt::Display for CodexWebSocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex WebSocket error: {}", self.message)
    }
}

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

struct PoolEntry {
    provenance: Option<super::models::OperationProvenance>,
    ws: Arc<AsyncMutex<CodexWebSocketStream>>,
    socket_id: u64,
    last_used_at: AtomicU64,
    last_activity: AtomicU64,
}

impl PoolEntry {
    fn authenticated(
        ws: CodexWebSocketStream,
        provenance: Option<super::models::OperationProvenance>,
    ) -> Self {
        Self {
            provenance,
            ws: Arc::new(AsyncMutex::new(ws)),
            socket_id: next_monotonic_nonzero(&NEXT_SOCKET_ID, "WebSocket ID"),
            last_used_at: AtomicU64::new(now_ms()),
            last_activity: AtomicU64::new(next_pool_activity()),
        }
    }

    fn touch(&self) {
        self.last_used_at.fetch_max(now_ms(), Ordering::Relaxed);
        self.last_activity
            .fetch_max(next_pool_activity(), Ordering::Relaxed);
    }
}

static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(0);
static POOLED_VALIDATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static POOL_ACTIVITY_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static WS_POOL: LazyLock<Mutex<HashMap<ConversationIdentity, Arc<PoolEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static WS_CONNECT_GATE: LazyLock<WebSocketConnectGate> =
    LazyLock::new(|| WebSocketConnectGate::new(crate::config::codex_websocket_connect_spacing()));

fn next_monotonic_nonzero(sequence: &AtomicU64, label: &str) -> u64 {
    let previous = sequence
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .unwrap_or_else(|_| panic!("{label} sequence exhausted"));
    previous + 1
}

fn next_pool_activity() -> u64 {
    POOL_ACTIVITY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn invalidate_codex_websocket_pool_owner(owner: &ConversationIdentity) {
    let mut guard = WS_POOL.lock().unwrap();
    guard.remove(owner);
}

pub(crate) fn invalidate_codex_websocket_pool_turn_for_owner(
    owner: &ConversationIdentity,
    turn_id: Option<u64>,
) {
    let reservation = ContinuationReservation::for_owner_turn(Some(owner), turn_id);
    super::continuation::with_current_turn_for_owner(&reservation, || {
        invalidate_codex_websocket_pool_owner(owner)
    });
}

fn invalidate_pool_entry(owner: &ConversationIdentity, entry: &Arc<PoolEntry>) {
    let mut guard = WS_POOL.lock().unwrap();
    if guard
        .get(owner)
        .is_some_and(|pooled| Arc::ptr_eq(pooled, entry))
    {
        guard.remove(owner);
    }
}

fn invalidate_pool_owner(owner: Option<&ConversationIdentity>, entry: Option<&Arc<PoolEntry>>) {
    let Some(owner) = owner else {
        return;
    };
    match entry {
        Some(entry) => invalidate_pool_entry(owner, entry),
        None => invalidate_codex_websocket_pool_owner(owner),
    }
}

fn reservation_pool_owner(
    reservation: Option<&ContinuationReservation>,
) -> Option<&ConversationIdentity> {
    let reservation = reservation?;
    if reservation.candidate().disabled_reason.as_deref() == Some("disabled") {
        return None;
    }
    reservation.owner()
}

fn pool_take_for_turn(reservation: &ContinuationReservation) -> Option<Arc<PoolEntry>> {
    let owner = reservation.owner()?;
    super::continuation::if_current_turn_for_owner(reservation, || {
        WS_POOL.lock().ok()?.remove(owner)
    })
    .flatten()
}

fn take_pool_entry_for_request(
    reservation: Option<&ContinuationReservation>,
    provenance: Option<&super::models::OperationProvenance>,
) -> Result<Option<Arc<PoolEntry>>, CodexError> {
    let candidate = reservation.map(ContinuationReservation::candidate);
    let requires_origin = candidate
        .and_then(|candidate| candidate.previous_response_id.as_deref())
        .is_some();
    let expected_socket_id = reservation.and_then(ContinuationReservation::origin_socket_id);
    let pool_owner = reservation_pool_owner(reservation);
    let pooled = reservation
        .filter(|_| pool_owner.is_some())
        .and_then(pool_take_for_turn);

    if requires_origin
        && (pool_owner.is_none()
            || expected_socket_id.is_none()
            || pooled
                .as_ref()
                .is_none_or(|entry| Some(entry.socket_id) != expected_socket_id))
    {
        if let (Some(owner), Some(entry)) = (pool_owner, pooled.as_ref()) {
            pool_insert_if_vacant_or_same(owner.clone(), entry.clone());
        }
        return Err(continuation_socket_missing_error());
    }

    // A new request's auth headers do not reauthenticate an existing socket.
    // Unknown provenance is deliberately not equal to unknown provenance.
    let pooled = pooled.filter(|entry| {
        provenance.is_some()
            && entry.provenance.as_ref() == provenance
            && (!requires_origin
                || reservation.and_then(ContinuationReservation::provenance) == provenance)
    });
    if requires_origin && pooled.is_none() {
        return Err(continuation_socket_missing_error());
    }
    Ok(pooled)
}

fn pool_insert_for_turn(reservation: &ContinuationReservation, entry: Arc<PoolEntry>) -> bool {
    if entry.provenance.is_none() || reservation.provenance() != entry.provenance.as_ref() {
        return false;
    }
    let Some(owner) = reservation_pool_owner(Some(reservation)).cloned() else {
        return false;
    };
    super::continuation::if_current_turn_for_owner(reservation, || {
        pool_insert_if_vacant_or_same(owner, entry)
    })
    .unwrap_or(false)
}

pub(super) fn invalidate_codex_websocket_pool_socket(
    reservation: &ContinuationReservation,
    socket_id: Option<u64>,
) {
    let Some(owner) = reservation_pool_owner(Some(reservation)) else {
        return;
    };
    let Some(socket_id) = socket_id else {
        return;
    };
    let entry = WS_POOL.lock().ok().and_then(|pool| {
        pool.get(owner)
            .filter(|entry| entry.socket_id == socket_id)
            .cloned()
    });
    if let Some(entry) = entry {
        invalidate_pool_entry(owner, &entry);
    }
}

fn pool_entry_is_idle(entry: &Arc<PoolEntry>) -> bool {
    Arc::strong_count(entry) == 1
        && Arc::strong_count(&entry.ws) == 1
        && entry.ws.try_lock().is_ok()
}

fn prune_idle_pool(pool: &mut HashMap<ConversationIdentity, Arc<PoolEntry>>) {
    let now = now_ms();
    pool.retain(|_, entry| {
        now.saturating_sub(entry.last_used_at.load(Ordering::Relaxed)) < POOL_IDLE_TTL_MS
            || !pool_entry_is_idle(entry)
    });
}

pub(crate) fn prune_idle_state() {
    super::continuation::prune_idle_state();
    super::compaction::prune_idle_state();
    super::threads::prune_idle_state();
    let mut pool = WS_POOL.lock().unwrap();
    prune_idle_pool(&mut pool);
    pool.shrink_to_fit();
}

fn pool_insert_if_vacant_or_same(owner: ConversationIdentity, entry: Arc<PoolEntry>) -> bool {
    entry.touch();
    let mut guard = WS_POOL.lock().unwrap();
    if let Some(existing) = guard.get(&owner) {
        return Arc::ptr_eq(existing, &entry);
    }
    prune_idle_pool(&mut guard);
    if guard.len() >= MAX_POOL_ENTRIES {
        let oldest_owner = guard
            .iter()
            .filter(|(_, pooled)| pool_entry_is_idle(pooled))
            .min_by_key(|(_, pooled)| pooled.last_activity.load(Ordering::Relaxed))
            .map(|(owner, _)| owner.clone());
        let Some(oldest_owner) = oldest_owner else {
            return false;
        };
        guard.remove(&oldest_owner);
    }
    guard.insert(owner, entry);
    true
}

fn cleanup_pool_before_connect() {
    let removed = {
        let mut guard = WS_POOL.lock().unwrap();
        if guard.len() <= POOL_CONNECT_CLEANUP_THRESHOLD {
            return;
        }

        let remove_count = guard.len() - POOL_CONNECT_CLEANUP_TARGET;
        let mut candidates: Vec<_> = guard
            .iter()
            .filter(|(_, entry)| pool_entry_is_idle(entry))
            .map(|(owner, entry)| (owner.clone(), entry.last_activity.load(Ordering::Relaxed)))
            .collect();
        candidates.sort_unstable_by_key(|(_, activity)| *activity);
        candidates
            .into_iter()
            .take(remove_count)
            .filter_map(|(owner, _)| guard.remove(&owner))
            .collect::<Vec<_>>()
    };
    drop(removed);
}

struct GateState {
    last_start: Option<tokio::time::Instant>,
    spacing: Duration,
    successes: u32,
}

/// Espacement adaptatif des ouvertures de WebSocket.
///
/// Un espacement fixe protège l'origine des rafales d'upgrades (voir 529e1d6) mais la
/// facture est permanente : la continuation étant désactivée par défaut, chaque requête
/// ouvre une socket neuve, et un espacement d'une seconde plafonne le PROCESSUS entier
/// à environ une génération par seconde même quand l'origine accepte tout. Mesuré le
/// 2026-09-07 sur un compte au repos : 12 requêtes triviales en 13.27 s (0.90 req/s).
///
/// L'espacement devient donc le prix d'un refus constaté et non une taxe d'avance : il
/// part du plancher configuré (zéro par défaut), double à chaque upgrade refusé jusqu'à
/// un plafond, puis se relâche après une série de connexions réussies.
struct WebSocketConnectGate {
    state: AsyncMutex<GateState>,
    floor: Duration,
}

impl WebSocketConnectGate {
    fn new(start_spacing: Duration) -> Self {
        Self {
            state: AsyncMutex::new(GateState {
                last_start: None,
                spacing: start_spacing,
                successes: 0,
            }),
            floor: start_spacing,
        }
    }

    async fn wait_to_start(&self) {
        let mut state = self.state.lock().await;
        if let Some(previous) = state.last_start
            && !state.spacing.is_zero()
        {
            tokio::time::sleep_until(previous + state.spacing).await;
        }
        cleanup_pool_before_connect();
        state.last_start = Some(tokio::time::Instant::now());
    }

    async fn note_origin_forbidden(&self) {
        // A busy gate must still account for rejected upgrades.
        let mut state = self.state.lock().await;
        state.successes = 0;
        state.spacing = if state.spacing.is_zero() {
            WEBSOCKET_CONNECT_BACKOFF_STEP
        } else {
            state.spacing.saturating_mul(2)
        }
        .min(WEBSOCKET_CONNECT_MAX_SPACING)
        .max(self.floor);
    }

    async fn note_connect_success(&self) {
        let mut state = self.state.lock().await;
        if state.spacing <= self.floor {
            return;
        }
        state.successes += 1;
        if state.successes < WEBSOCKET_CONNECT_RELAX_AFTER {
            return;
        }
        state.successes = 0;
        state.spacing = (state.spacing / 2).max(self.floor);
        if state.spacing < WEBSOCKET_CONNECT_BACKOFF_STEP {
            state.spacing = self.floor;
        }
    }
}

// ---------------------------------------------------------------------------
// URL conversion
// ---------------------------------------------------------------------------

pub fn to_websocket_url(url: &str) -> Result<String, CodexWebSocketError> {
    let mut parsed = url::Url::parse(url)
        .map_err(|e| CodexWebSocketError::new(format!("Failed to parse URL: {e}")))?;
    match parsed.scheme() {
        "http" => parsed.set_scheme("ws").map_err(|_| {
            CodexWebSocketError::new("Unsupported Codex WebSocket URL scheme".to_string())
        })?,
        "https" => parsed.set_scheme("wss").map_err(|_| {
            CodexWebSocketError::new("Unsupported Codex WebSocket URL scheme".to_string())
        })?,
        "ws" | "wss" => { /* already a ws scheme */ }
        other => {
            return Err(CodexWebSocketError::new(format!(
                "Unsupported Codex WebSocket URL scheme: {other}"
            )));
        }
    }
    Ok(parsed.to_string())
}

fn to_http_upgrade_url(url: &str) -> Result<String, CodexWebSocketError> {
    let mut parsed = url::Url::parse(url)
        .map_err(|e| CodexWebSocketError::new(format!("Failed to parse URL: {e}")))?;
    match parsed.scheme() {
        "ws" => parsed.set_scheme("http").map_err(|_| {
            CodexWebSocketError::new("Unsupported Codex WebSocket URL scheme".to_string())
        })?,
        "wss" => parsed.set_scheme("https").map_err(|_| {
            CodexWebSocketError::new("Unsupported Codex WebSocket URL scheme".to_string())
        })?,
        other => {
            return Err(CodexWebSocketError::new(format!(
                "Unsupported Codex WebSocket URL scheme: {other}"
            )));
        }
    }
    Ok(parsed.to_string())
}

// ---------------------------------------------------------------------------
// Header rewriting
// ---------------------------------------------------------------------------

pub fn codex_websocket_headers(http_headers: &HeaderMap) -> HeaderMap {
    let mut ws = HeaderMap::new();
    for (key, value) in http_headers.iter() {
        let key_str = key.as_str().to_lowercase();
        // Skip hop-by-hop headers
        if matches!(
            key_str.as_str(),
            "content-length"
                | "content-type"
                | "accept"
                | "connection"
                | "upgrade"
                | "proxy-authorization"
        ) {
            continue;
        }
        ws.insert(key.clone(), value.clone());
    }
    // Rewrite openai-beta for WebSocket protocol
    ws.insert("openai-beta", WEBSOCKET_PROTOCOL_HEADER.parse().unwrap());
    // Ensure WebSocket key is present
    if !ws.contains_key("sec-websocket-key") {
        ws.insert("sec-websocket-key", generate_key().parse().unwrap());
    }
    ws
}

// ---------------------------------------------------------------------------
// SSE framing
// ---------------------------------------------------------------------------

fn append_sse(out: &mut Vec<u8>, text: &str) -> Result<(), CodexError> {
    let incoming = text.lines().fold(1usize, |size, line| {
        size.saturating_add(line.len()).saturating_add(7)
    });
    super::client::ensure_transport_buffer_budget(
        out.len(),
        incoming,
        CodexErrorOrigin::WebSocket,
        "WebSocket response body/capture",
    )?;
    for line in text.lines() {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out.push(b'\n');
    Ok(())
}

fn frame_budget(text: &str) -> Result<(), CodexError> {
    if text.len() > super::client::MAX_HTTP_SSE_FRAME_BYTES {
        return Err(CodexError {
            status: 0,
            message: "Codex WebSocket event exceeded its 8 MiB frame limit".to_owned(),
            detail: Some("transport_buffer_limit".to_owned()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Terminal event detection
// ---------------------------------------------------------------------------

fn is_response_event(payload: &serde_json::Value) -> bool {
    match payload.get("type").and_then(|v| v.as_str()) {
        Some("error") => true,
        Some(t) => t.starts_with("response."),
        None => false,
    }
}

fn is_previous_response_missing(payload: &serde_json::Value) -> bool {
    let error = super::events::event_error(payload);
    if error
        .and_then(|error| error.get("code"))
        .and_then(|value| value.as_str())
        == Some("previous_response_not_found")
    {
        return true;
    }
    // Case-insensitive message check
    if let Some(msg) = error
        .and_then(|error| error.get("message"))
        .and_then(|value| value.as_str())
    {
        let lower = msg.to_lowercase();
        if lower.contains("previous response") && lower.contains("not found") {
            return true;
        }
    }
    false
}

fn reconnect_event_error(payload: &serde_json::Value) -> Option<CodexError> {
    let connection_limit = super::events::event_error(payload)
        .and_then(|error| error.get("code"))
        .and_then(serde_json::Value::as_str)
        == Some(WEBSOCKET_CONNECTION_LIMIT_REACHED);
    if !connection_limit && !is_previous_response_missing(payload) {
        return None;
    }
    let failure = super::events::classify_event_failure(payload)?;
    Some(CodexError {
        status: if connection_limit { failure.status } else { 0 },
        message: failure.message,
        detail: Some(if connection_limit {
            WEBSOCKET_CONNECTION_LIMIT_REACHED.to_string()
        } else {
            "previous_response_not_found".to_string()
        }),
        retry_after: failure.retry_after,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocket,
    })
}

pub(super) fn event_error_status(payload: &serde_json::Value) -> Option<u16> {
    super::events::classify_event_failure(payload).and_then(|failure| failure.explicit_status)
}

// ---------------------------------------------------------------------------
// Main request function
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(super) async fn codex_websocket_request(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    body_value: &serde_json::Value,
    traffic: Option<&TrafficCapture>,
    connect_timeout_ms: u64,
    idle_timeout_ms: u64,
    reservation: Option<&ContinuationReservation>,
    provenance: Option<super::models::OperationProvenance>,
) -> Result<OwnerAwareCodexResponse, CodexError> {
    super::rate_limits::record(Vec::new());
    let continuation = reservation.map(ContinuationReservation::candidate);
    let pool_owner = reservation_pool_owner(reservation);
    let ws_url = to_websocket_url(url).map_err(|e| CodexError {
        status: 0,
        message: e.message,
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;
    let body_json = serde_json::to_string(body_value).unwrap_or_default();
    if let Some(tc) = traffic {
        tc.write_json("020-upstream-request", body_value);
        tc.write_json(
            "021-upstream-request-metadata",
            &serde_json::json!({
                "provider": "codex",
                "transport": "websocket",
                "url": ws_url,
                "method": "GET",
                "headers": headers_to_json(headers),
                "size": summarize_json_request_size(body_value, &body_json),
                "continuation": {
                    "previousResponseId": continuation
                        .and_then(|c| c.previous_response_id.as_deref()),
                    "inputDeltaCount": continuation
                        .and_then(|c| c.input_delta.as_ref())
                        .map(|items| items.len()),
                    "disabledReason": continuation
                        .and_then(|c| c.disabled_reason.as_deref()),
                },
            }),
        );
    }
    let started_at = Instant::now();

    let requires_origin = continuation
        .and_then(|candidate| candidate.previous_response_id.as_deref())
        .is_some();
    let pooled = take_pool_entry_for_request(reservation, provenance.as_ref())?;
    let mut used_pooled = pooled.is_some();
    let mut entry = if let Some(entry) = pooled {
        entry
    } else {
        Arc::new(PoolEntry::authenticated(
            connect_with_timeout(
                websocket_client,
                proxy_config,
                &ws_url,
                headers,
                connect_timeout_ms,
            )
            .await?,
            provenance.clone(),
        ))
    };
    let mut guard = entry.ws.clone().lock_owned().await;

    if used_pooled
        && validate_pooled_websocket(&mut guard, connect_timeout_ms)
            .await
            .is_err()
    {
        drop(guard);
        if let Some(owner) = pool_owner {
            invalidate_pool_entry(owner, &entry);
        }
        if requires_origin {
            return Err(continuation_socket_missing_error());
        }
        entry = Arc::new(PoolEntry::authenticated(
            connect_with_timeout(
                websocket_client,
                proxy_config,
                &ws_url,
                headers,
                connect_timeout_ms,
            )
            .await?,
            provenance.clone(),
        ));
        guard = entry.ws.clone().lock_owned().await;
        used_pooled = false;
    }

    send_ws_message(&mut guard, Message::Text(body_json), idle_timeout_ms)
        .await
        .inspect_err(|_| {
            if let Some(owner) = pool_owner {
                invalidate_pool_entry(owner, &entry);
            }
        })?;

    let collected = collect_ws_events(
        &mut guard,
        idle_timeout_ms,
        pool_owner,
        Some(&entry),
        traffic,
    )
    .await;
    drop(guard);
    let (sse_body, terminal_event) = match collected {
        Ok(result) => result,
        Err(error) => {
            if let Some(owner) = pool_owner {
                invalidate_pool_entry(owner, &entry);
            }
            return Err(error);
        }
    };
    let Some(terminal_event) = terminal_event else {
        if let Some(owner) = pool_owner {
            invalidate_pool_entry(owner, &entry);
        }
        return Err(missing_terminal_error());
    };

    if let Some(error) = reconnect_event_error(&terminal_event.payload) {
        if let Some(owner) = pool_owner {
            invalidate_pool_entry(owner, &entry);
        }
        return Err(error);
    }

    let completed = terminal_event.event_type == "response.completed";
    let origin_reinserted = if completed {
        reservation.is_some_and(|reservation| pool_insert_for_turn(reservation, entry.clone()))
    } else {
        if let Some(owner) = pool_owner {
            invalidate_pool_entry(owner, &entry);
        }
        false
    };
    let status = if terminal_event.event_type == "error" {
        event_error_status(&terminal_event.payload).unwrap_or(500)
    } else {
        200
    };

    if let Some(tc) = traffic {
        write_websocket_metadata_capture(tc, &ws_url, reservation, used_pooled);
        write_websocket_response_capture(tc, status, started_at.elapsed(), &sse_body);
    }

    Ok(OwnerAwareCodexResponse::new(
        CodexResponse {
            body: sse_body,
            status,
            headers: vec![],
            transport: ActualTransport::WebSocket,
        },
        origin_reinserted.then_some(entry.socket_id),
    ))
}

async fn send_ws_message<S>(
    websocket: &mut WebSocketStream<S>,
    message: Message,
    timeout_ms: u64,
) -> Result<(), CodexError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let result =
        tokio::time::timeout(Duration::from_millis(timeout_ms), websocket.send(message)).await;
    match result {
        Ok(Ok(())) => Ok(()),
        _ => Err(CodexError {
            status: 0,
            message: match result {
                Ok(Err(error)) => format!("WebSocket send error: {error}"),
                _ => format!("WebSocket send timed out after {timeout_ms}ms"),
            },
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        }),
    }
}

async fn validate_pooled_websocket<S>(
    websocket: &mut WebSocketStream<S>,
    timeout_ms: u64,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let nonce = next_monotonic_nonzero(&POOLED_VALIDATION_SEQUENCE, "pooled validation")
        .to_be_bytes()
        .to_vec();
    tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        websocket
            .send(Message::Ping(nonce.clone()))
            .await
            .map_err(|error| error.to_string())?;
        loop {
            match websocket.next().await {
                Some(Ok(Message::Pong(payload))) if payload == nonce => return Ok(()),
                Some(Ok(Message::Ping(payload))) => websocket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|error| error.to_string())?,
                Some(Ok(Message::Pong(_))) => {
                    return Err("unexpected Pong during pooled validation".to_string());
                }
                Some(Ok(_)) => {
                    return Err("unexpected frame during pooled validation".to_string());
                }
                Some(Err(error)) => return Err(error.to_string()),
                None => return Err("connection closed during pooled validation".to_string()),
            }
        }
    })
    .await
    .map_err(|_| "validation timeout".to_string())?
}

pub(super) struct ReadyWebSocket {
    ws_url: String,
    guard: OwnedMutexGuard<CodexWebSocketStream>,
    entry: Arc<PoolEntry>,
    used_pooled: bool,
    reservation: Option<ContinuationReservation>,
    traffic: Option<Arc<TrafficCapture>>,
    idle_timeout_ms: u64,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_codex_websocket(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    traffic: Option<Arc<TrafficCapture>>,
    reservation: Option<&ContinuationReservation>,
    connect_timeout_ms: u64,
    idle_timeout_ms: u64,
    provenance: Option<super::models::OperationProvenance>,
) -> Result<ReadyWebSocket, CodexError> {
    super::rate_limits::record(Vec::new());
    let pool_owner = reservation_pool_owner(reservation);
    let continuation = reservation.map(ContinuationReservation::candidate);
    let ws_url = to_websocket_url(url).map_err(|error| CodexError {
        status: 0,
        message: error.message,
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;
    let requires_origin = continuation
        .and_then(|candidate| candidate.previous_response_id.as_deref())
        .is_some();
    let pooled = take_pool_entry_for_request(reservation, provenance.as_ref())?;
    let used_pooled = pooled.is_some();
    let entry = if let Some(entry) = pooled {
        entry
    } else {
        let stream = connect_with_timeout(
            websocket_client,
            proxy_config,
            &ws_url,
            headers,
            connect_timeout_ms,
        )
        .await?;
        Arc::new(PoolEntry::authenticated(stream, provenance))
    };
    let mut guard = entry.ws.clone().lock_owned().await;
    if used_pooled
        && let Err(detail) = validate_pooled_websocket(&mut guard, connect_timeout_ms).await
    {
        drop(guard);
        if let Some(owner) = pool_owner {
            invalidate_pool_entry(owner, &entry);
        }
        return Err(if requires_origin {
            continuation_socket_missing_error()
        } else {
            pooled_validation_error(detail)
        });
    }
    Ok(ReadyWebSocket {
        ws_url,
        guard,
        entry,
        used_pooled,
        reservation: reservation.cloned(),
        traffic,
        idle_timeout_ms,
    })
}

fn pooled_validation_error(detail: String) -> CodexError {
    CodexError {
        status: 0,
        message: format!("WebSocket pooled connection validation failed: {detail}"),
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

pub(super) fn start_codex_websocket_events(
    ready: ReadyWebSocket,
    body_value: &serde_json::Value,
    body_json: String,
    headers: &HeaderMap,
    reservation: Option<&ContinuationReservation>,
) -> CodexWebSocketEventStream {
    if let Some(tc) = ready.traffic.as_deref() {
        write_websocket_metadata_capture(tc, &ready.ws_url, reservation, ready.used_pooled);
        tc.write_json("020-upstream-request", body_value);
        tc.write_json(
            "021-upstream-request-metadata",
            &serde_json::json!({
                "provider": "codex",
                "transport": "websocket",
                "headers": headers_to_json(headers),
                "size": summarize_json_request_size(body_value, &body_json),
            }),
        );
    }
    let (tx, rx) = mpsc::channel(1);
    let (receiver, socket_id_publisher) = CodexWebSocketEventStream::pending(rx);
    socket_id_publisher.publish_provenance(ready.entry.provenance.clone());
    let telemetry = super::rate_limits::current();
    tokio::spawn(super::rate_limits::with_telemetry(telemetry, async move {
        let ReadyWebSocket {
            ws_url: _,
            mut guard,
            entry,
            used_pooled: _,
            reservation,
            traffic,
            idle_timeout_ms,
        } = ready;
        let pool_owner = reservation_pool_owner(reservation.as_ref());
        let sent = tokio::select! {
            biased;
            _ = tx.closed() => None,
            result = send_ws_message(&mut guard, Message::Text(body_json), idle_timeout_ms) => Some(result),
        };
        if !matches!(sent, Some(Ok(()))) {
            drop(guard);
            if let Some(owner) = pool_owner {
                invalidate_pool_entry(owner, &entry);
            }
            socket_id_publisher.publish(None);
            if let Some(Err(error)) = sent {
                let _ = tx.send(Err(error)).await;
            }
            return;
        }
        let (reusable, terminal_item) =
            stream_ws_events(&mut guard, idle_timeout_ms, traffic, &tx).await;
        drop(guard);

        let origin_reinserted = if reusable {
            reservation
                .as_ref()
                .is_some_and(|reservation| pool_insert_for_turn(reservation, entry.clone()))
        } else {
            if let Some(owner) = pool_owner {
                invalidate_pool_entry(owner, &entry);
            }
            false
        };
        socket_id_publisher.publish(origin_reinserted.then_some(entry.socket_id));
        if let Some(item) = terminal_item
            && tx.send(item).await.is_err()
            && origin_reinserted
            && let Some(owner) = pool_owner
        {
            invalidate_pool_entry(owner, &entry);
        }
    }));
    receiver
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn codex_websocket_event_stream(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    body_value: &serde_json::Value,
    traffic: Option<Arc<TrafficCapture>>,
    connect_timeout_ms: u64,
    idle_timeout_ms: u64,
    reservation: Option<&ContinuationReservation>,
    provenance: Option<super::models::OperationProvenance>,
) -> Result<CodexWebSocketEventStream, CodexError> {
    let body_json = serde_json::to_string(body_value).map_err(|error| CodexError {
        status: 500,
        message: "Failed to serialize WebSocket request".to_string(),
        detail: Some(error.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;
    let ready = prepare_codex_websocket(
        websocket_client,
        proxy_config,
        url,
        headers,
        traffic,
        reservation,
        connect_timeout_ms,
        idle_timeout_ms,
        provenance,
    )
    .await?;
    Ok(start_codex_websocket_events(
        ready,
        body_value,
        body_json,
        headers,
        reservation,
    ))
}

fn continuation_socket_missing_error() -> CodexError {
    CodexError {
        status: 0,
        message: "Previous response socket is no longer available".to_string(),
        detail: Some(WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

fn missing_terminal_error() -> CodexError {
    CodexError {
        status: 0,
        message: "WebSocket connection closed before terminal Codex response event".to_string(),
        detail: Some(WEBSOCKET_MISSING_TERMINAL_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

fn response_start_timeout_error(timeout_ms: u64) -> CodexError {
    CodexError {
        status: 0,
        message: format!("WebSocket response start timeout after {timeout_ms}ms"),
        detail: Some(WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

fn write_websocket_metadata_capture(
    traffic: &TrafficCapture,
    ws_url: &str,
    reservation: Option<&ContinuationReservation>,
    pooled: bool,
) {
    let pool_owner = reservation_pool_owner(reservation);
    let continuation = reservation.map(ContinuationReservation::candidate);
    traffic.write_json(
        "022-upstream-websocket-metadata",
        &serde_json::json!({
            "provider": "codex",
            "transport": "websocket",
            "url": ws_url,
            "poolingEnabled": pool_owner.is_some(),
            "pooled": pooled,
            "continuation": {
                "previousResponseId": continuation
                    .and_then(|c| c.previous_response_id.as_deref()),
                "inputDeltaCount": continuation
                    .and_then(|c| c.input_delta.as_ref())
                    .map(|items| items.len()),
                "disabledReason": continuation
                    .and_then(|c| c.disabled_reason.as_deref()),
            },
        }),
    );
}

fn write_websocket_response_capture(
    traffic: &TrafficCapture,
    status: u16,
    elapsed: Duration,
    sse_body: &[u8],
) {
    traffic.write_json(
        "030-upstream-response-headers",
        &serde_json::json!({
            "status": status,
            "elapsedMs": elapsed.as_millis(),
            "headers": {
                "content-type": "text/event-stream",
            },
        }),
    );
    if status >= 400 {
        traffic.write_text(
            "031-upstream-error-body",
            &String::from_utf8_lossy(sse_body),
        );
    } else {
        traffic.write_bytes("032-upstream-response-body.sse", sse_body);
    }
}

// ---------------------------------------------------------------------------
// Connection helper
// ---------------------------------------------------------------------------

const MAX_HANDSHAKE_ERROR_DETAIL_BYTES: usize = 1024;
const GENERIC_HANDSHAKE_ERROR_DETAIL: &str = "WebSocket upgrade was rejected";

fn handshake_error_detail(body: Option<&[u8]>) -> String {
    let Some(value) = body.and_then(|body| serde_json::from_slice::<serde_json::Value>(body).ok())
    else {
        return GENERIC_HANDSHAKE_ERROR_DETAIL.to_string();
    };
    let Some(message) = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .and_then(|value| value.as_str())
    else {
        return GENERIC_HANDSHAKE_ERROR_DETAIL.to_string();
    };
    let sanitized: String = message
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, ' '))
        .collect();
    let mut end = sanitized.len().min(MAX_HANDSHAKE_ERROR_DETAIL_BYTES);
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    sanitized[..end].to_string()
}

fn header_has_token(headers: &HeaderMap, name: &str, expected: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().ok().is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case(expected))
        })
    })
}

fn requested_subprotocols(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(http::header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn websocket_protocol_error(message: &str) -> CodexError {
    CodexError {
        status: 0,
        message: message.to_string(),
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

fn validate_websocket_upgrade(
    version: http::Version,
    headers: &HeaderMap,
    websocket_key: &str,
    requested_subprotocols: &[String],
) -> Result<(), CodexError> {
    if version != http::Version::HTTP_11 {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response did not use HTTP/1.1",
        ));
    }
    if !header_has_token(headers, http::header::UPGRADE.as_str(), "websocket") {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response is missing Upgrade: websocket",
        ));
    }
    if !header_has_token(headers, http::header::CONNECTION.as_str(), "upgrade") {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response is missing Connection: Upgrade",
        ));
    }

    let expected_accept = derive_accept_key(websocket_key.as_bytes());
    let mut accept_values = headers.get_all(http::header::SEC_WEBSOCKET_ACCEPT).iter();
    let accept = accept_values.next().and_then(|value| value.to_str().ok());
    if accept_values.next().is_some() || accept != Some(expected_accept.as_str()) {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response has an invalid Sec-WebSocket-Accept",
        ));
    }
    if headers.contains_key(http::header::SEC_WEBSOCKET_EXTENSIONS) {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response selected an unsolicited extension",
        ));
    }

    let mut response_protocols = headers.get_all(http::header::SEC_WEBSOCKET_PROTOCOL).iter();
    let response_protocol = response_protocols
        .next()
        .map(|value| value.to_str().map(str::trim));
    if response_protocols.next().is_some() {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response contains multiple subprotocols",
        ));
    }
    match response_protocol {
        None if requested_subprotocols.is_empty() => {}
        None => {
            return Err(websocket_protocol_error(
                "WebSocket upgrade response omitted the requested subprotocol",
            ));
        }
        Some(Err(_)) => {
            return Err(websocket_protocol_error(
                "WebSocket upgrade response contains an invalid subprotocol",
            ));
        }
        Some(Ok(_)) if requested_subprotocols.is_empty() => {
            return Err(websocket_protocol_error(
                "WebSocket upgrade response selected an unsolicited subprotocol",
            ));
        }
        Some(Ok(protocol))
            if !requested_subprotocols
                .iter()
                .any(|requested| requested == protocol) =>
        {
            return Err(websocket_protocol_error(
                "WebSocket upgrade response selected an unsupported subprotocol",
            ));
        }
        Some(Ok(_)) => {}
    }

    Ok(())
}

async fn bounded_handshake_error_body(mut response: reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    while body.len() < MAX_HANDSHAKE_ERROR_DETAIL_BYTES {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) | Err(_) => break,
        };
        let remaining = MAX_HANDSHAKE_ERROR_DETAIL_BYTES - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    body
}

fn error_chain_contains(error: &(dyn std::error::Error + 'static), expected: &str) -> bool {
    let expected = expected.to_ascii_lowercase();
    let mut current = Some(error);
    while let Some(error) = current {
        if error.to_string().to_ascii_lowercase().contains(&expected) {
            return true;
        }
        current = error.source();
    }
    false
}

fn reqwest_handshake_error(error: reqwest::Error) -> CodexError {
    let proxy_auth_required =
        error.is_connect() && error_chain_contains(&error, "proxy authorization required");
    let proxy_tunnel_rejected =
        error.is_connect() && error_chain_contains(&error, "tunnel error: unsuccessful");
    let status = if proxy_auth_required {
        http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16()
    } else {
        error.status().map(|status| status.as_u16()).unwrap_or(0)
    };
    let message = if proxy_auth_required {
        "WebSocket proxy authentication failed"
    } else if proxy_tunnel_rejected {
        "WebSocket proxy tunnel was rejected"
    } else if error.is_timeout() {
        "WebSocket upgrade request timed out"
    } else if error.is_connect() {
        "WebSocket connection failed"
    } else {
        "WebSocket upgrade request failed"
    };
    CodexError {
        status,
        message: message.to_string(),
        detail: proxy_tunnel_rejected.then(|| WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

struct PrefixedIo {
    prefix: Vec<u8>,
    position: usize,
    inner: BoxedWebSocketIo,
}

impl AsyncRead for PrefixedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.position < this.prefix.len() {
            let available = &this.prefix[this.position..];
            let len = available.len().min(buffer.remaining());
            buffer.put_slice(&available[..len]);
            this.position += len;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buffer)
    }
}

impl AsyncWrite for PrefixedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

fn skip_websocket_request_header(name: &http::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "upgrade"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "host"
            | "content-length"
            | "proxy-authorization"
    )
}

fn tunneled_websocket_request(
    url: &str,
    headers: &HeaderMap,
    websocket_key: &str,
) -> Result<http::Request<()>, CodexError> {
    let mut request = url
        .into_client_request()
        .map_err(|_| websocket_protocol_error("WebSocket request URL was invalid"))?;
    *request.version_mut() = http::Version::HTTP_11;
    request.headers_mut().insert(
        http::header::SEC_WEBSOCKET_KEY,
        http::HeaderValue::from_str(websocket_key)
            .map_err(|_| websocket_protocol_error("WebSocket key was invalid"))?,
    );
    for (name, value) in headers {
        if !skip_websocket_request_header(name) {
            request.headers_mut().append(name.clone(), value.clone());
        }
    }
    Ok(request)
}

fn tunnel_error(status: u16, retry_after: Option<String>) -> CodexError {
    let proxy_auth_required = status == http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16();
    CodexError {
        status,
        message: if proxy_auth_required {
            "WebSocket proxy authentication failed".to_string()
        } else {
            "WebSocket proxy tunnel was rejected".to_string()
        },
        detail: if proxy_auth_required {
            Some(GENERIC_HANDSHAKE_ERROR_DETAIL.to_string())
        } else {
            Some(WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL.to_string())
        },
        retry_after,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

fn invalid_tunnel_response(message: &str) -> CodexError {
    CodexError {
        status: 0,
        message: message.to_string(),
        detail: Some(WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

fn connect_response_header_end(response: &[u8]) -> Option<usize> {
    response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

fn parse_connect_response_head(response: &[u8]) -> Result<(u16, Option<String>), CodexError> {
    let status_line_end = response
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or_else(|| invalid_tunnel_response("WebSocket proxy returned an invalid response"))?;
    let status_line = std::str::from_utf8(&response[..status_line_end])
        .map_err(|_| invalid_tunnel_response("WebSocket proxy returned an invalid response"))?;
    let mut parts = status_line.split_ascii_whitespace();
    let version = parts.next().unwrap_or_default();
    let status = parts.next().unwrap_or_default();
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || status.len() != 3
        || !status.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid_tunnel_response(
            "WebSocket proxy returned an invalid response",
        ));
    }
    let status = status
        .parse::<u16>()
        .map_err(|_| invalid_tunnel_response("WebSocket proxy returned an invalid response"))?;
    let retry_after = response[status_line_end + 2..]
        .split(|byte| *byte == b'\n')
        .find_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let separator = line.iter().position(|byte| *byte == b':')?;
            let (name, value) = line.split_at(separator);
            if !name.eq_ignore_ascii_case(b"retry-after") {
                return None;
            }
            std::str::from_utf8(&value[1..])
                .ok()
                .map(|value| value.trim().to_string())
        });
    Ok((status, retry_after))
}

async fn establish_connect_tunnel(
    mut stream: BoxedWebSocketIo,
    authority: &str,
    basic_auth: Option<&http::HeaderValue>,
) -> Result<BoxedWebSocketIo, CodexError> {
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n").into_bytes();
    if let Some(auth) = basic_auth {
        request.extend_from_slice(b"Proxy-Authorization: ");
        request.extend_from_slice(auth.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    stream.write_all(&request).await.map_err(|_| CodexError {
        status: 0,
        message: "WebSocket proxy tunnel request failed".to_string(),
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;

    let mut response = Vec::new();
    loop {
        if let Some(header_end) = connect_response_header_end(&response) {
            let (status, retry_after) = parse_connect_response_head(&response[..header_end])?;
            if (100..200).contains(&status) {
                response.drain(..header_end);
                continue;
            }
            if (200..300).contains(&status) {
                let prefix = response.split_off(header_end);
                return Ok(Box::new(PrefixedIo {
                    prefix,
                    position: 0,
                    inner: stream,
                }));
            }
            return Err(tunnel_error(status, retry_after));
        }
        if response.len() == MAX_CONNECT_RESPONSE_HEADER_BYTES {
            return Err(invalid_tunnel_response(
                "WebSocket proxy response headers were too large",
            ));
        }
        let remaining = MAX_CONNECT_RESPONSE_HEADER_BYTES - response.len();
        let mut buffer = [0_u8; 1024];
        let capacity = remaining.min(buffer.len());
        let read = stream
            .read(&mut buffer[..capacity])
            .await
            .map_err(|_| invalid_tunnel_response("WebSocket proxy response could not be read"))?;
        if read == 0 {
            return Err(invalid_tunnel_response(
                "WebSocket proxy closed the tunnel response early",
            ));
        }
        response.extend_from_slice(&buffer[..read]);
    }
}

async fn tls_connect(
    stream: BoxedWebSocketIo,
    host: &str,
    tls_config: Arc<rustls::ClientConfig>,
    peer: &str,
) -> Result<BoxedWebSocketIo, CodexError> {
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| websocket_protocol_error("WebSocket TLS host name was invalid"))?;
    let stream = tokio_rustls::TlsConnector::from(tls_config)
        .connect(server_name, stream)
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!("WebSocket TLS connection to {peer} failed"),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        })?;
    Ok(Box::new(stream))
}

async fn connect_to_http_proxy(
    route: &WebSocketProxyRoute,
    tls_config: Arc<rustls::ClientConfig>,
) -> Result<BoxedWebSocketIo, CodexError> {
    let scheme = route.uri.scheme_str().unwrap_or_default();
    let host = route
        .uri
        .host()
        .ok_or_else(|| websocket_protocol_error("WebSocket proxy URL did not contain a host"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = route
        .uri
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    let stream = TcpStream::connect((host, port))
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: "WebSocket proxy connection failed".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        })?;
    let stream: BoxedWebSocketIo = Box::new(stream);
    if scheme == "https" {
        tls_connect(stream, host, tls_config, "proxy").await
    } else {
        Ok(stream)
    }
}

fn websocket_destination(url: &str) -> Result<(String, String), CodexError> {
    let destination = url::Url::parse(url)
        .map_err(|_| websocket_protocol_error("WebSocket destination URL was invalid"))?;
    let host = destination
        .host_str()
        .ok_or_else(|| websocket_protocol_error("WebSocket destination URL had no host"))?;
    let port = destination
        .port_or_known_default()
        .ok_or_else(|| websocket_protocol_error("WebSocket destination URL had no usable port"))?;
    let authority = match destination.host() {
        Some(url::Host::Ipv6(address)) => format!("[{address}]:{port}"),
        Some(_) => format!("{host}:{port}"),
        None => {
            return Err(websocket_protocol_error(
                "WebSocket destination URL had no host",
            ));
        }
    };
    Ok((host.to_string(), authority))
}

fn tungstenite_handshake_error(error: tokio_tungstenite::tungstenite::Error) -> CodexError {
    if let tokio_tungstenite::tungstenite::Error::Http(response) = error {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        return CodexError {
            status,
            message: format!("WebSocket upgrade rejected with status {status}"),
            detail: Some(GENERIC_HANDSHAKE_ERROR_DETAIL.to_string()),
            retry_after,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        };
    }
    CodexError {
        status: 0,
        message: "WebSocket upgrade request failed".to_string(),
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

enum ConnectAttemptError {
    Origin(CodexError),
    ProxyTunnel(CodexError),
}

impl ConnectAttemptError {
    fn is_origin_forbidden(&self) -> bool {
        matches!(
            self,
            Self::Origin(error) if error.status == http::StatusCode::FORBIDDEN.as_u16()
        )
    }

    fn into_error(self) -> CodexError {
        match self {
            Self::Origin(error) | Self::ProxyTunnel(error) => error,
        }
    }
}

async fn connect_via_http_proxy_tunnel(
    proxy_config: &WebSocketProxyConfig,
    route: WebSocketProxyRoute,
    url: &str,
    headers: &HeaderMap,
) -> Result<CodexWebSocketStream, ConnectAttemptError> {
    let (host, authority) = websocket_destination(url).map_err(ConnectAttemptError::Origin)?;
    let stream = connect_to_http_proxy(&route, proxy_config.tls_config.clone())
        .await
        .map_err(ConnectAttemptError::ProxyTunnel)?;
    let stream = establish_connect_tunnel(stream, &authority, route.basic_auth.as_ref())
        .await
        .map_err(ConnectAttemptError::ProxyTunnel)?;
    let stream = tls_connect(
        stream,
        &host,
        proxy_config.tls_config.clone(),
        "destination",
    )
    .await
    .map_err(ConnectAttemptError::Origin)?;
    let websocket_key = generate_key();
    let subprotocols = requested_subprotocols(headers);
    let request = tunneled_websocket_request(url, headers, &websocket_key)
        .map_err(ConnectAttemptError::Origin)?;
    let (websocket, response) =
        tokio_tungstenite::client_async_with_config(request, stream, Some(websocket_config()))
            .await
            .map_err(tungstenite_handshake_error)
            .map_err(ConnectAttemptError::Origin)?;
    validate_websocket_upgrade(
        response.version(),
        response.headers(),
        &websocket_key,
        &subprotocols,
    )
    .map_err(ConnectAttemptError::Origin)?;
    Ok(websocket)
}

fn websocket_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(super::client::MAX_HTTP_SSE_FRAME_BYTES),
        max_frame_size: Some(super::client::MAX_HTTP_SSE_FRAME_BYTES),
        ..Default::default()
    }
}

async fn connect_via_http_upgrade(
    websocket_client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
) -> Result<CodexWebSocketStream, CodexError> {
    let http_url = to_http_upgrade_url(url).map_err(|error| CodexError {
        status: 0,
        message: error.message,
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;
    let websocket_key = generate_key();
    let subprotocols = requested_subprotocols(headers);
    let mut request = websocket_client
        .get(http_url)
        .version(http::Version::HTTP_11)
        .header(http::header::CONNECTION, "Upgrade")
        .header(http::header::UPGRADE, "websocket")
        .header(http::header::SEC_WEBSOCKET_VERSION, "13")
        .header(http::header::SEC_WEBSOCKET_KEY, &websocket_key);

    for (key, value) in headers {
        if skip_websocket_request_header(key) {
            continue;
        }
        request = request.header(key.clone(), value.clone());
    }

    let response = request.send().await.map_err(reqwest_handshake_error)?;
    if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let detail = if status == http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16() {
            GENERIC_HANDSHAKE_ERROR_DETAIL.to_string()
        } else {
            let body = bounded_handshake_error_body(response).await;
            handshake_error_detail(Some(&body))
        };
        return Err(CodexError {
            status,
            message: format!("WebSocket upgrade rejected with status {status}"),
            detail: Some(detail),
            retry_after,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        });
    }

    validate_websocket_upgrade(
        response.version(),
        response.headers(),
        &websocket_key,
        &subprotocols,
    )?;
    let upgraded = response
        .upgrade()
        .await
        .map_err(|_| websocket_protocol_error("WebSocket upgrade stream was not available"))?;
    let upgraded: BoxedWebSocketIo = Box::new(upgraded);
    Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(websocket_config())).await)
}

async fn connect_once(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
) -> Result<CodexWebSocketStream, ConnectAttemptError> {
    if let Some(route) = proxy_config
        .http_connect_route(url)
        .map_err(ConnectAttemptError::Origin)?
    {
        connect_via_http_proxy_tunnel(proxy_config, route, url, headers).await
    } else {
        connect_via_http_upgrade(websocket_client, url, headers)
            .await
            .map_err(ConnectAttemptError::Origin)
    }
}

fn connect_timeout_error(connect_timeout: Duration) -> CodexError {
    websocket_protocol_error(&format!(
        "WebSocket connect timeout after {}ms",
        connect_timeout.as_millis()
    ))
}

async fn connect_with_timeout(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    connect_timeout_ms: u64,
) -> Result<CodexWebSocketStream, CodexError> {
    let gate = &*WS_CONNECT_GATE;
    let connect_timeout = Duration::from_millis(connect_timeout_ms);
    for attempt in 0..=1 {
        gate.wait_to_start().await;
        let result = tokio::time::timeout(
            connect_timeout,
            connect_once(websocket_client, proxy_config, url, headers),
        )
        .await
        .unwrap_or_else(|_| {
            Err(ConnectAttemptError::Origin(connect_timeout_error(
                connect_timeout,
            )))
        });
        // L'espacement suit ce que l'origine répond réellement : un upgrade refusé
        // l'élargit, une connexion établie le relâche vers le plancher configuré.
        if result
            .as_ref()
            .is_err_and(ConnectAttemptError::is_origin_forbidden)
        {
            gate.note_origin_forbidden().await;
            if attempt == 0 {
                retry_sleep(
                    u64::try_from(WEBSOCKET_CONNECT_FORBIDDEN_COOLDOWN.as_millis())
                        .unwrap_or(u64::MAX),
                )
                .await;
                continue;
            }
        } else if result.is_ok() {
            gate.note_connect_success().await;
        }
        return result.map_err(ConnectAttemptError::into_error);
    }
    unreachable!()
}

// ---------------------------------------------------------------------------
// Event collection
// ---------------------------------------------------------------------------

enum WebSocketRead {
    Frame(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
    Timeout,
    KeepaliveError(String),
}

async fn read_ws_frame_with_keepalive<S>(
    ws: &mut WebSocketStream<S>,
    read_timeout: Duration,
) -> WebSocketRead
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let timeout = tokio::time::sleep(read_timeout);
    tokio::pin!(timeout);
    let mut keepalive = tokio::time::interval(WEBSOCKET_KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await;

    loop {
        let message = tokio::select! {
            biased;
            frame = ws.next() => match frame {
                Some(Ok(Message::Ping(payload))) => Message::Pong(payload),
                frame => return WebSocketRead::Frame(frame),
            },
            _ = &mut timeout => return WebSocketRead::Timeout,
            _ = keepalive.tick() => Message::Ping(Vec::new()),
        };
        // Both control writes share the read deadline and caller's cancellation guard.
        let send = ws.send(message);
        let send_timeout = tokio::time::sleep(WEBSOCKET_KEEPALIVE_SEND_TIMEOUT);
        tokio::pin!(send_timeout);
        tokio::select! {
            biased;
            result = send => {
                if let Err(error) = result {
                    return WebSocketRead::KeepaliveError(error.to_string());
                }
            }
            _ = &mut timeout => return WebSocketRead::Timeout,
            _ = &mut send_timeout => {
                return WebSocketRead::KeepaliveError(format!(
                    "send timed out after {}ms",
                    WEBSOCKET_KEEPALIVE_SEND_TIMEOUT.as_millis(),
                ));
            }
        }
    }
}

struct WsEvent {
    event_type: String,
    payload: serde_json::Value,
}

async fn collect_ws_events<S>(
    ws: &mut WebSocketStream<S>,
    idle_timeout_ms: u64,
    pool_owner: Option<&ConversationIdentity>,
    pool_entry: Option<&Arc<PoolEntry>>,
    traffic: Option<&TrafficCapture>,
) -> Result<(Vec<u8>, Option<WsEvent>), CodexError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut sse_body: Vec<u8> = Vec::new();
    let mut terminal_event: Option<WsEvent> = None;
    let response_event_budget = Duration::from_millis(idle_timeout_ms);
    let response_wait_started = Instant::now();
    let mut last_response_event_at = response_wait_started;
    let mut response_started = false;

    loop {
        let response_deadline_started = if response_started {
            last_response_event_at
        } else {
            response_wait_started
        };
        let read_timeout = if response_started {
            match response_event_budget.checked_sub(response_deadline_started.elapsed()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    invalidate_pool_owner(pool_owner, pool_entry);
                    return Err(CodexError {
                        status: 0,
                        message: format!("WebSocket idle timeout after {idle_timeout_ms}ms"),
                        detail: None,
                        retry_after: None,
                        usage_limit: None,
                        origin: CodexErrorOrigin::WebSocket,
                    });
                }
            }
        } else {
            match response_event_budget.checked_sub(response_deadline_started.elapsed()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    invalidate_pool_owner(pool_owner, pool_entry);
                    return Err(response_start_timeout_error(idle_timeout_ms));
                }
            }
        };

        let frame = match read_ws_frame_with_keepalive(ws, read_timeout).await {
            WebSocketRead::Frame(frame) => frame,
            WebSocketRead::Timeout => {
                invalidate_pool_owner(pool_owner, pool_entry);
                return Err(if response_started {
                    CodexError {
                        status: 0,
                        message: format!("WebSocket idle timeout after {idle_timeout_ms}ms"),
                        detail: None,
                        retry_after: None,
                        usage_limit: None,
                        origin: CodexErrorOrigin::WebSocket,
                    }
                } else {
                    response_start_timeout_error(idle_timeout_ms)
                });
            }
            WebSocketRead::KeepaliveError(error) => {
                invalidate_pool_owner(pool_owner, pool_entry);
                return Err(CodexError {
                    status: 0,
                    message: format!("WebSocket keepalive error: {error}"),
                    detail: Some(WEBSOCKET_KEEPALIVE_FAILURE_DETAIL.to_string()),
                    retry_after: None,
                    usage_limit: None,
                    origin: CodexErrorOrigin::WebSocket,
                });
            }
        };

        match frame {
            Some(Ok(Message::Text(text))) => {
                frame_budget(&text)?;
                append_sse(&mut sse_body, &text)?;
                // Parse JSON
                let parsed: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => {
                        if let Some(tc) = traffic {
                            tc.write_json_event(
                                "040-upstream-event",
                                &serde_json::json!({
                                    "unparseable": true,
                                    "data": text,
                                }),
                            );
                        }
                        // Write invalid JSON as-is
                        continue;
                    }
                };

                // Convert to SSE bytes
                if let Some(tc) = traffic {
                    tc.write_json_event("040-upstream-event", &parsed);
                }

                if is_response_event(&parsed) {
                    response_started = true;
                    last_response_event_at = Instant::now();
                }

                // Check for terminal events
                if event_is_terminal(&parsed) {
                    terminal_event = Some(WsEvent {
                        event_type: parsed
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                        payload: parsed,
                    });
                    break;
                }
            }
            Some(Ok(Message::Binary(_))) => {
                // Reject binary frames
                invalidate_pool_owner(pool_owner, pool_entry);
                return Err(CodexError {
                    status: 0,
                    message: "WebSocket binary frames not supported".to_string(),
                    detail: None,
                    retry_after: None,
                    usage_limit: None,
                    origin: CodexErrorOrigin::WebSocket,
                });
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {
                continue;
            }
            Some(Ok(Message::Close(_))) => {
                // Connection closed - invalidate pool
                invalidate_pool_owner(pool_owner, pool_entry);
                break;
            }
            Some(Err(e)) => {
                // Stream error - invalidate pool
                invalidate_pool_owner(pool_owner, pool_entry);
                return Err(CodexError {
                    status: 0,
                    message: format!("WebSocket stream error: {e}"),
                    detail: None,
                    retry_after: None,
                    usage_limit: None,
                    origin: CodexErrorOrigin::WebSocket,
                });
            }
            None => {
                // Stream ended - invalidate pool
                invalidate_pool_owner(pool_owner, pool_entry);
                break;
            }
        }
    }

    Ok((sse_body, terminal_event))
}

async fn stream_ws_events<S>(
    ws: &mut WebSocketStream<S>,
    idle_timeout_ms: u64,
    traffic: Option<Arc<TrafficCapture>>,
    tx: &mpsc::Sender<Result<serde_json::Value, CodexError>>,
) -> (bool, Option<Result<serde_json::Value, CodexError>>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let started_at = Instant::now();
    let mut sse_body = traffic.as_ref().map(|_| Vec::new());
    let response_event_budget = Duration::from_millis(idle_timeout_ms);
    let response_wait_started = Instant::now();
    let mut last_response_event_at = response_wait_started;
    let mut response_started = false;
    let mut status = 200u16;
    let mut reusable = false;
    let mut terminal_item = None;

    loop {
        let response_deadline_started = if response_started {
            last_response_event_at
        } else {
            response_wait_started
        };
        let read_timeout =
            match response_event_budget.checked_sub(response_deadline_started.elapsed()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    terminal_item = Some(Err(if response_started {
                        CodexError {
                            status: 0,
                            message: format!("WebSocket idle timeout after {idle_timeout_ms}ms"),
                            detail: None,
                            retry_after: None,
                            usage_limit: None,
                            origin: CodexErrorOrigin::WebSocket,
                        }
                    } else {
                        response_start_timeout_error(idle_timeout_ms)
                    }));
                    break;
                }
            };

        let frame = tokio::select! {
            biased;
            _ = tx.closed() => break,
            frame = read_ws_frame_with_keepalive(ws, read_timeout) => match frame {
                WebSocketRead::Frame(frame) => frame,
                WebSocketRead::Timeout => {
                    terminal_item = Some(Err(if response_started {
                        CodexError {
                            status: 0,
                            message: format!("WebSocket idle timeout after {idle_timeout_ms}ms"),
                            detail: None,
                            retry_after: None,
                            usage_limit: None,
                            origin: CodexErrorOrigin::WebSocket,
                        }
                    } else {
                        response_start_timeout_error(idle_timeout_ms)
                    }));
                    break;
                }
                WebSocketRead::KeepaliveError(error) => {
                    terminal_item = Some(Err(CodexError {
                        status: 0,
                        message: format!("WebSocket keepalive error: {error}"),
                        detail: None,
                        retry_after: None,
                        usage_limit: None,
                        origin: CodexErrorOrigin::WebSocket,
                    }));
                    break;
                }
            },
        };

        match frame {
            Some(Ok(Message::Text(text))) => {
                let budget = frame_budget(&text).and_then(|()| {
                    if let Some(body) = sse_body.as_mut() {
                        append_sse(body, &text)?;
                    }
                    Ok(())
                });
                if let Err(error) = budget {
                    terminal_item = Some(Err(error));
                    break;
                }
                let parsed: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(value) => value,
                    Err(_) => {
                        if let Some(tc) = traffic.as_deref() {
                            tc.write_json_event(
                                "040-upstream-event",
                                &serde_json::json!({
                                    "unparseable": true,
                                    "data": text,
                                }),
                            );
                        }
                        continue;
                    }
                };

                if let Some(tc) = traffic.as_deref() {
                    tc.write_json_event("040-upstream-event", &parsed);
                }

                if is_response_event(&parsed) {
                    response_started = true;
                    last_response_event_at = Instant::now();
                }
                if parsed.get("type").and_then(|value| value.as_str()) == Some("error") {
                    status = event_error_status(&parsed).unwrap_or(500);
                }

                if event_is_terminal(&parsed) {
                    if let Some(error) = reconnect_event_error(&parsed) {
                        terminal_item = Some(Err(error));
                    } else {
                        reusable = parsed.get("type").and_then(|value| value.as_str())
                            == Some("response.completed");
                        terminal_item = Some(Ok(parsed));
                    }
                    break;
                }

                if tx.send(Ok(parsed)).await.is_err() {
                    break;
                }
            }
            Some(Ok(Message::Binary(_))) => {
                terminal_item = Some(Err(CodexError {
                    status: 0,
                    message: "WebSocket binary frames not supported".to_string(),
                    detail: None,
                    retry_after: None,
                    usage_limit: None,
                    origin: CodexErrorOrigin::WebSocket,
                }));
                break;
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
            Some(Ok(Message::Close(_))) | None => {
                terminal_item = Some(Err(missing_terminal_error()));
                break;
            }
            Some(Err(error)) => {
                terminal_item = Some(Err(CodexError {
                    status: 0,
                    message: format!("WebSocket stream error: {error}"),
                    detail: None,
                    retry_after: None,
                    usage_limit: None,
                    origin: CodexErrorOrigin::WebSocket,
                }));
                break;
            }
        }
    }

    if let Some(tc) = traffic.as_deref() {
        write_websocket_response_capture(
            tc,
            status,
            started_at.elapsed(),
            sse_body.as_deref().unwrap_or_default(),
        );
    }
    (reusable, terminal_item)
}
