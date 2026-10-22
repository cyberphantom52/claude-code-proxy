use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths;

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub bind_address: String,
    pub port: u16,
    pub log_verbose: bool,
    pub log_stderr: bool,
    pub config_dir: PathBuf,
}

#[derive(Deserialize)]
struct FileConfig {
    #[serde(rename = "bindAddress")]
    pub bind_address: Option<String>,
    pub port: Option<u16>,
    #[serde(rename = "autoReviewModel")]
    pub auto_review_model: Option<String>,
    pub log: Option<FileLog>,
    pub codex: Option<CodexConfig>,
}

#[derive(Deserialize, Clone)]
struct CodexConfig {
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(rename = "originator")]
    pub originator: Option<String>,
    #[serde(rename = "userAgent")]
    pub user_agent: Option<String>,
    #[serde(rename = "previousResponseId")]
    pub previous_response_id: Option<bool>,
    #[serde(rename = "serverCompaction")]
    pub server_compaction: Option<bool>,
    #[serde(rename = "responsesApi")]
    pub responses_api: Option<bool>,
    #[serde(rename = "imagesApi")]
    pub images_api: Option<bool>,
    #[serde(rename = "imagesBaseUrl")]
    pub images_base_url: Option<String>,
    #[serde(rename = "transcriptionsApi")]
    pub transcriptions_api: Option<bool>,
    #[serde(rename = "fullLane")]
    pub full_lane: Option<bool>,
    #[serde(rename = "serviceTier")]
    pub service_tier: Option<String>,
    #[serde(rename = "reasoningSummary")]
    pub reasoning_summary: Option<String>,
    #[serde(rename = "effort")]
    pub effort: Option<String>,
    #[serde(rename = "model")]
    pub model: Option<String>,
    pub transport: Option<String>,
    #[serde(rename = "headerTimeoutMs")]
    pub header_timeout_ms: Option<u64>,
    #[serde(rename = "websocketConnectSpacingMs")]
    pub websocket_connect_spacing_ms: Option<u64>,
}

#[derive(Deserialize)]
struct FileLog {
    pub verbose: Option<bool>,
    pub stderr: Option<bool>,
}

fn read_file_config(config_dir: &Path) -> Option<FileConfig> {
    let path = config_dir.join("config.json");
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Reject obsolete backend selection rather than silently sending its intent to Codex.
pub fn validate_config() -> anyhow::Result<()> {
    validate_config_for_env(&std::env::vars().collect())
}

pub fn validate_config_for_env(env: &HashMap<String, String>) -> anyhow::Result<()> {
    use anyhow::Context;

    let cfg = load_config_for_env(env);
    let path = cfg.config_dir.join("config.json");
    let file: serde_json::Value = match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("cannot parse {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let env_alias = env
        .get("CCP_ALIAS_PROVIDER")
        .map(|value| serde_json::json!(value));
    for (source, value) in [
        ("CCP_ALIAS_PROVIDER", env_alias.as_ref()),
        ("aliasProvider", file.get("aliasProvider")),
    ] {
        if let Some(value) = value
            && value.as_str() != Some("codex")
        {
            anyhow::bail!(
                "{source}={value} is no longer supported; this proxy only supports Codex through a ChatGPT subscription. Remove {source} or set it to \"codex\"."
            );
        }
    }
    Ok(())
}

pub fn load_config() -> LoadedConfig {
    let env = paths::DirResolverEnv::default();
    let config_dir = paths::resolve_config_dir(&env);
    load_config_from_env(&env.env, config_dir)
}

pub fn load_config_for_env(env: &HashMap<String, String>) -> LoadedConfig {
    let home = env
        .get("HOME")
        .or_else(|| env.get("USERPROFILE"))
        .cloned()
        .unwrap_or_else(|| "/".to_string());
    let resolver_env = paths::DirResolverEnv {
        platform: std::env::consts::OS.to_string(),
        env: env.clone(),
        home,
    };
    let config_dir = paths::resolve_config_dir(&resolver_env);
    load_config_from_env(env, config_dir)
}

fn load_config_from_env(env: &HashMap<String, String>, config_dir: PathBuf) -> LoadedConfig {
    let file = read_file_config(&config_dir);

    let mut out = LoadedConfig {
        bind_address: "127.0.0.1".to_string(),
        port: 18765,
        log_verbose: false,
        log_stderr: false,
        config_dir: config_dir.clone(),
    };

    if let Some(raw) = env.get("CCP_BIND_ADDRESS") {
        out.bind_address = raw.clone();
    } else if let Some(bind_address) = file.as_ref().and_then(|f| f.bind_address.clone()) {
        out.bind_address = bind_address;
    }

    if let Some(raw) = env.get("PORT") {
        if let Ok(port) = raw.parse::<u16>() {
            out.port = port;
        }
    } else if let Some(port) = file.as_ref().and_then(|f| f.port) {
        out.port = port;
    }

    if env.contains_key("CCP_LOG_VERBOSE") {
        out.log_verbose = true;
    } else if let Some(value) = file
        .as_ref()
        .and_then(|f| f.log.as_ref().and_then(|v| v.verbose))
    {
        out.log_verbose = value;
    }

    if env.contains_key("CCP_LOG_STDERR") {
        out.log_stderr = true;
    } else if let Some(value) = file
        .as_ref()
        .and_then(|f| f.log.as_ref().and_then(|v| v.stderr))
    {
        out.log_stderr = value;
    }

    out
}

pub fn config_path() -> PathBuf {
    paths::config_dir().join("config.json")
}

pub fn port() -> u16 {
    load_config().port
}

pub fn bind_address() -> String {
    load_config().bind_address
}

pub fn log_verbose() -> bool {
    load_config().log_verbose
}

pub fn log_stderr() -> bool {
    load_config().log_stderr
}

pub fn config_override_summary_lines(cfg: &LoadedConfig) -> Vec<String> {
    let file = read_file_config(&cfg.config_dir);
    let env: HashMap<_, _> = std::env::vars().collect();
    let mut out = Vec::new();
    if env.contains_key("CCP_BIND_ADDRESS") {
        out.push("bindAddress (env)".to_string());
    }
    if env.contains_key("PORT") {
        out.push("port (env)".to_string());
    }
    if env.contains_key("CCP_LOG_VERBOSE") {
        out.push("log.verbose (env)".to_string());
    }
    if env.contains_key("CCP_LOG_STDERR") {
        out.push("log.stderr (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_RESPONSES_API") {
        out.push("codex.responsesApi (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_IMAGES_API") {
        out.push("codex.imagesApi (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_IMAGES_BASE_URL") {
        out.push("codex.imagesBaseUrl (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_TRANSCRIPTIONS_API") {
        out.push("codex.transcriptionsApi (env)".to_string());
    }
    if env
        .get("CCP_CODEX_REASONING_SUMMARY")
        .is_some_and(|raw| !raw.is_empty())
    {
        out.push("CCP_CODEX_REASONING_SUMMARY (env)".to_string());
    }
    if env.contains_key("CCP_CODEX_SERVER_COMPACTION") {
        out.push("CCP_CODEX_SERVER_COMPACTION (env)".to_string());
    }
    if env
        .get("CCP_AUTO_REVIEW_MODEL")
        .is_some_and(|raw| !raw.is_empty())
    {
        out.push("CCP_AUTO_REVIEW_MODEL (env)".to_string());
    }
    if let Some(file_cfg) = file {
        if let Some(bind_address) = file_cfg.bind_address {
            out.push(format!("bindAddress: {bind_address}"));
        }
        if let Some(p) = file_cfg.port {
            out.push(format!("port: {p}"));
        }
        if file_cfg
            .auto_review_model
            .is_some_and(|model| !model.is_empty())
        {
            out.push("autoReviewModel (config)".to_string());
        }
        if let Some(log) = file_cfg.log {
            if let Some(v) = log.verbose {
                out.push(format!("log.verbose: {v}"));
            }
            if let Some(v) = log.stderr {
                out.push(format!("log.stderr: {v}"));
            }
        }
        if let Some(codex) = file_cfg.codex {
            if codex
                .reasoning_summary
                .is_some_and(|value| !value.is_empty())
            {
                out.push("codex.reasoningSummary (config)".to_string());
            }
            if let Some(enabled) = codex.server_compaction {
                out.push(format!("codex.serverCompaction: {enabled}"));
            }
            if codex.responses_api == Some(true) {
                out.push("codex.responsesApi: true".to_string());
            }
            if codex.images_api == Some(true) {
                out.push("codex.imagesApi: true".to_string());
            }
            if codex.images_base_url.is_some() {
                out.push("codex.imagesBaseUrl (config)".to_string());
            }
            if codex.transcriptions_api == Some(true) {
                out.push("codex.transcriptionsApi: true".to_string());
            }
            if let Some(ms) = codex.header_timeout_ms {
                out.push(format!("codex.headerTimeoutMs: {ms}"));
            }
        }
    }
    out
}

pub fn is_verbose() -> bool {
    log_verbose()
}

// ---------------------------------------------------------------------------
// Codex config
// ---------------------------------------------------------------------------

pub fn codex_base_url(default: &str) -> String {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_BASE_URL") {
        return raw.clone();
    }
    if let Some(raw) = env.get("CLAUDE_CODE_PROXY_CODEX_BASE_URL") {
        return raw.clone();
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(url) = codex.base_url
    {
        return url;
    }
    default.to_string()
}

pub fn codex_originator(default: &str) -> String {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_ORIGINATOR") {
        return raw.clone();
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(val) = codex.originator
    {
        return val;
    }
    default.to_string()
}

pub fn codex_user_agent(default: &str) -> String {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_USER_AGENT") {
        return raw.clone();
    }
    if let Some(raw) = env.get("CCP_USER_AGENT") {
        return raw.clone();
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(ua) = codex.user_agent
    {
        return ua;
    }
    default.to_string()
}

pub fn codex_previous_response_id() -> bool {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_PREVIOUS_RESPONSE_ID") {
        return matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(val) = codex.previous_response_id
    {
        return val;
    }
    false
}

pub fn codex_server_compaction() -> bool {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_SERVER_COMPACTION") {
        match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => return true,
            "0" | "false" | "no" | "off" => return false,
            _ => {}
        }
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(enabled) = codex.server_compaction
    {
        return enabled;
    }
    false
}

pub fn codex_responses_api() -> bool {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_RESPONSES_API") {
        return matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(enabled) = codex.responses_api
    {
        return enabled;
    }
    false
}

pub fn codex_images_api() -> bool {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_IMAGES_API") {
        return matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(enabled) = codex.images_api
    {
        return enabled;
    }
    false
}

pub fn codex_transcriptions_api() -> bool {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_TRANSCRIPTIONS_API") {
        return matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(enabled) = codex.transcriptions_api
    {
        return enabled;
    }
    false
}

pub fn codex_images_base_url() -> String {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_IMAGES_BASE_URL") {
        return raw.clone();
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(url) = codex.images_base_url
    {
        return url;
    }
    "https://chatgpt.com/backend-api/codex".to_string()
}

pub fn codex_service_tier() -> Option<String> {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_SERVICE_TIER") {
        return Some(raw.clone());
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
    {
        return codex.service_tier;
    }
    None
}

pub fn codex_effort() -> Option<String> {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_EFFORT") {
        return Some(raw.clone());
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
    {
        return codex.effort;
    }
    None
}

pub fn codex_full_lane() -> bool {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_FULL_LANE") {
        return raw == "1" || raw == "true";
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
    {
        return codex.full_lane.unwrap_or(false);
    }
    false
}

pub fn codex_reasoning_summary() -> Option<String> {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env
        .get("CCP_CODEX_REASONING_SUMMARY")
        .filter(|raw| !raw.is_empty())
    {
        return Some(raw.clone());
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(summary) = codex.reasoning_summary.filter(|raw| !raw.is_empty())
    {
        return Some(summary);
    }
    None
}

pub fn codex_model() -> Option<String> {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_MODEL") {
        return Some(raw.clone());
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
    {
        return codex.model;
    }
    None
}

pub fn auto_review_model() -> Option<String> {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env
        .get("CCP_AUTO_REVIEW_MODEL")
        .filter(|raw| !raw.is_empty())
    {
        return Some(raw.clone());
    }
    read_file_config(&paths::config_dir())
        .and_then(|file| file.auto_review_model)
        .filter(|model| !model.is_empty())
}

// ---------------------------------------------------------------------------
// Codex transport config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexTransport {
    Http,
    WebSocket,
    Auto,
}

impl CodexTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            CodexTransport::Http => "http",
            CodexTransport::WebSocket => "websocket",
            CodexTransport::Auto => "auto",
        }
    }
}

fn parse_codex_transport(raw: &str) -> Option<CodexTransport> {
    match raw {
        "http" => Some(CodexTransport::Http),
        "websocket" => Some(CodexTransport::WebSocket),
        "auto" => Some(CodexTransport::Auto),
        _ => None,
    }
}

/// Plancher d'espacement des ouvertures de WebSocket Codex, en millisecondes.
///
/// Zéro (le défaut) laisse le processus ouvrir ses connexions sans attente tant que
/// l'origine n'a refusé aucun upgrade ; l'espacement s'élargit alors tout seul. Un
/// opérateur qui préfère un rythme garanti peut imposer un plancher.
pub fn codex_websocket_connect_spacing() -> std::time::Duration {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_WS_CONNECT_SPACING_MS")
        && let Ok(ms) = raw.trim().parse::<u64>()
    {
        return std::time::Duration::from_millis(ms);
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(ms) = codex.websocket_connect_spacing_ms
    {
        return std::time::Duration::from_millis(ms);
    }
    std::time::Duration::ZERO
}

pub fn codex_transport() -> CodexTransport {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("CCP_CODEX_TRANSPORT")
        && let Some(transport) = parse_codex_transport(raw)
    {
        return transport;
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(transport) = codex.transport.as_deref().and_then(parse_codex_transport)
    {
        return transport;
    }
    CodexTransport::Auto
}

/// How long an HTTP-transport request waits for the Codex response headers.
///
/// Codex withholds the response head until the model produces its first
/// output, so the wait tracks how much reasoning the request asks for rather
/// than the health of the connection. A large request to a high-effort model
/// can hold the head for minutes, and the timeout firing fails the request
/// outright, so the default is generous. Lower it only where a request that
/// slow is better failed than waited out. Values below
/// `CODEX_MIN_HEADER_TIMEOUT_MS` are ignored rather than clamped, so a typo
/// does not hide behind a working default.
pub const CODEX_DEFAULT_HEADER_TIMEOUT_MS: u64 = 300_000;
pub const CODEX_MIN_HEADER_TIMEOUT_MS: u64 = 1_000;

pub fn codex_header_timeout_ms() -> u64 {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(ms) = env
        .get("CCP_CODEX_HEADER_TIMEOUT_MS")
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms >= CODEX_MIN_HEADER_TIMEOUT_MS)
    {
        return ms;
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(ms) = codex
            .header_timeout_ms
            .filter(|ms| *ms >= CODEX_MIN_HEADER_TIMEOUT_MS)
    {
        return ms;
    }
    CODEX_DEFAULT_HEADER_TIMEOUT_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    static ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    /// Clears the environment knobs the config accessors read and points the
    /// config dir at `config`, so neither the developer's shell nor their real
    /// config.json can leak into the test. Every change is restored when the
    /// guards drop, including `CCP_CONFIG_DIR`: a plain `remove_var` here would
    /// strip a `CCP_CONFIG_DIR` the developer exported to isolate the whole
    /// run, and every later test in the process that reads config through
    /// `paths::config_dir()` would fall back to the real config file.
    fn isolated_env(config: &tempfile::TempDir) -> Vec<EnvGuard> {
        let mut guards = vec![
            EnvGuard::unset("CCP_BIND_ADDRESS"),
            EnvGuard::unset("CCP_CODEX_TRANSPORT"),
            EnvGuard::unset("CCP_LOG_VERBOSE"),
            EnvGuard::unset("CCP_LOG_STDERR"),
            EnvGuard::unset("CCP_CODEX_REASONING_SUMMARY"),
            EnvGuard::unset("CCP_CODEX_SERVER_COMPACTION"),
            EnvGuard::unset("CCP_CODEX_RESPONSES_API"),
            EnvGuard::unset("CCP_CODEX_IMAGES_API"),
            EnvGuard::unset("CCP_CODEX_IMAGES_BASE_URL"),
            EnvGuard::unset("CCP_CODEX_TRANSCRIPTIONS_API"),
            EnvGuard::unset("CCP_CODEX_HEADER_TIMEOUT_MS"),
            EnvGuard::unset("CCP_AUTO_REVIEW_MODEL"),
        ];
        guards.push(EnvGuard::set("CCP_CONFIG_DIR", config.path()));
        guards
    }

    fn config_env(config: &tempfile::TempDir) -> HashMap<String, String> {
        HashMap::from([(
            "CCP_CONFIG_DIR".to_string(),
            config.path().to_string_lossy().into_owned(),
        )])
    }

    #[test]
    fn bind_address_defaults_to_loopback() {
        let config = tempfile::TempDir::new().unwrap();
        let env = config_env(&config);

        assert_eq!(load_config_for_env(&env).bind_address, "127.0.0.1");
    }

    #[test]
    fn codex_full_lane_defaults_off_reads_config_and_env_takes_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let _env = isolated_env(&config);
        let _full_lane = EnvGuard::unset("CCP_CODEX_FULL_LANE");

        assert!(!codex_full_lane());

        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"fullLane":true}}"#,
        )
        .unwrap();
        assert!(codex_full_lane());

        let _env = EnvGuard::set("CCP_CODEX_FULL_LANE", "0");
        assert!(!codex_full_lane());
    }

    #[test]
    fn bind_address_reads_config_and_env_takes_precedence() {
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"bindAddress":"192.0.2.10"}"#,
        )
        .unwrap();
        let mut env = config_env(&config);

        assert_eq!(load_config_for_env(&env).bind_address, "192.0.2.10");
        env.insert("CCP_BIND_ADDRESS".to_string(), "0.0.0.0".to_string());
        assert_eq!(load_config_for_env(&env).bind_address, "0.0.0.0");
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn isolated_env_restores_an_exported_config_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let exported = tempfile::TempDir::new().unwrap();
        let _exported_env = EnvGuard::set("CCP_CONFIG_DIR", exported.path());
        {
            let config = tempfile::TempDir::new().unwrap();
            let _env = isolated_env(&config);
            assert_eq!(paths::config_dir(), config.path());
        }
        assert_eq!(paths::config_dir(), exported.path());
    }

    #[test]
    fn codex_transport_defaults_to_auto() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let _env = isolated_env(&config);
        assert_eq!(codex_transport(), CodexTransport::Auto);
    }

    #[test]
    fn codex_transport_reads_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let _env = isolated_env(&config);
        let _transport = EnvGuard::set("CCP_CODEX_TRANSPORT", "auto");
        assert_eq!(codex_transport(), CodexTransport::Auto);
    }

    #[test]
    fn codex_transport_env_websocket() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let _env = isolated_env(&config);
        let _transport = EnvGuard::set("CCP_CODEX_TRANSPORT", "websocket");
        assert_eq!(codex_transport(), CodexTransport::WebSocket);
    }

    #[test]
    fn codex_transport_invalid_env_falls_back_to_auto() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let _env = isolated_env(&config);
        let _transport = EnvGuard::set("CCP_CODEX_TRANSPORT", "invalid");
        assert_eq!(codex_transport(), CodexTransport::Auto);
    }

    #[test]
    fn codex_transport_empty_env_falls_back_to_auto() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let _env = isolated_env(&config);
        let _transport = EnvGuard::set("CCP_CODEX_TRANSPORT", "");
        assert_eq!(codex_transport(), CodexTransport::Auto);
    }

    #[test]
    fn codex_transport_file_and_env_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let _env = isolated_env(&config);
        for (file_value, expected) in [
            ("http", CodexTransport::Http),
            ("websocket", CodexTransport::WebSocket),
            ("auto", CodexTransport::Auto),
        ] {
            std::fs::write(
                config.path().join("config.json"),
                serde_json::json!({"codex": {"transport": file_value}}).to_string(),
            )
            .unwrap();
            assert_eq!(codex_transport(), expected);
            for env_value in ["", "invalid"] {
                let _transport = EnvGuard::set("CCP_CODEX_TRANSPORT", env_value);
                assert_eq!(codex_transport(), expected);
            }
            for (env_value, expected) in [
                ("http", CodexTransport::Http),
                ("websocket", CodexTransport::WebSocket),
                ("auto", CodexTransport::Auto),
            ] {
                let _transport = EnvGuard::set("CCP_CODEX_TRANSPORT", env_value);
                assert_eq!(codex_transport(), expected);
            }
        }
    }

    #[test]
    fn codex_transport_invalid_or_empty_file_falls_back_to_auto() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let _env = isolated_env(&config);
        for value in ["", "invalid"] {
            std::fs::write(
                config.path().join("config.json"),
                serde_json::json!({"codex": {"transport": value}}).to_string(),
            )
            .unwrap();
            assert_eq!(codex_transport(), CodexTransport::Auto);
        }
    }

    #[test]
    fn codex_header_timeout_defaults_overrides_and_floors() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let _env = isolated_env(&config);

        assert_eq!(codex_header_timeout_ms(), CODEX_DEFAULT_HEADER_TIMEOUT_MS);
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"headerTimeoutMs":600000}}"#,
        )
        .unwrap();
        assert_eq!(codex_header_timeout_ms(), 600_000);
        {
            let _timeout_env = EnvGuard::set("CCP_CODEX_HEADER_TIMEOUT_MS", "120000");
            assert_eq!(codex_header_timeout_ms(), 120_000);
        }
        // Below the floor is ignored, not clamped: a value that small is a typo,
        // and clamping it would hide the typo behind a working default.
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"headerTimeoutMs":5}}"#,
        )
        .unwrap();
        assert_eq!(codex_header_timeout_ms(), CODEX_DEFAULT_HEADER_TIMEOUT_MS);
        let _timeout_env = EnvGuard::set("CCP_CODEX_HEADER_TIMEOUT_MS", "0");
        assert_eq!(codex_header_timeout_ms(), CODEX_DEFAULT_HEADER_TIMEOUT_MS);
    }

    #[test]
    fn parse_codex_transport_variants() {
        assert_eq!(parse_codex_transport("http"), Some(CodexTransport::Http));
        assert_eq!(
            parse_codex_transport("websocket"),
            Some(CodexTransport::WebSocket)
        );
        assert_eq!(parse_codex_transport("auto"), Some(CodexTransport::Auto));
        assert_eq!(parse_codex_transport(""), None);
        assert_eq!(parse_codex_transport("HTTP"), None);
        assert_eq!(parse_codex_transport("ws"), None);
    }

    #[test]
    fn codex_transport_as_str() {
        assert_eq!(CodexTransport::Http.as_str(), "http");
        assert_eq!(CodexTransport::WebSocket.as_str(), "websocket");
        assert_eq!(CodexTransport::Auto.as_str(), "auto");
    }

    #[test]
    fn log_env_presence_enables_legacy_verbose_and_stderr() {
        let config = tempfile::TempDir::new().unwrap();
        let mut env = config_env(&config);
        env.insert("CCP_LOG_VERBOSE".to_string(), "0".to_string());
        env.insert("CCP_LOG_STDERR".to_string(), String::new());

        let loaded = load_config_for_env(&env);
        assert!(loaded.log_verbose);
        assert!(loaded.log_stderr);
    }

    #[test]
    fn log_config_values_apply_without_env() {
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"log":{"verbose":true,"stderr":true}}"#,
        )
        .unwrap();
        let env = config_env(&config);

        let loaded = load_config_for_env(&env);
        assert!(loaded.log_verbose);
        assert!(loaded.log_stderr);
    }

    #[test]
    fn codex_responses_api_defaults_to_disabled() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let _env = isolated_env(&config);

        assert!(!codex_responses_api());
    }

    #[test]
    fn codex_responses_api_reads_config_and_env_takes_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"responsesApi":true}}"#,
        )
        .unwrap();
        let _env = isolated_env(&config);

        assert!(codex_responses_api());
        let _responses_env = EnvGuard::set("CCP_CODEX_RESPONSES_API", "false");
        assert!(!codex_responses_api());
    }

    #[test]
    fn codex_responses_api_accepts_enabled_env_values() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let _env = isolated_env(&config);

        for value in ["1", "true", "TRUE", "yes"] {
            let _responses_env = EnvGuard::set("CCP_CODEX_RESPONSES_API", value);
            assert!(codex_responses_api(), "{value}");
        }
    }

    #[test]
    fn codex_images_api_defaults_to_disabled_and_env_overrides_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"imagesApi":true,"imagesBaseUrl":"https://chatgpt.com/backend-api/codex-custom"}}"#,
        )
        .unwrap();
        let _env = isolated_env(&config);

        assert!(codex_images_api());
        assert_eq!(
            codex_images_base_url(),
            "https://chatgpt.com/backend-api/codex-custom"
        );
        let _enabled_env = EnvGuard::set("CCP_CODEX_IMAGES_API", "false");
        let _base_env = EnvGuard::set(
            "CCP_CODEX_IMAGES_BASE_URL",
            "https://chatgpt.com/backend-api/codex",
        );
        assert!(!codex_images_api());
        assert_eq!(
            codex_images_base_url(),
            "https://chatgpt.com/backend-api/codex"
        );
    }

    #[test]
    fn codex_transcriptions_api_defaults_to_disabled_and_env_overrides_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"transcriptionsApi":true}}"#,
        )
        .unwrap();
        let _env = isolated_env(&config);

        assert!(codex_transcriptions_api());
        let _enabled_env = EnvGuard::set("CCP_CODEX_TRANSCRIPTIONS_API", "false");
        assert!(!codex_transcriptions_api());
    }

    #[test]
    fn codex_reasoning_summary_reads_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"reasoningSummary":"off"}}"#,
        )
        .unwrap();
        let _env = isolated_env(&config);

        assert_eq!(codex_reasoning_summary().as_deref(), Some("off"));
    }

    #[test]
    fn codex_reasoning_summary_env_overrides_config_and_empty_falls_through() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"reasoningSummary":"off"}}"#,
        )
        .unwrap();
        let _env = isolated_env(&config);
        {
            let _summary_env = EnvGuard::set("CCP_CODEX_REASONING_SUMMARY", "auto");
            assert_eq!(codex_reasoning_summary().as_deref(), Some("auto"));
        }
        {
            let _summary_env = EnvGuard::set("CCP_CODEX_REASONING_SUMMARY", "");
            assert_eq!(codex_reasoning_summary().as_deref(), Some("off"));
        }
    }

    #[test]
    fn auto_review_model_reads_top_level_config_and_env_takes_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        std::fs::write(
            config.path().join("config.json"),
            r#"{"autoReviewModel":"gpt-5.4-mini"}"#,
        )
        .unwrap();
        let _env = isolated_env(&config);

        assert_eq!(auto_review_model().as_deref(), Some("gpt-5.4-mini"));
        {
            let _model_env = EnvGuard::set("CCP_AUTO_REVIEW_MODEL", "gpt-5.6-terra");
            assert_eq!(auto_review_model().as_deref(), Some("gpt-5.6-terra"));
        }
        {
            let _model_env = EnvGuard::set("CCP_AUTO_REVIEW_MODEL", "");
            assert_eq!(auto_review_model().as_deref(), Some("gpt-5.4-mini"));
        }
    }

    #[test]
    fn codex_server_compaction_defaults_and_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let _env = isolated_env(&config);

        assert!(!codex_server_compaction());
        {
            let _enabled_env = EnvGuard::set("CCP_CODEX_SERVER_COMPACTION", "on");
            assert!(codex_server_compaction());
        }
        std::fs::write(
            config.path().join("config.json"),
            r#"{"codex":{"serverCompaction":true}}"#,
        )
        .unwrap();
        assert!(codex_server_compaction());
        let _disabled_env = EnvGuard::set("CCP_CODEX_SERVER_COMPACTION", "false");
        assert!(!codex_server_compaction());
    }
}
