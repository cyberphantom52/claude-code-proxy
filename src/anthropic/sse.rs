use std::borrow::Cow;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

pub fn parse_sse_events(input: &[u8]) -> Vec<SseEvent> {
    parse_sse_events_from(input, true)
}

pub(crate) fn parse_sse_events_from(input: &[u8], stream_start: bool) -> Vec<SseEvent> {
    let text = String::from_utf8_lossy(input);
    sse_events_from(&text, stream_start)
        .map(|event| SseEvent {
            event: event.event.map(str::to_owned),
            data: event.data.into_owned(),
        })
        .collect()
}

pub(crate) struct BorrowedSseEvent<'a> {
    pub event: Option<&'a str>,
    pub data: Cow<'a, str>,
}

pub(crate) fn sse_events(text: &str) -> impl Iterator<Item = BorrowedSseEvent<'_>> {
    sse_events_from(text, true)
}

pub(crate) fn sse_events_from(
    mut text: &str,
    stream_start: bool,
) -> impl Iterator<Item = BorrowedSseEvent<'_>> {
    if stream_start {
        text = text.strip_prefix('\u{feff}').unwrap_or(text);
    }
    let mut event = None;
    let mut data: Option<Cow<'_, str>> = None;
    std::iter::from_fn(move || {
        while !text.is_empty() {
            let end = text.find(['\r', '\n']).unwrap_or(text.len());
            let line = &text[..end];
            let rest = &text[end..];
            text = rest
                .strip_prefix("\r\n")
                .or_else(|| rest.strip_prefix(['\r', '\n']))
                .unwrap_or(rest);
            if line.is_empty() {
                let previous_event = event.take();
                if let Some(data) = data.take() {
                    return Some(BorrowedSseEvent {
                        event: previous_event,
                        data,
                    });
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (key, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.trim_start_matches(' ');
            match key {
                "event" => {
                    let previous_event = event.replace(value);
                    if let Some(data) = data.take() {
                        return Some(BorrowedSseEvent {
                            event: previous_event,
                            data,
                        });
                    }
                }
                "data" => match &mut data {
                    Some(data) => {
                        let data = data.to_mut();
                        data.push('\n');
                        data.push_str(value);
                    }
                    None => data = Some(Cow::Borrowed(value)),
                },
                _ => {}
            }
        }
        data.take().map(|data| BorrowedSseEvent {
            event: event.take(),
            data,
        })
    })
}

pub fn encode_sse_event(event: Option<&str>, data: &str) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(event) = event {
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(event.as_bytes());
        out.push(b'\n');
    }
    for line in data.split('\n') {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out.push(b'\n');
    out
}

pub(crate) fn append_json_sse_event(out: &mut Vec<u8>, event: &str, data: &serde_json::Value) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    serde_json::to_writer(&mut *out, data).expect("JSON value serializes into a byte buffer");
    out.extend_from_slice(b"\n\n");
}
