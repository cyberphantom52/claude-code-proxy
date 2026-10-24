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

fn extract_request_path_and_query(
    stream: &mut TcpStream,
    deadline: std::time::Instant,
) -> Option<(String, String)> {
    const MAX_HEADERS: usize = 16 * 1024;
    let deadline = deadline.min(std::time::Instant::now() + Duration::from_secs(5));
    let mut request = Vec::new();
    let mut buf = [0; 1024];
    let header_end = loop {
        let remaining = deadline.checked_duration_since(std::time::Instant::now())?;
        if remaining.is_zero() || request.len() == MAX_HEADERS {
            return None;
        }
        stream.set_read_timeout(Some(remaining)).ok()?;
        let capacity = buf.len().min(MAX_HEADERS - request.len());
        let n = match stream.read(&mut buf[..capacity]) {
            Ok(0) => return None,
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        request.extend_from_slice(&buf[..n]);
        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break end;
        }
    };
    let request = std::str::from_utf8(&request[..header_end]).ok()?;
    let mut lines = request.split("\r\n");
    let mut parts = lines.next()?.split(' ');
    if parts.next()? != "GET" {
        return None;
    }
    let path_and_query = parts.next()?;
    if !matches!(parts.next()?, "HTTP/1.0" | "HTTP/1.1")
        || parts.next().is_some()
        || !path_and_query.starts_with('/')
        || path_and_query.contains('#')
        || path_and_query.bytes().any(|byte| byte.is_ascii_control())
    {
        return None;
    }
    for line in lines {
        let (name, value) = line.split_once(':')?;
        http::HeaderName::from_bytes(name.as_bytes()).ok()?;
        http::HeaderValue::from_str(value.trim()).ok()?;
    }
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    Some((path.to_string(), query.to_string()))
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

    let (path, query) = match extract_request_path_and_query(stream, deadline) {
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

    let mut params = HashMap::new();
    let mut duplicate = false;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()).into_owned() {
        duplicate |= params.insert(key, value).is_some();
    }
    // Unrelated/stale callbacks must not terminate the pending login. In
    // particular, authenticate state before trusting a provider error.
    if duplicate || params.get("state").map(String::as_str) != Some(state) {
        write_response(stream, deadline, 400, "text/plain", "Invalid callback");
        return Ok(None);
    }

    if let Some(error) = params.get("error").filter(|error| !error.is_empty()) {
        let message = if error == "access_denied" {
            "OAuth authorization denied or cancelled"
        } else {
            "OAuth authorization failed"
        };
        write_response(stream, deadline, 400, "text/plain", message);
        anyhow::bail!("{message}");
    }

    let Some(code) = params.get("code").filter(|code| !code.is_empty()) else {
        write_response(stream, deadline, 400, "text/plain", "Invalid callback");
        return Ok(None);
    };

    match exchange_code_for_tokens_until(issuer, code, pkce, redirect_uri, deadline) {
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
            write_response(stream, deadline, 500, "text/plain", "Token exchange failed");
            Err(e)
        }
    }
}
