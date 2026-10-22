use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use super::constants::{ISSUER, OAUTH_PORT};
use super::jwt::TokenResponse;
use super::pkce::{
    PkceCodes, build_authorize_url, exchange_code_for_tokens_until, generate_pkce, generate_state,
};

const BROWSER_LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

pub struct BrowserLoginConfig {
    pub issuer: String,
    pub port: u16,
    pub timeout: Duration,
}

impl BrowserLoginConfig {
    pub fn new(issuer: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            port: OAUTH_PORT,
            timeout: BROWSER_LOGIN_TIMEOUT,
        }
    }

    fn redirect_uri(&self) -> String {
        format!("http://localhost:{}/auth/callback", self.port)
    }
}

fn extract_request_path_and_query(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut buf = [0; 4096];
    let n = stream.read(&mut buf).ok()?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse first line: GET /path?query HTTP/1.1
    let first_line = request.lines().next()?;
    let mut parts = first_line.split_whitespace();
    let _method = parts.next()?;
    let path_and_query = parts.next()?;

    let mut split = path_and_query.splitn(2, '?');
    let path = split.next().unwrap_or("").to_string();
    let query = split.next().unwrap_or("").to_string();

    Some((path, query))
}

fn write_response(
    stream: &mut TcpStream,
    deadline: std::time::Instant,
    status: u16,
    content_type: &str,
    body: &str,
) {
    let Some(remaining) = deadline
        .checked_duration_since(std::time::Instant::now())
        .filter(|duration| !duration.is_zero())
    else {
        return;
    };
    if stream
        .set_write_timeout(Some(remaining.min(Duration::from_secs(5))))
        .is_err()
    {
        return;
    }
    let status_line = match status {
        200 => "200 OK",
        400 => "400 Bad Request",
        404 => "404 Not Found",
        500 => "500 Internal Server Error",
        _ => "500 Internal Server Error",
    };
    let response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------
// Main browser login flow
// ---------------------------------------------------------------------------

pub fn run_browser_login() -> Result<TokenResponse, anyhow::Error> {
    let config = BrowserLoginConfig::new(ISSUER);
    run_browser_login_with_config(&config)
}

pub fn run_browser_login_with_config(
    config: &BrowserLoginConfig,
) -> Result<TokenResponse, anyhow::Error> {
    let deadline = std::time::Instant::now()
        .checked_add(config.timeout)
        .ok_or_else(|| anyhow::anyhow!("OAuth timeout is too large"))?;
    let pkce = generate_pkce();
    let state = generate_state();
    let redirect_uri = config.redirect_uri();
    let auth_url = build_authorize_url(&config.issuer, &redirect_uri, &pkce, &state)?;

    let listener = TcpListener::bind(format!("127.0.0.1:{}", config.port))
        .map_err(|e| anyhow::anyhow!("Failed to bind to port {}: {e}", config.port))?;

    listener
        .set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("Failed to set non-blocking: {e}"))?;

    println!("Open this URL in your browser to authorize:\n\n  {auth_url}\n");

    loop {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("OAuth timeout");
        }

        match listener.accept() {
            Ok((mut stream, _)) => {
                match handle_callback_until(
                    &mut stream,
                    &config.issuer,
                    &redirect_uri,
                    &pkce,
                    &state,
                    deadline,
                ) {
                    // Ignore unrelated or malformed requests and keep waiting for
                    // the actual OAuth callback.
                    Ok(None) => continue,
                    Ok(Some(tokens)) => return Ok(tokens),
                    Err(e) => return Err(e),
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(
                    Duration::from_millis(100)
                        .min(deadline.saturating_duration_since(std::time::Instant::now())),
                );
            }
            Err(e) => {
                anyhow::bail!("Server error: {e}");
            }
        }
    }
}

fn handle_callback_until(
    stream: &mut TcpStream,
    issuer: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
    deadline: std::time::Instant,
) -> Result<Option<TokenResponse>, anyhow::Error> {
    stream.set_nonblocking(false)?;
    let remaining = deadline
        .checked_duration_since(std::time::Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| anyhow::anyhow!("OAuth timeout"))?;
    stream.set_read_timeout(Some(remaining.min(Duration::from_secs(5))))?;
    stream.set_write_timeout(Some(remaining.min(Duration::from_secs(5))))?;

    let (path, query) = match extract_request_path_and_query(stream) {
        Some(pair) => pair,
        None => {
            write_response(stream, deadline, 400, "text/plain", "Bad request");
            return Ok(None);
        }
    };

    if path != "/auth/callback" {
        write_response(stream, deadline, 404, "text/plain", "Not found");
        return Ok(None);
    }

    let params: HashMap<_, _> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();

    if let Some(error) = params.get("error") {
        write_response(
            stream,
            deadline,
            400,
            "text/plain",
            &format!("Auth failed: {error}"),
        );
        anyhow::bail!("{error}");
    }

    let code = match params.get("code") {
        Some(c) => c.clone(),
        None => {
            write_response(
                stream,
                deadline,
                400,
                "text/plain",
                "Auth failed: Invalid callback",
            );
            anyhow::bail!("Invalid callback");
        }
    };

    let received_state = params.get("state").cloned().unwrap_or_default();
    if received_state != state {
        write_response(
            stream,
            deadline,
            400,
            "text/plain",
            "Auth failed: Invalid callback",
        );
        anyhow::bail!("Invalid callback: state mismatch");
    }

    match exchange_code_for_tokens_until(issuer, &code, pkce, redirect_uri, deadline) {
        Ok(tokens) => {
            write_response(
                stream,
                deadline,
                200,
                "text/html",
                "<html><body><h1>Authorization Successful</h1><p>You can close this window.</p></body></html>",
            );
            Ok(Some(tokens))
        }
        Err(e) => {
            write_response(stream, deadline, 500, "text/plain", &e.to_string());
            Err(e)
        }
    }
}
