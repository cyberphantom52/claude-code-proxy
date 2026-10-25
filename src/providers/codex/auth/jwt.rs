use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    #[serde(rename = "https://api.openai.com/auth")]
    openai_auth: Option<OpenAiAuthClaim>,
    #[serde(default)]
    #[serde(rename = "https://api.openai.com/auth.chatgpt_account_id")]
    openai_chatgpt_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiAuthClaim {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub id_token: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RefreshResponse {
    id_token: Option<String>,
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

impl RefreshResponse {
    pub(super) fn into_tokens(self, current_refresh: &str) -> anyhow::Result<TokenResponse> {
        let tokens = TokenResponse {
            id_token: self.id_token,
            access_token: self.access_token,
            refresh_token: self
                .refresh_token
                .unwrap_or_else(|| current_refresh.to_string()),
            expires_in: self.expires_in,
        };
        validate_token_response(&tokens)?;
        Ok(tokens)
    }
}

fn parse_jwt_claims(token: &str) -> Option<IdTokenClaims> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload_b64 = parts[1].replace('-', "+").replace('_', "/");
    let padded = match payload_b64.len() % 4 {
        2 => format!("{payload_b64}=="),
        3 => format!("{payload_b64}="),
        _ => payload_b64,
    };
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&padded)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

pub fn validate_token_response(tokens: &TokenResponse) -> anyhow::Result<()> {
    if tokens.access_token.trim().is_empty() {
        anyhow::bail!("token response missing access token");
    }
    if tokens.refresh_token.trim().is_empty() {
        anyhow::bail!("token response missing refresh token");
    }
    if matches!(tokens.expires_in, Some(0)) {
        anyhow::bail!("token response has invalid expiration");
    }
    Ok(())
}

pub fn extract_account_id(tokens: &TokenResponse) -> Option<String> {
    let id = tokens.id_token.as_deref().and_then(parse_jwt_claims);
    let access = parse_jwt_claims(&tokens.access_token);
    let claims = [id.as_ref(), access.as_ref()];
    // Codex derives workspace identity from the namespaced auth claim, never
    // organization membership. Keep flat aliases only for legacy compatibility,
    // after checking the authoritative claim in both tokens (ID token first).
    claims
        .iter()
        .flatten()
        .find_map(|claims| {
            claims
                .openai_auth
                .as_ref()?
                .chatgpt_account_id
                .as_deref()
                .filter(|id| !id.trim().is_empty())
        })
        .or_else(|| {
            claims.iter().flatten().find_map(|claims| {
                claims
                    .chatgpt_account_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .or_else(|| {
                        claims
                            .openai_chatgpt_account_id
                            .as_deref()
                            .filter(|id| !id.trim().is_empty())
                    })
            })
        })
        .map(str::to_owned)
}
