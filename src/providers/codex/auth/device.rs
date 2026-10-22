use std::time::{Duration, Instant};

use super::constants::{CLIENT_ID, DEVICE_POLL_SAFETY_MARGIN_MS, ISSUER};
use super::jwt::TokenResponse;

const MAX_DEVICE_POLL_WAIT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Device init response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DeviceInit {
    pub device_auth_id: String,
    pub user_code: String,
    #[serde(default)]
    pub interval: String,
}

// ---------------------------------------------------------------------------
// Device token poll response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DeviceTokenPoll {
    pub authorization_code: String,
    pub code_verifier: String,
}

// ---------------------------------------------------------------------------
// Device auth client
// ---------------------------------------------------------------------------

pub struct DeviceAuthClient {
    issuer: String,
    client: reqwest::blocking::Client,
    max_wait: Duration,
}

impl DeviceAuthClient {
    pub fn new() -> Self {
        Self::with_issuer(ISSUER)
    }

    pub fn with_issuer(issuer: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to create device OAuth client"),
            max_wait: MAX_DEVICE_POLL_WAIT,
        }
    }

    fn deadline(&self) -> Result<Instant, anyhow::Error> {
        Instant::now()
            .checked_add(self.max_wait)
            .ok_or_else(|| anyhow::anyhow!("Device auth timeout is too large"))
    }

    fn remaining(deadline: Instant) -> Result<Duration, anyhow::Error> {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .map(|duration| duration.min(Duration::from_secs(30)))
            .ok_or_else(|| anyhow::anyhow!("Device auth timed out"))
    }

    fn request_error(stage: &str, error: reqwest::Error) -> anyhow::Error {
        if error.is_timeout() {
            anyhow::anyhow!("Device auth timed out ({stage})")
        } else {
            anyhow::anyhow!("{stage}: {error}")
        }
    }

    pub fn init(&self) -> Result<DeviceInit, anyhow::Error> {
        self.init_until(self.deadline()?)
    }

    fn init_until(&self, deadline: Instant) -> Result<DeviceInit, anyhow::Error> {
        let body = serde_json::json!({ "client_id": CLIENT_ID });
        let resp = self
            .client
            .post(format!("{}/api/accounts/deviceauth/usercode", self.issuer))
            .timeout(Self::remaining(deadline)?)
            .json(&body)
            .send()
            .map_err(|e| Self::request_error("Device init network error", e))?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let text = resp.text().unwrap_or_default();
            anyhow::bail!("Device init failed: {status} {text}");
        }

        let init: DeviceInit = resp
            .json()
            .map_err(|e| Self::request_error("failed to parse device init response", e))?;
        Ok(init)
    }

    fn poll_token(
        &self,
        device_auth_id: &str,
        user_code: &str,
        deadline: Instant,
    ) -> Result<Option<DeviceTokenPoll>, anyhow::Error> {
        let body = serde_json::json!({
            "device_auth_id": device_auth_id,
            "user_code": user_code,
        });
        let resp = self
            .client
            .post(format!("{}/api/accounts/deviceauth/token", self.issuer))
            .timeout(Self::remaining(deadline)?)
            .json(&body)
            .send()
            .map_err(|e| Self::request_error("Device poll network error", e))?;

        let status = resp.status().as_u16();
        if resp.status().is_success() {
            let poll: DeviceTokenPoll = resp
                .json()
                .map_err(|e| Self::request_error("failed to parse device poll response", e))?;
            return Ok(Some(poll));
        }
        if status == 403 || status == 404 {
            return Ok(None);
        }
        let text = resp.text().unwrap_or_default();
        anyhow::bail!("Device poll failed: {status} {text}");
    }

    fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        deadline: Instant,
    ) -> Result<TokenResponse, anyhow::Error> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            (
                "redirect_uri",
                &format!("{}/deviceauth/callback", self.issuer),
            ),
            ("client_id", CLIENT_ID),
            ("code_verifier", code_verifier),
        ];
        let resp = self
            .client
            .post(format!("{}/oauth/token", self.issuer))
            .timeout(Self::remaining(deadline)?)
            .form(&form)
            .send()
            .map_err(|e| Self::request_error("Token exchange network error", e))?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let text = resp.text().unwrap_or_default();
            anyhow::bail!("Token exchange failed: {status} {text}");
        }

        let tokens: TokenResponse = resp
            .json()
            .map_err(|e| Self::request_error("failed to parse token exchange response", e))?;
        Ok(tokens)
    }

    fn parse_interval(interval: &str) -> Duration {
        let digits = interval.strip_prefix('+').unwrap_or(interval);
        let ms = if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
            let seconds = digits.bytes().fold(0_u64, |value, byte| {
                value
                    .saturating_mul(10)
                    .saturating_add(u64::from(byte - b'0'))
            });
            if seconds == 0 {
                5000
            } else {
                seconds.saturating_mul(1000)
            }
        } else if interval.parse::<i64>().is_ok_and(|value| value < 0) {
            1000
        } else {
            5000
        };
        Duration::from_millis(ms)
    }

    pub fn run(&self) -> Result<TokenResponse, anyhow::Error> {
        let deadline = self.deadline()?;
        let init = self.init_until(deadline)?;

        println!(
            "\nVisit: {}/codex/device\nEnter code: {}\n",
            self.issuer, init.user_code
        );

        let sleep_dur = Self::parse_interval(&init.interval)
            .saturating_add(Duration::from_millis(DEVICE_POLL_SAFETY_MARGIN_MS));

        loop {
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "Device auth timed out after {} seconds",
                    self.max_wait.as_secs()
                );
            }

            match self.poll_token(&init.device_auth_id, &init.user_code, deadline)? {
                Some(poll) => {
                    let tokens = self.exchange_code(
                        &poll.authorization_code,
                        &poll.code_verifier,
                        deadline,
                    )?;
                    return Ok(tokens);
                }
                None => {
                    std::thread::sleep(
                        sleep_dur.min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
            }
        }
    }
}

impl Default for DeviceAuthClient {
    fn default() -> Self {
        Self::new()
    }
}
