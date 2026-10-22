use anyhow::Result;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::to_string_pretty;
use std::fs;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// A consistent value and lifecycle revision, kept outside stored credential JSON.
#[derive(Clone)]
pub struct AuthSnapshot<T> {
    pub value: Option<T>,
    pub revision: String,
}

impl<T> std::fmt::Debug for AuthSnapshot<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthSnapshot")
            .field("has_value", &self.value.is_some())
            .field("coordinated", &!self.revision.is_empty())
            .finish()
    }
}

pub trait AuthStorage<T>: Send + Sync
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    fn load(&self) -> Result<Option<T>>;
    fn save(&self, value: T) -> Result<()>;
    fn clear(&self) -> Result<()>;
    fn path(&self) -> String;

    /// Read value and revision atomically. Empty revision means the store has
    /// no coordinated lifecycle support (ordinary reads only).
    fn load_snapshot(&self) -> Result<AuthSnapshot<T>> {
        Ok(AuthSnapshot {
            value: self.load()?,
            revision: String::new(),
        })
    }

    /// Atomically compare the snapshot and replace/clear it. Implementations must
    /// coordinate with save/clear and advance a store-unique revision on every
    /// mutation, including clear and identical-value writes, without ABA reuse.
    fn replace_if_unchanged(&self, _expected: &AuthSnapshot<T>, _value: Option<T>) -> Result<bool> {
        anyhow::bail!("Auth store does not support coordinated refresh")
    }
}

pub trait Keychain: Send + Sync {
    fn read(&self, service: &str, account: &str) -> Result<Option<String>>;
    fn write(&self, service: &str, account: &str, value: &str) -> Result<()>;
    fn delete(&self, service: &str, account: &str) -> Result<()>;
}

#[derive(Default)]
pub struct StubKeychain;

impl Keychain for StubKeychain {
    fn read(&self, _service: &str, _account: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn write(&self, _service: &str, _account: &str, _value: &str) -> Result<()> {
        Ok(())
    }

    fn delete(&self, _service: &str, _account: &str) -> Result<()> {
        Ok(())
    }
}

#[derive(Default, Clone, Copy)]
pub struct SystemKeychain;

#[cfg(target_os = "macos")]
impl Keychain for SystemKeychain {
    fn read(&self, service: &str, account: &str) -> Result<Option<String>> {
        let output = run_security(&["find-generic-password", "-s", service, "-a", account, "-w"])?;
        if output.status.success() {
            let mut raw = String::from_utf8(output.stdout)
                .map_err(|err| anyhow::anyhow!("Keychain value is not valid UTF-8: {err}"))?;
            trim_one_trailing_newline(&mut raw);
            return Ok(Some(raw));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("could not be found") || stderr.contains("specified item could not") {
            return Ok(None);
        }
        Err(anyhow::anyhow!("Keychain read failed: {}", stderr.trim()))
    }

    fn write(&self, _service: &str, _account: &str, _value: &str) -> Result<()> {
        anyhow::bail!("Keychain write is not available through non-interactive compatibility mode")
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        let output = run_security(&["delete-generic-password", "-s", service, "-a", account])?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("could not be found") || stderr.contains("specified item could not") {
            return Ok(());
        }
        Err(anyhow::anyhow!("Keychain delete failed: {}", stderr.trim()))
    }
}

#[cfg(target_os = "macos")]
fn run_security(args: &[&str]) -> Result<std::process::Output> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new("/usr/bin/security")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow::anyhow!("Failed to start /usr/bin/security: {err}"))?;
    let start = Instant::now();
    loop {
        if child
            .try_wait()
            .map_err(|err| anyhow::anyhow!("Failed waiting for /usr/bin/security: {err}"))?
            .is_some()
        {
            return child.wait_with_output().map_err(|err| {
                anyhow::anyhow!("Failed collecting /usr/bin/security output: {err}")
            });
        }
        if start.elapsed() >= Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Timed out reading macOS Keychain");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(target_os = "macos")]
fn trim_one_trailing_newline(value: &mut String) {
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl Keychain for SystemKeychain {
    fn read(&self, _service: &str, _account: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn write(&self, _service: &str, _account: &str, _value: &str) -> Result<()> {
        anyhow::bail!("Keychain storage is not available on this platform")
    }

    fn delete(&self, _service: &str, _account: &str) -> Result<()> {
        Ok(())
    }
}

pub struct FileAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    file: String,
    legacy_file: String,
    _marker: std::marker::PhantomData<T>,
}

impl<T> FileAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    pub fn new(file: String, legacy_file: String) -> Self {
        Self {
            file,
            legacy_file,
            _marker: Default::default(),
        }
    }

    fn lock(&self) -> Result<fs::File> {
        let path = format!("{}.lock", self.file);
        let dir = std::path::Path::new(&path).parent().unwrap();
        fs::create_dir_all(dir)?;
        set_mode(dir, 0o700);
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        // Never rename/remove this inode: all cooperating processes lock it.
        let file = options.open(path)?;
        file.lock()?;
        Ok(file)
    }

    fn revision(&self) -> Result<AuthRevision> {
        match fs::read_to_string(format!("{}.revision", self.file)) {
            Ok(raw) => Ok(serde_json::from_str(&raw)?),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // Adopt pre-revision file/keychain credentials without changing
                // their public JSON or migration source.
                let mut revision = AuthRevision::next();
                revision.cleared = false;
                self.set_revision(&revision)?;
                Ok(revision)
            }
            Err(err) => Err(err.into()),
        }
    }

    fn set_revision(&self, revision: &AuthRevision) -> Result<()> {
        write_atomically(&format!("{}.revision", self.file), revision)
    }

    fn load_unlocked(&self) -> Result<Option<T>> {
        if self.revision()?.cleared {
            return Ok(None);
        }
        Ok(load_auth_file::<T>(&self.file).or_else(|| load_auth_file::<T>(&self.legacy_file)))
    }

    fn clear_unlocked(&self) -> Result<()> {
        delete_auth_file(
            std::path::Path::new(&self.file),
            std::path::Path::new(&self.legacy_file),
        )?;
        Ok(())
    }

    fn update_unlocked(&self, value: Option<T>) -> Result<()> {
        let mut revision = AuthRevision::next();
        // Publish invalidation before touching credentials. Interrupted writes
        // fail closed, including stale keychain/legacy fallback after logout.
        self.set_revision(&revision)?;
        if let Some(value) = value {
            write_atomically(&self.file, &value)?;
            revision.cleared = false;
            self.set_revision(&revision)?;
        } else {
            self.clear_unlocked()?;
        }
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct AuthRevision {
    revision: String,
    cleared: bool,
}

impl AuthRevision {
    fn next() -> Self {
        Self {
            revision: uuid::Uuid::new_v4().to_string(),
            cleared: true,
        }
    }
}

fn snapshot_matches<T: Serialize>(
    snapshot: &AuthSnapshot<T>,
    revision: &str,
    value: &Option<T>,
) -> Result<bool> {
    Ok(snapshot.revision == revision
        && serde_json::to_value(&snapshot.value)? == serde_json::to_value(value)?)
}

impl<T> AuthStorage<T> for FileAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    fn load(&self) -> Result<Option<T>> {
        let _lock = self.lock()?;
        self.load_unlocked()
    }

    fn save(&self, value: T) -> Result<()> {
        let _lock = self.lock()?;
        self.update_unlocked(Some(value))
    }

    fn clear(&self) -> Result<()> {
        let _lock = self.lock()?;
        self.update_unlocked(None)
    }

    fn load_snapshot(&self) -> Result<AuthSnapshot<T>> {
        let _lock = self.lock()?;
        Ok(AuthSnapshot {
            value: self.load_unlocked()?,
            revision: self.revision()?.revision,
        })
    }

    fn replace_if_unchanged(&self, expected: &AuthSnapshot<T>, value: Option<T>) -> Result<bool> {
        let _lock = self.lock()?;
        if !snapshot_matches(expected, &self.revision()?.revision, &self.load_unlocked()?)? {
            return Ok(false);
        }
        self.update_unlocked(value)?;
        Ok(true)
    }

    fn path(&self) -> String {
        self.file.clone()
    }
}

pub struct KeychainFileAuthStore<T, K = SystemKeychain>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
    K: Keychain,
{
    file_store: FileAuthStore<T>,
    keychain: K,
    service: String,
    account: String,
    use_keychain: bool,
}

impl<T, K> KeychainFileAuthStore<T, K>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
    K: Keychain,
{
    pub fn new(
        file: String,
        legacy_file: String,
        service: impl Into<String>,
        account: impl Into<String>,
        use_keychain: bool,
        keychain: K,
    ) -> Self {
        Self {
            file_store: FileAuthStore::new(file, legacy_file),
            keychain,
            service: service.into(),
            account: account.into(),
            use_keychain,
        }
    }

    fn load_unlocked(&self) -> Result<Option<T>> {
        if self.file_store.revision()?.cleared {
            return Ok(None);
        }
        if let Some(parsed) = self.file_store.load_unlocked()? {
            return Ok(Some(parsed));
        }
        if self.use_keychain
            && let Some(raw) = self.keychain.read(&self.service, &self.account)?
        {
            return serde_json::from_str::<T>(&raw)
                .map(Some)
                .map_err(|err| anyhow::anyhow!("Failed to parse Keychain auth JSON: {err}"));
        }
        Ok(None)
    }

    fn update_unlocked(&self, value: Option<T>) -> Result<()> {
        let mut revision = AuthRevision::next();
        self.file_store.set_revision(&revision)?;
        if let Some(value) = value {
            let keychain_saved = self.use_keychain
                && self
                    .keychain
                    .write(
                        &self.service,
                        &self.account,
                        &serde_json::to_string(&value)?,
                    )
                    .is_ok();
            // File/legacy credentials take precedence when present. Keep the
            // primary current rather than deleting migration sources.
            if !keychain_saved
                || std::path::Path::new(&self.file_store.file).exists()
                || std::path::Path::new(&self.file_store.legacy_file).exists()
            {
                write_atomically(&self.file_store.file, &value)?;
            }
            revision.cleared = false;
            self.file_store.set_revision(&revision)?;
        } else {
            if self.use_keychain {
                self.keychain.delete(&self.service, &self.account)?;
            }
            self.file_store.clear_unlocked()?;
        }
        Ok(())
    }
}

impl<T, K> AuthStorage<T> for KeychainFileAuthStore<T, K>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
    K: Keychain,
{
    fn load(&self) -> Result<Option<T>> {
        let _lock = self.file_store.lock()?;
        self.load_unlocked()
    }

    fn save(&self, value: T) -> Result<()> {
        let _lock = self.file_store.lock()?;
        self.update_unlocked(Some(value))
    }

    fn clear(&self) -> Result<()> {
        let _lock = self.file_store.lock()?;
        self.update_unlocked(None)
    }

    fn load_snapshot(&self) -> Result<AuthSnapshot<T>> {
        let _lock = self.file_store.lock()?;
        Ok(AuthSnapshot {
            value: self.load_unlocked()?,
            revision: self.file_store.revision()?.revision,
        })
    }

    fn replace_if_unchanged(&self, expected: &AuthSnapshot<T>, value: Option<T>) -> Result<bool> {
        let _lock = self.file_store.lock()?;
        if !snapshot_matches(
            expected,
            &self.file_store.revision()?.revision,
            &self.load_unlocked()?,
        )? {
            return Ok(false);
        }
        self.update_unlocked(value)?;
        Ok(true)
    }

    fn path(&self) -> String {
        if self.use_keychain {
            "macOS Keychain".to_string()
        } else {
            self.file_store.path()
        }
    }
}

pub fn load_auth_file<T: DeserializeOwned>(path: &str) -> Option<T> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str::<T>(&raw).ok()
}

pub fn load_auth_file_value(path: &std::path::Path) -> Option<serde_json::Value> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str::<serde_json::Value>(&raw).ok()
}

pub fn load_auth_file_with_legacy<T: DeserializeOwned>(
    primary: &std::path::Path,
    legacy: &std::path::Path,
) -> Option<T> {
    if let Some(value) = load_auth_file_value(primary) {
        return serde_json::from_value(value).ok();
    }
    if primary == legacy {
        None
    } else {
        load_auth_file_value(legacy).and_then(|value| serde_json::from_value(value).ok())
    }
}

pub fn delete_auth_file(primary: &std::path::Path, legacy: &std::path::Path) -> io::Result<()> {
    if let Err(err) = fs::remove_file(primary)
        && err.kind() != io::ErrorKind::NotFound
    {
        return Err(err);
    }
    if primary != legacy
        && let Err(err) = fs::remove_file(legacy)
        && err.kind() != io::ErrorKind::NotFound
    {
        return Err(err);
    }
    Ok(())
}

pub fn write_atomically<T: Serialize>(path: &str, value: &T) -> Result<()> {
    let dir = std::path::Path::new(path)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid auth path"))?;
    fs::create_dir_all(dir)?;
    set_mode(dir, 0o700);

    let tmp = format!("{path}.tmp-{}", uuid::Uuid::new_v4());
    #[cfg(unix)]
    let mut out = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?
    };
    #[cfg(not(unix))]
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    out.write_all(to_string_pretty(value)?.as_bytes())?;
    out.sync_all()?;
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    set_mode(std::path::Path::new(path), 0o600);
    #[cfg(unix)]
    fs::File::open(if dir.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        dir
    })?
    .sync_all()?;
    Ok(())
}

fn set_mode(path: &std::path::Path, mode: u32) {
    #[cfg(unix)]
    {
        if let Ok(meta) = fs::metadata(path) {
            let mut permissions = meta.permissions();
            permissions.set_mode(mode);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

pub struct InMemoryAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    inner: std::sync::Arc<std::sync::Mutex<AuthSnapshot<T>>>,
}

impl<T> Default for InMemoryAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T> InMemoryAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(AuthSnapshot {
                value: None,
                revision: uuid::Uuid::new_v4().to_string(),
            })),
        }
    }
}

impl<T> Clone for InMemoryAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> AuthStorage<T> for InMemoryAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    fn load(&self) -> Result<Option<T>> {
        let inner = self
            .inner
            .lock()
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        Ok(inner.value.clone())
    }

    fn save(&self, value: T) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        inner.value = Some(value);
        inner.revision = uuid::Uuid::new_v4().to_string();
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        inner.value = None;
        inner.revision = uuid::Uuid::new_v4().to_string();
        Ok(())
    }

    fn load_snapshot(&self) -> Result<AuthSnapshot<T>> {
        Ok(self
            .inner
            .lock()
            .map_err(|err| anyhow::anyhow!(err.to_string()))?
            .clone())
    }

    fn replace_if_unchanged(&self, expected: &AuthSnapshot<T>, value: Option<T>) -> Result<bool> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        if !snapshot_matches(expected, &inner.revision, &inner.value)? {
            return Ok(false);
        }
        inner.value = value;
        inner.revision = uuid::Uuid::new_v4().to_string();
        Ok(true)
    }

    fn path(&self) -> String {
        "memory".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MockKeychain {
        values: Arc<Mutex<HashMap<(String, String), String>>>,
    }

    impl MockKeychain {
        fn set_raw(&self, service: &str, account: &str, value: serde_json::Value) {
            self.values.lock().unwrap().insert(
                (service.to_string(), account.to_string()),
                value.to_string(),
            );
        }

        fn raw(&self, service: &str, account: &str) -> Option<String> {
            self.values
                .lock()
                .unwrap()
                .get(&(service.to_string(), account.to_string()))
                .cloned()
        }
    }

    impl Keychain for MockKeychain {
        fn read(&self, service: &str, account: &str) -> Result<Option<String>> {
            Ok(self.raw(service, account))
        }

        fn write(&self, service: &str, account: &str, value: &str) -> Result<()> {
            self.values.lock().unwrap().insert(
                (service.to_string(), account.to_string()),
                value.to_string(),
            );
            Ok(())
        }

        fn delete(&self, service: &str, account: &str) -> Result<()> {
            self.values
                .lock()
                .unwrap()
                .remove(&(service.to_string(), account.to_string()));
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct ReadOnlyKeychain(MockKeychain);

    impl Keychain for ReadOnlyKeychain {
        fn read(&self, service: &str, account: &str) -> Result<Option<String>> {
            self.0.read(service, account)
        }

        fn write(&self, _service: &str, _account: &str, _value: &str) -> Result<()> {
            anyhow::bail!("read-only")
        }

        fn delete(&self, service: &str, account: &str) -> Result<()> {
            self.0.delete(service, account)
        }
    }

    fn temp_auth_path(dir: &tempfile::TempDir, name: &str) -> String {
        dir.path().join(name).to_string_lossy().to_string()
    }

    #[test]
    fn file_store_preserves_legacy_auth_during_migration() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "primary/auth.json");
        let legacy = temp_auth_path(&temp, "legacy/auth.json");
        write_atomically(&legacy, &json!({"source": "legacy"})).unwrap();
        let store: FileAuthStore<serde_json::Value> =
            FileAuthStore::new(file.clone(), legacy.clone());

        assert_eq!(store.load().unwrap().unwrap()["source"], "legacy");
        store.save(json!({"source": "primary"})).unwrap();
        assert_eq!(store.load().unwrap().unwrap()["source"], "primary");
        assert_eq!(
            load_auth_file::<serde_json::Value>(&legacy).unwrap()["source"],
            "legacy"
        );
        assert!(std::path::Path::new(&file).exists());
    }

    #[test]
    fn legacy_loaders_preserve_their_schema_fallback_behavior() {
        let temp = tempfile::TempDir::new().unwrap();
        let primary = temp.path().join("primary.json");
        let legacy = temp.path().join("legacy.json");
        write_atomically(primary.to_str().unwrap(), &"wrong schema").unwrap();
        write_atomically(legacy.to_str().unwrap(), &7_u64).unwrap();
        let store: FileAuthStore<u64> = FileAuthStore::new(
            primary.to_str().unwrap().to_string(),
            legacy.to_str().unwrap().to_string(),
        );

        assert_eq!(store.load().unwrap(), Some(7));
        assert_eq!(load_auth_file_with_legacy::<u64>(&primary, &legacy), None);
        fs::write(&primary, "invalid JSON").unwrap();
        assert_eq!(
            load_auth_file_with_legacy::<u64>(&primary, &legacy),
            Some(7)
        );
    }

    #[test]
    fn keychain_file_store_loads_file_before_keychain() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        write_atomically(&file, &json!({"source": "file"})).unwrap();

        let keychain = MockKeychain::default();
        keychain.set_raw("svc", "acct", json!({"source": "keychain"}));

        let store: KeychainFileAuthStore<serde_json::Value, _> =
            KeychainFileAuthStore::new(file, legacy, "svc", "acct", true, keychain);

        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded["source"], json!("file"));
        assert_eq!(store.path(), "macOS Keychain");
    }

    #[test]
    fn keychain_file_store_falls_back_to_keychain_when_file_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        let keychain = MockKeychain::default();
        keychain.set_raw("svc", "acct", json!({"source": "keychain"}));

        let store: KeychainFileAuthStore<serde_json::Value, _> =
            KeychainFileAuthStore::new(file, legacy, "svc", "acct", true, keychain);

        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded["source"], json!("keychain"));
    }

    #[test]
    fn keychain_file_store_saves_and_clears_keychain_when_enabled() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        write_atomically(&file, &json!({"source": "file"})).unwrap();

        let keychain = MockKeychain::default();
        let store: KeychainFileAuthStore<serde_json::Value, _> =
            KeychainFileAuthStore::new(file.clone(), legacy, "svc", "acct", true, keychain.clone());

        store.save(json!({"source": "saved"})).unwrap();
        assert_eq!(store.load().unwrap().unwrap()["source"], "saved");
        let raw = keychain.raw("svc", "acct").unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&raw).unwrap()["source"],
            json!("saved")
        );

        store.clear().unwrap();
        assert!(keychain.raw("svc", "acct").is_none());
        assert!(!std::path::Path::new(&file).exists());
    }

    #[test]
    fn keychain_file_store_falls_back_to_file_when_keychain_write_fails() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        let store: KeychainFileAuthStore<serde_json::Value, _> = KeychainFileAuthStore::new(
            file.clone(),
            legacy,
            "svc",
            "acct",
            true,
            ReadOnlyKeychain::default(),
        );

        store.save(json!({"source": "file-fallback"})).unwrap();
        assert_eq!(
            store.load().unwrap().unwrap()["source"],
            json!("file-fallback")
        );
        assert!(std::path::Path::new(&file).exists());
    }

    #[test]
    fn revision_adopts_legacy_json_and_rejects_aba_and_corrupt_state() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        write_atomically(&legacy, &json!({"synthetic": "legacy"})).unwrap();
        let store = FileAuthStore::new(file.clone(), legacy.clone());
        let snapshot = store.load_snapshot().unwrap();
        assert!(!snapshot.revision.is_empty());
        assert_eq!(snapshot.value, Some(json!({"synthetic": "legacy"})));
        store.clear().unwrap();
        store.save(snapshot.value.clone().unwrap()).unwrap();
        assert!(!store.replace_if_unchanged(&snapshot, None).unwrap());
        assert!(
            !store
                .replace_if_unchanged(&snapshot, Some(json!({"synthetic": "stale rotation"})))
                .unwrap()
        );
        assert_eq!(store.load().unwrap(), snapshot.value);
        // Coordination metadata never changes the stored credential schema.
        assert_eq!(load_auth_file::<serde_json::Value>(&file), snapshot.value);
        fs::write(format!("{file}.revision"), "broken state").unwrap();
        assert!(store.load().is_err());
        assert!(store.load_snapshot().is_err());
    }

    #[test]
    fn tombstone_blocks_stale_keychain_and_legacy_fallback() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        let keychain = MockKeychain::default();
        keychain.set_raw("synthetic", "account", json!({"synthetic": "old keychain"}));
        let store: KeychainFileAuthStore<serde_json::Value, _> = KeychainFileAuthStore::new(
            file.clone(),
            legacy.clone(),
            "synthetic",
            "account",
            true,
            keychain.clone(),
        );
        let old = store.load_snapshot().unwrap();
        store.clear().unwrap();
        let cleared = store.load_snapshot().unwrap();
        assert_ne!(old.revision, cleared.revision);
        // Simulate stale external migration sources after a completed logout.
        keychain.set_raw("synthetic", "account", json!({"synthetic": "old keychain"}));
        write_atomically(&legacy, &json!({"synthetic": "old legacy"})).unwrap();
        assert!(store.load().unwrap().is_none());
        assert!(
            !store
                .replace_if_unchanged(&old, Some(json!({"synthetic": "stale refresh"})))
                .unwrap()
        );
        store.save(json!({"synthetic": "new login"})).unwrap();
        assert_eq!(store.load().unwrap().unwrap()["synthetic"], "new login");
    }

    #[test]
    fn keychain_file_store_uses_file_when_keychain_disabled() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        let keychain = MockKeychain::default();
        let store: KeychainFileAuthStore<serde_json::Value, _> = KeychainFileAuthStore::new(
            file.clone(),
            legacy,
            "svc",
            "acct",
            false,
            keychain.clone(),
        );

        store.save(json!({"source": "file"})).unwrap();
        assert!(keychain.raw("svc", "acct").is_none());
        assert_eq!(store.path(), file);
        assert_eq!(store.load().unwrap().unwrap()["source"], json!("file"));
    }
}
