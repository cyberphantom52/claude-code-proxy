use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::models::OperationProvenance;
use crate::anthropic::sse::parse_sse_events;
use crate::provider::RequestContext;
use crate::providers::codex::client::{CodexError, CodexHttpClient};
use crate::request_identity::ConversationIdentity;

use super::count_tokens::{
    approx_token_count, count_function_call_output_tokens, count_translated_tokens,
};
use super::translate::model_catalog::catalog_model;
use super::translate::request::{
    ResponsesContentPart, ResponsesFunctionCallOutput, ResponsesInputItem, ResponsesRequest,
    ResponsesToolChoice, ResponsesToolChoiceMode, is_compact_message_text,
};

const RETAINED_MESSAGE_TOKEN_BUDGET: u64 = 20_000;
const STATE_TTL_MS: u64 = 30 * 60 * 1_000;
const MAX_STATES: usize = 1_000;
const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOTAL_STATE_BYTES: usize = 20_000_000;
const MIN_PORTABLE_SUMMARY_BYTES: usize = 32;
// Codex 970111f6 core/src/compact_remote_history.rs.
const CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE: &str =
    "Output exceeded the available model context and was truncated";

#[derive(Debug)]
pub enum CompactionError {
    Upstream(CodexError),
    InvalidResponse(String),
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Upstream(error) => write!(f, "{error}"),
            Self::InvalidResponse(message) => f.write_str(message),
        }
    }
}

enum CompactionPhase {
    Preparing,
    Unconfirmed,
    Anchored { portable_summary: String },
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CompactionScope {
    pub(crate) owner: ConversationIdentity,
    pub(crate) provenance: OperationProvenance,
    pub(crate) branch: Option<String>,
}

impl CompactionScope {
    fn key(&self) -> CompactionKey {
        (self.owner.clone(), self.branch.clone())
    }
}

struct CompactionState {
    scope: CompactionScope,
    attempt: CompactionAttempt,
    model: String,
    native_history: Vec<ResponsesInputItem>,
    phase: CompactionPhase,
    updated_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionAttempt(u64);

pub struct CompactionReplay {
    pub request: ResponsesRequest,
    pub attempt: CompactionAttempt,
}

type CompactionKey = (ConversationIdentity, Option<String>);
type CompactionRegistry = HashMap<CompactionKey, CompactionState>;
static REGISTRY: Mutex<Option<CompactionRegistry>> = Mutex::new(None);
static NEXT_ATTEMPT_ID: AtomicU64 = AtomicU64::new(1);

pub async fn request_compaction(
    client: &CodexHttpClient,
    request: &ResponsesRequest,
    ctx: &RequestContext,
) -> Result<Vec<ResponsesInputItem>, CompactionError> {
    let mut compaction_request = build_compaction_request(request);
    fit_compaction_history(&mut compaction_request)?;
    let response = client
        .post_codex_for_owner(&compaction_request, ctx, None)
        .await
        .map_err(CompactionError::Upstream)?;
    if response.provenance != super::models::current_operation_provenance()
        || response.provenance.is_none()
    {
        return Err(CompactionError::InvalidResponse(
            "compaction operation provenance changed".into(),
        ));
    }
    let compaction = parse_compaction_response(&response.body)?;
    Ok(build_compacted_history(
        compaction_request.input,
        compaction,
    ))
}

fn build_compaction_request(request: &ResponsesRequest) -> ResponsesRequest {
    let (envelope, conversation) = split_input_envelope(&request.input);
    let conversation = without_compaction_instruction(conversation);
    let input = envelope
        .iter()
        .filter(|item| matches!(item, ResponsesInputItem::AdditionalTools { .. }))
        .cloned()
        .chain(conversation)
        .chain(std::iter::once(ResponsesInputItem::CompactionTrigger))
        .collect::<Vec<_>>();
    let mut compaction_request = request.with_input(input);
    compaction_request.instructions = None;
    compaction_request.include = Some(vec!["reasoning.encrypted_content".to_string()]);
    // Codex 970111f6 compact_remote_v2_attempt.rs builds a fresh prompt with no
    // output schema; core/client.rs chooses auto and disables parallelism on Lite.
    compaction_request.tool_choice = Some(ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto));
    compaction_request.text.format = None;
    compaction_request.parallel_tool_calls = request.client_metadata.is_none();
    compaction_request
}

fn fit_compaction_history(request: &mut ResponsesRequest) -> Result<(), CompactionError> {
    let Some(model) = catalog_model(&request.model) else {
        return Ok(());
    };
    let Some(window) = model.context_window.or(model.max_context_window) else {
        // Unknown model limits retain the existing upstream/fallback behavior.
        return Ok(());
    };
    // Codex 970111f6 protocol/src/openai_models.rs defaults this metadata field
    // to 95 and uses context_window before max_context_window (not auto-compact).
    let percent = model.effective_context_window_percent.unwrap_or(95);
    if window == 0 || !(1..=100).contains(&percent) {
        return Err(CompactionError::InvalidResponse(
            "invalid compaction model context-window metadata".into(),
        ));
    }
    let usable_window = (u128::from(window) * u128::from(percent) / 100) as u64;
    let mut estimated_tokens = count_translated_tokens(request);
    let marker_tokens = approx_token_count(CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE);
    let Some((ResponsesInputItem::CompactionTrigger, history)) = request.input.split_last_mut()
    else {
        return Err(CompactionError::InvalidResponse(
            "compaction input is missing its final trigger".into(),
        ));
    };
    // Match compact_remote_history.rs: only the contiguous output suffix is
    // eligible. Never cross messages, calls, reasoning, or opaque compactions.
    // ponytail: local token estimates, not provider admission guarantees; use
    // provider token accounting if it becomes available for compaction inputs.
    for item in history.iter_mut().rev() {
        if estimated_tokens <= usable_window {
            break;
        }
        let ResponsesInputItem::FunctionCallOutput { output, .. } = item else {
            break;
        };
        let output_tokens = count_function_call_output_tokens(output);
        if output_tokens > marker_tokens {
            *output = ResponsesFunctionCallOutput::Text(
                CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE.to_string(),
            );
            estimated_tokens = estimated_tokens
                .saturating_sub(output_tokens)
                .saturating_add(marker_tokens);
        }
    }
    if estimated_tokens > usable_window {
        return Err(CompactionError::InvalidResponse(
            "compaction input exceeds known model context after eligible tool-output truncation"
                .into(),
        ));
    }
    Ok(())
}

pub(crate) fn begin_compaction_scoped(scope: &CompactionScope, model: &str) -> CompactionAttempt {
    let attempt = CompactionAttempt(NEXT_ATTEMPT_ID.fetch_add(1, Ordering::Relaxed));
    let state = CompactionState {
        scope: scope.clone(),
        attempt,
        model: model.to_string(),
        native_history: Vec::new(),
        phase: CompactionPhase::Preparing,
        updated_at: now_ms(),
    };
    let now = state.updated_at;
    let mut guard = REGISTRY.lock().unwrap();
    let registry = guard.get_or_insert_with(HashMap::new);
    evict_states(registry, now);
    registry.insert(scope.key(), state);
    evict_states(registry, now);
    attempt
}

pub(crate) fn store_compaction_scoped(
    scope: &CompactionScope,
    attempt: CompactionAttempt,
    native_history: Vec<ResponsesInputItem>,
) -> bool {
    let now = now_ms();
    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return false;
    };
    evict_states(registry, now);
    let Some(state) = registry.get_mut(&scope.key()) else {
        return false;
    };
    if state.scope != *scope
        || state.attempt != attempt
        || !matches!(state.phase, CompactionPhase::Preparing)
    {
        return false;
    }
    state.native_history = native_history;
    state.phase = CompactionPhase::Unconfirmed;
    state.updated_at = now;
    if state_size(&scope.key(), state) > MAX_STATE_BYTES {
        registry.remove(&scope.key());
        return false;
    }
    evict_states(registry, now);
    registry
        .get(&scope.key())
        .is_some_and(|state| state.scope == *scope && state.attempt == attempt)
}

pub(crate) fn activate_compaction_scoped(
    scope: Option<&CompactionScope>,
    attempt: Option<CompactionAttempt>,
    model: &str,
    output: &[ResponsesInputItem],
) -> bool {
    let (Some(scope), Some(attempt)) = (scope, attempt) else {
        return false;
    };
    let Some(portable_summary) = portable_summary_text(output) else {
        abort_compaction_attempt_scoped(Some(scope), Some(attempt));
        return false;
    };

    let now = now_ms();
    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return false;
    };
    evict_states(registry, now);
    let Some(state) = registry.get_mut(&scope.key()) else {
        return false;
    };
    if state.scope != *scope || state.attempt != attempt {
        return false;
    }
    if state.model != model || !matches!(state.phase, CompactionPhase::Unconfirmed) {
        registry.remove(&scope.key());
        return false;
    }
    state.phase = CompactionPhase::Anchored { portable_summary };
    state.updated_at = now;
    if state_size(&scope.key(), state) > MAX_STATE_BYTES {
        registry.remove(&scope.key());
        return false;
    }
    evict_states(registry, now);
    registry.contains_key(&scope.key())
}

pub(crate) fn apply_compaction_replay_scoped(
    scope: Option<&CompactionScope>,
    request: &ResponsesRequest,
) -> Option<CompactionReplay> {
    let scope = scope?;
    let now = now_ms();
    let mut guard = REGISTRY.lock().unwrap();
    let registry = guard.as_mut()?;
    evict_states(registry, now);
    let state = registry.get_mut(&scope.key())?;
    if state.scope != *scope || !matches!(state.phase, CompactionPhase::Anchored { .. }) {
        return None;
    }
    if state.model != request.model {
        registry.remove(&scope.key());
        return None;
    }
    let CompactionPhase::Anchored { portable_summary } = &state.phase else {
        return None;
    };

    let (envelope, conversation) = split_input_envelope(&request.input);
    let summary_item = conversation.first()?;
    let Some(text) = message_text(summary_item) else {
        registry.remove(&scope.key());
        return None;
    };
    if text != portable_summary
        && text
            .strip_prefix("<summary>")
            .and_then(|text| text.strip_suffix("</summary>"))
            != Some(portable_summary.as_str())
    {
        registry.remove(&scope.key());
        return None;
    }
    if conversation.len() == 1 {
        return None;
    }

    let input = envelope
        .iter()
        .cloned()
        .chain(state.native_history.iter().cloned())
        .chain(conversation[1..].iter().cloned())
        .collect::<Vec<_>>();
    let replay = request.with_input(input);
    if serialized_size(&replay.input) > MAX_STATE_BYTES {
        registry.remove(&scope.key());
        return None;
    }
    state.updated_at = now;
    Some(CompactionReplay {
        request: replay,
        attempt: state.attempt,
    })
}

pub(crate) fn abort_compaction_attempt_scoped(
    scope: Option<&CompactionScope>,
    attempt: Option<CompactionAttempt>,
) {
    let (Some(scope), Some(attempt)) = (scope, attempt) else {
        return;
    };
    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return;
    };
    if registry
        .get(&scope.key())
        .is_some_and(|state| state.scope == *scope && state.attempt == attempt)
    {
        registry.remove(&scope.key());
    }
}

pub(crate) fn clear_compaction_scoped(scope: &CompactionScope) {
    let mut guard = REGISTRY.lock().unwrap();
    if let Some(registry) = guard.as_mut()
        && registry
            .get(&scope.key())
            .is_some_and(|state| state.scope == *scope)
    {
        registry.remove(&scope.key());
    }
}

fn split_input_envelope(
    input: &[ResponsesInputItem],
) -> (&[ResponsesInputItem], &[ResponsesInputItem]) {
    let prefix_len = input
        .iter()
        .take_while(|item| is_envelope_item(item))
        .count();
    input.split_at(prefix_len)
}

fn without_compaction_instruction(input: &[ResponsesInputItem]) -> Vec<ResponsesInputItem> {
    let mut input = input.to_vec();
    let remove_empty_message = if let Some(ResponsesInputItem::Message { role, content, .. }) =
        input.last_mut()
        && role == "user"
    {
        content.retain(|part| {
            !matches!(
                part,
                ResponsesContentPart::InputText { text }
                    if is_compact_message_text(text)
            )
        });
        content.is_empty()
    } else {
        false
    };
    if remove_empty_message {
        input.pop();
    }
    input
}

fn is_envelope_item(item: &ResponsesInputItem) -> bool {
    match item {
        ResponsesInputItem::AdditionalTools { .. } => true,
        ResponsesInputItem::Message { role, .. } => role == "developer",
        _ => false,
    }
}

fn portable_summary_text(output: &[ResponsesInputItem]) -> Option<String> {
    let text = output
        .iter()
        .filter_map(|item| match item {
            ResponsesInputItem::Message { role, content, .. } if role == "assistant" => {
                Some(content)
            }
            _ => None,
        })
        .flat_map(|content| content.iter())
        .filter_map(|part| match part {
            ResponsesContentPart::InputText { text }
            | ResponsesContentPart::OutputText { text, .. } => Some(text.as_str()),
            ResponsesContentPart::InputImage { .. } => None,
        })
        .collect::<String>();
    let trimmed = text.trim();
    let summary = trimmed
        .split_once("<summary>")
        .and_then(|(_, rest)| rest.split_once("</summary>"))
        .map(|(summary, _)| summary.trim())
        .filter(|summary| !summary.is_empty())
        .unwrap_or(trimmed);
    (summary.len() >= MIN_PORTABLE_SUMMARY_BYTES).then(|| summary.to_string())
}

fn message_text(item: &ResponsesInputItem) -> Option<&str> {
    let ResponsesInputItem::Message { role, content, .. } = item else {
        return None;
    };
    if role != "user" {
        return None;
    }
    match content.as_slice() {
        [ResponsesContentPart::InputText { text }] => Some(text),
        _ => None,
    }
}

fn parse_compaction_response(body: &[u8]) -> Result<ResponsesInputItem, CompactionError> {
    let mut completed = false;
    let mut compacted = Vec::new();

    for event in parse_sse_events(body) {
        if event.data == "[DONE]" {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) else {
            continue;
        };
        match payload.get("type").and_then(serde_json::Value::as_str) {
            Some("error" | "response.error" | "response.failed") => {
                let message = payload
                    .pointer("/response/error/message")
                    .or_else(|| payload.pointer("/error/message"))
                    .or_else(|| payload.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("remote compaction failed");
                return Err(CompactionError::InvalidResponse(message.to_string()));
            }
            Some("response.output_item.done") => {
                let Some(item) = payload.get("item") else {
                    continue;
                };
                if item.get("type").and_then(serde_json::Value::as_str) == Some("compaction")
                    && let Some(encrypted_content) = item
                        .get("encrypted_content")
                        .and_then(serde_json::Value::as_str)
                {
                    compacted.push(ResponsesInputItem::Compaction {
                        encrypted_content: encrypted_content.to_string(),
                    });
                }
            }
            Some("response.completed") => completed = true,
            _ => {}
        }
    }

    if !completed {
        return Err(CompactionError::InvalidResponse(
            "remote compaction stream ended before response.completed".to_string(),
        ));
    }
    if compacted.len() != 1 {
        return Err(CompactionError::InvalidResponse(format!(
            "remote compaction expected exactly one compaction item, got {}",
            compacted.len()
        )));
    }
    Ok(compacted.pop().expect("validated one compaction item"))
}

fn build_compacted_history(
    input: Vec<ResponsesInputItem>,
    compaction: ResponsesInputItem,
) -> Vec<ResponsesInputItem> {
    let retained = input
        .into_iter()
        .filter(|item| {
            matches!(
                item,
                ResponsesInputItem::Message { role, .. }
                    if matches!(role.as_str(), "user" | "developer" | "system")
            )
        })
        .collect::<Vec<_>>();
    let mut retained = truncate_retained_messages(retained, RETAINED_MESSAGE_TOKEN_BUDGET);
    retained.push(compaction);
    retained
}

fn truncate_retained_messages(
    items: Vec<ResponsesInputItem>,
    max_tokens: u64,
) -> Vec<ResponsesInputItem> {
    let mut remaining = max_tokens;
    let mut retained = Vec::new();
    for item in items.into_iter().rev() {
        if remaining == 0 {
            break;
        }
        let tokens = message_tokens(&item).max(1);
        if tokens <= remaining {
            retained.push(item);
            remaining -= tokens;
        } else if let Some(item) = truncate_message(item, remaining) {
            retained.push(item);
            remaining = 0;
        }
    }
    retained.reverse();
    retained
}

fn message_tokens(item: &ResponsesInputItem) -> u64 {
    let ResponsesInputItem::Message { content, .. } = item else {
        return 0;
    };
    content
        .iter()
        .map(|part| match part {
            ResponsesContentPart::InputText { text }
            | ResponsesContentPart::OutputText { text, .. } => text.len().div_ceil(4) as u64,
            ResponsesContentPart::InputImage { .. } => 2_000,
        })
        .sum()
}

fn truncate_message(item: ResponsesInputItem, max_tokens: u64) -> Option<ResponsesInputItem> {
    let ResponsesInputItem::Message { role, content, .. } = item else {
        return Some(item);
    };
    let mut remaining_tokens = max_tokens;
    let mut truncated = Vec::new();
    for part in content {
        match part {
            ResponsesContentPart::InputImage { .. } => {
                if remaining_tokens >= 2_000 {
                    truncated.push(part);
                    remaining_tokens -= 2_000;
                }
            }
            ResponsesContentPart::InputText { text } => {
                let text = truncate_text(text, &mut remaining_tokens);
                if !text.is_empty() {
                    truncated.push(ResponsesContentPart::InputText { text });
                }
            }
            ResponsesContentPart::OutputText { text, .. } => {
                let text = truncate_text(text, &mut remaining_tokens);
                if !text.is_empty() {
                    truncated.push(ResponsesContentPart::OutputText {
                        text,
                        annotations: None,
                        logprobs: None,
                    });
                }
            }
        }
    }
    (!truncated.is_empty()).then_some(ResponsesInputItem::Message {
        metadata: Default::default(),
        role,
        content: truncated,
    })
}

fn truncate_text(mut text: String, remaining_tokens: &mut u64) -> String {
    if *remaining_tokens == 0 {
        return String::new();
    }
    let remaining_bytes = remaining_tokens.saturating_mul(4).min(usize::MAX as u64) as usize;
    if text.len() > remaining_bytes {
        let mut boundary = remaining_bytes;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
    }
    *remaining_tokens -= text.len().div_ceil(4) as u64;
    text
}

fn serialized_size(items: &[ResponsesInputItem]) -> usize {
    super::request_summary::serialized_json_bytes(items)
        .try_into()
        .unwrap_or(usize::MAX)
}

fn state_size(key: &CompactionKey, state: &CompactionState) -> usize {
    let (owner, branch) = key;
    let summary_len = match &state.phase {
        CompactionPhase::Preparing | CompactionPhase::Unconfirmed => 0,
        CompactionPhase::Anchored { portable_summary } => portable_summary.len(),
    };
    (match owner {
        ConversationIdentity::Main(session) => session.len(),
        ConversationIdentity::Agent(session, agent) => session.len() + agent.len(),
    }) + state.model.len()
        + branch.as_ref().map_or(0, String::len)
        + summary_len
        + serialized_size(&state.native_history)
}

pub(super) fn prune_idle_state() {
    let mut guard = REGISTRY.lock().unwrap();
    if let Some(registry) = guard.as_mut() {
        evict_states(registry, now_ms());
        registry.shrink_to_fit();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn evict_states(registry: &mut CompactionRegistry, now: u64) {
    registry.retain(|_, state| now.saturating_sub(state.updated_at) <= STATE_TTL_MS);
    let mut total_bytes: usize = registry
        .iter()
        .map(|(owner, state)| state_size(owner, state))
        .sum();
    while registry.len() > MAX_STATES || total_bytes > MAX_TOTAL_STATE_BYTES {
        let oldest = registry
            .iter()
            .min_by_key(|(_, state)| state.updated_at)
            .map(|(session_id, _)| session_id.clone());
        let Some(oldest) = oldest else {
            break;
        };
        if let Some(state) = registry.remove(&oldest) {
            total_bytes = total_bytes.saturating_sub(state_size(&oldest, &state));
        }
    }
}
