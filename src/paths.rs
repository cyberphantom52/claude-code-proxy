use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DirResolverEnv {
    pub platform: String,
    pub env: HashMap<String, String>,
    pub home: String,
}

impl Default for DirResolverEnv {
    fn default() -> Self {
        Self {
            platform: std::env::consts::OS.into(),
            env: std::env::vars().collect(),
            home: std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_else(|_| "/".to_string()),
        }
    }
}

pub fn resolve_config_dir(deps: &DirResolverEnv) -> PathBuf {
    if let Some(override_dir) = deps.env.get("CCP_CONFIG_DIR") {
        return Path::new(override_dir).to_path_buf();
    }

    if matches!(deps.platform.as_str(), "windows" | "win32") {
        let appdata = deps
            .env
            .get("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(&deps.home).join("AppData").join("Roaming"));
        return appdata.join("claude-code-proxy");
    }

    if matches!(deps.platform.as_str(), "macos" | "darwin") {
        return legacy_config_dir(deps);
    }

    let base = deps
        .env
        .get("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&deps.home).join(".config"));
    base.join("claude-code-proxy")
}

pub fn resolve_state_dir(deps: &DirResolverEnv) -> PathBuf {
    if matches!(deps.platform.as_str(), "windows" | "win32") {
        let local = deps
            .env
            .get("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(&deps.home).join("AppData").join("Local"));
        return local.join("claude-code-proxy");
    }

    let base = deps
        .env
        .get("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&deps.home).join(".local").join("state"));
    base.join("claude-code-proxy")
}

pub fn legacy_config_dir(deps: &DirResolverEnv) -> PathBuf {
    Path::new(&deps.home)
        .join(".config")
        .join("claude-code-proxy")
}

pub fn config_dir() -> PathBuf {
    resolve_config_dir(&DirResolverEnv::default())
}

pub fn state_dir() -> PathBuf {
    resolve_state_dir(&DirResolverEnv::default())
}

pub fn codex_auth_file(deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join("codex").join("auth.json")
}

pub fn log_file() -> PathBuf {
    resolve_state_dir(&DirResolverEnv::default()).join("proxy.log")
}

pub fn provider_auth_file(provider: &str) -> PathBuf {
    let deps = DirResolverEnv::default();
    resolve_config_dir(&deps).join(provider).join("auth.json")
}

pub fn provider_legacy_auth_file(provider: &str) -> PathBuf {
    let deps = DirResolverEnv::default();
    legacy_config_dir(&deps).join(provider).join("auth.json")
}

pub fn resolve_config_dir_for_env(
    platform: &str,
    home: &str,
    env: &HashMap<String, String>,
) -> PathBuf {
    resolve_config_dir(&DirResolverEnv {
        platform: platform.to_string(),
        env: env.clone(),
        home: home.to_string(),
    })
}

pub fn resolve_state_dir_for_env(
    platform: &str,
    home: &str,
    env: &HashMap<String, String>,
) -> PathBuf {
    resolve_state_dir(&DirResolverEnv {
        platform: platform.to_string(),
        env: env.clone(),
        home: home.to_string(),
    })
}
