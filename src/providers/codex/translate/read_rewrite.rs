use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

const MAX_REWRITE_NOTES: usize = 4_096;
const READ_OFFSET_REWRITE_THRESHOLD: i64 = 1_000_000;
const BUFFERED_READ_REPAIR_TRAILING_WHITESPACE_BYTES: usize = 1_024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOffsetRewrite {
    pub offset: i64,
    pub file_path: Option<String>,
}

#[derive(Debug, Default)]
struct RewriteStore {
    order: VecDeque<String>,
    entries: HashMap<String, ReadOffsetRewrite>,
}

static READ_OFFSET_REWRITES: LazyLock<Mutex<RewriteStore>> =
    LazyLock::new(|| Mutex::new(RewriteStore::default()));

pub fn sanitize_read_args(name: &str, args: &str, call_id: Option<&str>) -> String {
    if name != "Read" || args.is_empty() {
        return args.to_string();
    }

    let parsed: Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return args.to_string(),
    };
    sanitize_parsed_read_args(name, args, Cow::Owned(parsed), call_id).into_owned()
}

pub(crate) fn sanitize_parsed_read_args<'a>(
    name: &str,
    args: &'a str,
    mut parsed: Cow<'_, Value>,
    call_id: Option<&str>,
) -> Cow<'a, str> {
    let has_empty_pages = parsed.get("pages").and_then(Value::as_str) == Some("");
    let offset = parsed
        .get("offset")
        .and_then(Value::as_i64)
        .filter(|offset| *offset >= READ_OFFSET_REWRITE_THRESHOLD);
    if name != "Read" || !(has_empty_pages || offset.is_some()) {
        return Cow::Borrowed(args);
    }
    let Value::Object(sanitized) = parsed.to_mut() else {
        return Cow::Borrowed(args);
    };
    if has_empty_pages {
        sanitized.remove("pages");
    }

    if let Some(offset) = offset {
        sanitized.remove("offset");
        if let Some(call_id) = call_id.filter(|id| !id.is_empty()) {
            record_read_offset_rewrite(
                call_id,
                ReadOffsetRewrite {
                    offset,
                    file_path: sanitized
                        .get("file_path")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                },
            );
        }
    }

    Cow::Owned(serde_json::to_string(sanitized).unwrap_or_else(|_| args.to_string()))
}

pub(crate) fn repair_whitespace_stalled_read_args(
    name: &str,
    args: &str,
    call_id: Option<&str>,
) -> Option<String> {
    if name != "Read" {
        return None;
    }
    let trimmed = args.trim_end();
    let trailing_whitespace = args.len().saturating_sub(trimmed.len());
    if trailing_whitespace < BUFFERED_READ_REPAIR_TRAILING_WHITESPACE_BYTES {
        return None;
    }
    parse_read_args_candidate(trimmed, call_id).or_else(|| {
        let with_brace = format!("{trimmed}}}");
        parse_read_args_candidate(&with_brace, call_id)
    })
}

fn parse_read_args_candidate(args: &str, call_id: Option<&str>) -> Option<String> {
    let parsed: Value = serde_json::from_str(args).ok()?;
    if !is_valid_read_args(&parsed) {
        return None;
    }
    let args = serde_json::to_string(&parsed).ok()?;
    Some(sanitize_parsed_read_args("Read", &args, Cow::Owned(parsed), call_id).into_owned())
}

fn is_valid_read_args(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    for key in obj.keys() {
        if !matches!(key.as_str(), "file_path" | "offset" | "limit" | "pages") {
            return false;
        }
    }
    let Some(file_path) = obj.get("file_path").and_then(Value::as_str) else {
        return false;
    };
    if file_path.is_empty() {
        return false;
    }
    if let Some(offset) = obj.get("offset").and_then(Value::as_i64)
        && offset < 0
    {
        return false;
    }
    if let Some(limit) = obj.get("limit").and_then(Value::as_i64)
        && limit <= 0
    {
        return false;
    }
    if obj.get("offset").is_some_and(|value| !value.is_i64()) {
        return false;
    }
    if obj.get("limit").is_some_and(|value| !value.is_i64()) {
        return false;
    }
    if obj.get("pages").is_some_and(|value| !value.is_string()) {
        return false;
    }
    true
}

pub fn read_offset_rewrite(call_id: &str) -> Option<ReadOffsetRewrite> {
    READ_OFFSET_REWRITES
        .lock()
        .ok()
        .and_then(|store| store.entries.get(call_id).cloned())
}

fn record_read_offset_rewrite(call_id: &str, note: ReadOffsetRewrite) {
    let Ok(mut store) = READ_OFFSET_REWRITES.lock() else {
        return;
    };

    if !store.entries.contains_key(call_id) {
        store.order.push_back(call_id.to_string());
    }
    store.entries.insert(call_id.to_string(), note);

    while store.entries.len() > MAX_REWRITE_NOTES {
        let Some(oldest) = store.order.pop_front() else {
            break;
        };
        store.entries.remove(&oldest);
    }
}
