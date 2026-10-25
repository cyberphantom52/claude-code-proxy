//! Codex quota telemetry, re-exposed to the local client.
//!
//! Codex reports subscription quota in `x-codex-*` HTTP response headers and
//! `codex.rate_limits` events. These are re-exposed under Codex's header names;
//! private protocol/continuation headers are not quotas.
//!
//! Messages responses use a request-local scope; native Responses and Chat use
//! their own HTTP response headers. Missing telemetry stays absent, never borrowed
//! from another request/account. Telemetry arriving after the downstream response
//! head has been emitted cannot update that head.

use std::{
    future::Future,
    sync::{Arc, Mutex},
};

use axum::response::Response;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

/// Header suffixes that carry a window's numbers.
const WINDOW_SUFFIXES: [&str; 6] = [
    "-used-percent",
    "-window-minutes",
    "-reset-at",
    "-reset-after-seconds",
    "-limit-name",
    "-over-secondary-limit-percent",
];

/// Whole header names that describe the subscription rather than a window.
const PLAN_HEADERS: [&str; 2] = ["x-codex-plan-type", "x-codex-active-limit"];

#[derive(Clone, Default)]
pub struct Telemetry(Arc<Mutex<Vec<(HeaderName, HeaderValue)>>>);

tokio::task_local! {
    static SNAPSHOT: Telemetry;
}

/// Capture before spawning a worker that observes this operation's telemetry.
pub fn current() -> Option<Telemetry> {
    SNAPSHOT.try_with(Clone::clone).ok()
}

pub async fn with_telemetry<F: Future>(telemetry: Option<Telemetry>, future: F) -> F::Output {
    match telemetry {
        Some(telemetry) => SNAPSHOT.scope(telemetry, future).await,
        None => future.await,
    }
}

/// Scope one Messages operation, stamping only telemetry observed by its tasks.
pub async fn with_response_telemetry(future: impl Future<Output = Response>) -> Response {
    with_telemetry(Some(Telemetry::default()), async {
        stamp_response(future.await)
    })
    .await
}

/// True for a header that reports quota rather than protocol state. Codex sends
/// several limit families (`x-codex-*`, `x-codex-<limit id>-*`) and a few opaque
/// continuation headers that share the prefix; only the former are quota.
fn is_quota_header(name: &str) -> bool {
    if !name.starts_with("x-codex-") {
        return false;
    }
    PLAN_HEADERS.contains(&name)
        || matches!(
            name,
            "x-codex-credits-has-credits" | "x-codex-credits-unlimited" | "x-codex-credits-balance"
        )
        || WINDOW_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// The quota headers of an upstream response head.
pub fn telemetry_headers(headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    headers
        .iter()
        .filter(|(name, _)| is_quota_header(name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// A response head replaces the attempt; an event replaces one limit family.
pub enum Update {
    Response(Vec<(HeaderName, HeaderValue)>),
    Event {
        prefix: Option<String>,
        headers: Vec<(HeaderName, HeaderValue)>,
    },
}

impl From<Vec<(HeaderName, HeaderValue)>> for Update {
    fn from(headers: Vec<(HeaderName, HeaderValue)>) -> Self {
        Self::Response(headers)
    }
}

/// Rebuild an event's named limit family using Codex's header normalization.
/// Invalid/unsupported family names are never relabeled as the default quota.
pub fn event_headers(payload: &Value) -> Update {
    let prefix = (|| {
        if payload.get("type")?.as_str()? != "codex.rate_limits" {
            return None;
        }
        let family = payload
            .get("metered_limit_name")
            .filter(|value| !value.is_null())
            .or_else(|| payload.get("limit_name").filter(|value| !value.is_null()));
        let family = match family {
            Some(value) => value.as_str()?,
            None => "codex",
        }
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-");
        if (family != "codex" && !family.starts_with("codex-"))
            || !family
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return None;
        }
        Some(format!("x-{family}"))
    })();
    let mut headers = Vec::new();
    let Some(ref family_prefix) = prefix else {
        return Update::Event { prefix, headers };
    };
    for window in ["primary", "secondary"] {
        let Some(values) = payload.pointer(&format!("/rate_limits/{window}")) else {
            continue;
        };
        // A window without its observed usage is not a quota snapshot.
        if !values
            .get("used_percent")
            .and_then(Value::as_f64)
            .is_some_and(f64::is_finite)
        {
            continue;
        }
        for (field, suffix) in [
            ("used_percent", "used-percent"),
            ("window_minutes", "window-minutes"),
            ("reset_at", "reset-at"),
            ("reset_after_seconds", "reset-after-seconds"),
        ] {
            let Some(value) = values.get(field).and_then(Value::as_number) else {
                continue;
            };
            if field != "used_percent" && value.as_i64().is_none() {
                continue;
            }
            push_header(
                &mut headers,
                &format!("{family_prefix}-{window}-{suffix}"),
                &value.to_string(),
            );
        }
    }
    if let Some(plan) = payload.get("plan_type").and_then(Value::as_str) {
        push_header(&mut headers, "x-codex-plan-type", plan);
    }
    if let Some(credits) = payload.get("credits")
        && let (Some(has_credits), Some(unlimited)) = (
            credits.get("has_credits").and_then(Value::as_bool),
            credits.get("unlimited").and_then(Value::as_bool),
        )
    {
        push_header(
            &mut headers,
            "x-codex-credits-has-credits",
            &has_credits.to_string(),
        );
        push_header(
            &mut headers,
            "x-codex-credits-unlimited",
            &unlimited.to_string(),
        );
        if let Some(balance) = credits.get("balance").and_then(Value::as_str) {
            push_header(&mut headers, "x-codex-credits-balance", balance);
        }
    }
    Update::Event { prefix, headers }
}

/// Response heads (including empty resets) replace the attempt's telemetry.
/// Events replace their family's windows, retaining absent credits/plan and other
/// families, like Codex snapshots. Nothing survives outside this request scope.
pub fn record(update: impl Into<Update>) {
    let _ = SNAPSHOT.try_with(|snapshot| {
        if let Ok(mut snapshot) = snapshot.0.lock() {
            match update.into() {
                Update::Response(mut headers) => {
                    headers.retain(|(name, _)| is_quota_header(name.as_str()));
                    *snapshot = headers;
                }
                Update::Event {
                    prefix: Some(prefix),
                    headers,
                } => {
                    let has_credits = headers
                        .iter()
                        .any(|(name, _)| name.as_str().starts_with("x-codex-credits-"));
                    let has_plan = headers.iter().any(|(name, _)| name == "x-codex-plan-type");
                    snapshot.retain(|(name, _)| {
                        let name = name.as_str();
                        let family_window = name.strip_prefix(&prefix).is_some_and(|suffix| {
                            suffix.starts_with("-primary-")
                                || suffix.starts_with("-secondary-")
                                || suffix == "-limit-name"
                        });
                        !family_window
                            && !(has_credits && name.starts_with("x-codex-credits-"))
                            && !(has_plan && name == "x-codex-plan-type")
                    });
                    snapshot.extend(headers);
                }
                Update::Event { prefix: None, .. } => {}
            }
        }
    });
}

/// Adds this operation's quota headers without replacing explicit response headers.
pub fn stamp(target: &mut HeaderMap) {
    let _ = SNAPSHOT.try_with(|snapshot| {
        let Ok(snapshot) = snapshot.0.lock() else {
            return;
        };
        for (name, value) in snapshot.iter() {
            if !target.contains_key(name) {
                target.insert(name.clone(), value.clone());
            }
        }
    });
}

/// Stamps an outgoing Codex response, whatever shape it took.
pub fn stamp_response(mut response: Response) -> Response {
    stamp(response.headers_mut());
    response
}

fn push_header(headers: &mut Vec<(HeaderName, HeaderValue)>, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.push((name, value));
    }
}
