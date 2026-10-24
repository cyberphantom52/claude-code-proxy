//! Authenticated, bounded model discovery. Catalogs and credentials are pinned
//! together for an operation; neither is selected from a global active account.
use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;

use super::auth::constants::CODEX_PROTOCOL_VERSION;
use super::auth::token_store::StoredAuth;
use super::client::{CodexHttpClient, build_native_codex_headers};
use super::translate::model_catalog::{self, CatalogSnapshot, MAX_CATALOG_BYTES};
use crate::provider::RequestContext;

const CACHE_CAPACITY: usize = 8;
const CACHE_TTL: Duration = Duration::from_secs(300);
const FAILURE_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_STALE: Duration = Duration::from_secs(900);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ETAG_BYTES: usize = 1024;

/// Immutable operation credentials and the metadata used to shape its requests.
/// Debug intentionally exposes neither credentials nor account identifiers.
pub struct ModelContext {
    endpoint: String,
    auth: Option<Arc<OperationAuth>>,
    catalog: Arc<CatalogSnapshot>,
}

impl std::fmt::Debug for ModelContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelContext")
            .field("authenticated", &self.auth.is_some())
            .field("authoritative", &self.catalog.authoritative)
            .field("model_count", &self.catalog.models.len())
            .finish_non_exhaustive()
    }
}

impl ModelContext {
    pub fn catalog(&self) -> Arc<CatalogSnapshot> {
        self.catalog.clone()
    }

    pub fn provenance(&self) -> Option<OperationProvenance> {
        self.auth.as_ref()?.provenance(&self.endpoint)
    }
}

/// Exact transport endpoint and selected credential generation; never credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct OperationProvenance {
    endpoint: String,
    credential: [u8; 32],
}

impl std::fmt::Debug for OperationProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OperationProvenance([redacted])")
    }
}

impl OperationProvenance {
    pub(super) fn retained_bytes(&self) -> usize {
        self.endpoint.len() + std::mem::size_of::<Self>()
    }

    pub(super) fn selected(
        endpoint: &str,
        account: Option<&str>,
        access: &str,
        revision: &str,
    ) -> Option<Self> {
        if endpoint.is_empty()
            || access.is_empty()
            || revision.is_empty()
            || account.is_none_or(|account| account.trim().is_empty())
        {
            return None;
        }
        let mut digest = Sha256::new();
        // Length-prefix each field: account/token boundaries cannot collide.
        digest.update([u8::from(account.is_some())]);
        let account = account.unwrap_or_default().as_bytes();
        digest.update((account.len() as u64).to_be_bytes());
        digest.update(account);
        digest.update((access.len() as u64).to_be_bytes());
        digest.update(access.as_bytes());
        digest.update((revision.len() as u64).to_be_bytes());
        digest.update(revision.as_bytes());
        Some(Self {
            endpoint: endpoint.to_owned(),
            credential: digest.finalize().into(),
        })
    }
}

pub fn operation_provenance(
    endpoint: &str,
    auth: &StoredAuth,
    revision: &str,
) -> Option<OperationProvenance> {
    OperationProvenance::selected(endpoint, auth.account_id.as_deref(), &auth.access, revision)
}

#[derive(Clone)]
pub(super) struct OperationAuth {
    auth: StoredAuth,
    revision: String,
    opaque_endpoint: Option<String>,
}

impl std::ops::Deref for OperationAuth {
    type Target = StoredAuth;
    fn deref(&self) -> &Self::Target {
        &self.auth
    }
}

impl OperationAuth {
    fn from_snapshot(snapshot: crate::auth::AuthSnapshot<StoredAuth>) -> anyhow::Result<Self> {
        Ok(Self {
            auth: snapshot
                .value
                .ok_or_else(|| anyhow::anyhow!("Not authenticated"))?,
            revision: snapshot.revision,
            opaque_endpoint: None,
        })
    }

    pub(super) fn require_opaque_provenance(&mut self, endpoint: &str) -> anyhow::Result<()> {
        let provenance = self.provenance(endpoint);
        anyhow::ensure!(
            provenance.is_some() && provenance == current_operation_provenance(),
            "operation_provenance_changed"
        );
        self.opaque_endpoint = Some(endpoint.to_owned());
        Ok(())
    }

    pub(super) fn provenance(&self, endpoint: &str) -> Option<OperationProvenance> {
        operation_provenance(endpoint, &self.auth, &self.revision)
    }
}

pub fn current_operation_provenance() -> Option<OperationProvenance> {
    current_model_context()?.provenance()
}

tokio::task_local! {
    static MODEL_CONTEXT: Arc<ModelContext>;
}

pub async fn with_model_context<F: Future>(context: Arc<ModelContext>, future: F) -> F::Output {
    model_catalog::scope_catalog(context.catalog(), MODEL_CONTEXT.scope(context, future)).await
}

pub fn current_model_context() -> Option<Arc<ModelContext>> {
    MODEL_CONTEXT.try_with(Arc::clone).ok()
}

// Keys contain only digests, never tokens. JWT claims are used solely as an
// isolation discriminator, not to authenticate or trust an unsigned token.
#[derive(PartialEq, Eq)]
struct CacheKey {
    identity: [u8; 32],
    endpoint: String,
}

fn subject(auth: &StoredAuth) -> Option<String> {
    let payload = auth.access.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("sub")?.as_str().map(str::to_owned)
}

fn account_id(auth: &StoredAuth) -> Option<String> {
    auth.account_id.clone().or_else(|| {
        // Older proxy auth files may omit the derived account-id field.
        // Reuse the OAuth claim reader rather than sharing same-sub catalogs
        // across different token accounts in that case.
        super::auth::jwt::extract_account_id(&super::auth::jwt::TokenResponse {
            id_token: None,
            access_token: auth.access.clone(),
            refresh_token: String::new(),
            expires_in: None,
        })
    })
}

fn identity(auth: &StoredAuth) -> [u8; 32] {
    let mut digest = Sha256::new();
    let account = account_id(auth);
    digest.update(account.as_deref().unwrap_or_default().as_bytes());
    digest.update([0]);
    // Opaque tokens or unknown accounts cannot establish a principal across
    // rotations. Partition those entries rather than sharing user metadata.
    let subject = account.and_then(|_| subject(auth));
    digest.update(subject.as_deref().unwrap_or(&auth.access).as_bytes());
    digest.finalize().into()
}

fn same_account(before: &StoredAuth, after: &StoredAuth) -> bool {
    let before_account = account_id(before);
    if before_account != account_id(after) {
        return false;
    }
    if before_account.is_none() {
        return before.access == after.access;
    }
    match (subject(before), subject(after)) {
        (Some(before), Some(after)) => before == after,
        (None, None) => true,
        _ => before.access == after.access,
    }
}

#[derive(Default)]
struct CacheEntry {
    snapshot: Option<Arc<CatalogSnapshot>>,
    validated_at: Option<Instant>,
    retry_at: Option<Instant>,
    etag: Option<http::HeaderValue>,
}

impl CacheEntry {
    fn fresh(&self, now: Instant) -> bool {
        self.validated_at
            .is_some_and(|at| now.saturating_duration_since(at) < CACHE_TTL)
    }

    fn usable(&self, now: Instant) -> Option<Arc<CatalogSnapshot>> {
        self.validated_at
            .filter(|at| now.saturating_duration_since(*at) < MAX_STALE)
            .and(self.snapshot.clone())
    }
}

#[derive(Default)]
pub(super) struct ModelCache {
    entries: Mutex<VecDeque<(CacheKey, Arc<AsyncMutex<CacheEntry>>)>>,
}

impl ModelCache {
    fn entry(&self, key: CacheKey) -> Option<Arc<AsyncMutex<CacheEntry>>> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(index) = entries.iter().position(|(candidate, _)| candidate == &key) {
            let entry = entries.remove(index).unwrap();
            let value = entry.1.clone();
            entries.push_back(entry);
            return Some(value);
        }
        if entries.len() == CACHE_CAPACITY {
            // Never evict an in-flight entry: doing so breaks same-key single-flight.
            let index = entries
                .iter()
                .position(|(_, entry)| Arc::strong_count(entry) == 1)?;
            entries.remove(index);
        }
        let value = Arc::new(AsyncMutex::new(CacheEntry::default()));
        entries.push_back((key, value.clone()));
        Some(value)
    }
}

fn models_endpoint(base_url: &str) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(base_url).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let path = url.path().trim_end_matches('/');
    let root = path.strip_suffix("/responses").unwrap_or(path);
    url.set_path(&format!("{root}/models"));
    url.set_query(None);
    url.set_fragment(None);
    url.query_pairs_mut()
        .append_pair("client_version", CODEX_PROTOCOL_VERSION);
    Some(url)
}

enum Discovery {
    Modified(Arc<CatalogSnapshot>, Option<http::HeaderValue>),
    NotModified,
    Unauthorized,
}

impl CodexHttpClient {
    pub async fn prepare_model_context(&self) -> Arc<ModelContext> {
        if let Some(context) =
            current_model_context().filter(|context| context.endpoint == self.base_url)
        {
            return context;
        }
        // OAuth refresh rotates credentials. Never cancel it with the shorter
        // metadata deadline before the auth manager can persist the response.
        let mut auth = self
            .auth_manager
            .get_auth_snapshot()
            .await
            .ok()
            .and_then(|snapshot| OperationAuth::from_snapshot(snapshot).ok())
            .map(Arc::new);
        let deadline = Instant::now() + DISCOVERY_TIMEOUT;
        let catalog = if crate::config::codex_model_discovery() {
            if let Some(auth) = auth.as_mut() {
                self.discover_catalog(auth, deadline).await
            } else {
                None
            }
        } else {
            None
        };
        Arc::new(ModelContext {
            endpoint: self.base_url.clone(),
            auth,
            catalog: catalog.unwrap_or_else(model_catalog::fallback_snapshot),
        })
    }

    async fn discover_catalog(
        &self,
        auth: &mut Arc<OperationAuth>,
        deadline: Instant,
    ) -> Option<Arc<CatalogSnapshot>> {
        let endpoint = models_endpoint(&self.base_url)?;
        let key = CacheKey {
            identity: identity(auth),
            // The endpoint already includes the client_version query parameter.
            endpoint: endpoint.to_string(),
        };
        let entry = self.model_cache.entry(key)?;
        let mut entry = tokio::time::timeout_at(deadline, entry.lock()).await.ok()?;
        let now = Instant::now();
        if entry.fresh(now) || entry.retry_at.is_some_and(|at| at > now) {
            return entry.usable(now);
        }
        // Set before I/O so cancellation and timeouts also observe the cooldown.
        entry.retry_at = Some(now + FAILURE_COOLDOWN);
        let result = self
            .fetch_catalog(auth, endpoint, entry.etag.as_ref(), deadline)
            .await;
        let now = Instant::now();
        match result {
            Some(Discovery::Modified(snapshot, etag)) => {
                entry.snapshot = Some(snapshot);
                entry.etag = etag;
                entry.validated_at = Some(now);
                entry.retry_at = None;
            }
            Some(Discovery::NotModified) if entry.snapshot.is_some() => {
                entry.validated_at = Some(now);
                entry.retry_at = None;
            }
            _ => entry.retry_at = Some(now + FAILURE_COOLDOWN),
        }
        entry.usable(now)
    }

    async fn fetch_catalog(
        &self,
        auth: &mut Arc<OperationAuth>,
        endpoint: reqwest::Url,
        etag: Option<&http::HeaderValue>,
        mut deadline: Instant,
    ) -> Option<Discovery> {
        let mut refreshed = false;
        loop {
            let result = tokio::time::timeout_at(
                deadline,
                self.fetch_catalog_attempt(auth, &endpoint, etag),
            )
            .await
            .ok()??;
            match result {
                Discovery::Unauthorized if !refreshed => {
                    refreshed = true;
                    // Credential rotation has the auth manager's own timeout;
                    // the models GET deadline must not interrupt persistence.
                    *auth = self.refresh_operation_auth(auth).await.ok()?;
                    deadline = Instant::now() + DISCOVERY_TIMEOUT;
                }
                Discovery::Unauthorized => return None,
                result => return Some(result),
            }
        }
    }

    async fn fetch_catalog_attempt(
        &self,
        auth: &StoredAuth,
        endpoint: &reqwest::Url,
        etag: Option<&http::HeaderValue>,
    ) -> Option<Discovery> {
        let ctx = RequestContext {
            notification_id: None,
            req_id: String::new(),
            session_id: None,
            session_seq: None,
            provider: "codex".to_string(),
            traffic: None,
            monitor: None,
        };
        let headers = build_native_codex_headers(auth, &ctx, false, false).ok()?;
        let mut request = self.native_client.get(endpoint.clone()).headers(headers);
        if let Some(etag) = etag {
            request = request.header(http::header::IF_NONE_MATCH, etag);
        }
        let mut response = request.send().await.ok()?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Some(Discovery::Unauthorized);
        }
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return etag.map(|_| Discovery::NotModified);
        }
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|len| len > MAX_CATALOG_BYTES as u64)
        {
            return None;
        }
        let etag = response
            .headers()
            .get(http::header::ETAG)
            .filter(|etag| etag.as_bytes().len() <= MAX_ETAG_BYTES)
            .cloned();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if chunk.len() > MAX_CATALOG_BYTES.saturating_sub(body.len()) {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        // The parser also bounds the raw models array before skipping
        // malformed entries, so invalid entries cannot bypass the cap.
        let models = model_catalog::parse_catalog(&body)?;
        Some(Discovery::Modified(
            Arc::new(CatalogSnapshot::new(models, true)),
            etag,
        ))
    }

    pub(super) async fn operation_auth(&self) -> anyhow::Result<Arc<OperationAuth>> {
        if let Some(context) = current_model_context() {
            anyhow::ensure!(
                context.endpoint == self.base_url,
                "Codex endpoint changed during operation; prepare a matching model context"
            );
            // Missing auth in a pinned fallback operation must not switch to a
            // newly logged-in account halfway through shaping the request.
            let mut auth = context
                .auth
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Not authenticated"))?;
            if super::threads::replaying() {
                Arc::make_mut(&mut auth).require_opaque_provenance(&self.base_url)?;
            }
            return Ok(auth);
        }
        OperationAuth::from_snapshot(self.auth_manager.get_auth_snapshot().await?).map(Arc::new)
    }

    pub(super) async fn refresh_operation_auth(
        &self,
        rejected: &OperationAuth,
    ) -> anyhow::Result<Arc<OperationAuth>> {
        let mut refreshed = OperationAuth::from_snapshot(
            self.auth_manager
                .force_refresh_revision(&rejected.revision)
                .await?,
        )?;
        anyhow::ensure!(
            same_account(rejected, &refreshed),
            "Codex account changed during operation; retry the operation"
        );
        if let Some(endpoint) = &rejected.opaque_endpoint {
            anyhow::ensure!(
                rejected.provenance(endpoint).is_some()
                    && rejected.provenance(endpoint) == refreshed.provenance(endpoint),
                "operation_provenance_changed"
            );
            refreshed.opaque_endpoint = Some(endpoint.clone());
        }
        Ok(Arc::new(refreshed))
    }
}
