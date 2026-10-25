//! Bounded proxy-managed Messages threads, independent of socket continuation.
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Json,
    body::Body,
    response::{IntoResponse, Response},
};
use http::StatusCode;
use http_body_util::{BodyExt, StreamBody};
use serde_json::{Value, json};

use super::translate::{
    reducer::FinishMetadata,
    request::{ResponsesInputItem, ResponsesRequest, TranslateOptions, thread_input_suffix},
};
use super::{models::OperationProvenance, native::NativeResponseOutcome};
use crate::anthropic::schema::{Message, MessagesRequest};
use crate::request_identity::ConversationIdentity;

const TTL: Duration = Duration::from_secs(30 * 60);
const MAX_SNAPSHOT: usize = 2_000_000;
const MAX_TOTAL: usize = 20_000_000;
// Reserve both a complete snapshot and the existing bounded upstream SSE capture.
const INFLIGHT_BYTES: usize = 2 * MAX_SNAPSHOT;
const MAX_HANDLES: usize = 10_000;

#[derive(Debug)]
pub(crate) struct ThreadError(pub(crate) &'static str);
impl ThreadError {
    pub(crate) fn response(&self) -> Response {
        let status = if self.0 == "thread_not_found" {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_REQUEST
        };
        (status, Json(json!({"type":"error", "error":{"type":"invalid_request_error", "message":self.0, "details":{"error_code":self.0}}}))).into_response()
    }
}
fn unsupported() -> ThreadError {
    ThreadError("thread_unsupported_request")
}
fn mismatch() -> ThreadError {
    ThreadError("thread_fingerprint_mismatch")
}

#[derive(serde::Serialize)]
struct Snapshot {
    request: MessagesRequest,
    native: Vec<ResponsesInputItem>,
    model: String,
    use_responses_lite: bool,
}
struct Replay {
    native: Vec<ResponsesInputItem>,
    model: String,
    use_responses_lite: bool,
    messages: usize,
}
struct Entry {
    owner: ConversationIdentity,
    provenance: OperationProvenance,
    branch: String,
    at: Instant,
    payload: Option<Box<Snapshot>>,
    reserved: bool,
    inflight: bool,
    bytes: usize,
    metadata_bytes: usize,
}
#[derive(Default)]
struct Registry {
    entries: HashMap<String, Entry>,
}
static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(Default::default);

impl Registry {
    fn prune(&mut self, now: Instant) {
        self.entries
            .retain(|_, e| e.reserved || e.inflight || now.saturating_duration_since(e.at) < TTL);
    }
    fn admit(&mut self, bytes: usize, now: Instant) -> bool {
        self.prune(now);
        // ponytail: bounded 10k-handle scan; use an LRU index if this becomes hot.
        while self.entries.len() >= MAX_HANDLES
            || self
                .entries
                .values()
                .map(|e| e.bytes)
                .sum::<usize>()
                .saturating_add(bytes)
                > MAX_TOTAL
        {
            let oldest = self
                .entries
                .iter()
                .filter(|(_, e)| !e.reserved && !e.inflight)
                .min_by_key(|(_, e)| e.at)
                .map(|(id, _)| id.clone());
            let Some(id) = oldest else {
                return false;
            };
            self.entries.remove(&id);
        }
        true
    }
}

struct AttemptState {
    request: Option<MessagesRequest>,
    replay: Option<Replay>,
    pending: Option<(String, Box<Snapshot>, usize)>,
    settled: bool,
    closed: bool,
    message_id: Option<String>,
    use_responses_lite: Option<bool>,
}
pub(crate) struct ThreadAttempt {
    id: String,
    parent: Option<String>,
    branch: String,
    provenance: OperationProvenance,
    metadata_bytes: usize,
    state: Mutex<AttemptState>,
}

tokio::task_local! { static ATTEMPT: Option<Arc<ThreadAttempt>>; }
pub(crate) fn current() -> Option<Arc<ThreadAttempt>> {
    ATTEMPT.try_with(Clone::clone).ok().flatten()
}
pub(crate) fn branch() -> Option<String> {
    current().map(|a| a.branch.clone())
}
pub(crate) fn replaying() -> bool {
    current().is_some_and(|a| a.parent.is_some())
}
pub(crate) async fn scope<F: Future>(attempt: Option<Arc<ThreadAttempt>>, future: F) -> F::Output {
    ATTEMPT.scope(attempt, future).await
}

fn size<T: serde::Serialize>(value: &T) -> usize {
    super::request_summary::serialized_json_bytes(value)
        .try_into()
        .unwrap_or(usize::MAX)
}
// Installed client's pW/tcr predicate: reporting is an exact match, not a prefix.
const REPORTING_SYSTEM: &str = "# Reporting outcomes\n\nReport what actually happened, not what you intended. When you say something is done, sent, saved, fixed, or verified, that claim must rest on a result you observed in this session — tool output, the file as it now reads, the page as it now loads — not on what the step should have produced. If you did not check, say you did not check. If any step failed, was skipped, or came back different from what you expected, say so in the first sentence of your report, before anything else, even when the rest of the work succeeded. Never quietly work around a failure in a way that makes it look resolved; a problem the user can see is recoverable, one your summary hides is not. When you stop before the task is complete, your first line says so plainly and names what is left. Do not describe partial work as done, and do not let a summary read as more certain than the evidence behind it.";
fn volatile(block: &Value) -> bool {
    block
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|s| s.starts_with("x-anthropic-billing-header:") || s == REPORTING_SYSTEM)
}
fn restore_system(old: &Value, new: &Value) -> Result<Value, ThreadError> {
    if old == new {
        return Ok(old.clone());
    }
    let (Some(old), Some(new)) = (old.as_array(), new.as_array()) else {
        return Err(mismatch());
    };
    // Recognize the verified billing projection; unknown projections fail closed.
    let projected = new.iter().all(volatile) && !new.is_empty() && old.iter().any(|b| !volatile(b));
    let mut replacements = new.iter().filter(|b| volatile(b));
    let mut restored = Vec::with_capacity(old.len());
    if !projected && old.len() != new.len() {
        return Err(mismatch());
    }
    for (i, block) in old.iter().enumerate() {
        if volatile(block) {
            let replacement = replacements.next().ok_or_else(mismatch)?;
            let old_billing = block["text"]
                .as_str()
                .is_some_and(|s| s.starts_with("x-anthropic-billing-header:"));
            let new_billing = replacement["text"]
                .as_str()
                .is_some_and(|s| s.starts_with("x-anthropic-billing-header:"));
            if old_billing != new_billing {
                return Err(mismatch());
            }
            // Only text is volatile; do not silently change cache or block controls.
            let mut expected = block.clone();
            expected["text"] = replacement["text"].clone();
            if expected != *replacement {
                return Err(mismatch());
            }
            restored.push(replacement.clone());
        } else {
            if !projected && new.get(i) != Some(block) {
                return Err(mismatch());
            }
            restored.push(block.clone());
        }
    }
    if replacements.next().is_some() {
        return Err(mismatch());
    }
    Ok(Value::Array(restored))
}

pub(crate) fn prepare(
    body: &mut MessagesRequest,
    owner: Option<&ConversationIdentity>,
    provenance: Option<OperationProvenance>,
    count: bool,
) -> Result<Option<Arc<ThreadAttempt>>, ThreadError> {
    let Some(thread) = body.extra.remove("thread").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    if count {
        return Err(unsupported());
    }
    let owner = owner.ok_or_else(unsupported)?;
    let provenance = provenance.ok_or_else(unsupported)?;
    let object = thread.as_object().ok_or_else(unsupported)?;
    let parent = match object.get("type").and_then(Value::as_str) {
        Some("create") if object.len() == 1 => None,
        Some("continue") if object.len() == 2 => Some(
            object
                .get("previous_message_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && s.len() <= 256)
                .ok_or_else(unsupported)?
                .to_owned(),
        ),
        _ => return Err(unsupported()),
    };
    let now = Instant::now();
    let id = uuid::Uuid::new_v4().to_string();
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    registry.prune(now);
    let mut replay = None;
    let mut branch = id.clone();
    if let Some(parent) = &parent {
        let entry = registry
            .entries
            .get_mut(parent)
            .filter(|e| e.owner == *owner && e.provenance == provenance)
            .ok_or(ThreadError("thread_not_found"))?;
        if entry.reserved || entry.payload.is_none() {
            return Err(ThreadError("thread_already_continued"));
        }
        let snapshot = entry.payload.as_ref().expect("checked payload");
        if body
            .model
            .as_ref()
            .is_some_and(|m| Some(m) != snapshot.request.model.as_ref())
        {
            return Err(mismatch());
        }
        body.model.clone_from(&snapshot.request.model);
        for field in [
            "system",
            "tools",
            "tool_choice",
            "thinking",
            "output_config",
            "parallel_tool_calls",
            "speed",
            "service_tier",
        ] {
            match (snapshot.request.extra.get(field), body.extra.get(field)) {
                (Some(old), Some(new)) if field == "system" => {
                    body.extra.insert(field.into(), restore_system(old, new)?);
                }
                (old, Some(new)) if old != Some(new) => return Err(mismatch()),
                (Some(old), None) if matches!(field, "system" | "tools") => {
                    body.extra.insert(field.into(), old.clone());
                }
                (Some(_), None) => return Err(mismatch()),
                _ => {}
            }
        }
        let mut messages = snapshot.request.messages.clone();
        messages.append(&mut body.messages);
        body.messages = messages;
        if size(body).saturating_add(size(&snapshot.native)) > MAX_SNAPSHOT {
            return Err(unsupported());
        }
        replay = Some(Replay {
            native: snapshot.native.clone(),
            model: snapshot.model.clone(),
            use_responses_lite: snapshot.use_responses_lite,
            messages: snapshot.request.messages.len(),
        });
        branch.clone_from(&entry.branch);
        entry.reserved = true;
        entry.at = now;
    }
    let owner_bytes = match owner {
        ConversationIdentity::Main(session) => session.len(),
        ConversationIdentity::Agent(session, agent) => session.len() + agent.len(),
    };
    // Includes keys, branch, reservation/tombstone and attempt bookkeeping.
    let metadata_bytes = 1024 + owner_bytes + provenance.retained_bytes();
    let admitted = size(body)
        .saturating_add(replay.as_ref().map_or(0, |r| size(&r.native)))
        .saturating_add(metadata_bytes)
        <= MAX_SNAPSHOT
        && registry.admit(INFLIGHT_BYTES.saturating_add(metadata_bytes), now);
    if !admitted {
        if let Some(entry) = parent.as_ref().and_then(|p| registry.entries.get_mut(p)) {
            entry.reserved = false;
        }
        return Err(unsupported());
    }
    registry.entries.insert(
        id.clone(),
        Entry {
            owner: owner.clone(),
            provenance: provenance.clone(),
            branch: branch.clone(),
            at: now,
            payload: None,
            reserved: false,
            inflight: true,
            bytes: INFLIGHT_BYTES.saturating_add(metadata_bytes),
            metadata_bytes,
        },
    );
    let request = body.clone();
    // Search blocks are downstream views, not native replay inputs. Only strip
    // the registry-owned prefix for translation; retain the full snapshot above.
    if let Some(replay) = &replay {
        for message in body.messages.iter_mut().take(replay.messages) {
            if message.role == "assistant"
                && let Some(blocks) = message.content.as_array_mut()
            {
                blocks.retain(|block| {
                    block["type"] != "web_search_tool_result"
                        && !(block["type"] == "server_tool_use" && block["name"] == "web_search")
                });
            }
        }
    }
    Ok(Some(Arc::new(ThreadAttempt {
        id,
        parent,
        branch,
        provenance,
        metadata_bytes,
        state: Mutex::new(AttemptState {
            request: Some(request),
            replay,
            pending: None,
            settled: false,
            closed: false,
            message_id: None,
            use_responses_lite: None,
        }),
    })))
}

pub(crate) fn set_message_id(id: &str) {
    if let Some(attempt) = current() {
        attempt
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .message_id = Some(id.to_owned());
    }
}
pub(crate) fn restore_native(
    body: &MessagesRequest,
    opts: TranslateOptions,
    translated: &mut ResponsesRequest,
) -> Result<(), ThreadError> {
    let Some(attempt) = current() else {
        return Ok(());
    };
    let mut state = attempt.state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(replay) = &state.replay {
        if replay.model != translated.model || replay.use_responses_lite != opts.use_responses_lite
        {
            return Err(mismatch());
        }
        let suffix = thread_input_suffix(body, opts.use_responses_lite, replay.messages)
            .map_err(|_| unsupported())?;
        translated.input = replay.native.iter().chain(&suffix).cloned().collect();
    }
    state.use_responses_lite = Some(opts.use_responses_lite);
    Ok(())
}

/// A validated terminal is staged here; the response-body EOF publishes it.
/// Both buffered accumulation and live terminal settlement use this hook.
pub(crate) fn settle(
    response: &Value,
    finish: Option<&FinishMetadata>,
    request: &ResponsesRequest,
    actual: Option<&OperationProvenance>,
) {
    let Some(attempt) = current() else {
        return;
    };
    let Some(finish) = finish.filter(|f| f.continuation_eligible) else {
        return;
    };
    if actual != Some(&attempt.provenance) {
        return;
    }
    let mut state = attempt.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.closed || state.settled {
        return;
    }
    let Some(use_responses_lite) = state.use_responses_lite else {
        return;
    };
    let Some(mut portable) = state.request.take() else {
        return;
    };
    state.replay = None;
    state.settled = true;
    let Some(id) = state.message_id.take() else {
        return;
    };
    let Some(content) = response.get("content") else {
        return;
    };
    portable.messages.push(Message {
        role: "assistant".into(),
        content: content.clone(),
    });
    let bytes = size(&portable)
        .saturating_add(size(&request.input))
        .saturating_add(size(&finish.output_items))
        .saturating_add(attempt.metadata_bytes);
    if bytes > MAX_SNAPSHOT {
        return;
    }
    let native = request
        .input
        .iter()
        .chain(&finish.output_items)
        .cloned()
        .collect();
    state.pending = Some((
        id,
        Box::new(Snapshot {
            request: portable,
            native,
            model: request.model.clone(),
            use_responses_lite,
        }),
        bytes,
    ));
}
impl ThreadAttempt {
    fn close(&self, publish: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return;
        }
        state.closed = true;
        state.request = None;
        state.replay = None;
        let pending = state.pending.take();
        let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut reservation) = registry.entries.remove(&self.id) else {
            return;
        };
        let now = Instant::now();
        if let Some(parent) = self
            .parent
            .as_ref()
            .and_then(|p| registry.entries.get_mut(p))
        {
            parent.reserved = false;
            parent.at = now;
            if publish && state.settled {
                parent.payload = None;
                parent.bytes = parent.metadata_bytes;
            }
        }
        if publish
            && state.settled
            && let Some((id, snapshot, bytes)) = pending
        {
            reservation.payload = Some(snapshot);
            reservation.inflight = false;
            reservation.at = now;
            reservation.bytes = bytes;
            registry.entries.insert(id, reservation);
        }
    }
}
impl Drop for ThreadAttempt {
    fn drop(&mut self) {
        self.close(false);
    }
}
struct Delivery(Arc<ThreadAttempt>);
impl Drop for Delivery {
    fn drop(&mut self) {
        self.0.close(false);
    }
}
pub(crate) fn response_body(response: Response, attempt: Option<Arc<ThreadAttempt>>) -> Response {
    let Some(attempt) = attempt else {
        return response;
    };
    let success = response.status().is_success();
    let outcome = response
        .extensions()
        .get::<NativeResponseOutcome>()
        .cloned();
    let (parts, body) = response.into_parts();
    let stream = futures_util::stream::unfold(
        (body, Delivery(attempt), false),
        move |(mut body, delivery, mut failed)| {
            let outcome = outcome.clone();
            async move {
                match body.frame().await {
                    Some(frame) => {
                        failed |= frame.is_err();
                        Some((frame, (body, delivery, failed)))
                    }
                    None => {
                        delivery.0.close(
                            success
                                && !failed
                                && outcome
                                    .as_ref()
                                    .and_then(NativeResponseOutcome::failure)
                                    .is_none(),
                        );
                        None
                    }
                }
            }
        },
    );
    Response::from_parts(parts, Body::new(StreamBody::new(stream)))
}
pub(super) fn prune_idle_state() {
    REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .prune(Instant::now());
}
