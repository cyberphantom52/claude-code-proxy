use crate::{
    provider::Provider,
    providers::codex::translate::{
        model_allowlist::{self, tier_model_variants},
        model_catalog,
    },
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub struct Registry {
    models: BTreeMap<String, Vec<String>>,
    handlers: BTreeMap<String, Arc<dyn Provider>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        let provider: Arc<dyn Provider> = Arc::new(crate::providers::codex::CodexProvider::new());
        Self::from_providers([provider])
    }

    pub fn from_providers(providers: impl IntoIterator<Item = Arc<dyn Provider>>) -> Self {
        let mut models = BTreeMap::new();
        let mut handlers = BTreeMap::new();
        for provider in providers {
            let name = provider.name().to_string();
            models.insert(name.clone(), provider.supported_models());
            handlers.insert(name, provider);
        }
        Self { models, handlers }
    }

    pub fn provider(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.handlers.get(name).cloned()
    }

    pub fn supported_models_for(&self, provider: &str) -> Vec<String> {
        let mut models = self.models.get(provider).cloned().unwrap_or_default();
        if provider == "codex"
            && let Some(catalog) = model_catalog::scoped_catalog()
        {
            if catalog.authoritative {
                models.clear();
            }
            models.extend(expand_codex_models());
            models.sort_unstable();
            models.dedup();
        }
        models.sort_unstable();
        models
    }

    pub fn all_supported_models(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for provider in self.handlers.keys() {
            for model in self.supported_models_for(provider) {
                out.push((model, provider.clone()));
            }
        }
        out
    }

    pub fn grouped_models(&self) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        for provider in self.handlers.keys() {
            out.insert(provider.clone(), self.supported_models_for(provider));
        }
        out
    }

    pub fn provider_for_model(&self, model: &str) -> Option<Arc<dyn Provider>> {
        if !model_allowlist::is_valid_model_for_codex(model) {
            return None;
        }
        for (name, models) in &self.models {
            if models.iter().any(|candidate| candidate == model) {
                return self.handlers.get(name).cloned();
            }
        }

        self.handlers.get("codex").cloned()
    }

    pub fn unknown_model_message(&self) -> String {
        let mut parts = Vec::new();
        for (provider, models) in self.grouped_models() {
            parts.push(format!("{}: {}", provider, models.join(", ")));
        }
        format!("Known models: {}.", parts.join("; "))
    }
}

pub(crate) fn expand_codex_models() -> Vec<String> {
    let mut models = BTreeSet::new();
    // A live account catalog is complete, not an extension of compiled IDs.
    let authoritative =
        model_catalog::scoped_catalog().is_some_and(|catalog| catalog.authoritative);
    let catalog = model_allowlist::listed_catalog_models();
    for model in model_allowlist::FALLBACK_MODELS
        .iter()
        .filter(|_| !authoritative)
        .map(|model| (*model).to_string())
        .chain(catalog)
    {
        models.extend(tier_model_variants(&model));
        models.insert(model);
    }
    models.into_iter().collect()
}
