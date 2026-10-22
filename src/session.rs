use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

const SESSION_IDLE_TTL_MS: u64 = 30 * 60 * 1000;
pub const MAX_SESSIONS: usize = 10_000;

#[derive(Debug, Clone)]
pub struct SessionState {
    pub seq: u64,
    pub last_seen: u64,
}

#[derive(Default)]
struct SessionStore {
    map: HashMap<String, SessionState>,
    order: VecDeque<String>,
}

static SESSIONS: LazyLock<Mutex<SessionStore>> =
    LazyLock::new(|| Mutex::new(SessionStore::default()));

pub fn existing_session(session_id: Option<&str>, now: u64) -> Option<SessionState> {
    let id = session_id?;
    let mut store = SESSIONS.lock().expect("session lock");
    let state = store.map.get(id).cloned()?;
    if now.saturating_sub(state.last_seen) > SESSION_IDLE_TTL_MS {
        store.map.remove(id);
        store.order.retain(|item| item != id);
        return None;
    }
    Some(state)
}

pub fn record_session_request(
    session_id: Option<&str>,
    prior: Option<&SessionState>,
    now: u64,
) -> Option<SessionState> {
    let id = session_id?;
    let mut store = SESSIONS.lock().expect("session lock");
    let stored = store
        .map
        .get(id)
        .cloned()
        .filter(|state| now.saturating_sub(state.last_seen) <= SESSION_IDLE_TTL_MS);
    if stored.is_none() && store.map.remove(id).is_some() {
        store.order.retain(|item| item != id);
    }
    let mut next = stored
        .or_else(|| {
            prior
                .filter(|state| now.saturating_sub(state.last_seen) <= SESSION_IDLE_TTL_MS)
                .cloned()
        })
        .unwrap_or(SessionState {
            seq: 0,
            last_seen: now,
        });
    next.seq += 1;
    next.last_seen = now;
    if !store.map.contains_key(id) {
        store.order.push_back(id.to_string());
    }
    store.map.insert(id.to_string(), next.clone());

    while store.order.len() > MAX_SESSIONS {
        if let Some(evict) = store.order.pop_front() {
            store.map.remove(&evict);
        } else {
            break;
        }
    }

    Some(next)
}
