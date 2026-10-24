use crate::config;

use super::model_catalog::{catalog_model, catalog_models};
use super::request::{ServiceTier, effort_wire_value};

/// Offline bootstrap metadata IDs only, never an availability gate.
/// Authenticated discovery drives listings; any well-formed ID can reach Codex.
pub const FALLBACK_MODELS: &[&str] = &[
    "gpt-5.2",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-6-astra",
    "gpt-6-luna",
    "gpt-6-sol",
    "gpt-6.1-sol",
];

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: String,
    pub service_tier: Option<ServiceTier>,
}

const FAST_SUFFIX: &str = "-fast";
const ULTRAFAST_SUFFIX: &str = "-ultrafast";
/// Legacy offline aliases only; live service_tiers always take precedence.
pub const ULTRAFAST_MODELS: &[&str] = &["gpt-6-astra", "gpt-6.1-sol"];

fn offers_ultrafast(model: &str) -> bool {
    advertised_service_tier(model, "ultrafast").unwrap_or_else(|| ULTRAFAST_MODELS.contains(&model))
}

/// Local suffixes apply to known base models; exact discovered IDs always win.
/// Unknown opaque IDs are forwarded unchanged, not guessed to be aliases.
pub fn split_tier_suffix(model: &str) -> Option<(&str, ServiceTier)> {
    if catalog_model(model).is_some() {
        return None;
    }
    if let Some(base) = model.strip_suffix(ULTRAFAST_SUFFIX) {
        let metadata = catalog_model(base)?;
        return (metadata.service_tiers.is_some() || offers_ultrafast(base))
            .then_some((base, ServiceTier::Ultrafast));
    }
    let base = model.strip_suffix(FAST_SUFFIX)?;
    if base.ends_with(FAST_SUFFIX) || base.ends_with(ULTRAFAST_SUFFIX) {
        return None;
    }
    catalog_model(base).map(|_| (base, ServiceTier::Priority))
}

/// Discovered labels stay exact; local tier aliases retain their visible suffix.
pub fn model_display_name(model: &str) -> Option<String> {
    let base = split_tier_suffix(model).map_or(model, |(base, _)| base);
    let name = catalog_model(base)?
        .display_name
        .clone()
        .filter(|name| !name.is_empty())?;
    Some(format!("{name}{}", &model[base.len()..]))
}

/// Compatibility name: validate syntax, never gate IDs against a local catalog.
/// Codex is authoritative for model availability and subscription access.
pub fn is_allowed_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 256
        && !model
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

/// Models visible in the subscription picker. Hidden entries remain requestable.
pub fn listed_catalog_models() -> Vec<String> {
    catalog_models()
        .models
        .iter()
        .filter(|entry| entry.listed)
        .map(|entry| entry.slug.clone())
        .collect()
}

/// Local tier-suffixed names advertised next to a registered model.
pub fn tier_model_variants(model: &str) -> Vec<String> {
    if !is_allowed_model(model) || catalog_model(model).is_none() {
        return Vec::new();
    }
    let mut variants = Vec::new();
    if advertised_service_tier(model, "priority") != Some(false) {
        variants.push(format!("{model}{FAST_SUFFIX}"));
    }
    if offers_ultrafast(model) {
        variants.push(format!("{model}{ULTRAFAST_SUFFIX}"));
    }
    variants
}

fn advertised_service_tier(model: &str, tier: &str) -> Option<bool> {
    catalog_model(model)?.supports_service_tier(tier)
}

/// Validate an explicit tier without downgrading it to a different speed.
pub fn resolve_service_tier_value(model: &str, tier: &str) -> anyhow::Result<ServiceTier> {
    let tier = if tier == "fast" { "priority" } else { tier };
    let standard = matches!(tier, "auto" | "default" | "flex");
    let advertised = advertised_service_tier(model, tier);
    if tier.is_empty()
        || tier.len() > 256
        || tier.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        anyhow::bail!("Invalid service_tier '{tier}'");
    }
    if !standard && advertised == Some(false) {
        let metadata = catalog_model(model);
        let allowed = metadata
            .as_ref()
            .and_then(|entry| entry.service_tiers.as_ref())
            .map(|tiers| {
                tiers
                    .iter()
                    .map(|entry| entry.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_else(|| "priority, ultrafast, flex".to_string());
        anyhow::bail!(
            "Model '{model}' does not support service_tier '{tier}'. Allowed: auto, default, flex{separator}{allowed}",
            separator = if allowed.is_empty() { "" } else { ", " }
        );
    }
    Ok(ServiceTier::from(tier.to_string()))
}

pub const KNOWN_EFFORTS: &[&str] = &[
    "none",
    "minimal",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "ultra",
    "persistent",
    "disabled",
];

pub fn resolve_model_effort(
    model: &str,
    requested: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let metadata = catalog_model(model);
    let effort = requested
        .map(str::to_string)
        .or_else(|| metadata.as_ref()?.default_reasoning_level.clone());
    // Codex harness aliases are not wire effort names.
    let effort = effort.map(|effort| match effort.as_str() {
        "ultra" => metadata
            .as_ref()
            .and_then(|entry| entry.multi_agent_reasoning_effort.clone())
            .filter(|value| {
                metadata
                    .as_ref()
                    .and_then(|entry| entry.supported_reasoning_levels.as_ref())
                    .is_some_and(|levels| levels.iter().any(|level| level.effort == *value))
            })
            .or_else(|| {
                metadata
                    .as_ref()?
                    .supported_reasoning_levels
                    .as_ref()?
                    .iter()
                    .find(|level| level.effort == "max")
                    .map(|level| level.effort.clone())
            })
            .or_else(|| {
                metadata
                    .as_ref()?
                    .supported_reasoning_levels
                    .as_ref()?
                    .iter()
                    .rev()
                    .find(|level| level.effort != "ultra")
                    .map(|level| level.effort.clone())
            })
            .unwrap_or_else(|| "medium".to_string()),
        "persistent" => "disabled".to_string(),
        _ => effort,
    });
    if let Some(effort) = &effort {
        let levels = metadata
            .as_ref()
            .and_then(|entry| entry.supported_reasoning_levels.as_ref());
        anyhow::ensure!(
            !effort.is_empty()
                && effort.len() <= 256
                && !effort.chars().any(|c| c.is_whitespace() || c.is_control()),
            "Invalid reasoning effort '{effort}'"
        );
        effort_wire_value(effort)?;
        let supported =
            levels.is_none_or(|levels| levels.iter().any(|level| level.effort == *effort));
        if !supported {
            let allowed = levels
                .map(|levels| {
                    levels
                        .iter()
                        .map(|level| level.effort.as_str())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| KNOWN_EFFORTS.to_vec())
                .join(", ");
            anyhow::bail!(
                "Model '{model}' does not advertise reasoning effort '{effort}'. Allowed: {allowed}"
            );
        }
    }
    Ok(effort)
}

pub fn model_reasoning_summary(
    model: &str,
    requested: Option<&str>,
    fallback: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let metadata = catalog_model(model);
    if metadata
        .as_ref()
        .and_then(|entry| entry.supports_reasoning_summaries)
        == Some(false)
    {
        if requested.is_some_and(|summary| !matches!(summary, "off" | "none")) {
            anyhow::bail!("Model '{model}' does not support reasoning summaries");
        }
        return Ok(None);
    }
    let summary = requested
        .map(str::to_string)
        .or_else(|| metadata.as_ref()?.default_reasoning_summary.clone())
        .or_else(|| fallback.map(str::to_string));
    Ok(summary.filter(|summary| !matches!(summary.as_str(), "off" | "none")))
}

pub fn model_verbosity(model: &str, requested: Option<&str>) -> anyhow::Result<Option<String>> {
    let metadata = catalog_model(model);
    if metadata.as_ref().and_then(|entry| entry.supports_verbosity) == Some(false) {
        if requested.is_some() {
            anyhow::bail!("Model '{model}' does not support text verbosity");
        }
        return Ok(None);
    }
    Ok(requested
        .map(str::to_string)
        .or_else(|| metadata.as_ref()?.default_verbosity.clone())
        .or_else(|| {
            (metadata
                .as_ref()
                .and_then(|entry| entry.supports_verbosity)
                .is_none())
            .then(|| "low".to_string())
        }))
}

pub fn model_parallel_tool_calls(
    model: &str,
    lite: bool,
    requested: Option<bool>,
) -> anyhow::Result<bool> {
    let supported = !lite
        && catalog_model(model).and_then(|entry| entry.supports_parallel_tool_calls) != Some(false);
    if requested == Some(true) && !supported {
        anyhow::bail!(
            "Model '{model}' does not support parallel_tool_calls=true on the selected {} lane",
            if lite { "Responses Lite" } else { "Responses" }
        );
    }
    Ok(requested.unwrap_or(supported))
}

/// Inspect only content containers, never schemas or arbitrary function arguments.
pub fn validate_input_modalities(model: &str, input: &serde_json::Value) -> anyhow::Result<()> {
    validate_input_modalities_iter(model, std::iter::once(input))
}

pub(super) fn validate_input_modalities_iter<'a>(
    model: &str,
    mut input: impl Iterator<Item = &'a serde_json::Value>,
) -> anyhow::Result<()> {
    fn has_images(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Array(values) => values.iter().any(has_images),
            serde_json::Value::Object(object) => {
                matches!(
                    object.get("type").and_then(serde_json::Value::as_str),
                    Some("image" | "image_url" | "input_image")
                ) || ["content", "output"]
                    .iter()
                    .any(|key| object.get(*key).is_some_and(has_images))
            }
            _ => false,
        }
    }
    if input.any(has_images)
        && let Some(metadata) = catalog_model(model)
        && let Some(modalities) = &metadata.input_modalities
        && !modalities.iter().any(|modality| modality == "image")
    {
        anyhow::bail!(
            "Model '{model}' does not support image inputs. Allowed input modalities: {}",
            modalities.join(", ")
        );
    }
    Ok(())
}

/// An operator pin may agree with a request, but must never silently redirect it.
pub fn validate_model_configuration(model: &str, apply_override: bool) -> anyhow::Result<()> {
    let configured = apply_override.then(config::codex_model).flatten();
    validate_model_override(model, configured.as_deref())
}

fn validate_model_override(model: &str, configured: Option<&str>) -> anyhow::Result<()> {
    if let Some(configured) = configured.filter(|value| !value.is_empty()) {
        let requested = resolve_model_request(model);
        let forced = resolve_model_request(configured);
        if requested.model != forced.model
            || (forced.service_tier.is_some() && requested.service_tier != forced.service_tier)
        {
            anyhow::bail!(
                "Requested model '{model}' resolves to '{}', conflicting with configured Codex model '{configured}'. The proxy will not substitute models; remove the model pin or request the configured model explicitly.",
                requested.model
            );
        }
    }
    Ok(())
}

pub fn resolve_model_request(model: &str) -> ResolvedModel {
    if !is_allowed_model(model) {
        return ResolvedModel {
            model: model.to_string(),
            service_tier: None,
        };
    }
    match split_tier_suffix(model) {
        Some((base, tier)) => ResolvedModel {
            model: base.to_string(),
            service_tier: Some(tier),
        },
        None => ResolvedModel {
            model: model.to_string(),
            service_tier: None,
        },
    }
}

#[derive(Debug, Clone)]
pub struct ModelNotAllowedError {
    pub model: String,
}

impl std::fmt::Display for ModelNotAllowedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Invalid model ID: '{}'. Use 1-256 bytes without whitespace or control characters",
            self.model
        )
    }
}

pub fn assert_allowed_model(model: &str) -> Result<(), ModelNotAllowedError> {
    if is_allowed_model(model) {
        Ok(())
    } else {
        Err(ModelNotAllowedError {
            model: model.to_string(),
        })
    }
}

/// The gpt-5.6 family defaults to the Responses Lite lane. The lite lane
/// requires `parallel_tool_calls: false` (the backend rejects the request
/// with 400 `unsupported_value` otherwise), so every tool call is serialized
/// into its own assistant turn there. `gpt-5.6-sol` and `gpt-5.6-terra` also
/// exist on the full Responses lane, where parallel tool calls work;
/// `codex.fullLane` / `CCP_CODEX_FULL_LANE` opts them into it. `gpt-5.6-luna`
/// stays on the lite lane unless discovery explicitly advertises otherwise.
pub fn uses_responses_lite(model: &str) -> bool {
    uses_responses_lite_with_full_lane(model, config::codex_full_lane())
}

fn uses_responses_lite_with_full_lane(model: &str, full_lane: bool) -> bool {
    if full_lane && matches!(model, "gpt-5.6-sol" | "gpt-5.6-terra") {
        return false;
    }
    catalog_model(model)
        .and_then(|entry| entry.use_responses_lite)
        .unwrap_or(false)
}

/// Known Lite-only Luna IDs cannot run hosted tools on the full lane.
/// The standalone alpha/search endpoint does not use this restriction.
pub fn validate_hosted_web_search(model: &str) -> anyhow::Result<()> {
    if matches!(model, "gpt-5.6-luna" | "gpt-6-luna")
        && catalog_model(model).and_then(|entry| entry.use_responses_lite) != Some(false)
    {
        anyhow::bail!(
            "Model '{model}' does not support hosted web_search on the full Responses lane. Use standalone search or explicitly choose a model supporting the full lane; the proxy will not substitute another model."
        );
    }
    Ok(())
}

pub fn is_valid_model_for_codex(model: &str) -> bool {
    is_allowed_model(model)
}
