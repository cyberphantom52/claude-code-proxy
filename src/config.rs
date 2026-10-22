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
#[serde(rename_all = "camelCase")]
struct FileConfig {
    pub bind_address: Option<String>,
    pub port: Option<u16>,
    pub auto_review_model: Option<String>,
    pub log: Option<FileLog>,
    pub codex: Option<CodexConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexConfig {
    pub base_url: Option<String>,
    pub originator: Option<String>,
    pub user_agent: Option<String>,
    pub previous_response_id: Option<bool>,
    pub server_compaction: Option<bool>,
    pub responses_api: Option<bool>,
    pub images_api: Option<bool>,
    pub images_base_url: Option<String>,
    pub transcriptions_api: Option<bool>,
    pub full_lane: Option<bool>,
    pub service_tier: Option<String>,
    pub reasoning_summary: Option<String>,
    pub effort: Option<String>,
    pub model: Option<String>,
    pub models_file: Option<PathBuf>,
    pub model_discovery: Option<bool>,
    pub transport: Option<String>,
    pub header_timeout_ms: Option<u64>,
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
        config_dir,
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

/// Disable live discovery for explicitly offline/fixed-catalog operation.
pub fn codex_model_discovery() -> bool {
    if let Ok(raw) = std::env::var("CCP_CODEX_MODEL_DISCOVERY") {
        match raw.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "no" | "off" => return false,
            "1" | "true" | "yes" | "on" => return true,
            _ => {}
        }
    }
    read_file_config(&paths::config_dir())
        .and_then(|file| file.codex)
        .and_then(|codex| codex.model_discovery)
        .unwrap_or(true)
}

/// Explicit offline catalog import; never discovers another application's state directory.
pub fn codex_models_file() -> Option<PathBuf> {
    std::env::var_os("CCP_CODEX_MODELS_FILE")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            read_file_config(&paths::config_dir())
                .and_then(|file| file.codex)
                .and_then(|codex| codex.models_file)
        })
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
