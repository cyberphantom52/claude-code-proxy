use serde_json::{Map, Value};
use std::fs::{self, File, create_dir_all};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::logging::REDACT_KEYS;
use crate::paths;
use crate::providers::codex::translate::reasoning_signature::is_proxy_reasoning_signature;

#[derive(Debug)]
pub struct TrafficCapture {
    root: PathBuf,
    artifact_counter: AtomicUsize,
    event_counter: AtomicUsize,
}

pub const MAX_SSE_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_STREAM_CAPTURE_EVENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_STREAM_CAPTURE_EVENTS: usize = 1_024;
pub const MAX_STREAM_CAPTURE_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct TrafficCaptureOptions {
    pub req_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub provider: Option<String>,
}

pub fn traffic_capture_enabled() -> bool {
    matches!(
        std::env::var("CCP_TRAFFIC_LOG").as_deref(),
        Ok("1" | "true" | "yes")
    )
}

pub fn create_traffic_capture(opts: TrafficCaptureOptions) -> Option<TrafficCapture> {
    if !traffic_capture_enabled() {
        return None;
    }
    let state_root = paths::state_dir()
        .join("traffic")
        .join(sanitize_path_part(
            opts.session_id.as_deref().unwrap_or("no-session"),
        ))
        .join(format!(
            "{:06}-{}-{}",
            opts.session_seq.unwrap_or(0),
            sanitize_path_part(opts.provider.as_deref().unwrap_or("unknown-provider")),
            sanitize_path_part(&opts.req_id),
        ));

    Some(TrafficCapture {
        root: state_root,
        artifact_counter: AtomicUsize::new(0),
        event_counter: AtomicUsize::new(0),
    })
}

impl TrafficCapture {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn write_json(&self, name: &str, value: &Value) {
        let value = redact_traffic(value);
        let payload = serde_json::to_string_pretty(&value)
            .unwrap_or_else(|_| "{}".to_string())
            .into_bytes();
        let path = self.next_artifact_path(name, true);
        let _ = write_bytes(path, &payload);
    }

    pub fn write_text(&self, name: &str, text: &str) {
        let file = if name.ends_with(".txt") {
            name.to_string()
        } else {
            format!("{name}.txt")
        };
        let path = self.next_artifact_path(&file, false);
        let _ = write_bytes(path, text.as_bytes());
    }

    pub fn write_bytes(&self, name: &str, value: &[u8]) {
        let path = self.next_artifact_path(name, false);
        let _ = write_bytes(path, value);
    }

    pub fn write_json_event(&self, name: &str, value: &Value) {
        let value = redact_traffic(value);
        let payload = serde_json::to_string_pretty(&value)
            .unwrap_or_else(|_| "{}".to_string())
            .into_bytes();
        let path = self.next_event_path(name);
        let _ = write_bytes(path, &payload);
    }

    fn next_artifact_path(&self, name: &str, ensure_ext_json: bool) -> PathBuf {
        let counter = self.artifact_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let file = if ensure_ext_json && !name.ends_with(".json") {
            format!("{name}.json")
        } else {
            name.to_string()
        };
        self.root
            .join(format!("{:03}-{}", counter, sanitize_path_part(&file)))
    }

    fn next_event_path(&self, name: &str) -> PathBuf {
        let counter = self.event_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let file = if !name.ends_with(".json") {
            format!("{name}.json")
        } else {
            name.to_string()
        };
        self.root
            .join("events")
            .join(format!("{:06}-{}", counter, sanitize_path_part(&file)))
    }
}

fn write_bytes(path: PathBuf, value: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
        set_mode(parent, 0o700);
    }
    let mut out = File::create(&path)?;
    out.write_all(value)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = out.metadata()?.permissions();
        perm.set_mode(0o600);
        let _ = fs::set_permissions(&path, perm);
    }
    Ok(())
}

pub fn sanitize_path_part(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .take(160)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect();

    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

pub fn redact_traffic(value: &Value) -> Value {
    redact_traffic_with_depth(value, 0)
}

fn redact_traffic_with_depth(value: &Value, depth: u16) -> Value {
    if depth > 100 {
        return Value::String("[depth-limit]".to_string());
    }

    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                let normalized = key.to_lowercase();
                if normalized == "image_url" {
                    // OpenAI `input_image` parts carry data URLs; never
                    // persist the payload in traffic captures.
                    out.insert(key.clone(), redact_traffic_value(value));
                } else if normalized == "data" && looks_like_image_source(map) {
                    // Anthropic image blocks carry raw base64 under
                    // `source.data`; redact it for the same reason.
                    out.insert(key.clone(), redact_traffic_value(value));
                } else if normalized == "encrypted_content" {
                    // Codex `compaction` and `reasoning` items carry an opaque
                    // blob that replays a whole conversation's context; it is
                    // bulk payload, not debug signal, so only its size stays.
                    out.insert(key.clone(), redact_traffic_value(value));
                } else if normalized == "signature"
                    && value.as_str().is_some_and(is_proxy_reasoning_signature)
                {
                    // The Codex translator replays reasoning as an Anthropic
                    // thinking signature of the form
                    // `ccp:codex:v1:<encoded-id>:<encrypted_content>`, which
                    // carries the same opaque conversation handle as the
                    // `encrypted_content` key. Only proxy-owned signatures are
                    // touched; foreign ones (for example, real Anthropic
                    // signatures) are preserved.
                    out.insert(key.clone(), redact_traffic_value(value));
                } else if REDACT_KEYS.contains(&normalized.as_str())
                    || matches!(
                        normalized.as_str(),
                        "token"
                            | "bearer_token"
                            | "oauth_token"
                            | "oauth_access_token"
                            | "oauth_refresh_token"
                            | "client_secret"
                            | "secret"
                            | "password"
                            | "email"
                            | "user_id"
                            | "account_id"
                            | "identity"
                            | "identity_id"
                            | "subject"
                            | "sub"
                    )
                {
                    out.insert(key.clone(), redact_traffic_value(value));
                } else {
                    out.insert(key.clone(), redact_traffic_with_depth(value, depth + 1));
                }
            }
            Value::Object(out)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| redact_traffic_with_depth(value, depth + 1))
                .collect(),
        ),
        Value::String(text) if is_data_url(text) => {
            Value::String(format!("[redacted data-url len={}]", text.len()))
        }
        _ => value.clone(),
    }
}

fn is_data_url(text: &str) -> bool {
    text.as_bytes()
        .get(..11)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"data:image/"))
        && text
            .as_bytes()
            .windows(8)
            .any(|part| part.eq_ignore_ascii_case(b";base64,"))
}

/// An object shaped like an Anthropic image source: `type: "base64"` and
/// either an image `media_type` or none at all. Used to redact only image
/// payloads, not arbitrary `data` keys. Missing `media_type` is treated as
/// an image: the capture is written before translation, so a malformed block
/// that the translator would reject must still not persist its payload.
fn looks_like_image_source(map: &Map<String, Value>) -> bool {
    if !map
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|source_type| source_type.eq_ignore_ascii_case("base64"))
    {
        return false;
    }
    match map.get("media_type").and_then(Value::as_str) {
        Some(media_type) => media_type.to_ascii_lowercase().starts_with("image/"),
        None => true,
    }
}

fn redact_traffic_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(format!("[redacted len={}]", s.len())),
        // Structured values (e.g. `{url: ...}` image_url) keep their
        // shape so captures stay parseable; only the payload leaves.
        Value::Object(_) | Value::Array(_) => redact_traffic(value),
        _ => Value::String("[redacted]".to_string()),
    }
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(mode);
            let _ = fs::set_permissions(path, perm);
        }
    }
}
