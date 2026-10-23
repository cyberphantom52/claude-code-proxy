use serde::Serialize;
use serde::ser::{SerializeMap, SerializeSeq};
use serde_json::{Map, Value};
use std::fs::{self, File, create_dir_all};
use std::io::{self, BufWriter, Write};
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
        let path = self.next_artifact_path(name, true);
        let _ = write_json(path, &redacted(value));
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
        self.write_json_event_with_type(name, None, value);
    }

    pub fn write_json_event_with_type(&self, name: &str, event: Option<&str>, value: &Value) {
        let path = self.next_event_path(name);
        let _ = write_json(
            path,
            &Redacted {
                value,
                depth: 0,
                event,
            },
        );
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

fn capture_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
        set_mode(parent, 0o700);
    }
    let file = File::create(path)?;
    set_mode(path, 0o600);
    Ok(file)
}

fn write_bytes(path: PathBuf, value: &[u8]) -> io::Result<()> {
    capture_file(&path)?.write_all(value)
}

fn write_json(path: PathBuf, value: &impl Serialize) -> io::Result<()> {
    let mut out = BufWriter::new(capture_file(&path)?);
    serde_json::to_writer_pretty(&mut out, value).map_err(io::Error::other)?;
    out.flush()
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
    serde_json::to_value(redacted(value)).expect("redacted JSON value serializes")
}

pub fn redacted(value: &Value) -> impl Serialize + '_ {
    Redacted {
        value,
        depth: 0,
        event: None,
    }
}

struct Redacted<'a> {
    value: &'a Value,
    depth: u16,
    event: Option<&'a str>,
}

impl Serialize for Redacted<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.depth > 100 {
            return serializer.serialize_str("[depth-limit]");
        }
        match self.value {
            Value::Object(fields) => {
                let event = self.event.filter(|_| !fields.contains_key("_sse_event"));
                let mut out =
                    serializer.serialize_map(Some(fields.len() + usize::from(event.is_some())))?;
                let mut event_pending = event;
                for (key, value) in fields {
                    if key.as_str() > "_sse_event"
                        && let Some(event) = event_pending.take()
                    {
                        out.serialize_entry("_sse_event", &RedactedText(event))?;
                    }
                    let normalized = key.to_lowercase();
                    if should_redact_field(fields, &normalized, value) {
                        out.serialize_entry(key, &RedactedField(value))?;
                    } else {
                        out.serialize_entry(
                            key,
                            &Redacted {
                                value,
                                depth: self.depth + 1,
                                event: None,
                            },
                        )?;
                    }
                }
                if let Some(event) = event_pending {
                    out.serialize_entry("_sse_event", &RedactedText(event))?;
                }
                out.end()
            }
            Value::Array(values) => {
                let mut out = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    out.serialize_element(&Redacted {
                        value,
                        depth: self.depth + 1,
                        event: None,
                    })?;
                }
                out.end()
            }
            Value::String(text) => RedactedText(text).serialize(serializer),
            value => value.serialize(serializer),
        }
    }
}

fn should_redact_field(fields: &Map<String, Value>, normalized: &str, value: &Value) -> bool {
    normalized == "image_url"
        || (normalized == "data" && looks_like_image_source(fields))
        || normalized == "encrypted_content"
        || (normalized == "signature" && value.as_str().is_some_and(is_proxy_reasoning_signature))
        || REDACT_KEYS.contains(&normalized)
        || matches!(
            normalized,
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
}

struct RedactedText<'a>(&'a str);

impl Serialize for RedactedText<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if is_data_url(self.0) {
            serializer.collect_str(&format_args!("[redacted data-url len={}]", self.0.len()))
        } else {
            serializer.serialize_str(self.0)
        }
    }
}

struct RedactedField<'a>(&'a Value);

impl Serialize for RedactedField<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::String(text) => {
                serializer.collect_str(&format_args!("[redacted len={}]", text.len()))
            }
            Value::Object(_) | Value::Array(_) => redacted(self.0).serialize(serializer),
            _ => serializer.serialize_str("[redacted]"),
        }
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
