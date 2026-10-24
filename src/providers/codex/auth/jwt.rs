use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    organizations: Option<Vec<OrgClaim>>,
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
struct OrgClaim {
    id: String,
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

fn extract_account_id_from_claims(claims: &IdTokenClaims) -> Option<String> {
    claims
        .chatgpt_account_id
        .clone()
        .or_else(|| claims.openai_auth.as_ref()?.chatgpt_account_id.clone())
        .or_else(|| claims.openai_chatgpt_account_id.clone())
        .or_else(|| claims.organizations.as_ref()?.first()?.id.clone().into())
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
    if let Some(ref id_token) = tokens.id_token
        && let Some(claims) = parse_jwt_claims(id_token)
        && let Some(account_id) = extract_account_id_from_claims(&claims)
    {
        return Some(account_id);
    }
    let claims = parse_jwt_claims(&tokens.access_token)?;
    extract_account_id_from_claims(&claims)
}
