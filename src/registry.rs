use crate::{
    provider::Provider,
    providers::codex::translate::{
        model_allowlist::{self, tier_model_variants},
        model_catalog,
    },
};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

pub const ANTHROPIC_STYLE_ALIASES: &[&str] = &[
    "haiku",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "claude-haiku-5-5",
    "sonnet",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "claude-sonnet-5-5",
    "opus",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-opus-5-5",
    "fable",
    "claude-fable-5",
    "claude-fable-5-1",
];

pub(crate) const CODEX_MODELS: &[&str] = model_allowlist::FALLBACK_MODELS;

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

    pub fn list_provider_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.handlers.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    pub fn provider(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.handlers.get(name).cloned()
    }

    pub fn supported_models_for(&self, provider: &str) -> Vec<String> {
        let mut models = self.models.get(provider).cloned().unwrap_or_default();
        if provider == "codex" {
            if let Some(catalog) = model_catalog::scoped_catalog() {
                if catalog.authoritative {
                    models.clear();
                }
                models.extend(expand_codex_models());
                models.sort_unstable();
                models.dedup();
            }
            for alias in ANTHROPIC_STYLE_ALIASES {
                let target =
                    model_allowlist::resolve_model_request_with_config_override(alias, false);
                if model_catalog::catalog_model(&target.model).is_some()
                    && !models.iter().any(|value| value == alias)
                {
                    models.push((*alias).to_string());
                }
            }
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

    pub fn provider_for_model(&self, raw_model: &str) -> Option<Arc<dyn Provider>> {
        let normalized = normalize_incoming_model(raw_model);
        if !model_allowlist::is_valid_model_for_codex(&normalized) {
            return None;
        }
        for (name, models) in &self.models {
            if models.iter().any(|candidate| candidate == &normalized) {
                return self.handlers.get(name).cloned();
            }
        }

        self.handlers.get("codex").cloned()
    }

    pub fn unknown_model_message(&self) -> String {
        let mut parts = Vec::new();
        for (provider, models) in self.grouped_models() {
            let mut models = models;
            models.sort_unstable();
            parts.push(format!("{}: {}", provider, models.join(", ")));
        }
        format!("Known models: {}.", parts.join("; "))
    }
}

pub fn normalize_incoming_model(model: &str) -> String {
    let suffix = "[1m]";
    if model.len() >= suffix.len() && model.to_ascii_lowercase().ends_with(suffix) {
        return model[..model.len() - suffix.len()].to_string();
    }
    model.to_string()
}

pub fn is_anthropic_alias(model: &str) -> bool {
    model_allowlist::is_anthropic_alias(model)
}

pub(crate) fn expand_codex_models() -> Vec<String> {
    let mut set = HashSet::new();
    let mut out = Vec::new();
    // A live account catalog is complete, not an extension of compiled IDs.
    let authoritative =
        model_catalog::scoped_catalog().is_some_and(|catalog| catalog.authoritative);
    let catalog = model_allowlist::listed_catalog_models();
    for model in CODEX_MODELS
        .iter()
        .filter(|_| !authoritative)
        .map(|model| (*model).to_string())
        .chain(catalog)
    {
        if set.insert(model.clone()) {
            out.push(model.clone());
        }
        for variant in tier_model_variants(&model) {
            if set.insert(variant.clone()) {
                out.push(variant);
            }
        }
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_rejects_only_malformed_ids_not_new_model_names() {
        let registry = Registry::new();
        for model in ["", "bad model", "bad\nmodel", "bad\tmodel"] {
            assert!(registry.provider_for_model(model).is_none());
        }
        assert!(registry.provider_for_model(&"x".repeat(257)).is_none());
        assert_eq!(
            registry
                .provider_for_model("unadvertised-model")
                .unwrap()
                .name(),
            "codex"
        );
    }

    #[test]
    fn normalize_model_trims_hint() {
        assert_eq!(normalize_incoming_model("gpt-5.4-fast[1m]"), "gpt-5.4-fast");
        assert_eq!(normalize_incoming_model("gpt-5.4-fast"), "gpt-5.4-fast");
        assert_eq!(
            normalize_incoming_model("gpt-6-astra-ultrafast[1m]"),
            "gpt-6-astra-ultrafast"
        );
    }

    #[test]
    fn syntactically_valid_tier_ids_route_without_catalog_admission() {
        let registry = Registry::new();
        for model in [
            "gpt-6-astra-ultrafast",
            "gpt-6-astra-ultrafast[1m]",
            "gpt-6.1-sol-ultrafast",
        ] {
            assert_eq!(registry.provider_for_model(model).unwrap().name(), "codex");
        }
        for model in ["gpt-6-sol-ultrafast", "gpt-6-luna-ultrafast"] {
            assert_eq!(registry.provider_for_model(model).unwrap().name(), "codex");
        }
    }

    #[test]
    fn alias_routes_to_codex() {
        let registry = Registry::new();
        let p = registry.provider_for_model("haiku");
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn opus_4_8_routes_to_codex() {
        let registry = Registry::new();
        let p = registry.provider_for_model("claude-opus-4-8");
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn claude_5_aliases_route_to_codex() {
        let registry = Registry::new();
        for model in [
            "claude-sonnet-5",
            "claude-opus-5",
            "claude-opus-5-5",
            "fable",
            "claude-fable-5",
        ] {
            let p = registry.provider_for_model(model);
            assert!(p.is_some(), "{model} should route to a provider");
            assert_eq!(p.expect("provider").name(), "codex");
        }
    }

    #[test]
    fn gpt_6_models_route_to_codex() {
        let registry = Registry::new();
        for model in [
            "gpt-6-sol",
            "gpt-6-sol-fast",
            "gpt-6-luna",
            "gpt-6.1-sol",
            "gpt-6.1-sol-fast",
        ] {
            assert_eq!(registry.provider_for_model(model).unwrap().name(), "codex");
        }
    }

    #[test]
    fn removed_backends_are_not_registered_and_ids_only_route_to_codex() {
        let registry = Registry::new();
        assert_eq!(registry.list_provider_names(), ["codex"]);
        for backend in ["kimi", "grok", "cursor", "opencode"] {
            assert!(registry.provider(backend).is_none());
            assert!(registry.supported_models_for(backend).is_empty());
        }
        for model in [
            "kimi-for-coding",
            "kimi-k2.6",
            "kimi-k3",
            "k2.6",
            "k3",
            "kimi-k2.7-code",
            "grok-composer-2.5-fast",
            "grok-4.5",
            "grok-4.6",
            "grok-4.7",
            "cursor",
            "cursor-agent",
            "cursor-composer",
            "cursor-composer-fast",
            "cursor-plan",
            "cursor-ask",
            "composer-2.5",
            "composer-2.5-fast",
            "cursor:gpt-5.5",
            "cursor-plan:gpt-5.5",
            "cursor-ask:gpt-5.5",
            "opencode-go/kimi-k2.6",
            "opencode-go/gpt-5.6-luna",
            "opencode-go/grok-4.5",
        ] {
            assert_eq!(registry.provider_for_model(model).unwrap().name(), "codex");
            assert!(
                registry
                    .provider_for_model(&format!("{model}[1m]"))
                    .is_some(),
                "{model}"
            );
            assert!(
                !registry
                    .all_supported_models()
                    .iter()
                    .any(|(id, _)| id == model),
                "{model}"
            );
        }
    }
}
