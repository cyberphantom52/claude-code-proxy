//! Search-only orchestration. Client functions are never executed here.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::client::{CodexError, CodexHttpClient};
use super::continuation::ContinuationReservation;
use super::native::NativeResponseOutcome;
use super::search::{SearchCommands, SearchPolicy, compatibility_blocks};
use super::translate::accumulate::accumulate_response_and_metadata_with_traffic;
use super::translate::live_stream::LiveStreamTranslator;
use super::translate::reducer::reduce_upstream_bytes;
use super::translate::request::{
    ResponsesFunctionCallOutput, ResponsesFunctionCallOutputContentPart, ResponsesInputItem,
    ResponsesRequest, ResponsesToolChoice, ResponsesToolChoiceMode,
};
use super::{LiveRequestStateCleanup, LiveStreamCompaction, events, models, rate_limits, threads};
use crate::anthropic::sse::{append_json_sse_event, parse_sse_events};
use crate::config::CodexTransport;
use crate::provider::RequestContext;

// ponytail: a fixed per-response round ceiling bounds malformed/budget-denied
// call loops; increase only when real search workflows need more than 16 rounds.
const MAX_ROUNDS: usize = 16;
type Sender = mpsc::Sender<Result<Bytes, std::io::Error>>;

#[allow(clippy::too_many_arguments)]
pub(super) async fn response(
    client: Arc<CodexHttpClient>,
    message_id: String,
    model: String,
    ctx: RequestContext,
    request: ResponsesRequest,
    continuation: ContinuationReservation,
    compaction: LiveStreamCompaction,
    transport: CodexTransport,
    policy: SearchPolicy,
    stream: bool,
) -> Response {
    let cleanup = LiveRequestStateCleanup::new(continuation.clone(), compaction.attempt);
    let mut frames = Frames::default();
    if !stream {
        return match run(
            client,
            message_id,
            model,
            ctx,
            request,
            continuation,
            compaction,
            transport,
            policy,
            None,
            cleanup,
            &mut frames,
        )
        .await
        {
            Ok(response) => axum::Json(response).into_response(),
            Err(error) => super::map_codex_error_to_response(&error),
        };
    }
    let (tx, rx) = mpsc::channel(8);
    let outcome = NativeResponseOutcome::default();
    let response_outcome = outcome.clone();
    let model_context = models::current_model_context();
    let thread = threads::current();
    let telemetry = rate_limits::current();
    let worker = async move {
        let result = tokio::select! {
            biased;
            _ = tx.closed() => return,
            result = run(client, message_id, model, ctx, request, continuation,
                compaction, transport, policy, Some(&tx), cleanup, &mut frames) => result,
        };
        if let Err(error) = result {
            outcome.fail(error.to_string());
            let mut chunk = frames.close_open();
            append_json_sse_event(
                &mut chunk,
                "error",
                &json!({"type":"error", "error": {
                    "type":super::codex_stream_error_type(&error), "message":super::codex_error_message(&error)
                }}),
            );
            let _ = tx.send(Ok(Bytes::from(chunk))).await;
        }
    };
    tokio::spawn(rate_limits::with_telemetry(
        telemetry,
        threads::scope(thread, async move {
            if let Some(context) = model_context {
                models::with_model_context(context, worker).await;
            } else {
                worker.await;
            }
        }),
    ));
    super::event_stream_response(
        futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }),
        response_outcome,
    )
}

fn error(message: impl Into<String>) -> CodexError {
    let message = message.into();
    CodexError {
        status: 502,
        detail: Some(message.clone()),
        message,
        retry_after: None,
        usage_limit: None,
        origin: super::client::CodexErrorOrigin::Http,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    client: Arc<CodexHttpClient>,
    message_id: String,
    model: String,
    ctx: RequestContext,
    mut request: ResponsesRequest,
    continuation: ContinuationReservation,
    compaction: LiveStreamCompaction,
    transport: CodexTransport,
    policy: SearchPolicy,
    tx: Option<&Sender>,
    mut cleanup: LiveRequestStateCleanup,
    frames: &mut Frames,
) -> Result<Value, CodexError> {
    let provenance = continuation
        .provenance()
        .cloned()
        .ok_or_else(|| error("search orchestration requires pinned operation provenance"))?;
    let mut restore_catalog = Vec::new();
    if policy.forced {
        // Lite catalogs live in input, including temporal additions. Restrict
        // every catalog for this sampling step, then append the original catalog
        // after its native output/results; do not rewrite the sampled history.
        for item in &mut request.input {
            if let ResponsesInputItem::AdditionalTools { role, tools, .. } = item {
                restore_catalog.push(ResponsesInputItem::AdditionalTools {
                    id: None,
                    role: role.clone(),
                    tools: tools.clone(),
                });
                tools.retain(|tool| tool["type"] == "namespace" && tool["name"] == "web");
            }
        }
    }
    // Every internal round deliberately sends full native input. A previous
    // turn's cached delta is never valid after appending model output/results.
    let full_context = continuation.full_context_retry();
    let mut content = Vec::new();
    let mut usage = json!({"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0});
    let mut searches = 0_u64;
    let mut calls_seen = HashSet::new();
    for round in 0..MAX_ROUNDS {
        let retained = retained_bytes(&request, &content, &restore_catalog)?;
        let body = Arc::new(request.clone());
        let mut upstream = match transport {
            CodexTransport::Http => {
                client
                    .stream_codex_http_events_for_owner(&body, &ctx)
                    .await?
            }
            CodexTransport::WebSocket => {
                client
                    .stream_codex_websocket_events_for_owner(body, &ctx, Some(&full_context))
                    .await?
            }
            CodexTransport::Auto => {
                client
                    .stream_codex_auto_events_for_owner(body, &ctx, Some(&full_context))
                    .await?
            }
        };
        let mut translator =
            LiveStreamTranslator::with_estimated_input_tokens(&message_id, &model, 0);
        let mut native_body = Vec::new();
        let mut terminal = None;
        frames.round_indices.clear();
        while let Some(payload) = upstream.recv().await {
            let payload = payload?;
            if let Some(failure) = events::classify_event_failure(&payload) {
                let mut failure = super::codex_event_failure_error(
                    &failure,
                    if matches!(transport, CodexTransport::Http) {
                        super::client::CodexErrorOrigin::Http
                    } else {
                        super::client::CodexErrorOrigin::WebSocket
                    },
                );
                if let Some(limit) = events::usage_limit_from_event(&payload) {
                    failure.status = 429;
                    failure.message = limit.message.clone();
                    failure.detail = Some(limit.message.clone());
                    failure.usage_limit = Some(Box::new(limit));
                }
                return Err(failure);
            }
            if retained
                + native_body.len() as u64
                + super::request_summary::serialized_json_bytes(&payload)
                + 8
                > super::continuation::MAX_OWNER_TRANSCRIPT_BYTES
            {
                return Err(error(
                    "search response exceeded cumulative native transcript limit",
                ));
            }
            native_body.extend_from_slice(b"data: ");
            serde_json::to_writer(&mut native_body, &payload).map_err(|e| error(e.to_string()))?;
            native_body.extend_from_slice(b"\n\n");
            if tx.is_some() {
                let chunk = translator.accept(&payload, None).map_err(error)?;
                let chunk = frames.filter(&chunk)?;
                send(tx, &ctx, chunk).await?;
            }
            if events::event_is_terminal(&payload) {
                terminal = Some(payload);
                break;
            }
        }
        let terminal =
            terminal.ok_or_else(|| error("search model stream closed before terminal response"))?;
        if upstream.provenance().as_ref() != Some(&provenance) {
            return Err(error("operation_provenance_changed"));
        }
        let (mut public, finish) = accumulate_response_and_metadata_with_traffic(
            reduce_upstream_bytes(&native_body),
            &message_id,
            &model,
            None,
        )
        .map_err(|e| error(e.to_string()))?;
        let mut finish =
            finish.ok_or_else(|| error("search model round has no validated terminal metadata"))?;
        sum_usage(&mut usage, &public["usage"], round == 0);
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.usage_updated(
                &ctx.req_id,
                crate::monitor::TokenUsage {
                    input_tokens: usage["input_tokens"].as_u64(),
                    cached_input_tokens: usage["cache_read_input_tokens"].as_u64(),
                    output_tokens: usage["output_tokens"].as_u64(),
                },
            );
        }
        let internal_ids: HashSet<_> = finish
            .output_items
            .iter()
            .filter_map(|item| {
                if let ResponsesInputItem::FunctionCall {
                    call_id,
                    namespace,
                    name,
                    ..
                } = item
                    && namespace.as_deref() == Some("web")
                    && name == "run"
                {
                    Some(call_id.clone())
                } else {
                    None
                }
            })
            .collect();
        if tx.is_some() {
            send(tx, &ctx, frames.flush_pending(&internal_ids)?).await?;
        }
        let public_blocks = public["content"]
            .as_array_mut()
            .ok_or_else(|| error("search model round has no content array"))?;
        public_blocks.retain(|block| {
            !block
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| internal_ids.contains(id))
        });
        content.append(public_blocks);
        retained_bytes(&request, &content, &restore_catalog)?;
        let calls: Vec<_> = finish
            .output_items
            .iter()
            .filter_map(|item| {
                if let ResponsesInputItem::FunctionCall {
                    call_id,
                    name,
                    namespace,
                    arguments,
                } = item
                    && namespace.as_deref() == Some("web")
                    && name == "run"
                {
                    Some((call_id.clone(), arguments.clone()))
                } else {
                    None
                }
            })
            .collect();
        let external = finish.output_items.iter().any(|item| {
            matches!(item,
            ResponsesInputItem::FunctionCall { namespace, name, .. }
                if namespace.as_deref() != Some("web") || name != "run")
        });
        let executable = events::event_is_success_terminal(&terminal)
            && !events::response_is_incomplete_terminal(&terminal)
            && !matches!(
                public["stop_reason"].as_str(),
                Some("refusal" | "max_tokens")
            );
        let mixed_round = !calls.is_empty() && external && executable;
        if executable
            && (!calls.is_empty() || (!external && events::response_needs_follow_up(&terminal)))
            && !finish.continuation_eligible
        {
            return Err(error(
                "internal model round cannot be replayed without losing native state",
            ));
        }
        if executable && !calls.is_empty() && !policy.allow_search {
            return Err(error(
                "model requested web.run despite tool_choice disabling search",
            ));
        }
        if !calls.is_empty() && executable {
            request.input.append(&mut finish.output_items);
            retained_bytes(&request, &content, &restore_catalog)?;
            for (call_id, arguments) in calls {
                if !calls_seen.insert(call_id.clone()) {
                    return Err(error("search model repeated a previously executed call_id"));
                }
                let parsed = SearchCommands::parse(&arguments);
                let result = if searches >= policy.max_uses {
                    Err("web search max_uses limit reached".to_string())
                } else if round + 1 == MAX_ROUNDS && !external {
                    Err("internal search round limit reached".to_string())
                } else {
                    match parsed {
                        Err(e) => Err(format!("invalid web.run commands: {e}")),
                        Ok(commands) => {
                            searches += 1;
                            Ok(client
                                .post_search(
                                    &policy.request(&request.model, commands, &request.input),
                                    &ctx,
                                )
                                .await?)
                        }
                    }
                };
                let output = match &result {
                    Ok(response) => response.output.clone(),
                    Err(message) => format!("web.run error: {message}"),
                };
                request.input.push(ResponsesInputItem::FunctionCallOutput {
                    call_id: call_id.clone(),
                    output: ResponsesFunctionCallOutput::ContentItems(vec![
                        ResponsesFunctionCallOutputContentPart::InputText { text: output },
                    ]),
                });
                let blocks = compatibility_blocks(
                    &call_id,
                    &serde_json::from_str(&arguments).map_err(|e| error(e.to_string()))?,
                    result.as_ref().map_err(String::as_str),
                );
                for block in &blocks {
                    if tx.is_some() {
                        send(tx, &ctx, frames.block(block)).await?;
                    }
                }
                content.extend(blocks);
                retained_bytes(&request, &content, &restore_catalog)?;
            }
            request.input.append(&mut restore_catalog);
            retained_bytes(&request, &content, &restore_catalog)?;
            request.tool_choice = Some(ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto));
            if !external {
                if round + 1 == MAX_ROUNDS {
                    return Err(error(
                        "internal search round limit reached; no final model response",
                    ));
                }
                continue;
            }
            // A mixed round ends at its external calls. Its full native output
            // and private results are already in input; never execute the calls
            // that belong to the client, or duplicate that round in settlement.
        }
        if executable && !external && events::response_needs_follow_up(&terminal) {
            request.input.append(&mut finish.output_items);
            retained_bytes(&request, &content, &restore_catalog)?;
            continue;
        }
        usage["server_tool_use"] = json!({"web_search_requests":searches});
        public["content"] = Value::Array(content);
        public["usage"] = usage;
        threads::settle(&public, Some(&finish), &request, Some(&provenance));
        if mixed_round {
            // The sampled response predates the appended private results. That
            // transcript is valid thread history, not a transport-cache entry.
            cleanup.abort();
        } else {
            super::update_continuation_from_finish(
                super::compaction_scope(&continuation).as_ref(),
                &continuation,
                compaction.attempt,
                &request,
                Some(&finish),
                upstream.socket_id(),
                Some(&provenance),
                compaction.compact_boundary,
            );
            cleanup.disarm();
        }
        if tx.is_some() {
            let mut chunk = Vec::new();
            append_json_sse_event(
                &mut chunk,
                "message_delta",
                &json!({"type":"message_delta",
                "delta":{"stop_reason":public["stop_reason"],"stop_sequence":public["stop_sequence"]},
                "usage":public["usage"]}),
            );
            append_json_sse_event(&mut chunk, "message_stop", &json!({"type":"message_stop"}));
            send(tx, &ctx, chunk).await?;
        }
        return Ok(public);
    }
    Err(error("internal search round limit reached"))
}

fn retained_bytes(
    request: &ResponsesRequest,
    content: &[Value],
    catalogs: &[ResponsesInputItem],
) -> Result<u64, CodexError> {
    let bytes = super::request_summary::serialized_json_bytes(&request.input)
        .saturating_add(super::request_summary::serialized_json_bytes(content))
        .saturating_add(super::request_summary::serialized_json_bytes(catalogs));
    if bytes > super::continuation::MAX_OWNER_TRANSCRIPT_BYTES {
        Err(error(
            "search response exceeded cumulative native transcript limit",
        ))
    } else {
        Ok(bytes)
    }
}

fn sum_usage(total: &mut Value, round: &Value, first: bool) {
    let (Some(total), Some(round)) = (total.as_object_mut(), round.as_object()) else {
        return;
    };
    const OPTIONAL: [&str; 3] = [
        "codex_cache_write_input_tokens",
        "codex_cache_write_tokens_details",
        "output_tokens_details",
    ];
    for key in OPTIONAL {
        let next = round.get(key).filter(|value| {
            if key == "codex_cache_write_input_tokens" {
                value.as_u64().is_some()
            } else {
                value.is_object() && numeric_usage(value)
            }
        });
        if first {
            if let Some(value) = next {
                total.insert(key.into(), value.clone());
            }
        } else if let (Some(previous), Some(next)) = (total.get_mut(key), next) {
            if !sum_compatible_usage(previous, next) {
                total.remove(key);
            }
        } else {
            total.remove(key);
        }
    }
    for (key, value) in round {
        if OPTIONAL.contains(&key.as_str()) {
            continue;
        }
        if let Some(value) = value.as_u64() {
            let entry = total.entry(key.clone()).or_insert(Value::Null);
            *entry = Value::from(entry.as_u64().unwrap_or(0).saturating_add(value));
        } else if key == "codex_rate_limits" || key == "speed" {
            total.insert(key.clone(), value.clone());
        }
    }
}

fn numeric_usage(value: &Value) -> bool {
    value.as_u64().is_some()
        || value
            .as_object()
            .is_some_and(|fields| !fields.is_empty() && fields.values().all(numeric_usage))
}

fn sum_compatible_usage(total: &mut Value, next: &Value) -> bool {
    if let (Some(a), Some(b)) = (total.as_u64(), next.as_u64()) {
        *total = Value::from(a.saturating_add(b));
        return true;
    }
    let (Some(total), Some(next)) = (total.as_object_mut(), next.as_object()) else {
        return false;
    };
    total.len() == next.len()
        && next.iter().all(|(key, next)| {
            total
                .get_mut(key)
                .is_some_and(|total| sum_compatible_usage(total, next))
        })
}

async fn send(tx: Option<&Sender>, ctx: &RequestContext, chunk: Vec<u8>) -> Result<(), CodexError> {
    if chunk.is_empty() {
        return Ok(());
    }
    super::record_live_stream_downstream_capture(ctx, &chunk);
    super::record_live_stream_progress(ctx, &chunk);
    if let Some(tx) = tx {
        tx.send(Ok(Bytes::from(chunk)))
            .await
            .map_err(|_| error("response consumer disconnected"))?;
    }
    Ok(())
}

#[derive(Default)]
struct Frames {
    started: bool,
    next_index: usize,
    round_indices: HashMap<u64, Option<usize>>,
    pending: Vec<Value>,
    open: BTreeSet<usize>,
}

impl Frames {
    fn filter(&mut self, chunk: &[u8]) -> Result<Vec<u8>, CodexError> {
        let mut out = Vec::new();
        for event in parse_sse_events(chunk) {
            let data: Value =
                serde_json::from_str(&event.data).map_err(|e| error(e.to_string()))?;
            match data["type"].as_str() {
                Some("message_start") if self.started => continue,
                Some("message_start") => self.started = true,
                Some("message_delta" | "message_stop") => continue,
                _ => {}
            }
            // Hold the suffix starting at the first uncertain tool identity.
            // Earlier text/thinking stays live; later blocks retain original
            // order once validated native call IDs can hide private tools.
            if !self.pending.is_empty()
                || data.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use")
            {
                self.pending.push(data);
            } else {
                self.emit(data, &HashSet::new(), &mut out)?;
            }
        }
        Ok(out)
    }

    fn emit(
        &mut self,
        mut data: Value,
        internal: &HashSet<String>,
        out: &mut Vec<u8>,
    ) -> Result<(), CodexError> {
        let kind = data["type"].as_str().unwrap_or("").to_owned();
        if matches!(
            kind.as_str(),
            "content_block_start" | "content_block_delta" | "content_block_stop"
        ) {
            let old = data["index"]
                .as_u64()
                .ok_or_else(|| error("missing streamed block index"))?;
            if kind == "content_block_start" {
                let hidden = data
                    .pointer("/content_block/id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| internal.contains(id));
                let index = if hidden {
                    None
                } else {
                    let index = self.next_index;
                    self.next_index += 1;
                    self.open.insert(index);
                    Some(index)
                };
                self.round_indices.insert(old, index);
            }
            let Some(Some(index)) = self.round_indices.get(&old) else {
                return Ok(());
            };
            if kind == "content_block_stop" {
                self.open.remove(index);
            }
            data["index"] = json!(index);
        }
        append_json_sse_event(out, &kind, &data);
        Ok(())
    }

    fn flush_pending(&mut self, internal: &HashSet<String>) -> Result<Vec<u8>, CodexError> {
        let mut out = Vec::new();
        for data in std::mem::take(&mut self.pending) {
            self.emit(data, internal, &mut out)?;
        }
        Ok(out)
    }

    fn close_open(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for index in std::mem::take(&mut self.open) {
            append_json_sse_event(
                &mut out,
                "content_block_stop",
                &json!({"type":"content_block_stop","index":index}),
            );
        }
        out
    }

    fn block(&mut self, block: &Value) -> Vec<u8> {
        let index = self.next_index;
        self.next_index += 1;
        let mut out = Vec::new();
        append_json_sse_event(
            &mut out,
            "content_block_start",
            &json!({"type":"content_block_start", "index":index, "content_block":block}),
        );
        append_json_sse_event(
            &mut out,
            "content_block_stop",
            &json!({"type":"content_block_stop","index":index}),
        );
        out
    }
}
