use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

mod mock;
pub mod remote;
pub mod snapshot;

pub use mock::MockMonitor;

const DEFAULT_RECENT_LIMIT: usize = 200;
pub(crate) const MAX_ERROR_MESSAGE_BYTES: usize = 4096;

pub(crate) fn bounded_error_message(message: &str) -> &str {
    &message[..message.floor_char_boundary(MAX_ERROR_MESSAGE_BYTES.min(message.len()))]
}
pub const SESSION_TOKEN_BUCKET_SECS: u64 = 10;
pub const SESSION_OUTPUT_HISTORY_BUCKETS: u64 = 60 * 60 / SESSION_TOKEN_BUCKET_SECS;
const SESSION_USAGE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const SESSION_PRUNE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    Messages,
    CountTokens,
    Responses,
    ChatCompletions,
    Images,
    Transcriptions,
}

impl EndpointKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::CountTokens => "count_tokens",
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
            Self::Images => "images",
            Self::Transcriptions => "transcriptions",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Started,
    ProviderSelected,
    Compacting,
    Upstream,
    Streaming,
    Completed,
    Failed,
}

impl RequestStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::ProviderSelected => "selected",
            Self::Compacting => "compacting",
            Self::Upstream => "upstream",
            Self::Streaming => "streaming",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Input not served from cache; cached input is tracked separately.
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl TokenUsage {
    pub fn from_codex(usage: &serde_json::Value) -> Self {
        let input_tokens = usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64);
        let cached_input_tokens = usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(serde_json::Value::as_u64)
            .or(input_tokens.map(|_| 0));
        Self {
            input_tokens: input_tokens
                .map(|tokens| tokens.saturating_sub(cached_input_tokens.unwrap_or(0))),
            cached_input_tokens,
            output_tokens: usage
                .get("output_tokens")
                .and_then(serde_json::Value::as_u64),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActiveRequest {
    pub request_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub project: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub endpoint: EndpointKind,
    pub started_at: SystemTime,
    started_instant: Instant,
    pub generation_started_at: Option<SystemTime>,
    generation_started_instant: Option<Instant>,
    generation_initial_output_tokens: u64,
    pub generation_finished_at: Option<SystemTime>,
    pub generation_duration: Option<Duration>,
    pub status: RequestStatus,
    pub streamed_bytes: u64,
    pub stream_chunks: u64,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub error: Option<String>,
    pub traffic_capture_path: Option<PathBuf>,
}

impl ActiveRequest {
    pub fn elapsed(&self) -> Duration {
        self.started_instant.elapsed()
    }

    pub fn rate(&self) -> Throughput {
        throughput(
            self.output_tokens
                .and_then(|tokens| tokens.checked_sub(self.generation_initial_output_tokens)),
            self.streamed_bytes,
            self.stream_chunks,
            self.generation_duration.unwrap_or(Duration::ZERO),
        )
    }
}

#[derive(Debug, Clone)]
pub struct CompletedRequest {
    pub request_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub project: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub endpoint: EndpointKind,
    pub started_at: SystemTime,
    pub finished_at: SystemTime,
    pub generation_started_at: Option<SystemTime>,
    generation_started_instant: Option<Instant>,
    generation_initial_output_tokens: u64,
    pub generation_finished_at: Option<SystemTime>,
    pub generation_duration: Option<Duration>,
    pub status: RequestStatus,
    pub http_status: Option<u16>,
    pub latency: Duration,
    pub streamed_bytes: u64,
    pub stream_chunks: u64,
    output_history_at: Option<SystemTime>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub error: Option<String>,
    pub traffic_capture_path: Option<PathBuf>,
}

impl CompletedRequest {
    pub fn rate(&self) -> Throughput {
        throughput(
            self.output_tokens
                .and_then(|tokens| tokens.checked_sub(self.generation_initial_output_tokens)),
            self.streamed_bytes,
            self.stream_chunks,
            self.generation_duration.unwrap_or(Duration::ZERO),
        )
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "unit", content = "value", rename_all = "snake_case")]
pub enum Throughput {
    TokensPerSecond(f64),
    BytesPerSecond(f64),
    EventsPerSecond(f64),
    None,
}

impl Throughput {
    pub fn label(&self) -> String {
        match self {
            Self::TokensPerSecond(value) => format!("{value:.1} tok/s"),
            Self::BytesPerSecond(value) if *value >= 1024.0 => {
                format!("{:.1} KB/s", value / 1024.0)
            }
            Self::BytesPerSecond(value) => format!("{value:.0} B/s"),
            Self::EventsPerSecond(value) => format!("{value:.1} ev/s"),
            Self::None => "-".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MonitorState {
    pub started_at: SystemTime,
    pub uptime: Duration,
    pub sessions: Vec<SessionSummary>,
    pub active: Vec<ActiveRequest>,
    pub recent: Vec<CompletedRequest>,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session_id: Option<String>,
    pub project: Option<String>,
    pub active_count: usize,
    pub request_count: usize,
    pub failure_count: usize,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub last_seen: SystemTime,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub output_token_samples: Vec<(SystemTime, u64)>,
    rate_output_tokens: u64,
    pub generation_duration: Duration,
    pub last_status: String,
}

impl SessionSummary {
    pub fn rate(&self) -> Throughput {
        throughput(
            Some(self.rate_output_tokens).filter(|tokens| *tokens > 0),
            0,
            0,
            self.generation_duration,
        )
    }

    pub fn label(&self) -> &str {
        self.session_id.as_deref().unwrap_or("no-session")
    }
}

#[derive(Debug)]
struct MonitorStore {
    started_at: SystemTime,
    started_instant: Instant,
    last_session_prune_instant: Instant,
    active: HashMap<String, ActiveRequest>,
    recent: VecDeque<CompletedRequest>,
    session_usage: HashMap<Option<String>, SessionUsage>,
    session_output_buckets: HashMap<Option<String>, Vec<(u64, u64)>>,
}

#[derive(Debug)]
struct SessionUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    last_seen: SystemTime,
}

impl Default for SessionUsage {
    fn default() -> Self {
        Self {
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            last_seen: SystemTime::now(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MonitorHandle {
    store: Arc<Mutex<MonitorStore>>,
}

impl Default for MonitorHandle {
    fn default() -> Self {
        Self {
            store: Arc::new(Mutex::new(MonitorStore {
                started_at: SystemTime::now(),
                started_instant: Instant::now(),
                last_session_prune_instant: Instant::now(),
                active: HashMap::new(),
                recent: VecDeque::new(),
                session_usage: HashMap::new(),
                session_output_buckets: HashMap::new(),
            })),
        }
    }
}

impl MonitorHandle {
    fn mutate(&self, mutation: impl FnOnce(&mut MonitorStore)) {
        if let Ok(mut store) = self.store.lock() {
            mutation(&mut store);
            store.prune_session_history_if_due(SystemTime::now());
        }
    }

    pub fn snapshot(&self) -> MonitorState {
        match self.store.lock() {
            Ok(mut store) => store.snapshot(),
            Err(_) => MonitorState {
                started_at: SystemTime::now(),
                uptime: Duration::ZERO,
                sessions: Vec::new(),
                active: Vec::new(),
                recent: Vec::new(),
            },
        }
    }

    pub fn request_started(
        &self,
        request_id: impl Into<String>,
        session_id: Option<String>,
        session_seq: Option<u64>,
        endpoint: EndpointKind,
    ) {
        let request_id = request_id.into();
        self.mutate(|store| {
            store.active.insert(
                request_id.clone(),
                ActiveRequest {
                    request_id,
                    session_id,
                    session_seq,
                    project: None,
                    provider: None,
                    model: None,
                    effort: None,
                    endpoint,
                    started_at: SystemTime::now(),
                    started_instant: Instant::now(),
                    generation_started_at: None,
                    generation_started_instant: None,
                    generation_initial_output_tokens: 0,
                    generation_finished_at: None,
                    generation_duration: None,
                    status: RequestStatus::Started,
                    streamed_bytes: 0,
                    stream_chunks: 0,
                    input_tokens: None,
                    cached_input_tokens: None,
                    output_tokens: None,
                    error: None,
                    traffic_capture_path: None,
                },
            );
        });
    }

    pub fn project_resolved(&self, request_id: impl Into<String>, project: impl Into<String>) {
        let request_id = request_id.into();
        let project = project.into();
        self.mutate(|store| {
            if let Some(active) = store.active.get_mut(&request_id) {
                active.project = Some(project);
            }
        });
    }

    pub fn session_sequence_resolved(&self, request_id: impl Into<String>, session_seq: u64) {
        let request_id = request_id.into();
        self.mutate(|store| {
            if let Some(active) = store.active.get_mut(&request_id) {
                active.session_seq = Some(session_seq);
            }
        });
    }

    pub fn provider_selected(
        &self,
        request_id: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        effort: Option<String>,
    ) {
        let request_id = request_id.into();
        let provider = provider.into();
        let model = model.into();
        self.mutate(|store| {
            if let Some(active) = store.active.get_mut(&request_id) {
                active.provider = Some(provider);
                active.model = Some(model);
                active.effort = effort;
                active.status = RequestStatus::ProviderSelected;
            }
        });
    }

    pub fn model_resolved(&self, request_id: impl Into<String>, model: impl Into<String>) {
        let request_id = request_id.into();
        let model = model.into();
        self.mutate(|store| {
            if let Some(active) = store.active.get_mut(&request_id) {
                active.model = Some(match active.model.take() {
                    Some(incoming) if incoming != model => format!("{incoming} → {model}"),
                    Some(incoming) => incoming,
                    None => model,
                });
            }
        });
    }

    pub fn compaction_started(&self, request_id: impl Into<String>) {
        let request_id = request_id.into();
        self.mutate(|store| {
            if let Some(active) = store.active.get_mut(&request_id) {
                active.status = RequestStatus::Compacting;
            }
        });
    }

    pub fn upstream_started(&self, request_id: impl Into<String>) {
        let request_id = request_id.into();
        self.mutate(|store| {
            if let Some(active) = store.active.get_mut(&request_id) {
                active.status = RequestStatus::Upstream;
            }
        });
    }

    pub fn generation_started(&self, request_id: impl Into<String>) {
        let request_id = request_id.into();
        self.mutate(|store| {
            if let Some(active) = store.active.get_mut(&request_id) {
                active.generation_started_at = Some(SystemTime::now());
                active.generation_started_instant = Some(Instant::now());
                active.generation_initial_output_tokens = active.output_tokens.unwrap_or(0);
                active.generation_finished_at = None;
                active.generation_duration = None;
            }
        });
    }

    pub fn traffic_capture_path(&self, request_id: impl Into<String>, path: PathBuf) {
        let request_id = request_id.into();
        self.mutate(|store| {
            if let Some(active) = store.active.get_mut(&request_id) {
                active.traffic_capture_path = Some(path);
            }
        });
    }

    pub fn stream_progress(
        &self,
        request_id: impl Into<String>,
        bytes: u64,
        chunks: u64,
        usage: TokenUsage,
    ) {
        let request_id = request_id.into();
        self.mutate(|store| {
            if let Some(active) = store.active.get_mut(&request_id) {
                active.status = RequestStatus::Streaming;
                if active.generation_started_instant.is_none() {
                    active.generation_started_at = Some(SystemTime::now());
                    active.generation_started_instant = Some(Instant::now());
                    active.generation_initial_output_tokens = active.output_tokens.unwrap_or(0);
                } else {
                    active.generation_finished_at = Some(SystemTime::now());
                    active.generation_duration = active
                        .generation_started_instant
                        .map(|started| started.elapsed());
                }
                active.streamed_bytes = active.streamed_bytes.saturating_add(bytes);
                active.stream_chunks = active.stream_chunks.saturating_add(chunks);
            } else if let Some(completed) = store
                .recent
                .iter_mut()
                .find(|request| request.request_id == request_id)
            {
                if let Some(started) = completed.generation_started_instant {
                    completed.generation_finished_at = Some(SystemTime::now());
                    completed.generation_duration = Some(started.elapsed());
                }
                completed.streamed_bytes = completed.streamed_bytes.saturating_add(bytes);
                completed.stream_chunks = completed.stream_chunks.saturating_add(chunks);
            }
            store.update_usage(&request_id, usage);
        });
    }

    pub fn usage_updated(&self, request_id: impl Into<String>, usage: TokenUsage) {
        let request_id = request_id.into();
        self.mutate(|store| {
            store.update_usage(&request_id, usage);
        });
    }

    pub fn request_completed(&self, request_id: impl Into<String>, http_status: u16) {
        let request_id = request_id.into();
        self.mutate(|store| {
            store.finish(
                &request_id,
                RequestStatus::Completed,
                Some(http_status),
                None,
            );
        });
    }

    pub fn request_failed(
        &self,
        request_id: impl Into<String>,
        http_status: Option<u16>,
        error: impl Into<String>,
    ) {
        let request_id = request_id.into();
        let error = error.into();
        self.mutate(|store| {
            store.finish(&request_id, RequestStatus::Failed, http_status, Some(error));
        });
    }

    pub fn request_abandoned(&self, request_id: impl Into<String>, error: impl Into<String>) {
        let request_id = request_id.into();
        let error = error.into();
        self.mutate(|store| {
            if store.active.contains_key(&request_id) {
                store.finish(&request_id, RequestStatus::Failed, None, Some(error));
            }
        });
    }
}

impl MonitorStore {
    fn finish(
        &mut self,
        request_id: &str,
        status: RequestStatus,
        http_status: Option<u16>,
        error: Option<String>,
    ) {
        let active = self
            .active
            .remove(request_id)
            .unwrap_or_else(|| ActiveRequest {
                request_id: request_id.to_string(),
                session_id: None,
                session_seq: None,
                project: None,
                provider: None,
                model: None,
                effort: None,
                endpoint: EndpointKind::Messages,
                started_at: SystemTime::now(),
                started_instant: Instant::now(),
                generation_started_at: None,
                generation_started_instant: None,
                generation_initial_output_tokens: 0,
                generation_finished_at: None,
                generation_duration: None,
                status: RequestStatus::Started,
                streamed_bytes: 0,
                stream_chunks: 0,
                input_tokens: None,
                cached_input_tokens: None,
                output_tokens: None,
                error: None,
                traffic_capture_path: None,
            });
        let mut completed = CompletedRequest {
            request_id: active.request_id,
            session_id: active.session_id,
            session_seq: active.session_seq,
            project: active.project,
            provider: active.provider,
            model: active.model,
            effort: active.effort,
            endpoint: active.endpoint,
            started_at: active.started_at,
            finished_at: SystemTime::now(),
            generation_started_at: active.generation_started_at,
            generation_started_instant: active.generation_started_instant,
            generation_initial_output_tokens: active.generation_initial_output_tokens,
            generation_finished_at: active.generation_finished_at,
            generation_duration: active.generation_duration,
            status,
            http_status,
            latency: active.started_instant.elapsed(),
            streamed_bytes: active.streamed_bytes,
            stream_chunks: active.stream_chunks,
            input_tokens: active.input_tokens,
            cached_input_tokens: active.cached_input_tokens,
            output_tokens: active.output_tokens,
            output_history_at: None,
            error: error.or(active.error).map(|error| {
                if error.len() > MAX_ERROR_MESSAGE_BYTES {
                    bounded_error_message(&error).to_owned()
                } else {
                    error
                }
            }),
            traffic_capture_path: active.traffic_capture_path,
        };
        if let Some(tokens) = completed.output_tokens.filter(|tokens| *tokens > 0) {
            let timestamp = completed
                .generation_finished_at
                .unwrap_or(completed.finished_at);
            Self::record_session_output(
                &mut self.session_output_buckets,
                &completed.session_id,
                timestamp,
                (0, tokens),
            );
            completed.output_history_at = Some(timestamp);
        }
        self.recent.push_front(completed);
        while self.recent.len() > DEFAULT_RECENT_LIMIT {
            self.recent.pop_back();
        }
    }

    fn update_usage(&mut self, request_id: &str, usage: TokenUsage) {
        let mut history_update = None;
        let (session_id, input, cached, output) =
            if let Some(active) = self.active.get_mut(request_id) {
                if active.endpoint == EndpointKind::CountTokens {
                    return;
                }
                if usage.output_tokens.is_some()
                    && let Some(started) = active.generation_started_instant
                {
                    active.generation_finished_at = Some(SystemTime::now());
                    active.generation_duration = Some(started.elapsed());
                }
                (
                    &active.session_id,
                    update_token_count(&mut active.input_tokens, usage.input_tokens),
                    update_token_count(&mut active.cached_input_tokens, usage.cached_input_tokens),
                    update_token_count(&mut active.output_tokens, usage.output_tokens),
                )
            } else if let Some(completed) = self
                .recent
                .iter_mut()
                .find(|request| request.request_id == request_id)
            {
                if completed.endpoint == EndpointKind::CountTokens {
                    return;
                }
                if usage.output_tokens.is_some()
                    && let Some(started) = completed.generation_started_instant
                {
                    completed.generation_finished_at = Some(SystemTime::now());
                    completed.generation_duration = Some(started.elapsed());
                }
                let output = update_token_count(&mut completed.output_tokens, usage.output_tokens);
                if output.0 != output.1 {
                    let timestamp = completed
                        .generation_finished_at
                        .unwrap_or(completed.finished_at);
                    history_update =
                        Some((completed.output_history_at.unwrap_or(timestamp), timestamp));
                    completed.output_history_at = Some(timestamp);
                }
                (
                    &completed.session_id,
                    update_token_count(&mut completed.input_tokens, usage.input_tokens),
                    update_token_count(
                        &mut completed.cached_input_tokens,
                        usage.cached_input_tokens,
                    ),
                    output,
                )
            } else {
                return;
            };
        Self::record_session_usage(&mut self.session_usage, session_id, input, cached, output);
        if let Some((previous, timestamp)) = history_update {
            Self::record_session_output(
                &mut self.session_output_buckets,
                session_id,
                previous,
                (output.0, 0),
            );
            Self::record_session_output(
                &mut self.session_output_buckets,
                session_id,
                timestamp,
                (0, output.1),
            );
        }
    }

    fn record_session_usage(
        session_usage: &mut HashMap<Option<String>, SessionUsage>,
        session_id: &Option<String>,
        input: (u64, u64),
        cached: (u64, u64),
        output: (u64, u64),
    ) {
        if input.0 == input.1 && cached.0 == cached.1 && output.0 == output.1 {
            if let Some(usage) = session_usage.get_mut(session_id) {
                usage.last_seen = SystemTime::now();
            }
            return;
        }
        let usage = match session_usage.get_mut(session_id) {
            Some(usage) => usage,
            None => session_usage.entry(session_id.clone()).or_default(),
        };
        usage.input_tokens = usage
            .input_tokens
            .saturating_sub(input.0)
            .saturating_add(input.1);
        usage.cached_input_tokens = usage
            .cached_input_tokens
            .saturating_sub(cached.0)
            .saturating_add(cached.1);
        usage.output_tokens = usage
            .output_tokens
            .saturating_sub(output.0)
            .saturating_add(output.1);
        usage.last_seen = SystemTime::now();
    }

    fn record_session_output(
        session_output_buckets: &mut HashMap<Option<String>, Vec<(u64, u64)>>,
        session_id: &Option<String>,
        timestamp: SystemTime,
        tokens: (u64, u64),
    ) {
        if tokens.0 == tokens.1 {
            return;
        }
        let bucket = session_token_bucket(timestamp);
        let buckets = match session_output_buckets.get_mut(session_id) {
            Some(buckets) => buckets,
            None if tokens.1 > 0 => session_output_buckets
                .entry(session_id.clone())
                .or_default(),
            None => return,
        };
        match buckets.binary_search_by_key(&bucket, |(bucket, _)| *bucket) {
            Ok(index) => {
                buckets[index].1 = buckets[index]
                    .1
                    .saturating_sub(tokens.0)
                    .saturating_add(tokens.1)
            }
            Err(index) if tokens.1 > 0 => buckets.insert(index, (bucket, tokens.1)),
            Err(_) => {}
        }
    }

    fn prune_session_history_if_due(&mut self, now: SystemTime) {
        if self.last_session_prune_instant.elapsed() >= SESSION_PRUNE_INTERVAL {
            self.prune_session_history(now);
        }
    }

    fn prune_session_history(&mut self, now: SystemTime) {
        let retained_sessions: HashSet<_> = self
            .active
            .values()
            .map(|request| &request.session_id)
            .chain(self.recent.iter().map(|request| &request.session_id))
            .collect();
        self.session_usage.retain(|session_id, usage| {
            let retained_by_age = match now.duration_since(usage.last_seen) {
                Ok(age) => age < SESSION_USAGE_RETENTION,
                Err(_) => true,
            };
            retained_sessions.contains(session_id) || retained_by_age
        });
        let first_bucket = first_session_output_bucket(now);
        for buckets in self.session_output_buckets.values_mut() {
            buckets.retain(|(bucket, _)| *bucket >= first_bucket);
        }
        self.session_output_buckets
            .retain(|_, buckets| !buckets.is_empty());
        self.last_session_prune_instant = Instant::now();
    }

    fn snapshot(&mut self) -> MonitorState {
        let now = SystemTime::now();
        self.prune_session_history_if_due(now);
        let mut active: Vec<_> = self.active.values().cloned().collect();
        active.sort_by_key(|request| request.started_at);
        let sessions = session_summaries(
            &active,
            &self.recent,
            &self.session_usage,
            &self.session_output_buckets,
            now,
        );
        MonitorState {
            started_at: self.started_at,
            uptime: self.started_instant.elapsed(),
            sessions,
            active,
            recent: self.recent.iter().cloned().collect(),
        }
    }
}

fn session_summaries(
    active: &[ActiveRequest],
    recent: &VecDeque<CompletedRequest>,
    session_usage: &HashMap<Option<String>, SessionUsage>,
    session_output_buckets: &HashMap<Option<String>, Vec<(u64, u64)>>,
    now: SystemTime,
) -> Vec<SessionSummary> {
    struct SessionFold<'a> {
        project: Option<&'a str>,
        provider: Option<&'a str>,
        model: Option<&'a str>,
        effort: Option<&'a str>,
        active_count: usize,
        request_count: usize,
        failure_count: usize,
        last_seen: SystemTime,
        rate_output_tokens: u64,
        generation_duration: Duration,
        last_status: &'static str,
    }

    impl Default for SessionFold<'_> {
        fn default() -> Self {
            Self {
                project: None,
                provider: None,
                model: None,
                effort: None,
                active_count: 0,
                request_count: 0,
                failure_count: 0,
                last_seen: SystemTime::UNIX_EPOCH,
                rate_output_tokens: 0,
                generation_duration: Duration::ZERO,
                last_status: "-",
            }
        }
    }

    let mut sessions: HashMap<&Option<String>, SessionFold<'_>> = HashMap::new();
    for request in recent.iter().rev() {
        let entry = sessions
            .entry(&request.session_id)
            .or_insert_with(|| SessionFold {
                last_seen: request.finished_at,
                ..SessionFold::default()
            });
        entry.request_count += 1;
        if request.status == RequestStatus::Failed {
            entry.failure_count += 1;
        }
        entry.project = request.project.as_deref().or(entry.project);
        entry.provider = request.provider.as_deref().or(entry.provider);
        entry.model = request.model.as_deref().or(entry.model);
        entry.effort = request.effort.as_deref().or(entry.effort);
        entry.last_seen = entry.last_seen.max(request.finished_at);
        if let (Some(tokens), Some(duration)) = (
            request
                .output_tokens
                .and_then(|tokens| tokens.checked_sub(request.generation_initial_output_tokens))
                .filter(|tokens| *tokens > 0),
            request
                .generation_duration
                .filter(|duration| !duration.is_zero()),
        ) {
            entry.rate_output_tokens = entry.rate_output_tokens.saturating_add(tokens);
            entry.generation_duration = entry.generation_duration.saturating_add(duration);
        }
        entry.last_status = request.status.label();
    }

    for request in active {
        let entry = sessions
            .entry(&request.session_id)
            .or_insert_with(|| SessionFold {
                last_seen: request.started_at,
                ..SessionFold::default()
            });
        entry.active_count += 1;
        entry.request_count += 1;
        entry.project = request.project.as_deref().or(entry.project);
        entry.provider = request.provider.as_deref().or(entry.provider);
        entry.model = request.model.as_deref().or(entry.model);
        entry.effort = request.effort.as_deref().or(entry.effort);
        entry.last_seen = entry.last_seen.max(request.started_at);
        if let (Some(tokens), Some(duration)) = (
            request
                .output_tokens
                .and_then(|tokens| tokens.checked_sub(request.generation_initial_output_tokens))
                .filter(|tokens| *tokens > 0),
            request
                .generation_duration
                .filter(|duration| !duration.is_zero()),
        ) {
            entry.rate_output_tokens = entry.rate_output_tokens.saturating_add(tokens);
            entry.generation_duration = entry.generation_duration.saturating_add(duration);
        }
        entry.last_status = request.status.label();
    }

    let first_bucket = first_session_output_bucket(now);
    let mut out: Vec<_> = sessions
        .into_iter()
        .map(|(session_id, session)| {
            let usage = session_usage.get(session_id);
            SessionSummary {
                session_id: session_id.clone(),
                project: session.project.map(str::to_owned),
                provider: session.provider.map(str::to_owned),
                model: session.model.map(str::to_owned),
                effort: session.effort.map(str::to_owned),
                active_count: session.active_count,
                request_count: session.request_count,
                failure_count: session.failure_count,
                last_seen: session.last_seen,
                input_tokens: usage.map_or(0, |usage| usage.input_tokens),
                cached_input_tokens: usage.map_or(0, |usage| usage.cached_input_tokens),
                output_tokens: usage.map_or(0, |usage| usage.output_tokens),
                output_token_samples: session_output_buckets
                    .get(session_id)
                    .into_iter()
                    .flatten()
                    .filter(|(bucket, _)| *bucket >= first_bucket)
                    .map(|(bucket, tokens)| (session_token_bucket_start(*bucket), *tokens))
                    .collect(),
                rate_output_tokens: session.rate_output_tokens,
                generation_duration: session.generation_duration,
                last_status: session.last_status.to_owned(),
            }
        })
        .collect();
    out.sort_by(|a, b| a.label().cmp(b.label()));
    out
}

fn session_token_bucket(timestamp: SystemTime) -> u64 {
    timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        / SESSION_TOKEN_BUCKET_SECS
}

fn session_token_bucket_start(bucket: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(bucket.saturating_mul(SESSION_TOKEN_BUCKET_SECS))
}

fn first_session_output_bucket(now: SystemTime) -> u64 {
    session_token_bucket(now).saturating_sub(SESSION_OUTPUT_HISTORY_BUCKETS.saturating_sub(1))
}

fn update_token_count(current: &mut Option<u64>, incoming: Option<u64>) -> (u64, u64) {
    let previous = current.unwrap_or(0);
    if let Some(incoming) = incoming {
        *current = Some(incoming);
    }
    (previous, current.unwrap_or(0))
}

fn throughput(
    output_tokens: Option<u64>,
    streamed_bytes: u64,
    stream_chunks: u64,
    elapsed: Duration,
) -> Throughput {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return Throughput::None;
    }
    if let Some(tokens) = output_tokens.filter(|tokens| *tokens > 0) {
        return Throughput::TokensPerSecond(tokens as f64 / secs);
    }
    if streamed_bytes > 0 {
        return Throughput::BytesPerSecond(streamed_bytes as f64 / secs);
    }
    if stream_chunks > 0 {
        return Throughput::EventsPerSecond(stream_chunks as f64 / secs);
    }
    Throughput::None
}
