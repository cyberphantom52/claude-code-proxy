use sha2::{Digest, Sha256};

use super::constants::{CLIENT_ID, ORIGINATOR};

// ---------------------------------------------------------------------------
// PKCE types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PkceCodes {
    pub verifier: String,
    pub challenge: String,
}

// ---------------------------------------------------------------------------
// Code generation
// ---------------------------------------------------------------------------

fn base64_url_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn generate_pkce() -> PkceCodes {
    let mut verifier_bytes = [0u8; 32];
    rand::fill(&mut verifier_bytes);
    let verifier = base64_url_encode(&verifier_bytes);

    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = base64_url_encode(&hash);

    PkceCodes {
        verifier,
        challenge,
    }
}

pub fn generate_state() -> String {
    let mut state_bytes = [0u8; 32];
    rand::fill(&mut state_bytes);
    base64_url_encode(&state_bytes)
}

// ---------------------------------------------------------------------------
// Authorize URL
// ---------------------------------------------------------------------------

pub fn build_authorize_url(
    issuer: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
) -> Result<String, anyhow::Error> {
    let mut url = url::Url::parse(&format!("{issuer}/oauth/authorize"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", "openid profile email offline_access")
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", ORIGINATOR);
    Ok(url.to_string())
}

// ---------------------------------------------------------------------------
// Token exchange
// ---------------------------------------------------------------------------

pub(super) fn exchange_code_for_tokens_until(
    issuer: &str,
    code: &str,
    pkce: &PkceCodes,
    redirect_uri: &str,
    deadline: std::time::Instant,
) -> Result<crate::providers::codex::auth::jwt::TokenResponse, anyhow::Error> {
    let remaining = deadline
        .checked_duration_since(std::time::Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| anyhow::anyhow!("OAuth timeout"))?;
    let client = reqwest::blocking::Client::builder()
        .tls_backend_preconfigured(super::super::client::http_tls_config())
        .timeout(remaining.min(std::time::Duration::from_secs(30)))
        .build()?;
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", &pkce.verifier),
    ];

    let resp = client
        .post(format!("{issuer}/oauth/token"))
        .timeout(
            deadline
                .checked_duration_since(std::time::Instant::now())
                .filter(|duration| !duration.is_zero())
                .ok_or_else(|| anyhow::anyhow!("OAuth timeout"))?
                .min(std::time::Duration::from_secs(30)),
        )
        .form(&form)
        .send()
        .map_err(|e| anyhow::anyhow!("Token exchange network error: {e}"))?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let text = resp.text().unwrap_or_default();
        anyhow::bail!("Token exchange failed: {status} {text}");
    }

    let tokens = resp
        .json()
        .map_err(|e| anyhow::anyhow!("failed to parse token exchange response: {e}"))?;
    Ok(tokens)
}
