use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::models::{OperationProvenance, current_operation_provenance};
use crate::request_identity::ConversationIdentity;

use super::translate::request::{ResponsesInputItem, ResponsesRequest};

const TTL_MS: u64 = 30 * 60 * 1000;
const MAX_STATES: usize = 10_000;
pub(crate) const MAX_OWNER_TRANSCRIPT_BYTES: u64 = 2_000_000;
const MAX_TOTAL_TRANSCRIPT_BYTES: u64 = 20_000_000;

struct ContinuationState {
    response_id: String,
    socket_id: u64,
    prompt_signature: String,
    transcript: Vec<ResponsesInputItem>,
    retained_bytes: u64,
    updated_at: u64,
}

struct OwnerState {
    provenance: OperationProvenance,
    current_turn: u64,
    continuation: Option<ContinuationState>,
    updated_at: u64,
}

#[derive(Default)]
struct ContinuationRegistry {
    owners: HashMap<ConversationIdentity, OwnerState>,
    total_retained_bytes: u64,
}

static REGISTRY: Mutex<Option<ContinuationRegistry>> = Mutex::new(None);
static NEXT_TURN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct ContinuationCandidate {
    pub turn_id: Option<u64>,
    pub previous_response_id: Option<String>,
    pub input_delta: Option<Vec<ResponsesInputItem>>,
    pub disabled_reason: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ContinuationReservation {
    candidate: ContinuationCandidate,
    owner: Option<ConversationIdentity>,
    origin_socket_id: Option<u64>,
    provenance: Option<OperationProvenance>,
}

impl ContinuationReservation {
    pub(crate) fn new(
        candidate: ContinuationCandidate,
        owner: Option<ConversationIdentity>,
        origin_socket_id: Option<u64>,
    ) -> Self {
        Self {
            candidate,
            owner,
            origin_socket_id,
            provenance: current_operation_provenance(),
        }
    }

    pub(crate) fn for_owner_turn(
        owner: Option<&ConversationIdentity>,
        turn_id: Option<u64>,
    ) -> Self {
        Self::new(
            ContinuationCandidate {
                turn_id,
                previous_response_id: None,
                input_delta: None,
                disabled_reason: None,
            },
            owner.cloned(),
            None,
        )
    }

    pub(crate) fn provenance(&self) -> Option<&OperationProvenance> {
        self.provenance.as_ref()
    }

    pub(crate) fn candidate(&self) -> &ContinuationCandidate {
        &self.candidate
    }

    pub(crate) fn owner(&self) -> Option<&ConversationIdentity> {
        self.owner.as_ref()
    }

    pub(crate) fn turn_id(&self) -> Option<u64> {
        self.candidate.turn_id
    }

    pub(crate) fn origin_socket_id(&self) -> Option<u64> {
        self.origin_socket_id
    }

    pub(crate) fn full_context_retry(&self) -> Self {
        let candidate = ContinuationCandidate {
            turn_id: self.candidate.turn_id,
            previous_response_id: None,
            input_delta: None,
            disabled_reason: Some("full_context_retry".to_string()),
        };
        let mut retry = Self::new(candidate, self.owner.clone(), None);
        retry.provenance = self.provenance.clone();
        retry
    }
}

pub(super) fn prune_idle_state() {
    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return;
    };
    let now = now_ms();
    registry.owners.retain(|_, owner| {
        // A pending turn is an invalidation guard, not expired history.
        let expired = owner
            .continuation
            .as_ref()
            .is_some_and(|state| now.saturating_sub(state.updated_at) > TTL_MS);
        if expired && let Some(state) = &owner.continuation {
            registry.total_retained_bytes = registry
                .total_retained_bytes
                .saturating_sub(state.retained_bytes);
        }
        !expired
    });
    registry.owners.shrink_to_fit();
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn continuation_candidate_for_owner(
    owner: Option<&ConversationIdentity>,
    body: &ResponsesRequest,
    enabled: bool,
) -> ContinuationReservation {
    let provenance = current_operation_provenance();
    let mut reservation = continuation_candidate_inner(owner, body, enabled, provenance.as_ref());
    reservation.provenance = provenance;
    reservation
}

fn continuation_candidate_inner(
    owner: Option<&ConversationIdentity>,
    body: &ResponsesRequest,
    enabled: bool,
    provenance: Option<&OperationProvenance>,
) -> ContinuationReservation {
    if !enabled {
        return ContinuationReservation::new(
            ContinuationCandidate {
                turn_id: None,
                previous_response_id: None,
                input_delta: None,
                disabled_reason: Some("disabled".to_string()),
            },
            owner.cloned(),
            None,
        );
    }

    let Some(owner) = owner else {
        return ContinuationReservation::new(
            ContinuationCandidate {
                turn_id: None,
                previous_response_id: None,
                input_delta: None,
                disabled_reason: Some("missing_identity".to_string()),
            },
            None,
            None,
        );
    };

    let Some(provenance) = provenance else {
        let mut reservation = continuation_candidate_inner(Some(owner), body, false, None);
        reservation.candidate.disabled_reason = Some("missing_provenance".to_string());
        return reservation;
    };

    let turn_id = NEXT_TURN_ID.fetch_add(1, Ordering::Relaxed);
    let now = now_ms();
    let (state, superseded_turn, provenance_changed) = {
        let mut guard = REGISTRY.lock().unwrap();
        let registry = guard.get_or_insert_with(ContinuationRegistry::default);
        let existing = registry.owners.remove(owner);
        let superseded_turn = existing.is_some();
        let provenance_changed = existing
            .as_ref()
            .is_some_and(|state| state.provenance != *provenance);
        let state = existing.and_then(|owner| owner.continuation);
        if let Some(state) = &state {
            registry.total_retained_bytes = registry
                .total_retained_bytes
                .saturating_sub(state.retained_bytes);
        }
        registry.owners.insert(
            owner.clone(),
            OwnerState {
                provenance: provenance.clone(),
                current_turn: turn_id,
                continuation: None,
                updated_at: now,
            },
        );
        evict_oldest(registry);
        (
            if provenance_changed { None } else { state },
            superseded_turn,
            provenance_changed,
        )
    };

    let mut reservation =
        continuation_candidate_from_state(owner, turn_id, body, state, superseded_turn, now);
    if provenance_changed {
        reservation.candidate.disabled_reason = Some("provenance_changed".to_string());
    }
    reservation
}

fn continuation_candidate_from_state(
    owner: &ConversationIdentity,
    turn_id: u64,
    body: &ResponsesRequest,
    state: Option<ContinuationState>,
    superseded_turn: bool,
    now: u64,
) -> ContinuationReservation {
    let state = match state {
        Some(state) if now.saturating_sub(state.updated_at) <= TTL_MS => state,
        Some(_) | None => {
            return ContinuationReservation::new(
                ContinuationCandidate {
                    turn_id: Some(turn_id),
                    previous_response_id: None,
                    input_delta: None,
                    disabled_reason: Some(if superseded_turn {
                        "superseded_turn".to_string()
                    } else {
                        "missing_state".to_string()
                    }),
                },
                Some(owner.clone()),
                None,
            );
        }
    };

    let signature = prompt_signature(body);
    if signature != state.prompt_signature {
        return ContinuationReservation::new(
            ContinuationCandidate {
                turn_id: Some(turn_id),
                previous_response_id: None,
                input_delta: None,
                disabled_reason: Some("prompt_changed".to_string()),
            },
            Some(owner.clone()),
            None,
        );
    }

    let Some(suffix) = input_suffix_after_prefix(&body.input, &state.transcript) else {
        return ContinuationReservation::new(
            ContinuationCandidate {
                turn_id: Some(turn_id),
                previous_response_id: None,
                input_delta: None,
                disabled_reason: Some("not_append_only".to_string()),
            },
            Some(owner.clone()),
            None,
        );
    };

    if suffix.is_empty() {
        return ContinuationReservation::new(
            ContinuationCandidate {
                turn_id: Some(turn_id),
                previous_response_id: None,
                input_delta: None,
                disabled_reason: Some("empty_delta".to_string()),
            },
            Some(owner.clone()),
            None,
        );
    }

    ContinuationReservation::new(
        ContinuationCandidate {
            turn_id: Some(turn_id),
            previous_response_id: Some(state.response_id),
            input_delta: Some(suffix),
            disabled_reason: None,
        },
        Some(owner.clone()),
        Some(state.socket_id),
    )
}

pub(crate) fn record_continuation_for_owner(
    reservation: &ContinuationReservation,
    request_body: &ResponsesRequest,
    response_id: Option<&str>,
    socket_id: Option<u64>,
    output_items: &[ResponsesInputItem],
) {
    let (owner, turn_id, provenance) = match (
        reservation.owner(),
        reservation.turn_id(),
        reservation.provenance(),
    ) {
        (Some(owner), Some(turn_id), Some(provenance)) => (owner, turn_id, provenance),
        _ => return,
    };

    let (response_id, socket_id) = match (response_id, socket_id) {
        (Some(response_id), Some(socket_id)) if socket_id != 0 => {
            (response_id.to_string(), socket_id)
        }
        _ => {
            abort_continuation_inner(Some(owner), Some(turn_id));
            return;
        }
    };
    let input_bytes = super::request_summary::serialized_json_bytes(&request_body.input);
    let output_bytes = super::request_summary::serialized_json_bytes(output_items);
    if input_bytes.saturating_add(output_bytes) > MAX_OWNER_TRANSCRIPT_BYTES {
        abort_continuation_inner(Some(owner), Some(turn_id));
        return;
    }
    let mut transcript: Vec<ResponsesInputItem> = request_body.input.clone();
    transcript.extend_from_slice(output_items);

    let prompt_signature = prompt_signature(request_body);
    let retained_bytes = super::request_summary::serialized_json_bytes(&transcript)
        .saturating_add(prompt_signature.len() as u64);

    if retained_bytes > MAX_OWNER_TRANSCRIPT_BYTES {
        abort_continuation_inner(Some(owner), Some(turn_id));
        return;
    }

    let state = ContinuationState {
        response_id,
        socket_id,
        prompt_signature,
        transcript,
        retained_bytes,
        updated_at: now_ms(),
    };

    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return;
    };
    let Some(owner_state) = registry.owners.get_mut(owner) else {
        return;
    };
    if owner_state.current_turn != turn_id || owner_state.provenance != *provenance {
        return;
    }
    if let Some(existing) = owner_state.continuation.replace(state) {
        registry.total_retained_bytes = registry
            .total_retained_bytes
            .saturating_sub(existing.retained_bytes);
    }
    registry.total_retained_bytes += retained_bytes;
    evict_oldest(registry);
}

pub(crate) fn abort_continuation_for_owner(reservation: &ContinuationReservation) {
    abort_continuation_inner(reservation.owner(), reservation.turn_id());
}

fn abort_continuation_inner(owner: Option<&ConversationIdentity>, turn_id: Option<u64>) {
    let (Some(owner), Some(turn_id)) = (owner, turn_id) else {
        return;
    };
    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return;
    };
    if registry
        .owners
        .get(owner)
        .is_some_and(|state| state.current_turn == turn_id)
        && let Some(state) = registry.owners.remove(owner)
        && let Some(continuation) = state.continuation
    {
        registry.total_retained_bytes = registry
            .total_retained_bytes
            .saturating_sub(continuation.retained_bytes);
    }
}

pub(crate) fn if_current_turn_for_owner<T>(
    reservation: &ContinuationReservation,
    action: impl FnOnce() -> T,
) -> Option<T> {
    let (Some(owner), Some(turn_id)) = (reservation.owner(), reservation.turn_id()) else {
        return None;
    };
    let guard = REGISTRY.lock().unwrap();
    let current = guard
        .as_ref()
        .and_then(|registry| registry.owners.get(owner))
        .is_some_and(|state| state.current_turn == turn_id);
    current.then(action)
}

pub(crate) fn with_current_turn_for_owner(
    reservation: &ContinuationReservation,
    action: impl FnOnce(),
) -> bool {
    if_current_turn_for_owner(reservation, action).is_some()
}

fn input_suffix_after_prefix(
    input: &[ResponsesInputItem],
    prefix: &[ResponsesInputItem],
) -> Option<Vec<ResponsesInputItem>> {
    if prefix.len() > input.len() {
        return None;
    }
    for i in 0..prefix.len() {
        if !input_items_equivalent(&input[i], &prefix[i]) {
            return None;
        }
    }
    Some(input[prefix.len()..].to_vec())
}

/// Compares two transcript items for continuation prefix purposes.
///
/// `function_call.arguments` is a JSON string that is asymmetric across the two
/// sides: the recorded transcript keeps the raw text the model streamed, while
/// the next request re-serializes the client's parsed tool input (sorted keys,
/// no whitespace). Compare those two as JSON values so a purely textual
/// difference does not break an otherwise append-only prefix.
fn input_items_equivalent(a: &ResponsesInputItem, b: &ResponsesInputItem) -> bool {
    match (a, b) {
        (
            ResponsesInputItem::FunctionCall {
                call_id: a_call_id,
                name: a_name,
                arguments: a_arguments,
            },
            ResponsesInputItem::FunctionCall {
                call_id: b_call_id,
                name: b_name,
                arguments: b_arguments,
            },
        ) => {
            a_call_id == b_call_id
                && a_name == b_name
                && (a_arguments == b_arguments
                    || matches!(
                        (
                            serde_json::from_str::<serde_json::Value>(a_arguments),
                            serde_json::from_str::<serde_json::Value>(b_arguments),
                        ),
                        (Ok(a_value), Ok(b_value)) if a_value == b_value
                    ))
        }
        _ => a == b,
    }
}

fn prompt_signature(body: &ResponsesRequest) -> String {
    let mut value = serde_json::to_value(body.with_input(())).unwrap_or_default();
    let Some(object) = value.as_object_mut() else {
        return String::new();
    };
    object.remove("input");
    value.sort_all_objects();
    serde_json::to_string(&value).unwrap_or_default()
}

fn evict_oldest(registry: &mut ContinuationRegistry) {
    while registry.owners.len() > MAX_STATES
        || registry.total_retained_bytes > MAX_TOTAL_TRANSCRIPT_BYTES
    {
        let owner = registry
            .owners
            .iter()
            .min_by_key(|(_, state)| state.updated_at)
            .map(|(owner, _)| owner.clone());
        let Some(owner) = owner else {
            break;
        };
        if let Some(state) = registry.owners.remove(&owner)
            && let Some(continuation) = state.continuation
        {
            registry.total_retained_bytes = registry
                .total_retained_bytes
                .saturating_sub(continuation.retained_bytes);
        }
    }
}
