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
    auth: Option<OperationAuth>,
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
    #[cfg(test)]
    pub(crate) fn for_test(endpoint: &str, auth: StoredAuth) -> Arc<Self> {
        Arc::new(Self {
            endpoint: endpoint.to_owned(),
            auth: Some(auth.into()),
            catalog: Arc::new(model_catalog::fallback_snapshot()),
        })
    }

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

#[cfg(test)]
impl From<StoredAuth> for OperationAuth {
    fn from(auth: StoredAuth) -> Self {
        Self {
            auth,
            revision: "synthetic-test-revision".into(),
            opaque_endpoint: None,
        }
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

pub fn current_auth_for_endpoint(endpoint: &str) -> Option<StoredAuth> {
    current_model_context()
        .filter(|context| context.endpoint == endpoint)
        .and_then(|context| context.auth.as_ref().map(|auth| auth.auth.clone()))
}

// Keys contain only digests, never tokens. JWT claims are used solely as an
// isolation discriminator, not to authenticate or trust an unsigned token.
#[derive(Clone, PartialEq, Eq)]
struct CacheKey {
    identity: [u8; 32],
    endpoint: String,
    version: String,
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

fn models_endpoint(base_url: &str, version: &str) -> Option<reqwest::Url> {
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
    url.query_pairs_mut().append_pair("client_version", version);
    Some(url)
}

enum Discovery {
    Modified(Arc<CatalogSnapshot>, Option<http::HeaderValue>),
    NotModified,
    Unauthorized,
}

impl CodexHttpClient {
    #[cfg(test)]
    pub(crate) async fn test_model_context(&self) -> Arc<ModelContext> {
        Arc::new(ModelContext {
            endpoint: self.base_url.clone(),
            auth: Some(self.operation_auth().await.unwrap()),
            catalog: Arc::new(model_catalog::fallback_snapshot()),
        })
    }

    pub async fn prepare_model_context(&self) -> Arc<ModelContext> {
        self.prepare_model_context_version(CODEX_PROTOCOL_VERSION)
            .await
    }

    async fn prepare_model_context_version(&self, version: &str) -> Arc<ModelContext> {
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
            .and_then(|snapshot| OperationAuth::from_snapshot(snapshot).ok());
        let deadline = Instant::now() + DISCOVERY_TIMEOUT;
        let catalog = if crate::config::codex_model_discovery() {
            if let Some(auth) = auth.as_mut() {
                self.discover_catalog(auth, version, deadline).await
            } else {
                None
            }
        } else {
            None
        };
        Arc::new(ModelContext {
            endpoint: self.base_url.clone(),
            auth,
            catalog: catalog.unwrap_or_else(|| Arc::new(model_catalog::fallback_snapshot())),
        })
    }

    async fn discover_catalog(
        &self,
        auth: &mut OperationAuth,
        version: &str,
        deadline: Instant,
    ) -> Option<Arc<CatalogSnapshot>> {
        let endpoint = models_endpoint(&self.base_url, version)?;
        let key = CacheKey {
            identity: identity(auth),
            endpoint: endpoint.to_string(),
            version: version.to_owned(),
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
        auth: &mut OperationAuth,
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

    pub(super) async fn operation_auth(&self) -> anyhow::Result<OperationAuth> {
        if let Some(context) = current_model_context() {
            anyhow::ensure!(
                context.endpoint == self.base_url,
                "Codex endpoint changed during operation; prepare a matching model context"
            );
            // Missing auth in a pinned fallback operation must not switch to a
            // newly logged-in account halfway through shaping the request.
            return context
                .auth
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Not authenticated"));
        }
        OperationAuth::from_snapshot(self.auth_manager.get_auth_snapshot().await?)
    }

    pub(super) async fn refresh_operation_auth(
        &self,
        rejected: &OperationAuth,
    ) -> anyhow::Result<OperationAuth> {
        let snapshot = crate::auth::AuthSnapshot {
            value: Some(rejected.auth.clone()),
            revision: rejected.revision.clone(),
        };
        let mut refreshed = OperationAuth::from_snapshot(
            self.auth_manager.force_refresh_snapshot(&snapshot).await?,
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
        Ok(refreshed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, Bytes};
    use axum::http::{Request, Response, StatusCode};
    use axum::routing::any;
    use model_catalog::MAX_CATALOG_MODELS;
    use tokio::sync::Notify;

    type Seen = Arc<Mutex<Vec<(String, http::HeaderMap)>>>;

    struct Mock {
        url: String,
        seen: Seen,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Mock {
        async fn new(responses: Vec<Response<Body>>) -> Self {
            Self::with_action(responses, None).await
        }

        async fn with_action(
            responses: Vec<Response<Body>>,
            action: Option<Box<dyn FnOnce() + Send>>,
        ) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!(
                "http://{}/backend-api/codex/responses",
                listener.local_addr().unwrap()
            );
            let seen: Seen = Default::default();
            let captured = seen.clone();
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let action = Arc::new(Mutex::new(action));
            let app = axum::Router::new().fallback(any(move |request: Request<Body>| {
                captured
                    .lock()
                    .unwrap()
                    .push((request.uri().to_string(), request.headers().clone()));
                if let Some(action) = action.lock().unwrap().take() {
                    action();
                }
                let response = responses.lock().unwrap().pop_front().unwrap_or_else(|| {
                    Response::builder().status(500).body(Body::empty()).unwrap()
                });
                async move { response }
            }));
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self { url, seen, task }
        }

        fn count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    fn response(status: u16, body: impl Into<Body>, etag: Option<&str>) -> Response<Body> {
        let mut response = Response::builder().status(status);
        if let Some(etag) = etag {
            response = response.header(http::header::ETAG, etag);
        }
        response.body(body.into()).unwrap()
    }

    fn catalog(slug: &str) -> String {
        serde_json::json!({"models": [{"slug": slug, "use_responses_lite": true}]}).to_string()
    }

    fn auth(access: &str, account: &str) -> StoredAuth {
        StoredAuth {
            access: access.to_owned(),
            refresh: "synthetic-refresh-never-send".to_owned(),
            expires: u64::MAX,
            account_id: Some(account.to_owned()),
        }
    }

    fn client(url: &str) -> CodexHttpClient {
        // new_for_test uses direct HTTP clients; test_auth bypasses HOME,
        // keychain, and durable credentials entirely, including on refresh.
        let client =
            CodexHttpClient::new_for_test(reqwest::Client::new(), url.to_owned(), 1000, 1000);
        client
            .auth_manager()
            .set_test_auth(auth("synthetic-access-a", "account-a"));
        client
    }

    async fn age(client: &CodexHttpClient, age: Duration) {
        let entries: Vec<_> = client
            .model_cache
            .entries
            .lock()
            .unwrap()
            .iter()
            .map(|(_, entry)| entry.clone())
            .collect();
        for entry in entries {
            let mut entry = entry.lock().await;
            entry.validated_at = entry.snapshot.as_ref().map(|_| Instant::now() - age);
            entry.retry_at = None;
        }
    }

    #[tokio::test]
    async fn authenticated_single_flight_cache_and_scoped_auth_are_pinned() {
        let server = Mock::new(vec![response(200, catalog("discovered-a"), Some("\"a\""))]).await;
        let client = client(&server.url);
        let (first, second) = tokio::join!(
            client.prepare_model_context(),
            client.prepare_model_context()
        );
        assert!(first.catalog.authoritative);
        assert!(Arc::ptr_eq(&first.catalog, &second.catalog));
        assert_eq!(server.count(), 1);
        {
            let seen = server.seen.lock().unwrap();
            assert_eq!(
                seen[0].0,
                format!("/backend-api/codex/models?client_version={CODEX_PROTOCOL_VERSION}")
            );
            assert_eq!(seen[0].1[http::header::ACCEPT], "application/json");
            assert_eq!(
                seen[0].1[http::header::AUTHORIZATION],
                "Bearer synthetic-access-a"
            );
            assert_eq!(seen[0].1["chatgpt-account-id"], "account-a");
        }
        assert!(!format!("{first:?}").contains("synthetic"));
        assert!(!format!("{first:?}").contains("account-a"));

        client
            .auth_manager()
            .set_test_auth(auth("synthetic-access-b", "account-b"));
        with_model_context(first.clone(), async {
            assert_eq!(
                client.operation_auth().await.unwrap().access,
                "synthetic-access-a"
            );
            assert_eq!(
                current_auth_for_endpoint(&server.url)
                    .unwrap()
                    .account_id
                    .as_deref(),
                Some("account-a")
            );
            assert!(current_auth_for_endpoint("http://elsewhere/responses").is_none());
            assert!(Arc::ptr_eq(&first, &client.prepare_model_context().await));
            assert_eq!(
                model_catalog::catalog_model("discovered-a").unwrap().slug,
                "discovered-a"
            );
        })
        .await;
        assert!(current_model_context().is_none());
        assert_eq!(
            client.operation_auth().await.unwrap().access,
            "synthetic-access-b"
        );
    }

    #[tokio::test]
    async fn concurrent_scopes_keep_both_auth_and_catalog_isolated() {
        let server = Mock::new(vec![]).await;
        let client = client(&server.url);
        let context = |slug: &str| {
            Arc::new(ModelContext {
                endpoint: server.url.clone(),
                auth: Some(auth(slug, slug).into()),
                catalog: Arc::new(CatalogSnapshot::new(
                    model_catalog::parse_catalog(catalog(slug).as_bytes()).unwrap(),
                    true,
                )),
            })
        };
        let check = |slug: &'static str| {
            let client = &client;
            async move {
                tokio::task::yield_now().await;
                assert_eq!(client.operation_auth().await.unwrap().access, slug);
                assert_eq!(model_catalog::catalog_models()[0].slug, slug);
            }
        };
        tokio::join!(
            with_model_context(context("account-a"), check("account-a")),
            with_model_context(context("account-b"), check("account-b")),
        );
        assert_eq!(server.count(), 0);
    }

    #[tokio::test]
    async fn unchanged_etag_does_not_suppress_a_200_update_and_inference_rejects_account_switch() {
        let server = Mock::new(vec![
            response(200, catalog("before"), Some("\"same\"")),
            response(200, catalog("after"), Some("\"same\"")),
            response(401, Body::empty(), None),
        ])
        .await;
        let client = client(&server.url);
        let before = client.prepare_model_context().await;
        age(&client, CACHE_TTL + Duration::from_secs(1)).await;
        let after = client.prepare_model_context().await;
        assert_eq!(after.catalog.models[0].slug, "after");
        assert_eq!(
            before.catalog.models[0].slug, "before",
            "existing operations stay immutable"
        );
        client
            .auth_manager()
            .set_test_auth(auth("synthetic-access-b", "account-b"));
        let ctx = RequestContext {
            notification_id: None,
            req_id: String::new(),
            session_id: None,
            session_seq: None,
            provider: "codex".to_owned(),
            traffic: None,
            monitor: None,
        };
        let result = with_model_context(
            after,
            client.post_native_responses(
                &serde_json::json!({"model":"after","input":[]}),
                &ctx,
                false,
                false,
            ),
        )
        .await;
        assert_eq!(result.unwrap_err().status, 401);
        assert_eq!(server.count(), 3, "account switch must not retry inference");
        assert_eq!(
            server.seen.lock().unwrap()[2].1[http::header::AUTHORIZATION],
            "Bearer synthetic-access-a"
        );
    }

    #[tokio::test]
    async fn ttl_etag_and_bounded_same_key_stale_failure_cooldown() {
        let server = Mock::new(vec![
            response(200, catalog("etag-model"), Some("\"version1\"")),
            response(304, Body::empty(), None),
            response(500, Body::empty(), None),
            response(500, Body::empty(), None),
        ])
        .await;
        let client = client(&server.url);
        let first = client.prepare_model_context().await;
        age(&client, CACHE_TTL + Duration::from_secs(1)).await;
        let revalidated = client.prepare_model_context().await;
        assert!(Arc::ptr_eq(&first.catalog, &revalidated.catalog));
        assert_eq!(
            server.seen.lock().unwrap()[1].1[http::header::IF_NONE_MATCH],
            "\"version1\""
        );
        assert_eq!(server.count(), 2);
        // 304 resets freshness, so the next call does not hit the network.
        client.prepare_model_context().await;
        assert_eq!(server.count(), 2);

        age(&client, CACHE_TTL + Duration::from_secs(1)).await;
        let stale = client.prepare_model_context().await;
        assert!(Arc::ptr_eq(&first.catalog, &stale.catalog));
        let cooldown = client.prepare_model_context().await;
        assert!(Arc::ptr_eq(&stale.catalog, &cooldown.catalog));
        assert_eq!(server.count(), 3);

        age(&client, MAX_STALE + Duration::from_secs(1)).await;
        let expired = client.prepare_model_context().await;
        assert!(!expired.catalog.authoritative);
        assert!(
            !expired
                .catalog
                .models
                .iter()
                .any(|model| model.slug == "etag-model")
        );
        assert_eq!(server.count(), 4);
    }

    #[tokio::test]
    async fn failure_cooldown_and_identity_endpoint_version_isolation() {
        let server = Mock::new(vec![
            response(500, Body::empty(), None),
            response(200, catalog("account-b-only"), None),
            response(200, catalog("version-two-only"), None),
            response(200, catalog("other-endpoint-only"), None),
            response(200, catalog("retry-account-a"), None),
        ])
        .await;
        let mut client = client(&server.url);
        let failed = client.prepare_model_context().await;
        assert!(!failed.catalog.authoritative);
        assert!(!client.prepare_model_context().await.catalog.authoritative);
        assert_eq!(server.count(), 1);
        client
            .auth_manager()
            .set_test_auth(auth("synthetic-access-b", "account-b"));
        assert_eq!(
            client.prepare_model_context().await.catalog.models[0].slug,
            "account-b-only"
        );
        assert_eq!(
            client
                .prepare_model_context_version("other-version")
                .await
                .catalog
                .models[0]
                .slug,
            "version-two-only"
        );
        client.base_url = server.url.replace("/backend-api/codex/", "/other/");
        assert_eq!(
            client.prepare_model_context().await.catalog.models[0].slug,
            "other-endpoint-only"
        );
        client.base_url = server.url.clone();
        client
            .auth_manager()
            .set_test_auth(auth("synthetic-access-a", "account-a"));
        // Account A must not inherit the successful B/version/endpoint entries.
        assert!(!client.prepare_model_context().await.catalog.authoritative);
        assert_eq!(server.count(), 4);
        age(&client, Duration::ZERO).await; // explicit cooldown-expiry hook
        assert_eq!(
            client.prepare_model_context().await.catalog.models[0].slug,
            "retry-account-a"
        );
        assert_eq!(server.count(), 5);
    }

    async fn refresh_client(
        responses: Vec<Response<Body>>,
        next_auth: StoredAuth,
    ) -> (Mock, Arc<CodexHttpClient>) {
        let target: Arc<Mutex<Option<Arc<CodexHttpClient>>>> = Default::default();
        let callback_target = target.clone();
        let server = Mock::with_action(
            responses,
            Some(Box::new(move || {
                callback_target
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .auth_manager()
                    .set_test_auth(next_auth);
            })),
        )
        .await;
        let client = Arc::new(client(&server.url));
        *target.lock().unwrap() = Some(client.clone());
        (server, client)
    }

    #[tokio::test]
    async fn one_401_refresh_preserves_same_account_but_rejects_account_switch() {
        let (server, client) = refresh_client(
            vec![
                response(401, Body::empty(), None),
                response(200, catalog("refreshed-model"), None),
            ],
            auth("synthetic-rotated", "account-a"),
        )
        .await;
        let context = client.prepare_model_context().await;
        assert!(context.catalog.authoritative);
        assert_eq!(context.auth.as_ref().unwrap().access, "synthetic-rotated");
        assert_eq!(server.count(), 2);
        assert_eq!(
            server.seen.lock().unwrap()[1].1[http::header::AUTHORIZATION],
            "Bearer synthetic-rotated"
        );

        let (server, client) = refresh_client(
            vec![
                response(401, Body::empty(), None),
                response(401, Body::empty(), None),
            ],
            auth("synthetic-rotated", "account-a"),
        )
        .await;
        assert!(!client.prepare_model_context().await.catalog.authoritative);
        assert_eq!(
            server.count(),
            2,
            "a second 401 must not refresh/retry again"
        );

        let (server, client) = refresh_client(
            vec![response(401, Body::empty(), None)],
            auth("synthetic-other-account", "account-b"),
        )
        .await;
        let context = client.prepare_model_context().await;
        assert!(!context.catalog.authoritative);
        assert_eq!(
            context.auth.as_ref().unwrap().account_id.as_deref(),
            Some("account-a")
        );
        assert_eq!(
            server.count(),
            1,
            "never send account-A metadata with account-B auth"
        );
    }

    #[tokio::test]
    async fn body_model_and_redirect_bounds_fall_back_without_secrets() {
        let target = Mock::new(vec![response(200, catalog("must-not-follow"), None)]).await;
        let models = serde_json::json!({"models": vec![serde_json::json!({"slug":"model"}); MAX_CATALOG_MODELS + 1]}).to_string();
        // Chunked, unknown-length body exercises the incremental cap, not only
        // the Content-Length shortcut.
        let chunks = futures_util::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from(vec![b' '; MAX_CATALOG_BYTES])),
            Ok(Bytes::from_static(b"x")),
        ]);
        let server = Mock::new(vec![
            response(200, "{}", None),
            response(200, models, None),
            response(200, Body::from_stream(chunks), None),
            Response::builder()
                .status(StatusCode::FOUND)
                .header(http::header::LOCATION, &target.url)
                .body(Body::empty())
                .unwrap(),
            response(304, Body::empty(), None),
            Response::builder()
                .status(200)
                .header(http::header::CONTENT_LENGTH, MAX_CATALOG_BYTES + 1)
                .body(Body::empty())
                .unwrap(),
        ])
        .await;
        let client = client(&server.url);
        for index in 0..6 {
            let context = client
                .prepare_model_context_version(&format!("bounds-{index}"))
                .await;
            assert!(!context.catalog.authoritative);
        }
        assert_eq!(server.count(), 6);
        assert_eq!(
            target.count(),
            0,
            "native client must not forward auth on redirects"
        );
    }

    #[tokio::test]
    async fn oauth_rotation_outlives_metadata_deadline_and_is_persisted_before_discovery() {
        use super::super::auth::manager::CodexAuthManager;
        use super::super::auth::token_store::CodexTokenStore;
        use crate::auth::{KeychainFileAuthStore, SystemKeychain};

        for initially_expired in [true, false] {
            let refreshing = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let started = refreshing.clone();
            let released = release.clone();
            let oauth_body = Body::from_stream(futures_util::stream::once(async move {
                started.notify_one();
                released.notified().await;
                Ok::<_, std::io::Error>(Bytes::from_static(
                    br#"{"access_token":"synthetic-rotated","refresh_token":"synthetic-next-refresh","expires_in":3600}"#,
                ))
            }));
            let oauth = Mock::new(vec![response(200, oauth_body, None)]).await;
            let mut responses = Vec::new();
            if !initially_expired {
                responses.push(response(401, Body::empty(), None));
            }
            responses.push(response(200, catalog("after-rotation"), None));
            let models = Mock::new(responses).await;
            let dir = tempfile::tempdir().unwrap();
            let store = CodexTokenStore::new(KeychainFileAuthStore::new(
                dir.path().join("auth.json").to_string_lossy().into_owned(),
                dir.path()
                    .join("legacy.json")
                    .to_string_lossy()
                    .into_owned(),
                "synthetic-test",
                "auth",
                false,
                SystemKeychain,
            ));
            let mut initial = auth("synthetic-access-a", "account-a");
            if initially_expired {
                initial.expires = 0;
            }
            store.save_auth(initial).unwrap();
            let manager = CodexAuthManager::new_with_token_endpoint(store, oauth.url.clone());
            let mut client = CodexHttpClient::new_with_client(
                reqwest::Client::builder().no_proxy().build().unwrap(),
                manager,
                models.url.clone(),
            );
            client.native_client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let client = Arc::new(client);
            let caller = client.clone();
            let request = tokio::spawn(async move { caller.prepare_model_context().await });
            refreshing.notified().await;
            tokio::time::pause();
            tokio::time::advance(DISCOVERY_TIMEOUT + Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            assert!(
                !request.is_finished(),
                "metadata timeout must not cancel rotating OAuth credentials"
            );
            tokio::time::resume();
            release.notify_one();
            let context = request.await.unwrap();
            assert!(context.catalog.authoritative);
            assert_eq!(context.catalog.models[0].slug, "after-rotation");
            let persisted = client.auth_manager().store.load_auth().unwrap().unwrap();
            assert_eq!(persisted.access, "synthetic-rotated");
            assert_eq!(persisted.refresh, "synthetic-next-refresh");
            assert_eq!(oauth.count(), 1);
            assert_eq!(models.count(), if initially_expired { 1 } else { 2 });
        }
    }

    #[tokio::test]
    async fn model_get_timeout_includes_a_stalled_body_and_cools_down() {
        let reading = Arc::new(Notify::new());
        let notify = reading.clone();
        let body = Body::from_stream(futures_util::stream::once(async move {
            notify.notify_one();
            std::future::pending::<Result<Bytes, std::io::Error>>().await
        }));
        let server = Mock::new(vec![response(200, body, None)]).await;
        let client = Arc::new(client(&server.url));
        let caller = client.clone();
        let request = tokio::spawn(async move { caller.prepare_model_context().await });
        reading.notified().await;
        tokio::time::pause();
        tokio::time::advance(DISCOVERY_TIMEOUT + Duration::from_secs(1)).await;
        assert!(!request.await.unwrap().catalog.authoritative);
        assert!(!client.prepare_model_context().await.catalog.authoritative);
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn missing_auth_scope_cannot_pick_up_another_account_and_nested_scopes_restore() {
        let server = Mock::new(vec![]).await;
        let client = client(&server.url);
        let mismatched_client = CodexHttpClient::new_for_test(
            reqwest::Client::new(),
            format!("{}/other", server.url),
            1000,
            1000,
        );
        mismatched_client
            .auth_manager()
            .set_test_auth(auth("other-endpoint-secret", "other-account"));
        let missing = Arc::new(ModelContext {
            endpoint: server.url.clone(),
            auth: None,
            catalog: Arc::new(model_catalog::fallback_snapshot()),
        });
        let other = Arc::new(ModelContext {
            endpoint: server.url.clone(),
            auth: Some(auth("other-secret", "other-account").into()),
            catalog: Arc::new(CatalogSnapshot::new(vec![], true)),
        });
        with_model_context(missing.clone(), async {
            assert!(client.operation_auth().await.is_err());
            assert!(Arc::ptr_eq(&missing, &client.prepare_model_context().await));
            with_model_context(other, async {
                assert_eq!(
                    client.operation_auth().await.unwrap().access,
                    "other-secret"
                );
                assert!(model_catalog::scoped_catalog().unwrap().authoritative);
                assert!(
                    mismatched_client.operation_auth().await.is_err(),
                    "mismatched endpoint must not combine scoped metadata and another auth source"
                );
            })
            .await;
            assert!(client.operation_auth().await.is_err());
            assert!(!model_catalog::scoped_catalog().unwrap().authoritative);
        })
        .await;
        assert_eq!(server.count(), 0);
        assert!(model_catalog::scoped_catalog().is_none());
    }

    #[test]
    fn operation_provenance_binds_exact_selected_auth_and_generation_without_secrets() {
        let original = auth("synthetic-secret", "selected-account");
        let provenance =
            operation_provenance("http://localhost/responses?mode=a", &original, "epoch-a")
                .unwrap();
        for changed in [
            operation_provenance("http://localhost/responses?mode=b", &original, "epoch-a"),
            operation_provenance(
                "http://localhost/responses?mode=a",
                &auth("rotated-secret", "selected-account"),
                "epoch-a",
            ),
            operation_provenance(
                "http://localhost/responses?mode=a",
                &auth("synthetic-secret", "other-account"),
                "epoch-a",
            ),
            operation_provenance("http://localhost/responses?mode=a", &original, "epoch-b"),
        ] {
            assert_ne!(Some(provenance.clone()), changed);
        }
        assert_eq!(
            operation_provenance("http://localhost/responses?mode=a", &original, "epoch-a"),
            Some(provenance.clone())
        );
        assert_eq!(format!("{provenance:?}"), "OperationProvenance([redacted])");
        assert!(operation_provenance("http://localhost/responses", &original, "").is_none());
        assert!(
            operation_provenance(
                "http://localhost/responses",
                &auth("", "selected-account"),
                "epoch"
            )
            .is_none()
        );
        let mut unknown = original;
        unknown.account_id = None;
        assert!(operation_provenance("http://localhost/responses", &unknown, "epoch").is_none());
    }

    #[test]
    fn jwt_subject_and_account_changes_are_distinct_but_rotations_are_allowed() {
        let jwt = |sub: &str| {
            format!(
                "header.{}.signature",
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(serde_json::json!({"sub":sub}).to_string())
            )
        };
        let a = auth(&jwt("user-a"), "account");
        let b = auth(&jwt("user-b"), "account");
        assert_ne!(identity(&a), identity(&b));
        assert!(!same_account(&a, &b));
        assert!(same_account(&a, &a));
        assert!(same_account(
            &auth("old", "account"),
            &auth("new", "account")
        ));
        assert!(!same_account(
            &auth("old", "account-a"),
            &auth("new", "account-b")
        ));
        let mut no_account = auth("opaque-a", "account");
        no_account.account_id = None;
        let mut other = no_account.clone();
        other.access = "opaque-b".into();
        assert!(!same_account(&no_account, &other));
        let legacy = |account: &str| StoredAuth {
            access: format!(
                "header.{}.signature",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                    serde_json::json!({"sub":"same-user", "chatgpt_account_id":account})
                        .to_string(),
                )
            ),
            account_id: None,
            ..auth("", "")
        };
        let legacy_a = legacy("account-a");
        let legacy_b = legacy("account-b");
        assert_ne!(identity(&legacy_a), identity(&legacy_b));
        assert!(!same_account(&legacy_a, &legacy_b));
        let mut unknown_account = auth(&jwt("same-user"), "");
        unknown_account.account_id = None;
        let mut rotated_unknown = unknown_account.clone();
        rotated_unknown.access = rotated_unknown
            .access
            .replace("signature", "rotated-signature");
        assert_ne!(identity(&unknown_account), identity(&rotated_unknown));
        assert!(!same_account(&unknown_account, &rotated_unknown));
    }

    #[test]
    fn bounded_cache_never_evicts_in_flight_keys_and_endpoint_derivation_is_safe() {
        let cache = ModelCache::default();
        let key = |index: u8| CacheKey {
            identity: [index; 32],
            endpoint: "http://localhost/models".into(),
            version: "v".into(),
        };
        let mut held: Vec<_> = (0..CACHE_CAPACITY)
            .map(|i| cache.entry(key(i as u8)).unwrap())
            .collect();
        assert!(cache.entry(key(99)).is_none());
        assert_eq!(cache.entries.lock().unwrap().len(), CACHE_CAPACITY);
        assert!(Arc::ptr_eq(&held[0], &cache.entry(key(0)).unwrap()));
        held.pop();
        assert!(cache.entry(key(99)).is_some());
        assert_eq!(cache.entries.lock().unwrap().len(), CACHE_CAPACITY);
        assert_eq!(
            models_endpoint(
                "http://localhost/api/responses/?secret=not-forwarded#fragment",
                "1.2.3"
            )
            .unwrap()
            .as_str(),
            "http://localhost/api/models?client_version=1.2.3"
        );
        assert!(models_endpoint("http://user:secret@localhost/responses", "v").is_none());
    }
}
