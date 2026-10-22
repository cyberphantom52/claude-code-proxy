//! Model metadata used by discovery and request shaping.
//!
//! Each operation pins an immutable catalog: concurrent requests must not see
//! another account's models. Unscoped callers use built-ins and an explicitly
//! configured import, never another client's credentials or `~/.codex` files.

use std::{
    collections::HashSet,
    fs,
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};

use serde::Deserialize;

pub const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CATALOG_MODELS: usize = 2048;

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ReasoningLevel {
    pub effort: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct CatalogServiceTier {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CatalogModel {
    pub slug: String,
    pub display_name: Option<String>,
    pub use_responses_lite: Option<bool>,
    pub supported_in_api: bool,
    #[serde(skip)]
    pub listed: bool,
    pub default_reasoning_level: Option<String>,
    pub multi_agent_reasoning_effort: Option<String>,
    pub supported_reasoning_levels: Option<Vec<ReasoningLevel>>,
    #[serde(rename = "supports_reasoning_summary_parameter")]
    pub supports_reasoning_summaries: Option<bool>,
    pub default_reasoning_summary: Option<String>,
    #[serde(rename = "support_verbosity")]
    pub supports_verbosity: Option<bool>,
    pub default_verbosity: Option<String>,
    pub context_window: Option<u64>,
    pub max_context_window: Option<u64>,
    pub effective_context_window_percent: Option<u64>,
    pub auto_compact_token_limit: Option<u64>,
    pub service_tiers: Option<Vec<CatalogServiceTier>>,
    pub default_service_tier: Option<String>,
    pub input_modalities: Option<Vec<String>>,
    pub supports_parallel_tool_calls: Option<bool>,
    pub priority: Option<i64>,
}

impl Default for CatalogModel {
    fn default() -> Self {
        Self {
            slug: String::new(),
            display_name: None,
            use_responses_lite: None,
            supported_in_api: true,
            listed: true,
            default_reasoning_level: None,
            multi_agent_reasoning_effort: None,
            supported_reasoning_levels: None,
            supports_reasoning_summaries: None,
            default_reasoning_summary: None,
            supports_verbosity: None,
            default_verbosity: None,
            context_window: None,
            max_context_window: None,
            effective_context_window_percent: None,
            auto_compact_token_limit: None,
            service_tiers: None,
            default_service_tier: None,
            input_modalities: None,
            supports_parallel_tool_calls: None,
            priority: None,
        }
    }
}

impl CatalogModel {
    pub fn supports_effort(&self, effort: &str) -> Option<bool> {
        self.supported_reasoning_levels
            .as_ref()
            .map(|levels| levels.iter().any(|level| level.effort == effort))
    }

    pub fn supports_service_tier(&self, tier: &str) -> Option<bool> {
        if matches!(tier, "auto" | "default" | "flex") {
            return Some(true);
        }
        self.service_tiers
            .as_ref()
            .map(|tiers| tiers.iter().any(|entry| entry.id == tier))
    }
}

#[derive(Debug, Clone)]
pub struct CatalogSnapshot {
    pub models: Vec<CatalogModel>,
    pub authoritative: bool,
}

impl CatalogSnapshot {
    pub fn new(models: Vec<CatalogModel>, authoritative: bool) -> Self {
        Self {
            models,
            authoritative,
        }
    }
}

tokio::task_local! {
    static ACTIVE_CATALOG: Arc<CatalogSnapshot>;
}

pub async fn scope_catalog<F: Future>(catalog: Arc<CatalogSnapshot>, future: F) -> F::Output {
    // Workers receive resolved request values; task locals do not cross spawn.
    ACTIVE_CATALOG.scope(catalog, future).await
}

pub fn with_catalog<T>(catalog: Arc<CatalogSnapshot>, f: impl FnOnce() -> T) -> T {
    ACTIVE_CATALOG.sync_scope(catalog, f)
}

pub fn scoped_catalog() -> Option<Arc<CatalogSnapshot>> {
    ACTIVE_CATALOG.try_with(Arc::clone).ok()
}

pub fn catalog_path() -> Option<PathBuf> {
    crate::config::codex_models_file()
}

/// Missing fields mean unknown capabilities, not a denial or an entitlement.
/// Malformed entries are skipped; malformed/oversized envelopes are rejected.
pub fn parse_catalog(bytes: &[u8]) -> Option<Vec<CatalogModel>> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let entries = value.get("models")?.as_array()?;
    if entries.len() > MAX_CATALOG_MODELS {
        return None;
    }
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for entry in entries {
        let Ok(mut model) = serde_json::from_value::<CatalogModel>(entry.clone()) else {
            continue;
        };
        if model.slug.is_empty()
            || model.slug.len() > 256
            || model.slug.chars().any(char::is_whitespace)
            || model.slug.chars().any(char::is_control)
            || !seen.insert(model.slug.clone())
        {
            continue;
        }
        model.listed = entry
            .get("visibility")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|visibility| visibility == "list");
        models.push(model);
    }
    Some(models)
}

#[derive(Default)]
struct ImportCache {
    key: Option<(PathBuf, SystemTime, u64)>,
    models: Vec<CatalogModel>,
}

static IMPORT: OnceLock<Mutex<ImportCache>> = OnceLock::new();

fn imported_models() -> Vec<CatalogModel> {
    let Some(path) = catalog_path() else {
        return Vec::new();
    };
    let mut cache = IMPORT
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some((modified, len)) = fs::metadata(&path)
        .ok()
        .and_then(|meta| Some((meta.modified().ok()?, meta.len())))
    else {
        return if cache.key.as_ref().is_some_and(|key| key.0 == path) {
            cache.models.clone()
        } else {
            Vec::new()
        };
    };
    if len > MAX_CATALOG_BYTES as u64 {
        return Vec::new();
    }
    let key = (path, modified, len);
    if cache.key.as_ref() != Some(&key) {
        match fs::read(&key.0)
            .ok()
            .and_then(|bytes| parse_catalog(&bytes))
        {
            Some(models) => {
                cache.models = models;
                cache.key = Some(key);
            }
            None if cache.key.as_ref().is_none_or(|old| old.0 != key.0) => return Vec::new(),
            None => {}
        }
    }
    cache.models.clone()
}

pub fn fallback_snapshot() -> CatalogSnapshot {
    use super::model_allowlist::FALLBACK_MODELS;

    let mut models: Vec<_> = FALLBACK_MODELS
        .iter()
        .map(|slug| {
            let lite = matches!(
                *slug,
                "gpt-5.6-luna"
                    | "gpt-5.6-sol"
                    | "gpt-5.6-terra"
                    | "gpt-6-astra"
                    | "gpt-6-luna"
                    | "gpt-6-sol"
                    | "gpt-6.1-sol"
            );
            // Context-only offline fallback, verified in OpenAI Codex models.json
            // at 61f34c35af77777db4d77d0ea2aeb8547a6b4523. Other capabilities
            // remain unknown until discovery; do not guess limits for older IDs.
            let known_context = lite || *slug == "gpt-5.5";
            CatalogModel {
                slug: (*slug).to_string(),
                use_responses_lite: Some(lite),
                context_window: known_context.then_some(272_000),
                max_context_window: known_context.then_some(if lite { 872_000 } else { 272_000 }),
                ..Default::default()
            }
        })
        .collect();
    for imported in imported_models() {
        if let Some(existing) = models.iter_mut().find(|model| model.slug == imported.slug) {
            let mut imported = imported;
            imported.use_responses_lite =
                imported.use_responses_lite.or(existing.use_responses_lite);
            imported.context_window = imported.context_window.or(existing.context_window);
            imported.max_context_window =
                imported.max_context_window.or(existing.max_context_window);
            *existing = imported;
        } else {
            models.push(imported);
        }
    }
    CatalogSnapshot::new(models, false)
}

pub fn catalog_models() -> Vec<CatalogModel> {
    scoped_catalog().map_or_else(
        || fallback_snapshot().models,
        |catalog| catalog.models.clone(),
    )
}

pub fn catalog_model(slug: &str) -> Option<CatalogModel> {
    match scoped_catalog() {
        Some(catalog) => catalog
            .models
            .iter()
            .find(|model| model.slug == slug)
            .cloned(),
        None => fallback_snapshot()
            .models
            .into_iter()
            .find(|model| model.slug == slug),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_real_capability_names_without_inventing_missing_capabilities() {
        let models = parse_catalog(&serde_json::to_vec(&json!({"models": [
            {"slug":"new-model", "context_window":272000,"max_context_window":872000,
             "effective_context_window_percent":95,"auto_compact_token_limit":240000,
             "default_reasoning_level":"low", "supported_reasoning_levels":[{"effort":"low"},{"effort":"ultra"}],
             "supports_reasoning_summary_parameter":false,"support_verbosity":true,"default_verbosity":"low",
             "service_tiers":[{"id":"priority","name":"Fast"}],"input_modalities":["text"],
             "use_responses_lite":false},
            {"slug":"hidden", "visibility":"hide"},
            {"slug":"unavailable","supported_in_api":false},
            {"slug":"minimal"}, {"nonsense":true}, {"slug":""}, {"slug":"bad id"}, {"slug":"new-model"}
        ]})).unwrap()).unwrap();
        assert_eq!(models.len(), 4);
        let model = &models[0];
        assert_eq!(model.context_window, Some(272_000));
        assert_eq!(model.max_context_window, Some(872_000));
        assert_eq!(model.effective_context_window_percent, Some(95));
        assert_eq!(model.auto_compact_token_limit, Some(240_000));
        assert_eq!(model.supports_effort("ultra"), Some(true));
        assert_eq!(model.supports_effort("none"), Some(false));
        assert_eq!(model.supports_reasoning_summaries, Some(false));
        assert_eq!(model.supports_verbosity, Some(true));
        assert_eq!(model.supports_service_tier("priority"), Some(true));
        assert_eq!(model.supports_service_tier("ultrafast"), Some(false));
        assert!(!models[1].listed);
        assert!(!models[2].supported_in_api);
        assert_eq!(models[3].use_responses_lite, None);
        assert_eq!(models[3].supports_effort("low"), None);
        assert_eq!(models[3].supports_service_tier("priority"), None);
        let legacy = parse_catalog(br#"{"models":[{"slug":"legacy","supports_reasoning_summaries":false,"supports_verbosity":false}]}"#).unwrap();
        assert_eq!(legacy[0].supports_reasoning_summaries, None);
        assert_eq!(legacy[0].supports_verbosity, None);
    }

    #[test]
    fn bounds_and_envelope_are_validated() {
        for invalid in [b"not json".as_slice(), b"[]", b"{}", b"{\"models\":{}}"] {
            assert!(parse_catalog(invalid).is_none());
        }
        assert!(parse_catalog(&vec![b' '; MAX_CATALOG_BYTES + 1]).is_none());
        let many = json!({"models": vec![json!({"slug":"a"}); MAX_CATALOG_MODELS + 1]});
        assert!(parse_catalog(&serde_json::to_vec(&many).unwrap()).is_none());
        assert_eq!(parse_catalog(b"{\"models\":[]}").unwrap(), Vec::new());
    }

    #[tokio::test]
    async fn concurrent_operations_keep_their_own_catalogs() {
        let snapshot = |slug: &str| {
            Arc::new(CatalogSnapshot::new(
                vec![CatalogModel {
                    slug: slug.into(),
                    ..Default::default()
                }],
                true,
            ))
        };
        let (a, b) = tokio::join!(
            scope_catalog(snapshot("account-a"), async {
                tokio::task::yield_now().await;
                catalog_models()[0].slug.clone()
            }),
            scope_catalog(snapshot("account-b"), async {
                tokio::task::yield_now().await;
                catalog_models()[0].slug.clone()
            })
        );
        assert_eq!(a, "account-a");
        assert_eq!(b, "account-b");
        assert!(scoped_catalog().is_none());
    }
}
