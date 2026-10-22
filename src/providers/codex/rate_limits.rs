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
        || name.starts_with("x-codex-credits-")
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

/// The same headers, rebuilt from a `codex.rate_limits` event. The event names
/// its extra limit families by label where the headers name them by id, so only
/// the default family and the plan are reconstructed.
pub fn event_headers(payload: &Value) -> Vec<(HeaderName, HeaderValue)> {
    let mut headers = Vec::new();
    for window in ["primary", "secondary"] {
        let Some(values) = payload.pointer(&format!("/rate_limits/{window}")) else {
            continue;
        };
        for (field, suffix) in [
            ("used_percent", "used-percent"),
            ("window_minutes", "window-minutes"),
            ("reset_at", "reset-at"),
            ("reset_after_seconds", "reset-after-seconds"),
        ] {
            let Some(number) = values.get(field).and_then(Value::as_f64) else {
                continue;
            };
            push_header(
                &mut headers,
                &format!("x-codex-{window}-{suffix}"),
                &format_number(number),
            );
        }
    }
    if let Some(plan) = payload.get("plan_type").and_then(Value::as_str) {
        push_header(&mut headers, "x-codex-plan-type", plan);
    }
    headers
}

/// Replace this operation's telemetry. Empty clears the preceding attempt's
/// numbers; outside a request scope nothing is retained.
pub fn record(mut headers: Vec<(HeaderName, HeaderValue)>) {
    headers.retain(|(name, _)| is_quota_header(name.as_str()));
    let _ = SNAPSHOT.try_with(|snapshot| {
        if let Ok(mut snapshot) = snapshot.0.lock() {
            *snapshot = headers;
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

/// Codex sends whole numbers as integers; JSON parsing turns them into floats.
fn format_number(number: f64) -> String {
    if number.fract() == 0.0 {
        format!("{}", number as i64)
    } else {
        format!("{number}")
    }
}

fn push_header(headers: &mut Vec<(HeaderName, HeaderValue)>, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.push((name, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn value_of(headers: &[(HeaderName, HeaderValue)], name: &str) -> Option<String> {
        headers
            .iter()
            .find(|(key, _)| key.as_str() == name)
            .map(|(_, value)| value.to_str().unwrap().to_string())
    }

    // A ChatGPT Pro subscription reports its weekly limit as the primary window,
    // so the window's declared length matters to the client and travels with it.
    #[test]
    fn keeps_the_quota_headers_of_a_response_head() {
        let captured = telemetry_headers(&header_map(&[
            ("x-codex-primary-used-percent", "1"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-reset-at", "1788765936"),
            ("x-codex-plan-type", "pro"),
        ]));

        assert_eq!(
            value_of(&captured, "x-codex-primary-used-percent").as_deref(),
            Some("1")
        );
        assert_eq!(
            value_of(&captured, "x-codex-primary-window-minutes").as_deref(),
            Some("10080")
        );
        assert_eq!(
            value_of(&captured, "x-codex-primary-reset-at").as_deref(),
            Some("1788765936")
        );
        assert_eq!(
            value_of(&captured, "x-codex-plan-type").as_deref(),
            Some("pro")
        );
    }

    // `x-codex-turn-state` is continuation state, and a large opaque blob at
    // that. Forwarding the whole prefix would put it on every response.
    #[test]
    fn drops_headers_that_are_not_quota() {
        let captured = telemetry_headers(&header_map(&[
            ("x-codex-primary-used-percent", "1"),
            ("x-codex-turn-state", "gAAAAAB-opaque"),
            ("content-type", "text/event-stream"),
        ]));

        assert_eq!(captured.len(), 1);
        assert_eq!(
            value_of(&captured, "x-codex-primary-used-percent").as_deref(),
            Some("1")
        );
    }

    // The WebSocket transport has no response headers at all, so the same
    // numbers have to come out of the event instead.
    #[test]
    fn rebuilds_the_headers_from_a_rate_limits_event() {
        let payload = serde_json::json!({
            "type": "codex.rate_limits",
            "plan_type": "pro",
            "rate_limits": {
                "allowed": true,
                "limit_reached": false,
                "primary": {
                    "used_percent": 1.0,
                    "window_minutes": 10080,
                    "reset_at": 1788765936_i64,
                    "reset_after_seconds": 603581_i64
                },
                "secondary": null
            }
        });

        let captured = event_headers(&payload);

        assert_eq!(
            value_of(&captured, "x-codex-primary-used-percent").as_deref(),
            Some("1")
        );
        assert_eq!(
            value_of(&captured, "x-codex-primary-window-minutes").as_deref(),
            Some("10080")
        );
        assert_eq!(
            value_of(&captured, "x-codex-primary-reset-at").as_deref(),
            Some("1788765936")
        );
        assert_eq!(
            value_of(&captured, "x-codex-primary-reset-after-seconds").as_deref(),
            Some("603581")
        );
        assert_eq!(
            value_of(&captured, "x-codex-plan-type").as_deref(),
            Some("pro")
        );
        assert_eq!(value_of(&captured, "x-codex-secondary-used-percent"), None);
    }

    // A window the plan does not meter must stay absent rather than arrive as
    // zero, which a client would read as a bucket that is empty and usable.
    #[test]
    fn omits_a_window_the_event_reports_as_absent() {
        let payload = serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": { "primary": null, "secondary": null }
        });

        assert!(event_headers(&payload).is_empty());
    }

    #[tokio::test]
    async fn interleaved_accounts_and_telemetry_free_responses_are_isolated() {
        let (first_ready, ready) = tokio::sync::oneshot::channel();
        let (second_done, done) = tokio::sync::oneshot::channel();
        let first = with_response_telemetry(async {
            record(telemetry_headers(&header_map(&[(
                "x-codex-plan-type",
                "pro",
            )])));
            first_ready.send(()).unwrap();
            done.await.unwrap();
            Response::new(axum::body::Body::empty())
        });
        let second = with_response_telemetry(async {
            ready.await.unwrap();
            record(telemetry_headers(&header_map(&[(
                "x-codex-plan-type",
                "plus",
            )])));
            second_done.send(()).unwrap();
            Response::new(axum::body::Body::empty())
        });
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.headers()["x-codex-plan-type"], "pro");
        assert_eq!(second.headers()["x-codex-plan-type"], "plus");
        let empty =
            with_response_telemetry(async { Response::new(axum::body::Body::empty()) }).await;
        assert!(empty.headers().get("x-codex-plan-type").is_none());
        let retry = with_response_telemetry(async {
            record(telemetry_headers(&header_map(&[(
                "x-codex-plan-type",
                "pro",
            )])));
            record(Vec::new());
            Response::new(axum::body::Body::empty())
        })
        .await;
        assert!(retry.headers().get("x-codex-plan-type").is_none());
    }

    #[tokio::test]
    async fn explicit_scope_reaches_spawned_transport_workers() {
        let response = with_response_telemetry(async {
            let telemetry = current();
            tokio::spawn(with_telemetry(telemetry, async {
                record(
                    header_map(&[
                        ("x-codex-plan-type", "pro"),
                        ("x-codex-turn-state", "private"),
                    ])
                    .into_iter()
                    .map(|(name, value)| (name.unwrap(), value))
                    .collect(),
                );
            }))
            .await
            .unwrap();
            Response::new(axum::body::Body::empty())
        })
        .await;
        assert_eq!(response.headers()["x-codex-plan-type"], "pro");
        assert!(!response.headers().contains_key("x-codex-turn-state"));
    }

    #[test]
    fn recording_outside_a_request_does_not_cache_quotas() {
        record(telemetry_headers(&header_map(&[(
            "x-codex-plan-type",
            "pro",
        )])));
        let mut headers = HeaderMap::new();
        stamp(&mut headers);
        assert!(headers.is_empty());
    }
}
