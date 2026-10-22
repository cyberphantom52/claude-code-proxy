use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexFailureKind {
    RateLimit,
    Overloaded,
    Transient,
    Permanent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexStreamEventKind {
    Control,
    Structural,
    Semantic,
    TerminalSuccess,
    TerminalFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexEventFailure {
    pub kind: CodexFailureKind,
    pub explicit_status: Option<u16>,
    pub status: u16,
    pub message: String,
    pub retry_after: Option<String>,
}

impl CodexEventFailure {
    pub fn retryable(&self) -> bool {
        !matches!(self.kind, CodexFailureKind::Permanent)
    }

    pub fn client_status(&self) -> u16 {
        self.status
    }

    pub fn client_error_type(&self) -> &'static str {
        match self.client_status() {
            400 | 422 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            413 => "request_too_large",
            429 => "rate_limit_error",
            529 => "overloaded_error",
            _ => "api_error",
        }
    }
}

pub(crate) fn classify_stream_event(payload: &Value) -> CodexStreamEventKind {
    if classify_event_failure(payload).is_some() {
        return CodexStreamEventKind::TerminalFailure;
    }
    match payload.get("type").and_then(Value::as_str) {
        Some("response.completed" | "response.done") => CodexStreamEventKind::TerminalSuccess,
        Some(
            "keepalive"
            | "response.created"
            | "response.in_progress"
            | "codex.rate_limits"
            | "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed",
        ) => CodexStreamEventKind::Control,
        Some(
            "response.reasoning_summary_text.delta"
            | "response.output_text.delta"
            | "response.function_call_arguments.delta",
        ) if payload
            .get("delta")
            .and_then(Value::as_str)
            .is_some_and(|delta| !delta.is_empty()) =>
        {
            CodexStreamEventKind::Semantic
        }
        Some("response.output_item.done")
            if matches!(
                payload.pointer("/item/type").and_then(Value::as_str),
                Some("function_call" | "web_search_call")
            ) =>
        {
            CodexStreamEventKind::Semantic
        }
        _ => CodexStreamEventKind::Structural,
    }
}

pub(crate) fn event_is_terminal(payload: &Value) -> bool {
    matches!(
        classify_stream_event(payload),
        CodexStreamEventKind::TerminalSuccess | CodexStreamEventKind::TerminalFailure
    )
}

pub(crate) fn event_is_success_terminal(payload: &Value) -> bool {
    classify_stream_event(payload) == CodexStreamEventKind::TerminalSuccess
}

pub(crate) fn event_error(payload: &Value) -> Option<&Value> {
    payload
        .get("error")
        .filter(|error| !error.is_null())
        .or_else(|| {
            payload
                .pointer("/response/error")
                .filter(|error| !error.is_null())
        })
}

pub(crate) fn response_is_incomplete_terminal(payload: &Value) -> bool {
    let event_type = payload.get("type").and_then(Value::as_str);
    let incomplete_details = payload.pointer("/response/incomplete_details");
    matches!(
        event_type,
        Some("response.completed" | "response.incomplete" | "response.done")
    ) && (event_type == Some("response.incomplete")
        || payload.pointer("/response/status").and_then(Value::as_str) == Some("incomplete")
        || incomplete_details.is_some_and(|details| !details.is_null()))
}

pub(crate) fn classify_event_failure(payload: &Value) -> Option<CodexEventFailure> {
    let event_type = payload.get("type").and_then(Value::as_str)?;
    let response_status = payload.pointer("/response/status").and_then(Value::as_str);
    if event_type == "codex.rate_limits" {
        return None;
    }
    if response_is_incomplete_terminal(payload) {
        let reason = payload
            .pointer("/response/incomplete_details/reason")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Some(CodexEventFailure {
            kind: CodexFailureKind::Transient,
            explicit_status: None,
            status: 503,
            message: format!("Incomplete response returned, reason: {reason}"),
            retry_after: None,
        });
    }
    let error = event_error(payload);
    if !matches!(event_type, "response.failed" | "response.error" | "error")
        && response_status != Some("failed")
        && error.is_none()
    {
        return None;
    }

    let explicit_status = numeric_status(payload)
        .or_else(|| {
            error
                .and_then(|value| value.get("status"))
                .and_then(Value::as_u64)
        })
        .and_then(|status| u16::try_from(status).ok());
    let code = error
        .and_then(|value| value.get("code"))
        .and_then(Value::as_str);
    let error_type = error
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str);
    let raw_message = error
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str);
    let message = match (code, raw_message) {
        (Some("cyber_policy"), None | Some("")) => {
            "This request has been flagged for possible cybersecurity risk.".to_string()
        }
        (Some("cyber_policy"), Some(message)) if message.trim().is_empty() => {
            "This request has been flagged for possible cybersecurity risk.".to_string()
        }
        (Some("invalid_prompt" | "bio_policy"), None) => "Invalid request.".to_string(),
        (_, Some(message)) => message.to_string(),
        _ => "Upstream error".to_string(),
    };
    let lower = message.to_ascii_lowercase();
    let context_window = code == Some("context_length_exceeded")
        || lower.contains("context window")
        || lower.contains("context length exceeded");

    let kind = if context_window
        || matches!(
            code,
            Some(
                "context_length_exceeded"
                    | "insufficient_quota"
                    | "usage_not_included"
                    | "cyber_policy"
                    | "invalid_prompt"
                    | "bio_policy"
            )
        ) {
        CodexFailureKind::Permanent
    } else if matches!(code, Some("server_is_overloaded" | "slow_down")) {
        CodexFailureKind::Overloaded
    } else if explicit_status == Some(429) || lower.contains("rate limit") {
        CodexFailureKind::RateLimit
    } else if explicit_status == Some(529)
        || code == Some("overloaded_error")
        || error_type == Some("overloaded_error")
        || lower.contains("overloaded")
    {
        CodexFailureKind::Overloaded
    } else if explicit_status.is_some_and(|status| matches!(status, 500 | 502 | 503 | 504))
        || matches!(
            code,
            Some("server_error" | "internal_server_error" | "internal_error")
        )
        || matches!(
            error_type,
            Some("server_error" | "internal_server_error" | "internal_error")
        )
        || retryable_message(&lower)
    {
        CodexFailureKind::Transient
    } else if explicit_status.is_some() || matches!(event_type, "response.error" | "error") {
        CodexFailureKind::Permanent
    } else {
        // Native Codex treats an unclassified response.failed event as
        // retryable instead of silently converting it into a successful turn.
        CodexFailureKind::Transient
    };
    let status = if context_window {
        413
    } else {
        explicit_status.unwrap_or(match code {
            Some("cyber_policy" | "invalid_prompt" | "bio_policy") => 400,
            Some("insufficient_quota") => 429,
            Some("usage_not_included") => 403,
            _ => match kind {
                CodexFailureKind::RateLimit => 429,
                CodexFailureKind::Overloaded => 529,
                CodexFailureKind::Transient => 503,
                CodexFailureKind::Permanent => 500,
            },
        })
    };
    let retry_after = error
        .and_then(|value| value.get("retry_after"))
        .and_then(scalar_string_value)
        .or_else(|| {
            error
                .and_then(|value| value.get("retry_after_seconds"))
                .and_then(scalar_string_value)
        })
        .or_else(|| scalar_string(payload.get("retry_after_seconds")))
        .or_else(|| scalar_string(payload.pointer("/headers/retry-after")))
        .or_else(|| scalar_string(payload.pointer("/headers/Retry-After")));

    Some(CodexEventFailure {
        kind,
        explicit_status,
        status,
        message,
        retry_after,
    })
}

/// Which known upstream quota window ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexLimitWindow {
    FiveHour,
    SevenDay,
}

impl CodexLimitWindow {
    pub(crate) fn claim(self) -> &'static str {
        match self {
            CodexLimitWindow::FiveHour => "five_hour",
            CodexLimitWindow::SevenDay => "seven_day",
        }
    }
}

/// Quota exhaustion reported by Codex, together with the reset clock upstream
/// sends alongside it. Unlike a transient rate limit this does not clear on a
/// backoff, so the reset time is the only useful thing to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexUsageLimit {
    pub message: String,
    pub resets_at: Option<u64>,
    pub window: Option<CodexLimitWindow>,
}

/// Recognise the `usage_limit_reached` error Codex emits when a subscription
/// window is spent. Upstream puts the clock both in the error body
/// (`resets_at`, `resets_in_seconds`) and in `X-Codex-*` headers mirrored into
/// the event payload.
pub(crate) fn usage_limit_from_event(payload: &Value) -> Option<CodexUsageLimit> {
    usage_limit_from_event_with_headers(payload, &[])
}

/// Read a quota exhaustion event while consulting the current HTTP response
/// headers for metadata that is absent from the event payload.
pub(crate) fn usage_limit_from_event_with_headers(
    payload: &Value,
    headers: &[(String, String)],
) -> Option<CodexUsageLimit> {
    if !matches!(
        payload.get("type").and_then(Value::as_str),
        Some("response.failed" | "response.error" | "error")
    ) {
        return None;
    }
    usage_limit_from_payload_with_headers(payload, headers)
}

/// Read quota exhaustion from either a JSON error response or an SSE event
/// body. HTTP response headers are included because Codex does not always
/// mirror its quota clocks into the error payload.
pub(crate) fn usage_limit_from_response(
    body: &[u8],
    headers: &[(String, String)],
) -> Option<CodexUsageLimit> {
    let direct = serde_json::from_slice::<Value>(body).ok().into_iter();
    let events = crate::anthropic::sse::parse_sse_events(body)
        .into_iter()
        .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok());
    direct
        .chain(events)
        .find_map(|payload| usage_limit_from_payload_with_headers(&payload, headers))
}

fn usage_limit_from_payload_with_headers(
    payload: &Value,
    response_headers: &[(String, String)],
) -> Option<CodexUsageLimit> {
    let error = event_error(payload)?;
    if error.get("type").and_then(Value::as_str) != Some("usage_limit_reached") {
        return None;
    }

    let resets_at = numeric_value(error.get("resets_at"));
    let resets_in_seconds = numeric_value(error.get("resets_in_seconds"));
    let limiting_prefix =
        limiting_window_prefix(payload, response_headers, resets_in_seconds, resets_at);
    let resets_at = resets_at.or_else(|| {
        let prefix = limiting_prefix?;
        header_number(
            payload,
            response_headers,
            &format!("X-Codex-{prefix}-Reset-At"),
        )
    });
    let window = limiting_prefix.and_then(|prefix| {
        match header_number(
            payload,
            response_headers,
            &format!("X-Codex-{prefix}-Window-Minutes"),
        ) {
            Some(300) => Some(CodexLimitWindow::FiveHour),
            Some(10_080) => Some(CodexLimitWindow::SevenDay),
            _ => None,
        }
    });

    Some(CodexUsageLimit {
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Usage limit reached")
            .to_string(),
        resets_at,
        window,
    })
}

/// Codex sends the clock for both windows on every limit error, so the one that
/// actually ran out is the one whose countdown or reset epoch matches the
/// error's own clock.
fn limiting_window_prefix(
    payload: &Value,
    response_headers: &[(String, String)],
    resets_in_seconds: Option<u64>,
    resets_at: Option<u64>,
) -> Option<&'static str> {
    let by_countdown = resets_in_seconds.and_then(|actual| {
        closest_window(
            actual,
            header_number(
                payload,
                response_headers,
                "X-Codex-Primary-Reset-After-Seconds",
            ),
            header_number(
                payload,
                response_headers,
                "X-Codex-Secondary-Reset-After-Seconds",
            ),
        )
    });
    let by_epoch = resets_at.and_then(|actual| {
        closest_window(
            actual,
            header_number(payload, response_headers, "X-Codex-Primary-Reset-At"),
            header_number(payload, response_headers, "X-Codex-Secondary-Reset-At"),
        )
    });
    by_countdown.or(by_epoch).or_else(|| {
        match (
            window_headers_present(payload, response_headers, "Primary"),
            window_headers_present(payload, response_headers, "Secondary"),
        ) {
            (true, false) => Some("Primary"),
            (false, true) => Some("Secondary"),
            _ => None,
        }
    })
}

fn closest_window(
    actual: u64,
    primary: Option<u64>,
    secondary: Option<u64>,
) -> Option<&'static str> {
    match (primary, secondary) {
        (Some(primary), Some(secondary)) => {
            match (actual.abs_diff(primary), actual.abs_diff(secondary)) {
                (primary_distance, secondary_distance) if primary_distance < secondary_distance => {
                    Some("Primary")
                }
                (primary_distance, secondary_distance) if secondary_distance < primary_distance => {
                    Some("Secondary")
                }
                _ => None,
            }
        }
        (Some(_), None) => Some("Primary"),
        (None, Some(_)) => Some("Secondary"),
        (None, None) => None,
    }
}

fn window_headers_present(
    payload: &Value,
    response_headers: &[(String, String)],
    prefix: &str,
) -> bool {
    ["Reset-After-Seconds", "Reset-At", "Window-Minutes"]
        .into_iter()
        .any(|suffix| {
            header_number(
                payload,
                response_headers,
                &format!("X-Codex-{prefix}-{suffix}"),
            )
            .is_some()
        })
}

fn header_number(
    payload: &Value,
    response_headers: &[(String, String)],
    name: &str,
) -> Option<u64> {
    payload
        .get("headers")
        .and_then(Value::as_object)
        .and_then(|headers| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .and_then(|(_, value)| numeric_value(Some(value)))
        })
        .or_else(|| {
            response_headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .and_then(|(_, value)| value.parse().ok())
        })
}

fn numeric_value(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64(),
        Value::String(raw) => raw.parse().ok(),
        _ => None,
    }
}

pub(crate) fn first_retryable_failure(body: &[u8]) -> Option<CodexEventFailure> {
    first_event_failure(body).filter(CodexEventFailure::retryable)
}

pub(crate) fn first_event_failure(body: &[u8]) -> Option<CodexEventFailure> {
    for event in crate::anthropic::sse::parse_sse_events(body) {
        if event.data == "[DONE]" {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        if let Some(failure) = classify_event_failure(&payload) {
            return Some(failure);
        }
    }
    None
}

pub(crate) fn numeric_status(payload: &Value) -> Option<u64> {
    payload
        .get("status")
        .and_then(Value::as_u64)
        .or_else(|| payload.get("status_code").and_then(Value::as_u64))
}

fn scalar_string(value: Option<&Value>) -> Option<String> {
    value.and_then(scalar_string_value)
}

fn scalar_string_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn retryable_message(message: &str) -> bool {
    [
        "server error",
        "internal server error",
        "service unavailable",
        "bad gateway",
        "gateway timeout",
        "temporarily unavailable",
        "you can retry your request",
        "socket connection was closed unexpectedly",
        "connection closed unexpectedly",
        "operation timed out",
        "connection reset",
        "connection closed",
        "timed out",
        "timeout",
        "econnreset",
        "epipe",
        "etimedout",
        "und_err_socket",
        "fetch failed",
        "unexpected eof",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}
