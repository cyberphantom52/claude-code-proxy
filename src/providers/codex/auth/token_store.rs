use serde::{Deserialize, Serialize};

use crate::auth::{AuthSnapshot, AuthStorage, KeychainFileAuthStore, SystemKeychain};
use crate::paths;

pub const KEYCHAIN_SERVICE: &str = "claude-code-proxy.codex";
pub const KEYCHAIN_ACCOUNT: &str = "auth";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredAuth {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    #[serde(
        default,
        rename = "accountId",
        alias = "account_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

pub struct CodexTokenStore<S: AuthStorage<StoredAuth>> {
    store: S,
}

impl<S: AuthStorage<StoredAuth>> CodexTokenStore<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn load_auth(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        self.store.load()
    }

    pub fn save_auth(&self, value: StoredAuth) -> Result<(), anyhow::Error> {
        self.store.save(value)
    }

    pub fn clear_auth(&self) -> Result<(), anyhow::Error> {
        self.store.clear()
    }

    pub fn load_snapshot(&self) -> Result<AuthSnapshot<StoredAuth>, anyhow::Error> {
        self.store.load_snapshot()
    }

    pub fn replace_if_unchanged(
        &self,
        expected: &AuthSnapshot<StoredAuth>,
        value: Option<StoredAuth>,
    ) -> Result<bool, anyhow::Error> {
        self.store.replace_if_unchanged(expected, value)
    }

    pub fn auth_path(&self) -> String {
        self.store.path()
    }
}

pub type DefaultCodexAuthStore = KeychainFileAuthStore<StoredAuth, SystemKeychain>;

pub fn file_store() -> CodexTokenStore<DefaultCodexAuthStore> {
    let primary = paths::provider_auth_file("codex");
    let legacy = paths::provider_legacy_auth_file("codex");
    let store = KeychainFileAuthStore::new(
        primary.to_string_lossy().to_string(),
        legacy.to_string_lossy().to_string(),
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
        use_macos_keychain(),
        SystemKeychain,
    );
    CodexTokenStore::new(store)
}

fn use_macos_keychain() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("CCP_CONFIG_DIR").is_none()
}
