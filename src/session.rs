use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};

const SESSION_IDLE_TTL_MS: u64 = 30 * 60 * 1000;
pub const MAX_SESSIONS: usize = 10_000;

#[derive(Debug, Clone)]
pub struct SessionState {
    pub seq: u64,
    pub last_seen: u64,
}

#[derive(Default)]
struct SessionStore {
    map: HashMap<Arc<str>, SessionState>,
    order: VecDeque<Arc<str>>,
}

static SESSIONS: LazyLock<Mutex<SessionStore>> =
    LazyLock::new(|| Mutex::new(SessionStore::default()));

pub fn existing_session(session_id: Option<&str>, now: u64) -> Option<SessionState> {
    let id = session_id?;
    let mut store = SESSIONS.lock().expect("session lock");
    let state = store.map.get(id).cloned()?;
    if now.saturating_sub(state.last_seen) > SESSION_IDLE_TTL_MS {
        store.map.remove(id);
        store.order.retain(|item| item.as_ref() != id);
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
    if let Some(state) = store.map.get_mut(id)
        && now.saturating_sub(state.last_seen) <= SESSION_IDLE_TTL_MS
    {
        state.seq += 1;
        state.last_seen = now;
        return Some(state.clone());
    }
    if store.map.remove(id).is_some() {
        store.order.retain(|item| item.as_ref() != id);
    }
    let mut next = prior
        .filter(|state| now.saturating_sub(state.last_seen) <= SESSION_IDLE_TTL_MS)
        .cloned()
        .unwrap_or(SessionState {
            seq: 0,
            last_seen: now,
        });
    next.seq += 1;
    next.last_seen = now;
    let id: Arc<str> = Arc::from(id);
    store.order.push_back(Arc::clone(&id));
    store.map.insert(id, next.clone());

    while store.order.len() > MAX_SESSIONS {
        if let Some(evict) = store.order.pop_front() {
            store.map.remove(&evict);
        } else {
            break;
        }
    }

    Some(next)
}
