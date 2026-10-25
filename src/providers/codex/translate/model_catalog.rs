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
    sync::{Arc, LazyLock, Mutex},
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
    let mut value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let serde_json::Value::Array(entries) = value.as_object_mut()?.remove("models")? else {
        return None;
    };
    if entries.len() > MAX_CATALOG_MODELS {
        return None;
    }
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for entry in entries {
        let listed = entry
            .get("visibility")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|visibility| visibility == "list");
        let Ok(mut model) = serde_json::from_value::<CatalogModel>(entry) else {
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
        model.listed = listed;
        models.push(model);
    }
    Some(models)
}

#[derive(Default)]
struct ImportCache {
    key: Option<(PathBuf, SystemTime, u64)>,
    snapshot: Option<Arc<CatalogSnapshot>>,
}

static IMPORT: LazyLock<Mutex<ImportCache>> = LazyLock::new(Default::default);

fn builtin_snapshot() -> Arc<CatalogSnapshot> {
    use super::model_allowlist::FALLBACK_MODELS;

    static BUILTIN: LazyLock<Arc<CatalogSnapshot>> = LazyLock::new(|| {
        let models = FALLBACK_MODELS
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
                    max_context_window: known_context.then_some(if lite {
                        872_000
                    } else {
                        272_000
                    }),
                    ..Default::default()
                }
            })
            .collect();
        Arc::new(CatalogSnapshot::new(models, false))
    });
    BUILTIN.clone()
}

pub fn fallback_snapshot() -> Arc<CatalogSnapshot> {
    let Some(path) = catalog_path() else {
        return builtin_snapshot();
    };
    let mut cache = IMPORT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some((modified, len)) = fs::metadata(&path)
        .ok()
        .and_then(|meta| Some((meta.modified().ok()?, meta.len())))
    else {
        return if cache.key.as_ref().is_some_and(|key| key.0 == path) {
            cache.snapshot.clone().unwrap_or_else(builtin_snapshot)
        } else {
            builtin_snapshot()
        };
    };
    if len > MAX_CATALOG_BYTES as u64 {
        return builtin_snapshot();
    }
    let key = (path, modified, len);
    if cache.key.as_ref() != Some(&key) {
        match fs::read(&key.0)
            .ok()
            .and_then(|bytes| parse_catalog(&bytes))
        {
            Some(imported) => {
                let mut models = builtin_snapshot().models.clone();
                for mut imported in imported {
                    if let Some(existing) =
                        models.iter_mut().find(|model| model.slug == imported.slug)
                    {
                        imported.use_responses_lite =
                            imported.use_responses_lite.or(existing.use_responses_lite);
                        imported.context_window =
                            imported.context_window.or(existing.context_window);
                        imported.max_context_window =
                            imported.max_context_window.or(existing.max_context_window);
                        *existing = imported;
                    } else {
                        models.push(imported);
                    }
                }
                cache.snapshot = Some(Arc::new(CatalogSnapshot::new(models, false)));
                cache.key = Some(key);
            }
            None if cache.key.as_ref().is_none_or(|old| old.0 != key.0) => {
                return builtin_snapshot();
            }
            None => {}
        }
    }
    cache.snapshot.clone().unwrap_or_else(builtin_snapshot)
}

pub fn catalog_models() -> Arc<CatalogSnapshot> {
    scoped_catalog().unwrap_or_else(fallback_snapshot)
}

/// A model view that keeps its immutable catalog alive without copying metadata.
#[derive(Debug, Clone)]
pub struct CatalogModelRef {
    catalog: Arc<CatalogSnapshot>,
    index: usize,
}

impl std::ops::Deref for CatalogModelRef {
    type Target = CatalogModel;

    fn deref(&self) -> &Self::Target {
        &self.catalog.models[self.index]
    }
}

pub fn catalog_model(slug: &str) -> Option<CatalogModelRef> {
    let catalog = catalog_models();
    let index = catalog.models.iter().position(|model| model.slug == slug)?;
    Some(CatalogModelRef { catalog, index })
}
