use std::sync::Arc;
use std::sync::LazyLock;
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
    refresh_lock: Arc<AsyncMutex<()>>,
    refresh_client: reqwest::Client,
}

impl<S: AuthStorage<StoredAuth>> CodexAuthManager<S> {
    pub fn new(store: CodexTokenStore<S>) -> Self {
        Self {
            store,
            refresh_lock: CODEX_REFRESH_LOCK.clone(),
            refresh_client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to create Codex OAuth refresh client"),
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
        let snapshot = self.store.load_snapshot()?;
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

    async fn refresh(
        &self,
        force: bool,
        rejected_access: Option<&str>,
        rejected_revision: Option<&str>,
    ) -> Result<AuthSnapshot<StoredAuth>, anyhow::Error> {
        let _refresh_guard = self.refresh_lock.lock().await;
        let snapshot = self.store.load_snapshot()?;
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
            .post(format!("{ISSUER}/oauth/token"))
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
}
