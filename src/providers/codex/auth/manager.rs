use std::sync::Arc;
use std::sync::LazyLock;
#[cfg(test)]
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

use super::constants::{CLIENT_ID, ISSUER, REFRESH_MARGIN_MS};
use super::jwt::{TokenResponse, extract_account_id, validate_token_response};
use super::token_store::{CodexTokenStore, StoredAuth};
use crate::auth::{AuthSnapshot, AuthStorage};

static CODEX_REFRESH_LOCK: LazyLock<Arc<AsyncMutex<()>>> =
    LazyLock::new(|| Arc::new(AsyncMutex::new(())));

pub struct CodexAuthManager<S: AuthStorage<StoredAuth>> {
    pub store: CodexTokenStore<S>,
    #[cfg(test)]
    test_auth: Arc<Mutex<Option<AuthSnapshot<StoredAuth>>>>,
    refresh_lock: Arc<AsyncMutex<()>>,
    refresh_client: reqwest::Client,
    token_endpoint: String,
}

impl<S: AuthStorage<StoredAuth>> CodexAuthManager<S> {
    pub fn new(store: CodexTokenStore<S>) -> Self {
        Self::new_with_token_endpoint(store, format!("{ISSUER}/oauth/token"))
    }

    pub(in crate::providers::codex) fn new_with_token_endpoint(
        store: CodexTokenStore<S>,
        token_endpoint: String,
    ) -> Self {
        Self {
            store,
            #[cfg(test)]
            test_auth: Arc::new(Mutex::new(None)),
            refresh_lock: CODEX_REFRESH_LOCK.clone(),
            refresh_client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to create Codex OAuth refresh client"),
            token_endpoint,
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    pub async fn get_auth(&self) -> Result<StoredAuth, anyhow::Error> {
        Ok(self
            .get_auth_snapshot()
            .await?
            .value
            .expect("authenticated snapshot"))
    }

    pub async fn get_auth_snapshot(&self) -> Result<AuthSnapshot<StoredAuth>, anyhow::Error> {
        let snapshot = self.load_snapshot()?;
        let stored = snapshot.value.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Not authenticated. Run: claude-code-proxy codex auth login")
        })?;
        if stored.expires > Self::now_ms().saturating_add(REFRESH_MARGIN_MS) {
            return Ok(snapshot);
        }
        self.refresh(false, None, None).await
    }

    pub async fn force_refresh(&self, rejected_access: &str) -> Result<StoredAuth, anyhow::Error> {
        Ok(self
            .refresh(true, Some(rejected_access), None)
            .await?
            .value
            .expect("authenticated snapshot"))
    }

    pub async fn force_refresh_snapshot(
        &self,
        rejected: &AuthSnapshot<StoredAuth>,
    ) -> Result<AuthSnapshot<StoredAuth>, anyhow::Error> {
        self.refresh(true, None, Some(&rejected.revision)).await
    }

    fn load_snapshot(&self) -> Result<AuthSnapshot<StoredAuth>, anyhow::Error> {
        #[cfg(test)]
        if let Some(auth) = self
            .test_auth
            .lock()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .clone()
        {
            return Ok(auth);
        }
        self.store.load_snapshot()
    }

    async fn refresh(
        &self,
        force: bool,
        rejected_access: Option<&str>,
        rejected_revision: Option<&str>,
    ) -> Result<AuthSnapshot<StoredAuth>, anyhow::Error> {
        let _refresh_guard = self.refresh_lock.lock().await;
        let snapshot = self.load_snapshot()?;
        let current = snapshot
            .value
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Not authenticated"))?;
        if (!force && current.expires > Self::now_ms().saturating_add(REFRESH_MARGIN_MS))
            || rejected_access.is_some_and(|access| current.access != access)
            || rejected_revision.is_some_and(|revision| snapshot.revision != revision)
        {
            return Ok(snapshot);
        }
        anyhow::ensure!(
            !snapshot.revision.is_empty(),
            "Auth store does not support coordinated refresh"
        );
        self.refresh_now(&snapshot).await
    }

    async fn refresh_now(
        &self,
        snapshot: &AuthSnapshot<StoredAuth>,
    ) -> Result<AuthSnapshot<StoredAuth>, anyhow::Error> {
        let current = snapshot.value.as_ref().expect("authenticated snapshot");
        if current.refresh.is_empty() {
            anyhow::bail!("No refresh token stored; re-authenticate");
        }

        let form = [
            ("client_id", CLIENT_ID.to_string()),
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", current.refresh.clone()),
        ];

        let resp = self
            .refresh_client
            .post(&self.token_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("refresh network error: {e}"))?;

        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            // A peer may have rotated this refresh token successfully while its
            // response is still in flight. A rejection must not tombstone that
            // generation and prevent the peer from persisting its valid result.
            let latest = self.store.load_snapshot()?;
            if latest.revision != snapshot.revision {
                anyhow::ensure!(latest.value.is_some(), "Not authenticated");
                return Ok(latest);
            }
            let err_msg = resp
                .text()
                .await
                .unwrap_or_else(|_| "Token refresh unauthorized".to_string());
            anyhow::bail!("{err_msg}");
        }

        if !resp.status().is_success() {
            anyhow::bail!("Token refresh failed: {status}");
        }

        let tokens: TokenResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("failed to parse token response: {e}"))?;
        validate_token_response(&tokens)?;
        let account_id = extract_account_id(&tokens).or_else(|| current.account_id.clone());
        let expires =
            Self::now_ms().saturating_add(tokens.expires_in.unwrap_or(3600).saturating_mul(1000));
        let next = StoredAuth {
            access: tokens.access_token,
            refresh: tokens.refresh_token,
            expires,
            account_id,
        };
        self.store.replace_if_unchanged(snapshot, Some(next))?;
        self.latest_authenticated()
    }

    fn latest_authenticated(&self) -> Result<AuthSnapshot<StoredAuth>, anyhow::Error> {
        let latest = self.store.load_snapshot()?;
        anyhow::ensure!(
            latest.value.is_some(),
            "Not authenticated: credentials changed during refresh"
        );
        Ok(latest)
    }

    pub fn persist_initial_tokens(
        &self,
        tokens: &TokenResponse,
    ) -> Result<StoredAuth, anyhow::Error> {
        validate_token_response(tokens)?;
        let account_id = extract_account_id(tokens);
        let expires =
            Self::now_ms().saturating_add(tokens.expires_in.unwrap_or(3600).saturating_mul(1000));
        let auth = StoredAuth {
            access: tokens.access_token.clone(),
            refresh: tokens.refresh_token.clone(),
            expires,
            account_id,
        };
        self.store.save_auth(auth.clone())?;
        Ok(auth)
    }

    #[cfg(test)]
    pub fn set_test_auth(&self, auth: StoredAuth) {
        if let Ok(mut guard) = self.test_auth.lock() {
            *guard = Some(AuthSnapshot {
                value: Some(auth),
                revision: uuid::Uuid::new_v4().to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    fn test_store() -> CodexTokenStore<InMemoryAuthStore<StoredAuth>> {
        CodexTokenStore::new(InMemoryAuthStore::new())
    }

    #[tokio::test]
    async fn get_auth_returns_stored() {
        let store = test_store();
        let auth = StoredAuth {
            access: "test_access".into(),
            refresh: "test_refresh".into(),
            expires: 9999999999999,
            account_id: Some("acct_1".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let manager = CodexAuthManager::new(store);
        let result = manager.get_auth().await.unwrap();
        assert_eq!(result.access, "test_access");
        assert_eq!(result.account_id.as_deref(), Some("acct_1"));
    }

    #[tokio::test]
    async fn custom_read_only_store_supports_requests_but_refuses_unsafe_refresh() {
        struct ReadOnly(StoredAuth);
        impl AuthStorage<StoredAuth> for ReadOnly {
            fn load(&self) -> anyhow::Result<Option<StoredAuth>> {
                Ok(Some(self.0.clone()))
            }
            fn save(&self, _: StoredAuth) -> anyhow::Result<()> {
                anyhow::bail!("read only")
            }
            fn clear(&self) -> anyhow::Result<()> {
                anyhow::bail!("read only")
            }
            fn path(&self) -> String {
                "synthetic-read-only".into()
            }
        }
        let manager = CodexAuthManager::new_with_token_endpoint(
            CodexTokenStore::new(ReadOnly(StoredAuth {
                access: "synthetic-unexpired".into(),
                refresh: "synthetic-refresh".into(),
                expires: u64::MAX,
                account_id: None,
            })),
            "http://127.0.0.1:1/must-not-request".into(),
        );
        let selected = manager.get_auth_snapshot().await.unwrap();
        assert!(selected.revision.is_empty());
        assert_eq!(
            manager.get_auth().await.unwrap().access,
            "synthetic-unexpired"
        );
        assert!(!format!("{selected:?}").contains("synthetic-refresh"));
        assert!(
            manager
                .force_refresh_snapshot(&selected)
                .await
                .unwrap_err()
                .to_string()
                .contains("coordinated refresh")
        );
    }

    #[tokio::test]
    async fn get_auth_fails_when_no_auth() {
        let store = test_store();
        let manager = CodexAuthManager::new(store);
        assert!(manager.get_auth().await.is_err());
        assert!(
            manager
                .get_auth()
                .await
                .unwrap_err()
                .to_string()
                .contains("Not authenticated")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_expired_auth_refreshes_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let server_refreshes = refreshes.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).unwrap();
            assert!(read > 0);
            assert!(String::from_utf8_lossy(&request[..read]).contains("refresh_token=stale"));
            server_refreshes.fetch_add(1, Ordering::SeqCst);

            let body = br#"{"access_token":"rotated","refresh_token":"rotated-refresh","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });

        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "expired".into(),
                refresh: "stale".into(),
                expires: 0,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = Arc::new(CodexAuthManager::new_with_token_endpoint(
            store,
            format!("http://{addr}/oauth/token"),
        ));
        let (first, second) = tokio::join!(manager.get_auth(), manager.get_auth());
        let results = [first.unwrap(), second.unwrap()];
        server.join().unwrap();

        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert!(results.iter().all(|auth| auth.access == "rotated"));
        assert!(results.iter().all(|auth| auth.refresh == "rotated-refresh"));
    }

    #[tokio::test]
    async fn stale_401_reuses_already_rotated_auth() {
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "rotated".into(),
                refresh: "rotated-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = CodexAuthManager::new_with_token_endpoint(
            store,
            "http://127.0.0.1:1/should-not-be-called".into(),
        );

        let auth = manager.force_refresh("rejected").await.unwrap();
        assert_eq!(auth.access, "rotated");
        assert_eq!(auth.refresh, "rotated-refresh");
    }

    #[tokio::test]
    async fn unauthorized_refresh_preserves_changed_refresh_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let backing = InMemoryAuthStore::new();
        let server_backing = backing.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            server_backing
                .save(StoredAuth {
                    access: "same-access".into(),
                    refresh: "replacement-refresh".into(),
                    expires: u64::MAX,
                    account_id: Some("acct_1".into()),
                })
                .unwrap();
            let body = b"rejected refresh token";
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });

        let store = CodexTokenStore::new(backing);
        store
            .save_auth(StoredAuth {
                access: "same-access".into(),
                refresh: "rejected-refresh".into(),
                expires: 0,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager =
            CodexAuthManager::new_with_token_endpoint(store, format!("http://{addr}/oauth/token"));

        let auth = manager.get_auth().await.unwrap();
        server.join().unwrap();
        assert_eq!(auth.access, "same-access");
        assert_eq!(auth.refresh, "replacement-refresh");
        assert_eq!(manager.store.load_auth().unwrap(), Some(auth));
    }

    // Invoked only by the persisted race regression below, with a private temp path.
    #[test]
    fn lifecycle_mutation_child() {
        let Some(path) = std::env::var_os("CCP_SYNTHETIC_AUTH_RACE_PATH") else {
            return;
        };
        let path = path.to_str().unwrap().to_owned();
        let store = CodexTokenStore::new(crate::auth::FileAuthStore::<StoredAuth>::new(
            path.clone(),
            path,
        ));
        let previous = store.load_auth().unwrap().unwrap();
        match std::env::var("CCP_SYNTHETIC_AUTH_RACE_ACTION")
            .unwrap()
            .as_str()
        {
            "logout" => store.clear_auth().unwrap(),
            "login" => {
                let mut next = previous;
                next.access = "synthetic-new-login".into();
                next.refresh = "synthetic-new-refresh".into();
                next.account_id = Some("synthetic-other-account".into());
                next.expires = u64::MAX;
                store.save_auth(next).unwrap();
            }
            "reject-refresh" => {
                let before = store.load_snapshot().unwrap();
                let endpoint = std::env::var("CCP_SYNTHETIC_AUTH_RACE_ENDPOINT").unwrap();
                let manager = CodexAuthManager::new_with_token_endpoint(store, endpoint);
                let error = tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(manager.get_auth())
                    .unwrap_err();
                assert!(error.to_string().contains("synthetic unauthorized"));
                let after = manager.store.load_snapshot().unwrap();
                assert_eq!(before.value, after.value);
                assert_eq!(before.revision, after.revision);
            }
            "aba" => {
                store.clear_auth().unwrap();
                store.save_auth(previous).unwrap();
            }
            action => panic!("unexpected synthetic mutation: {action}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blocked_refresh_cannot_overwrite_cross_process_logout_login_or_aba() {
        use crate::auth::FileAuthStore;
        use std::sync::mpsc;

        for status in [200, 401] {
            for action in ["logout", "login", "aba"] {
                let temp = tempfile::TempDir::new().unwrap();
                let path = temp.path().join("auth.json").to_str().unwrap().to_owned();
                let store = CodexTokenStore::new(FileAuthStore::new(path.clone(), path.clone()));
                let original = StoredAuth {
                    access: "synthetic-expired".into(),
                    refresh: "synthetic-old-refresh".into(),
                    expires: 0,
                    account_id: Some("synthetic-account".into()),
                };
                store.save_auth(original.clone()).unwrap();
                let initial_revision = store.load_snapshot().unwrap().revision;
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let addr = listener.local_addr().unwrap();
                let (started_tx, started_rx) = mpsc::channel();
                let (release_tx, release_rx) = mpsc::channel();
                let server = thread::spawn(move || {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut request = [0; 4096];
                    assert!(stream.read(&mut request).unwrap() > 0);
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(20)).unwrap();
                    let body = if status == 200 {
                        r#"{"access_token":"synthetic-rotated","refresh_token":"synthetic-rotation","expires_in":3600}"#
                    } else {
                        "synthetic unauthorized"
                    };
                    write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                });
                let manager = Arc::new(CodexAuthManager::new_with_token_endpoint(
                    store,
                    format!("http://{addr}/oauth/token"),
                ));
                let refreshing = manager.clone();
                let refresh = tokio::spawn(async move { refreshing.get_auth_snapshot().await });
                started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                // A separate process must finish its write while HTTP is blocked:
                // this also proves no filesystem lock spans the network request.
                let mutation = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "providers::codex::auth::manager::tests::lifecycle_mutation_child",
                    ])
                    .env("CCP_SYNTHETIC_AUTH_RACE_PATH", &path)
                    .env("CCP_SYNTHETIC_AUTH_RACE_ACTION", action)
                    .output()
                    .unwrap();
                assert!(
                    mutation.status.success(),
                    "{}",
                    String::from_utf8_lossy(&mutation.stderr)
                );
                let after_mutation = manager.store.load_snapshot().unwrap();
                assert_ne!(initial_revision, after_mutation.revision);
                release_tx.send(()).unwrap();
                let result = refresh.await.unwrap();
                server.join().unwrap();
                let persisted = manager.store.load_snapshot().unwrap();
                assert_eq!(
                    persisted.revision, after_mutation.revision,
                    "{status}/{action}"
                );
                assert_eq!(persisted.value, after_mutation.value, "{status}/{action}");
                if action == "logout" {
                    assert!(result.is_err());
                    assert!(persisted.value.is_none());
                } else {
                    let returned = result.unwrap();
                    assert_eq!(returned.revision, after_mutation.revision);
                    assert_eq!(returned.value, after_mutation.value);
                    assert_ne!(returned.value.unwrap().access, "synthetic-rotated");
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejected_peer_refresh_cannot_discard_delayed_successful_rotation() {
        use crate::auth::FileAuthStore;
        use std::sync::mpsc;

        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("auth.json").to_str().unwrap().to_owned();
        let store = CodexTokenStore::new(FileAuthStore::new(path.clone(), path.clone()));
        store
            .save_auth(StoredAuth {
                access: "synthetic-expired".into(),
                refresh: "synthetic-shared-refresh".into(),
                expires: 0,
                account_id: Some("synthetic-account".into()),
            })
            .unwrap();
        let before = store.load_snapshot().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/oauth/token", listener.local_addr().unwrap());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut successful, _) = listener.accept().unwrap();
            successful
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert!(
                super::super::test_http::read_http_request(&mut successful)
                    .unwrap()
                    .contains("synthetic-shared-refresh")
            );
            started_tx.send(()).unwrap();
            let (mut rejected, _) = listener.accept().unwrap();
            assert!(
                super::super::test_http::read_http_request(&mut rejected)
                    .unwrap()
                    .contains("synthetic-shared-refresh")
            );
            rejected
                .write_all(
                    super::super::test_http::response(401, "text/plain", "synthetic unauthorized")
                        .as_bytes(),
                )
                .unwrap();
            drop(rejected);
            release_rx.recv_timeout(Duration::from_secs(20)).unwrap();
            successful.write_all(super::super::test_http::json_response(200,
                r#"{"access_token":"synthetic-peer-rotated","refresh_token":"synthetic-peer-next","expires_in":3600}"#,
            ).as_bytes()).unwrap();
        });
        let manager = Arc::new(CodexAuthManager::new_with_token_endpoint(
            store,
            endpoint.clone(),
        ));
        let refreshing = manager.clone();
        let success = tokio::spawn(async move { refreshing.get_auth_snapshot().await });
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let rejected = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "providers::codex::auth::manager::tests::lifecycle_mutation_child",
            ])
            .env("CCP_SYNTHETIC_AUTH_RACE_PATH", &path)
            .env("CCP_SYNTHETIC_AUTH_RACE_ACTION", "reject-refresh")
            .env("CCP_SYNTHETIC_AUTH_RACE_ENDPOINT", &endpoint)
            .output()
            .unwrap();
        let unchanged = manager.store.load_snapshot().unwrap();
        release_tx.send(()).unwrap();
        let rotated = success.await.unwrap().unwrap();
        server.join().unwrap();
        assert!(
            rejected.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&rejected.stdout),
            String::from_utf8_lossy(&rejected.stderr)
        );
        assert_eq!(unchanged.value, before.value);
        assert_eq!(unchanged.revision, before.revision);
        assert_eq!(
            rotated.value.as_ref().unwrap().access,
            "synthetic-peer-rotated"
        );
        assert_ne!(rotated.revision, before.revision);
        assert_eq!(manager.store.load_auth().unwrap(), rotated.value);
    }

    #[tokio::test]
    async fn durable_rotation_and_logout_are_observed_by_shared_manager() {
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "first".into(),
                refresh: "first-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = CodexAuthManager::new(store);
        assert_eq!(manager.get_auth().await.unwrap().access, "first");

        manager
            .store
            .save_auth(StoredAuth {
                access: "rotated".into(),
                refresh: "rotated-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_2".into()),
            })
            .unwrap();
        let rotated = manager.get_auth().await.unwrap();
        assert_eq!(rotated.access, "rotated");
        assert_eq!(rotated.account_id.as_deref(), Some("acct_2"));

        manager.store.clear_auth().unwrap();
        assert!(manager.get_auth().await.is_err());
    }
}
