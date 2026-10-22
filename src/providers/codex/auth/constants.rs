pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";
pub const CODEX_API_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
// Codex models protocol compatibility, not the proxy package version.
// openai/codex rust-v0.163.0 (83086188ae0614bd882818ff1aa23f3194962012).
pub const CODEX_PROTOCOL_VERSION: &str = "0.163.0";
pub const ORIGINATOR: &str = "claude-code-proxy";
pub const RESPONSES_LITE_ORIGINATOR: &str = "codex_cli_rs";
pub const REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;
pub const OAUTH_PORT: u16 = 1455;
pub const OAUTH_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const DEVICE_POLL_SAFETY_MARGIN_MS: u64 = 3000;
