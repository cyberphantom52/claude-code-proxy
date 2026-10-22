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

    /// Read value and revision atomically.
    fn load_snapshot(&self) -> Result<AuthSnapshot<T>>;

    /// Atomically compare the snapshot and replace/clear it. Implementations must
    /// coordinate with save/clear and advance a store-unique revision on every
    /// mutation, including clear and identical-value writes, without ABA reuse.
    fn replace_if_unchanged(&self, expected: &AuthSnapshot<T>, value: Option<T>) -> Result<bool>;
}

struct SystemKeychain;

#[cfg(target_os = "macos")]
impl SystemKeychain {
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
impl SystemKeychain {
    fn read(&self, _service: &str, _account: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn delete(&self, _service: &str, _account: &str) -> Result<()> {
        Ok(())
    }
}

struct FileAuthStore<T>
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

pub struct KeychainFileAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    file_store: FileAuthStore<T>,
    service: String,
    account: String,
    use_keychain: bool,
}

impl<T> KeychainFileAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    pub fn new(
        file: String,
        legacy_file: String,
        service: impl Into<String>,
        account: impl Into<String>,
        use_keychain: bool,
    ) -> Self {
        Self {
            file_store: FileAuthStore::new(file, legacy_file),
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
            && let Some(raw) = SystemKeychain.read(&self.service, &self.account)?
        {
            return serde_json::from_str::<T>(&raw)
                .map(Some)
                .map_err(|err| anyhow::anyhow!("Failed to parse Keychain auth JSON: {err}"));
        }
        Ok(None)
    }

    fn update_unlocked(&self, value: Option<T>) -> Result<()> {
        let mut revision = AuthRevision::next();
        // Publish invalidation before touching credentials. Interrupted writes
        // fail closed, including stale keychain/legacy fallback after logout.
        self.file_store.set_revision(&revision)?;
        if let Some(value) = value {
            write_atomically(&self.file_store.file, &value)?;
            revision.cleared = false;
            self.file_store.set_revision(&revision)?;
        } else {
            if self.use_keychain {
                SystemKeychain.delete(&self.service, &self.account)?;
            }
            self.file_store.clear_unlocked()?;
        }
        Ok(())
    }
}

impl<T> AuthStorage<T> for KeychainFileAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
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
            self.file_store.file.clone()
        }
    }
}

pub fn load_auth_file<T: DeserializeOwned>(path: &str) -> Option<T> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str::<T>(&raw).ok()
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
