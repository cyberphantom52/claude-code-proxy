use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::anthropic::schema::{Message, MessagesRequest};
use crate::config;
use crate::providers::translate_shared::{
    ContentBlock, flatten_system_text, normalize_content, parallel_tool_calls,
    validated_image_data_url,
};

use super::model_allowlist::{
    KNOWN_EFFORTS, model_parallel_tool_calls, model_reasoning_summary, model_verbosity,
    resolve_model_effort, resolve_service_tier_value, validate_hosted_web_search,
    validate_input_modalities_iter,
};
use super::read_rewrite::{ReadOffsetRewrite, read_offset_rewrite};
use super::reasoning_signature::decode_reasoning_signature;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Ultra,
    Other(String),
}

/// Codex custom numeric efforts use unsigned JSON integers, not numeric strings.
pub(crate) fn effort_wire_value(effort: &str) -> anyhow::Result<Value> {
    if !effort.is_empty() && effort.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(Value::from(effort.parse::<u64>().map_err(|_| {
            anyhow::anyhow!("Numeric reasoning effort must fit an unsigned 64-bit integer")
        })?));
    }
    anyhow::ensure!(
        effort.parse::<f64>().is_err(),
        "Numeric reasoning effort must be an unsigned 64-bit integer"
    );
    Ok(Value::String(effort.to_string()))
}

impl Serialize for Effort {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        effort_wire_value(&self.to_string())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Effort {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let effort = match value {
            Value::String(effort) => effort,
            Value::Number(number) => number
                .as_u64()
                .ok_or_else(|| {
                    serde::de::Error::custom(
                        "reasoning effort must be a string or unsigned 64-bit integer",
                    )
                })?
                .to_string(),
            _ => {
                return Err(serde::de::Error::custom(
                    "reasoning effort must be a string or unsigned 64-bit integer",
                ));
            }
        };
        effort_wire_value(&effort).map_err(serde::de::Error::custom)?;
        Ok(Self::from(effort))
    }
}

impl From<String> for Effort {
    fn from(value: String) -> Self {
        match value.as_str() {
            "none" => Self::None,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::Xhigh,
            "max" => Self::Max,
            "ultra" => Self::Ultra,
            _ => Self::Other(value),
        }
    }
}

impl From<Effort> for String {
    fn from(value: Effort) -> Self {
        value.to_string()
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effort::None => write!(f, "none"),
            Effort::Minimal => write!(f, "minimal"),
            Effort::Low => write!(f, "low"),
            Effort::Medium => write!(f, "medium"),
            Effort::High => write!(f, "high"),
            Effort::Xhigh => write!(f, "xhigh"),
            Effort::Max => write!(f, "max"),
            Effort::Ultra => write!(f, "ultra"),
            Effort::Other(value) => f.write_str(value),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(from = "String", into = "String")]
pub enum ServiceTier {
    Priority,
    Ultrafast,
    Flex,
    Other(String),
}

impl From<String> for ServiceTier {
    fn from(value: String) -> Self {
        match value.as_str() {
            "priority" => Self::Priority,
            "ultrafast" => Self::Ultrafast,
            "flex" => Self::Flex,
            _ => Self::Other(value),
        }
    }
}

impl From<ServiceTier> for String {
    fn from(value: ServiceTier) -> Self {
        match value {
            ServiceTier::Priority => "priority".to_string(),
            ServiceTier::Ultrafast => "ultrafast".to_string(),
            ServiceTier::Flex => "flex".to_string(),
            ServiceTier::Other(value) => value,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesToolChoiceMode {
    Auto,
    None,
    Required,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesToolChoice {
    Mode(ResponsesToolChoiceMode),
    AllowedTools {
        r#type: String,
        mode: String,
        tools: Vec<Value>,
    },
    Function {
        r#type: String,
        name: String,
    },
    WebSearch {
        r#type: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(serialize = "I: Serialize"))]
pub struct ResponsesRequest<I = Vec<ResponsesInputItem>> {
    #[serde(skip)]
    pub stop_sequences: Vec<String>,
    #[serde(skip)]
    pub codex_auto_review: bool,
    #[serde(skip)]
    pub(crate) turn_state: super::super::client::CodexTurnState,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(serialize_with = "serialize_native_input")]
    pub input: I,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ResponsesToolChoice>,
    pub store: bool,
    pub stream: bool,
    pub parallel_tool_calls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_metadata: Option<std::collections::HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    pub text: ResponsesText,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ResponsesReasoning>,
}

impl ResponsesRequest {
    pub(crate) fn with_input<I>(&self, input: I) -> ResponsesRequest<I> {
        ResponsesRequest {
            stop_sequences: self.stop_sequences.clone(),
            codex_auto_review: self.codex_auto_review,
            turn_state: self.turn_state.clone(),
            model: self.model.clone(),
            instructions: self.instructions.clone(),
            input,
            tools: self.tools.clone(),
            tool_choice: self.tool_choice.clone(),
            store: self.store,
            stream: self.stream,
            parallel_tool_calls: self.parallel_tool_calls,
            include: self.include.clone(),
            client_metadata: self.client_metadata.clone(),
            service_tier: self.service_tier.clone(),
            prompt_cache_key: self.prompt_cache_key.clone(),
            text: self.text.clone(),
            reasoning: self.reasoning.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesReasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesText {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ResponsesTextFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ResponsesTextFormat {
    Text,
    JsonObject,
    JsonSchema {
        name: String,
        schema: Value,
        #[serde(default)]
        strict: Option<bool>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ResponsesInputItem {
    #[serde(rename = "additional_tools")]
    AdditionalTools {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        role: String,
        tools: Vec<Value>,
    },
    #[serde(rename = "message")]
    Message {
        role: String,
        content: Vec<ResponsesContentPart>,
        #[serde(flatten)]
        metadata: ResponsesMessageMetadata,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(default)]
        call_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        arguments: String,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        #[serde(default)]
        call_id: String,
        output: ResponsesFunctionCallOutput,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        id: String,
        summary: Vec<Value>,
        encrypted_content: String,
    },
    #[serde(rename = "web_search_call")]
    WebSearchCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<ResponsesWebSearchAction>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        results: Option<Vec<Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        internal_chat_message_metadata_passthrough: Option<ResponsesNativeMetadata>,
    },
    #[serde(rename = "compaction")]
    Compaction { encrypted_content: String },
    #[serde(rename = "compaction_trigger")]
    CompactionTrigger,
}

// Retain known native fields only. Request projection removes unsuitable IDs
// and per-content attribution; no corresponding Lite feature is assumed enabled.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResponsesNativeMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_time: Option<serde_json::Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_item_metadata: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_item_kinds: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResponsesMessageMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_chat_message_metadata_passthrough: Option<ResponsesNativeMetadata>,
}

fn invalid_native_id(id: &str) -> bool {
    !id.split_once('_')
        .is_some_and(|(prefix, suffix)| !prefix.is_empty() && !suffix.is_empty())
}

fn serialize_native_input<I: Serialize, S: serde::Serializer>(
    input: &I,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    // ponytail: one temporary input tree; use borrowed per-item projection if
    // large request serialization becomes a bottleneck.
    let mut input = serde_json::to_value(input).map_err(serde::ser::Error::custom)?;
    for item in input.as_array_mut().into_iter().flatten() {
        if item["id"].as_str().is_some_and(invalid_native_id) {
            item.as_object_mut()
                .expect("item with ID is an object")
                .remove("id");
        }
        if let Some(metadata) = item
            .get_mut("internal_chat_message_metadata_passthrough")
            .and_then(Value::as_object_mut)
        {
            metadata.remove("content_item_metadata");
            metadata.remove("content_item_kinds");
        }
    }
    input.serialize(serializer)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesWebSearchAction {
    Search {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queries: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sources: Option<Vec<Value>>,
    },
    OpenPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    FindInPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ResponsesFunctionCallOutput {
    Text(String),
    ContentItems(Vec<ResponsesFunctionCallOutputContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesFunctionCallOutputContentPart {
    InputText {
        text: String,
    },
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ResponsesContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "output_text")]
    OutputText {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Vec<Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        logprobs: Option<Vec<Value>>,
    },
    #[serde(rename = "input_image")]
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesTool {
    Namespace {
        #[serde(rename = "type")]
        kind: String,
        name: String,
        description: String,
        tools: Vec<ResponsesTool>,
    },
    Function(ResponsesFunctionTool),
    WebSearch(ResponsesWebSearchTool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesFunctionTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
    #[serde(default)]
    pub strict: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesWebSearchTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub external_web_access: bool,
    pub search_content_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<ResponsesWebSearchFilters>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_context_size: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesWebSearchFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<Vec<String>>,
}

#[derive(Clone)]
pub struct TranslateOptions {
    pub session_id: Option<String>,
    pub service_tier: Option<ServiceTier>,
    pub model: String,
    pub use_responses_lite: bool,
}

// ---------------------------------------------------------------------------
// Translation entry point
// ---------------------------------------------------------------------------

pub(crate) fn to_codex_effort(effort: Option<&str>) -> Option<Effort> {
    effort.map(|value| Effort::from(value.to_string()))
}

pub(crate) fn resolve_effort_override(
    effort: Option<Effort>,
    override_effort: Option<&str>,
) -> Result<Option<Effort>, anyhow::Error> {
    if let Some(val) = override_effort {
        anyhow::ensure!(!val.is_empty(), "Invalid empty effort override");
        anyhow::ensure!(
            effort
                .as_ref()
                .is_none_or(|effort| effort.to_string() == val),
            "explicit reasoning effort conflicts with configured Codex effort '{val}'"
        );
        return Ok(to_codex_effort(Some(val)));
    }
    Ok(effort)
}

/// Whether the resolved effort permits reasoning artifacts.
/// An absent effort leaves the upstream default in control; explicit no-reasoning
/// efforts must not request summaries or encrypted continuation content.
pub(crate) fn reasoning_requested(effort: Option<&Effort>) -> bool {
    effort.is_none_or(|effort| {
        !matches!(effort, Effort::None)
            && !matches!(effort, Effort::Other(value) if value == "disabled")
    })
}

// ---------------------------------------------------------------------------
// Compaction fast path
// ---------------------------------------------------------------------------

const COMPACT_SYSTEM_MARKER: &str =
    "You are a helpful AI assistant tasked with summarizing conversations";
const COMPACT_MESSAGE_PREFIX: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.";
const COMPACT_MESSAGE_TASK: &str =
    "Your task is to create a detailed summary of the conversation so far";
const COMPACT_DETECTION_TAIL_MESSAGES: usize = 8;

pub(crate) fn is_compact_request(instructions: Option<&str>) -> bool {
    instructions.is_some_and(|text| text.contains(COMPACT_SYSTEM_MARKER))
}

pub(crate) fn is_compact_message_text(text: &str) -> bool {
    text.contains(COMPACT_MESSAGE_PREFIX) && text.contains(COMPACT_MESSAGE_TASK)
}

fn is_compact_message_content(content: &Value) -> bool {
    match content {
        Value::String(text) => is_compact_message_text(text),
        Value::Array(blocks) => blocks.iter().any(|block| {
            block.get("type").and_then(Value::as_str) == Some("text")
                && block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(is_compact_message_text)
        }),
        _ => false,
    }
}

pub(crate) fn is_compact_messages_request(request: &MessagesRequest) -> bool {
    request.compaction_request
        || is_compact_request(flatten_system_text(request.extra.get("system")).as_deref())
        || request
            .messages
            .iter()
            .rev()
            .take(COMPACT_DETECTION_TAIL_MESSAGES)
            .any(|message| message.role == "user" && is_compact_message_content(&message.content))
}

/// Reasoning-effort cap applied to compaction requests, or None when the
/// fast path is disabled. Summarization is extraction, not problem solving:
/// native Claude Code compacts without extended thinking, so burning
/// medium/high reasoning on a 200k-token summary only adds latency. The cap
/// never raises effort — a request already below it is left alone.
///
/// A request naming no effort at all also takes the cap. Left unset it would
/// run at the upstream default, which is the effort level the cap exists to
/// avoid.
fn compact_effort_cap() -> Option<Effort> {
    compact_effort_cap_from(std::env::var("CCP_COMPACT_EFFORT").ok().as_deref())
}

/// Applies the cap to a request's resolved effort. A missing effort takes
/// the cap; an explicit effort at or below it is preserved.
fn apply_compact_effort_cap(resolved: Option<Effort>, cap: Option<Effort>) -> Option<Effort> {
    match (resolved, cap) {
        (resolved, None) => resolved,
        (Some(effort), Some(cap)) if effort <= cap => Some(effort),
        (_, Some(cap)) => Some(cap),
    }
}

fn compact_effort_cap_from(raw: Option<&str>) -> Option<Effort> {
    match raw {
        None | Some("") => Some(Effort::Low),
        Some("off") => None,
        Some("none") => Some(Effort::None),
        Some(other) if KNOWN_EFFORTS.contains(&other) => to_codex_effort(Some(other)),
        Some(_) => Some(Effort::Low),
    }
}

fn resolve_service_tier(
    req: &MessagesRequest,
    model_tier: Option<ServiceTier>,
    model: &str,
) -> Result<Option<ServiceTier>, anyhow::Error> {
    let speed = match req.extra.get("speed") {
        None | Some(Value::Null) => None,
        Some(Value::String(speed)) if matches!(speed.as_str(), "fast" | "standard") => {
            Some(speed.as_str())
        }
        Some(_) => anyhow::bail!("Invalid speed: expected 'fast' or 'standard'"),
    };
    if req.auto_review && model == "codex-auto-review" {
        // Codex 970111f6 core/src/client.rs clears reviewer service tiers.
        // Do not silently discard explicit caller speed/tier requirements.
        anyhow::ensure!(
            speed != Some("fast")
                && model_tier.is_none()
                && req.extra.get("service_tier").is_none_or(Value::is_null),
            "codex-auto-review does not support explicit fast speed or service_tier"
        );
        return Ok(None);
    }
    // An interactive /fast choice is explicit caller intent, not an operator default.
    let configured = config::codex_service_tier();
    let tier = match speed {
        Some("fast") => Some("priority"),
        Some("standard") => Some("default"),
        _ => configured.as_deref(),
    };
    pick_service_tier(tier, model_tier, model)
}

/// The configured override wins over the tier implied by the model name. Either
/// one is narrowed to a tier the upstream `model` offers.
fn pick_service_tier(
    override_tier: Option<&str>,
    model_tier: Option<ServiceTier>,
    model: &str,
) -> Result<Option<ServiceTier>, anyhow::Error> {
    match override_tier {
        Some("default") => Ok(None),
        Some(value) => resolve_service_tier_value(model, value).map(Some),
        None => match model_tier {
            Some(tier) => resolve_service_tier_value(model, &String::from(tier)).map(Some),
            None => Ok(None),
        },
    }
}

/// Messages compatibility concessions, not provider enforcement guarantees.
/// Stamp errors as well: startup failure must not hide the unsupported controls.
pub fn disclose_messages_compatibility(
    mut response: axum::response::Response,
) -> axum::response::Response {
    for header in [
        "x-codex-max-tokens-enforced",
        "x-codex-cache-policy-enforced",
    ] {
        response
            .headers_mut()
            .insert(header, http::HeaderValue::from_static("false"));
    }
    response
}

/// Validate Messages intent before search, replay, compaction or inference.
/// Native Responses passthrough deliberately does not use this adapter.
pub fn validate_messages_capabilities(req: &MessagesRequest) -> anyhow::Result<()> {
    anyhow::ensure!(
        req.max_tokens != Some(0),
        "max_tokens must be a positive integer; its bound is not enforced by this Codex adapter"
    );
    for field in ["temperature", "top_p", "top_k"] {
        if let Some(value) = req.extra.get(field) {
            let valid = match field {
                "top_k" => value.as_u64().is_some(),
                _ => value
                    .as_f64()
                    .is_some_and(|value| (0.0..=1.0).contains(&value)),
            };
            anyhow::ensure!(valid, "{field} has an invalid type or range");
            anyhow::bail!(
                "{field} enforcement has no verified authenticated Codex contract; omit this control"
            );
        }
    }
    parse_stop_sequences(req)?;
    if let Some(thinking) = req.extra.get("thinking") {
        anyhow::ensure!(
            thinking.is_object() && thinking.get("type").is_some_and(Value::is_string),
            "thinking must be an object with a string type"
        );
        if let Some(budget) = thinking.get("budget_tokens") {
            anyhow::ensure!(
                budget.as_u64().is_some_and(|budget| budget >= 1024),
                "thinking.budget_tokens must be an integer >= 1024"
            );
        }
        // Compatibility only: existing clients send these defaults. Codex effort
        // remains independent; neither the mode nor budget is enforced here.
        anyhow::ensure!(
            matches!(
                thinking["type"].as_str(),
                Some("enabled" | "adaptive" | "disabled")
            ),
            "unsupported thinking type"
        );
    }
    if let Some(context) = req.extra.get("context_management") {
        let context = context
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("context_management must be an object"))?;
        anyhow::ensure!(
            context.keys().all(|key| key == "edits"),
            "unsupported context_management field"
        );
        if let Some(edits) = context.get("edits") {
            let edits = edits
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("context_management.edits must be an array"))?;
            for edit in edits {
                let edit = edit
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("context edit must be an object"))?;
                anyhow::ensure!(
                    edit.get("type").and_then(Value::as_str) == Some("clear_thinking_20251015")
                        && edit.get("keep").and_then(Value::as_str) == Some("all")
                        && edit
                            .keys()
                            .all(|key| matches!(key.as_str(), "type" | "keep")),
                    "context edit requires unavailable exact accounting or encrypted-context semantics; only empty edits and clear_thinking keep='all' are supported"
                );
            }
        }
    }
    for field in [
        "prompt_cache_retention",
        "cache_policy",
        "cache_breakpoints",
    ] {
        anyhow::ensure!(
            !req.extra.contains_key(field),
            "{field} has no verified Codex cache-policy contract"
        );
    }
    validate_cache_hint(req.extra.get("cache_control"))?;
    if let Some(system) = req.extra.get("system") {
        anyhow::ensure!(
            system.is_string() || system.is_array(),
            "system must be text or text blocks"
        );
        if let Some(blocks) = system.as_array() {
            for block in blocks {
                anyhow::ensure!(
                    block.get("type").and_then(Value::as_str) == Some("text")
                        && block.get("text").is_some_and(Value::is_string),
                    "system supports only text blocks"
                );
                validate_cache_hint(block.get("cache_control"))?;
            }
        }
    }
    for (message_index, message) in req.messages.iter().enumerate() {
        // Claude Code recognizes this rejection and retries without per-turn
        // effort/timing; never silently reinterpret them as request controls.
        anyhow::ensure!(
            message.output_config.is_none(),
            "messages.{message_index}.output_config: Extra inputs are not permitted"
        );
        if message.role == "system"
            && (message_index == 0 || req.messages[message_index - 1].role != "system")
        {
            let end = message_index
                + req.messages[message_index..]
                    .iter()
                    .take_while(|message| message.role == "system")
                    .count();
            let has_tool_update = req.messages[message_index..end].iter().any(|message| {
                message.content.as_array().is_some_and(|blocks| {
                    blocks.iter().any(|block| block["type"] == "tool_addition")
                })
            });
            if has_tool_update {
                anyhow::ensure!(
                    message_index > 0 && req.messages[message_index - 1].role == "user",
                    "system tool updates must follow a user turn"
                );
                anyhow::ensure!(
                    end == req.messages.len() || req.messages[end].role == "assistant",
                    "system tool updates must precede an assistant turn or end the request"
                );
            }
        }
        anyhow::ensure!(
            matches!(message.role.as_str(), "user" | "assistant" | "system"),
            "messages[{message_index}]: unsupported role"
        );
        let parsed = normalize_content(&message.content)
            .map_err(|error| anyhow::anyhow!("messages[{message_index}].{error}"))?;
        for (block_index, block) in parsed.iter().enumerate() {
            anyhow::ensure!(
                matches!(
                    (message.role.as_str(), block),
                    (_, ContentBlock::Text { .. })
                        | (
                            "user",
                            ContentBlock::Image { .. } | ContentBlock::ToolResult { .. }
                        )
                        | (
                            "assistant",
                            ContentBlock::ToolUse { .. } | ContentBlock::Thinking { .. }
                        )
                        | ("system", ContentBlock::ToolAddition { .. })
                ),
                "messages[{message_index}].content[{block_index}]: unsupported content block for role '{}'",
                message.role
            );
            if let ContentBlock::ToolAddition { tool } = block
                && tool["type"] == "tool_definition"
            {
                let definition = &tool["definition"];
                validate_tool_definition(definition)?;
                anyhow::ensure!(
                    definition.get("input_schema").is_some(),
                    "inline tool definition requires input_schema"
                );
                anyhow::ensure!(
                    definition
                        .get("type")
                        .is_none_or(|kind| matches!(kind.as_str(), Some("custom" | "function"))),
                    "inline hosted tool additions have no verified Codex contract"
                );
            }
        }
        if let Some(blocks) = message.content.as_array() {
            for block in blocks {
                validate_cache_hint(block.get("cache_control"))?;
                if block.get("type").and_then(Value::as_str) == Some("tool_result")
                    && let Some(children) = block.get("content").and_then(Value::as_array)
                {
                    for child in children {
                        validate_cache_hint(child.get("cache_control"))?;
                    }
                }
            }
        }
    }
    if let Some(tools) = req.extra.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("tools must be an array"))?;
        for tool in tools {
            validate_tool_definition(tool)?;
        }
    }
    if let Some(parallel) = req.extra.get("parallel_tool_calls") {
        anyhow::ensure!(
            parallel.is_boolean(),
            "parallel_tool_calls must be a boolean"
        );
    }
    if let Some(disabled) = req
        .extra
        .get("tool_choice")
        .and_then(|choice| choice.get("disable_parallel_tool_use"))
    {
        anyhow::ensure!(
            disabled.is_boolean(),
            "disable_parallel_tool_use must be a boolean"
        );
        anyhow::ensure!(
            disabled != &Value::Bool(true)
                || req.extra.get("parallel_tool_calls") != Some(&Value::Bool(true)),
            "parallel_tool_calls=true conflicts with disable_parallel_tool_use=true"
        );
    }
    if let Some(output) = req.extra.get("output_config") {
        anyhow::ensure!(output.is_object(), "output_config must be an object");
        if let Some(effort) = output.get("effort") {
            let effort = effort
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("output_config.effort must be a string"))?;
            anyhow::ensure!(!effort.is_empty(), "output_config.effort must not be empty");
            resolve_effort_override(
                to_codex_effort(Some(effort)),
                config::codex_effort().as_deref(),
            )?;
        }
        read_output_format(req)?;
    }
    Ok(())
}

fn parse_stop_sequences(req: &MessagesRequest) -> anyhow::Result<Vec<String>> {
    use super::stop_sequences::{MAX_STOP_BYTES, MAX_STOP_SEQUENCES};
    let Some(stops) = req.extra.get("stop_sequences") else {
        return Ok(Vec::new());
    };
    let stops = stops
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("stop_sequences must be an array of strings"))?;
    anyhow::ensure!(
        stops.iter().all(Value::is_string),
        "stop_sequences must contain only strings"
    );
    anyhow::ensure!(
        stops.len() <= MAX_STOP_SEQUENCES,
        "stop_sequences exceed the local limit of {MAX_STOP_SEQUENCES} strings"
    );
    let mut bytes = 0usize;
    let mut out = Vec::with_capacity(stops.len());
    for stop in stops {
        let stop = stop.as_str().expect("validated string");
        anyhow::ensure!(
            !stop.is_empty(),
            "stop_sequences must not contain empty strings"
        );
        bytes = bytes.saturating_add(stop.len());
        anyhow::ensure!(
            bytes <= MAX_STOP_BYTES,
            "stop_sequences exceed the local aggregate limit of {MAX_STOP_BYTES} UTF-8 bytes"
        );
        out.push(stop.to_owned());
    }
    Ok(out)
}

fn validate_tool_definition(tool: &Value) -> anyhow::Result<()> {
    anyhow::ensure!(tool.is_object(), "tool must be an object");
    anyhow::ensure!(
        tool.get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.is_empty()),
        "tool.name must be a nonempty string"
    );
    for field in ["strict", "defer_loading"] {
        if let Some(value) = tool.get(field) {
            anyhow::ensure!(value.is_boolean(), "tool.{field} must be a boolean");
        }
    }
    anyhow::ensure!(
        tool.get("strict") != Some(&Value::Bool(true)),
        "strict=true tool enforcement has no verified Codex contract"
    );
    if let Some(callers) = tool.get("allowed_callers") {
        anyhow::ensure!(
            callers
                .as_array()
                .is_some_and(|callers| callers.len() == 1 && callers[0] == "direct"),
            "tool.allowed_callers supports only ['direct']; programmatic tool execution is unavailable"
        );
    }
    validate_cache_hint(tool.get("cache_control"))?;
    match tool.get("type") {
        None => {}
        Some(Value::String(kind))
            if matches!(kind.as_str(), "custom" | "function" | "web_search_20250305") => {}
        _ => anyhow::bail!(
            "unsupported hosted tool type; only ordinary functions and verified web_search_20250305 are supported"
        ),
    }
    if let Some(description) = tool.get("description") {
        anyhow::ensure!(description.is_string(), "tool.description must be a string");
    }
    if let Some(schema) = tool.get("input_schema") {
        anyhow::ensure!(
            schema.is_object() || schema.is_boolean(),
            "input_schema must be a JSON Schema object or boolean"
        );
    }
    for field in ["allowed_domains", "blocked_domains"] {
        if let Some(domains) = tool.get(field) {
            anyhow::ensure!(
                domains
                    .as_array()
                    .is_some_and(|domains| domains.iter().all(Value::is_string)),
                "{field} must be an array of strings"
            );
        }
    }
    if tool["type"] == "web_search_20250305" {
        anyhow::ensure!(
            tool["name"] == "web_search",
            "web_search_20250305 requires the tool name web_search"
        );
        anyhow::ensure!(
            tool.as_object()
                .expect("validated tool object")
                .keys()
                .all(|key| matches!(
                    key.as_str(),
                    "type"
                        | "name"
                        | "max_uses"
                        | "allowed_domains"
                        | "blocked_domains"
                        | "user_location"
                        | "search_context_size"
                        | "allowed_callers"
                        | "cache_control"
                )),
            "unsupported web_search tool control"
        );
        if let Some(max_uses) = tool.get("max_uses") {
            anyhow::ensure!(
                max_uses.as_u64().is_some_and(|n| u32::try_from(n).is_ok()),
                "web_search.max_uses must be an integer between 0 and 4294967295"
            );
        }
        if let Some(location) = tool.get("user_location") {
            let location = location.as_object().ok_or_else(|| {
                anyhow::anyhow!("web_search.user_location must be an approximate location object")
            })?;
            anyhow::ensure!(
                location.get("type").and_then(Value::as_str) == Some("approximate")
                    && location.iter().all(|(key, value)| {
                        key == "type"
                            || (matches!(key.as_str(), "country" | "region" | "city" | "timezone")
                                && value.is_string())
                    }),
                "web_search.user_location supports only approximate country, region, city and timezone strings"
            );
        }
        if let Some(size) = tool.get("search_context_size") {
            anyhow::ensure!(
                matches!(size.as_str(), Some("low" | "medium" | "high")),
                "web_search.search_context_size must be low, medium or high"
            );
        }
    }
    Ok(())
}

fn validate_cache_hint(hint: Option<&Value>) -> anyhow::Result<()> {
    if let Some(hint) = hint {
        let hint = hint
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("cache_control must be an object"))?;
        anyhow::ensure!(
            hint.len() == 1 && hint.get("type").and_then(Value::as_str) == Some("ephemeral"),
            "explicit cache TTL/breakpoint policy has no verified Codex contract; only advisory ephemeral hints without TTL are accepted (not enforced)"
        );
    }
    Ok(())
}

/// Whether the Messages catalog requests server-executed web search.
pub fn has_hosted_web_search(req: &MessagesRequest) -> bool {
    req.extra
        .get("tools")
        .and_then(|v| v.as_array())
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.get("type").and_then(|v| v.as_str()) == Some("web_search_20250305")
            })
        })
}

pub fn translate_request(
    mut req: MessagesRequest,
    opts: TranslateOptions,
) -> Result<ResponsesRequest, anyhow::Error> {
    validate_messages_capabilities(&req)?;
    if !opts.use_responses_lite && has_hosted_web_search(&req) {
        validate_hosted_web_search(&opts.model)?;
    }
    let instructions = flatten_system_text(req.extra.get("system"));
    let is_compact = is_compact_messages_request(&req);
    let input = build_input(&req, opts.use_responses_lite, None)?;
    let tools = read_tools(&mut req, opts.use_responses_lite)?;
    let req = &req;
    let tool_choice = map_tool_choice(req, opts.use_responses_lite)?;
    validate_input_modalities_iter(
        &opts.model,
        req.messages.iter().map(|message| &message.content),
    )?;
    // Claude's disable_parallel_tool_use=false is permission, not an explicit
    // requirement for parallel execution; preserve omissions on serial models.
    let requested_parallel = match req.extra.get("parallel_tool_calls") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(parallel)) => Some(*parallel),
        Some(_) => anyhow::bail!("parallel_tool_calls must be a boolean"),
    }
    .or_else(|| (parallel_tool_calls(req) == Some(false)).then_some(false));
    let parallel_tool_calls =
        model_parallel_tool_calls(&opts.model, opts.use_responses_lite, requested_parallel)?;

    let mut text = ResponsesText {
        verbosity: model_verbosity(&opts.model, None)?,
        format: None,
    };

    if let Some(fmt) = read_output_format(req)? {
        text.format = Some(fmt);
    }

    let mut out = ResponsesRequest {
        stop_sequences: parse_stop_sequences(req)?,
        codex_auto_review: req.auto_review && opts.model == "codex-auto-review",
        turn_state: Default::default(),
        model: opts.model,
        instructions,
        input,
        store: false,
        stream: true,
        parallel_tool_calls,
        tool_choice,
        text,
        tools: None,
        include: None,
        client_metadata: None,
        service_tier: None,
        prompt_cache_key: None,
        reasoning: None,
    };

    if opts.use_responses_lite {
        out.client_metadata = Some(std::collections::HashMap::from([(
            "ws_request_header_x_openai_internal_codex_responses_lite".to_string(),
            "true".to_string(),
        )]));
        // The lite lane hard-requires this: sending `true` is rejected with
        // 400 unsupported_value ("X-OpenAI-Internal-Codex-Responses-Lite
        // requires `parallel_tool_calls` to be false").
        out.parallel_tool_calls = false;

        let mut prefix = Vec::new();
        if let Some(tools) = tools
            && !tools.is_empty()
        {
            let loads = tool_reference_loads(&req.messages);
            let prefix_tools = split_loaded_deferred_tools(req, tools, &loads)?;
            if !prefix_tools.is_empty() {
                prefix.push(ResponsesInputItem::AdditionalTools {
                    id: None,
                    role: "developer".to_string(),
                    tools: prefix_tools,
                });
            }
        }
        if let Some(instructions) = out.instructions.take()
            && !instructions.is_empty()
        {
            prefix.push(ResponsesInputItem::Message {
                metadata: Default::default(),
                role: "developer".to_string(),
                content: vec![ResponsesContentPart::InputText { text: instructions }],
            });
        }
        if !prefix.is_empty() {
            prefix.extend(out.input);
            out.input = prefix;
        }
    } else if let Some(tools) = tools
        && !tools.is_empty()
    {
        let deferred = deferred_tool_names(req);
        let tools: Vec<ResponsesTool> = tools
            .into_iter()
            .filter(|tool| !is_deferred_placeholder(tool, &deferred))
            .collect();
        if !tools.is_empty() {
            out.tools = Some(tools);
        }
    }

    // Never force a web_search tool_choice the request didn't register —
    // upstream 502s instead of ignoring it.
    if matches!(
        out.tool_choice,
        Some(ResponsesToolChoice::WebSearch { .. } | ResponsesToolChoice::AllowedTools { .. })
    ) {
        let has_web_search = out.tools.as_ref().is_some_and(|t| {
            t.iter()
                .any(|tool| matches!(tool, ResponsesTool::WebSearch(_)))
        });
        if !has_web_search {
            out.tool_choice = Some(ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto));
        }
    }

    if let Some(sid) = opts.session_id {
        out.prompt_cache_key = Some(sid);
    }
    if let Some(key) = req.prompt_cache_key_override.clone() {
        out.prompt_cache_key = Some(key);
    }

    out.service_tier = resolve_service_tier(req, opts.service_tier, &out.model)?;

    let effort = match req
        .extra
        .get("output_config")
        .and_then(|config| config.get("effort"))
    {
        None | Some(Value::Null) => None,
        Some(Value::String(effort)) => Some(effort.as_str()),
        Some(_) => anyhow::bail!("output_config.effort must be a string"),
    };
    let configured_effort = config::codex_effort();
    let mut resolved_effort =
        resolve_effort_override(to_codex_effort(effort), configured_effort.as_deref())?;
    if req.auto_review && resolved_effort.is_none() {
        let metadata = super::model_catalog::catalog_model(&out.model);
        let low_supported = metadata
            .as_ref()
            .and_then(|model| model.supported_reasoning_levels.as_ref())
            .map_or_else(
                // Verified low effort at Codex 970111f6 models-manager/models.json.
                || matches!(out.model.as_str(), "gpt-6-luna" | "codex-auto-review"),
                |levels| levels.iter().any(|level| level.effort == "low"),
            );
        if low_supported {
            resolved_effort = Some(Effort::Low);
        }
    }
    let resolved_string = resolved_effort.as_ref().map(ToString::to_string);
    resolved_effort =
        resolve_model_effort(&out.model, resolved_string.as_deref())?.map(Effort::from);
    if is_compact && effort.is_none() && configured_effort.is_none() {
        let capped = apply_compact_effort_cap(resolved_effort.clone(), compact_effort_cap());
        // A local compaction optimization must not invent an unadvertised level.
        if let Ok(effort) = resolve_model_effort(
            &out.model,
            capped.as_ref().map(ToString::to_string).as_deref(),
        ) {
            resolved_effort = effort.map(Effort::from);
        }
    }
    let wants_reasoning = reasoning_requested(resolved_effort.as_ref());
    anyhow::ensure!(
        resolved_effort != Some(Effort::None),
        "no-reasoning effort has no verified authenticated Codex model/lane contract"
    );
    let summary = if wants_reasoning && !req.omit_reasoning_artifacts {
        model_reasoning_summary(
            &out.model,
            config::codex_reasoning_summary().as_deref(),
            Some("auto"),
        )?
    } else {
        None
    };
    if resolved_effort.is_some() || opts.use_responses_lite || summary.is_some() {
        out.reasoning = Some(ResponsesReasoning {
            effort: resolved_effort,
            summary,
            context: opts.use_responses_lite.then_some("all_turns".to_string()),
        });
    }
    if wants_reasoning && !req.omit_reasoning_artifacts {
        out.include = Some(vec!["reasoning.encrypted_content".to_string()]);
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_output_format(req: &MessagesRequest) -> anyhow::Result<Option<ResponsesTextFormat>> {
    let Some(format) = req
        .extra
        .get("output_config")
        .and_then(|config| config.get("format"))
    else {
        return Ok(None);
    };
    let kind = format.get("type").and_then(Value::as_str).ok_or_else(|| {
        anyhow::anyhow!("output_config.format must be an object with a string type")
    })?;
    let format = match kind {
        "json_schema" => {
            let name = match format.get("name") {
                None => "response",
                Some(Value::String(name)) if !name.is_empty() => name,
                _ => anyhow::bail!("output_config.format.name must be a nonempty string"),
            };
            let schema = format
                .get("schema")
                .filter(|schema| schema.is_object() || schema.is_boolean())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "output_config.format.schema must be a JSON Schema object or boolean"
                    )
                })?;
            ResponsesTextFormat::JsonSchema {
                name: name.to_string(),
                schema: schema.clone(),
                strict: Some(true),
            }
        }
        "json_object" => ResponsesTextFormat::JsonObject,
        "text" => ResponsesTextFormat::Text,
        _ => anyhow::bail!("unsupported output_config.format.type '{kind}'"),
    };
    Ok(Some(format))
}

/// Claude Code sends this deferred stub only to keep Anthropic's deferred loading on.
const DEFERRED_TOOL_PLACEHOLDER: &str = "DeferredToolPlaceholder";

fn responses_tool_name(tool: &ResponsesTool) -> Option<&str> {
    match tool {
        ResponsesTool::Function(function) => Some(function.name.as_str()),
        ResponsesTool::WebSearch(_) | ResponsesTool::Namespace { .. } => None,
    }
}

fn deferred_tool_names(req: &MessagesRequest) -> HashSet<String> {
    req.extra
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| tool.get("defer_loading").and_then(Value::as_bool) == Some(true))
        .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}

fn is_deferred_placeholder(tool: &ResponsesTool, deferred: &HashSet<String>) -> bool {
    responses_tool_name(tool)
        .is_some_and(|name| name == DEFERRED_TOOL_PLACEHOLDER && deferred.contains(name))
}

/// Tool names each `tool_result` loaded through `tool_reference` blocks, keyed by tool_use_id.
fn tool_reference_loads(messages: &[Message]) -> HashMap<String, Vec<String>> {
    let mut loads: HashMap<String, Vec<String>> = HashMap::new();
    for msg in messages.iter().filter(|msg| msg.role == "user") {
        let Some(blocks) = msg.content.as_array() else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            let names = block
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|child| child.get("type").and_then(Value::as_str) == Some("tool_reference"))
                .filter_map(|child| child.get("tool_name").and_then(Value::as_str));
            loads
                .entry(id.to_string())
                .or_default()
                .extend(names.map(str::to_string));
        }
    }
    loads.retain(|_, names| !names.is_empty());
    loads
}

/// Splits the catalog for the Responses Lite lane, the way Codex's incremental tools do:
/// deferred tools that a `tool_reference` in the history loaded are taken out of the
/// prefix so they can be appended at the point they were loaded, which keeps the cached
/// prefix byte-identical across loads. Deferred tools with no visible load (e.g. loaded
/// before a compaction) stay in the prefix so they remain callable.
fn split_loaded_deferred_tools(
    req: &MessagesRequest,
    tools: Vec<ResponsesTool>,
    loads: &HashMap<String, Vec<String>>,
) -> Result<Vec<Value>, anyhow::Error> {
    let deferred = deferred_tool_names(req);
    let loaded: HashSet<&str> = loads.values().flatten().map(String::as_str).collect();
    let temporal = temporal_tool_names(&req.messages);
    let mut prefix = Vec::new();
    for mut tool in tools {
        if is_deferred_placeholder(&tool, &deferred) {
            continue;
        }
        // Serialize the existing tool schema, then move its potentially large
        // parameters tree into the same field instead of serializing a copy.
        let parameters = match &mut tool {
            ResponsesTool::Function(function) => Some(std::mem::take(&mut function.parameters)),
            ResponsesTool::WebSearch(_) | ResponsesTool::Namespace { .. } => None,
        };
        let mut value = serde_json::to_value(&tool)?;
        if let Some(parameters) = parameters {
            value["parameters"] = parameters;
        }
        match responses_tool_name(&tool) {
            Some(name)
                if deferred.contains(name)
                    && (loaded.contains(name) || temporal.contains(name)) => {}
            _ => prefix.push(value),
        }
    }
    Ok(prefix)
}

fn read_tools(
    req: &mut MessagesRequest,
    lite: bool,
) -> Result<Option<Vec<ResponsesTool>>, anyhow::Error> {
    let Some(tools) = req.extra.get_mut("tools").and_then(Value::as_array_mut) else {
        return Ok(None);
    };
    let search_count = tools
        .iter()
        .filter(|tool| tool["type"] == "web_search_20250305")
        .count();
    anyhow::ensure!(
        search_count <= 1,
        "multiple server web search definitions are ambiguous"
    );
    if lite && search_count != 0 {
        anyhow::ensure!(
            !tools
                .iter()
                .any(|tool| matches!(tool["name"].as_str(), Some("web" | "web.run")))
                && !temporal_tool_names(&req.messages)
                    .iter()
                    .any(|name| matches!(name.as_str(), "web" | "web.run")),
            "tool name collides with the private web namespace"
        );
    }
    let mut out = Vec::new();
    for tool in tools {
        let tool_type = tool
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("function");
        if tool_type == "web_search_20250305" {
            if lite {
                out.push(crate::providers::codex::search::native_search_tool());
                continue;
            }
            anyhow::ensure!(
                tool.get("max_uses").is_none(),
                "web_search.max_uses has no verified full Responses enforcement; use Responses Lite or omit this limit"
            );
            let mut filters = ResponsesWebSearchFilters {
                allowed_domains: None,
                blocked_domains: None,
            };
            let allowed = tool.get("allowed_domains").and_then(|v| v.as_array());
            if allowed.is_some_and(|a| !a.is_empty()) {
                filters.allowed_domains = allowed.map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });
            }
            let blocked = tool.get("blocked_domains").and_then(|v| v.as_array());
            if blocked.is_some_and(|a| !a.is_empty()) {
                filters.blocked_domains = blocked.map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });
            }
            let has_filters =
                filters.allowed_domains.is_some() || filters.blocked_domains.is_some();
            out.push(ResponsesTool::WebSearch(ResponsesWebSearchTool {
                kind: "web_search".to_string(),
                external_web_access: true,
                search_content_types: vec!["text".to_string(), "image".to_string()],
                filters: if has_filters { Some(filters) } else { None },
                user_location: tool.get("user_location").cloned(),
                search_context_size: tool
                    .get("search_context_size")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }));
        } else {
            out.push(function_tool(tool));
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

fn function_tool(tool: &mut Value) -> ResponsesTool {
    let name = tool["name"]
        .as_str()
        .expect("validated tool name")
        .to_string();
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string);
    let parameters = tool
        .get_mut("input_schema")
        .map(std::mem::take)
        .unwrap_or(serde_json::json!({}));
    ResponsesTool::Function(ResponsesFunctionTool {
        kind: "function".into(),
        description: codex_tool_description(&name, description),
        parameters: codex_tool_parameters(&name, parameters),
        name,
        strict: false,
    })
}

/// References in SYSTEM blocks use `name`, unlike tool-result references' `tool_name`.
fn temporal_tool_names(messages: &[Message]) -> HashSet<String> {
    messages
        .iter()
        .filter(|message| message.role == "system")
        .filter_map(|message| message.content.as_array())
        .flatten()
        .filter(|block| block["type"] == "tool_addition")
        .filter_map(|block| match block["tool"]["type"].as_str() {
            Some("tool_reference") => block["tool"]["name"].as_str(),
            Some("tool_definition") => block["tool"]["definition"]["name"].as_str(),
            _ => None,
        })
        .map(str::to_string)
        .collect()
}

fn add_temporal_tool(
    tool: &Value,
    catalog: &mut HashMap<String, Value>,
    active: &mut HashSet<String>,
) -> anyhow::Result<Option<Value>> {
    let definition = if tool["type"] == "tool_reference" {
        let name = tool["name"].as_str().expect("validated reference name");
        catalog
            .get(name)
            .ok_or_else(|| {
                anyhow::anyhow!("tool_addition reference '{name}' has no retained definition")
            })?
            .clone()
    } else {
        tool["definition"].clone()
    };
    let name = definition["name"]
        .as_str()
        .expect("validated tool name")
        .to_string();
    anyhow::ensure!(
        definition
            .get("type")
            .is_none_or(|kind| matches!(kind.as_str(), Some("custom" | "function"))),
        "temporal hosted tools have no verified Codex contract"
    );
    // Compare provider declarations, not advisory cache/deferred-loading metadata.
    let wire = serde_json::to_value(function_tool(&mut definition.clone()))?;
    let identical = catalog.get(&name).is_some_and(|old| {
        old.get("type")
            .is_none_or(|kind| matches!(kind.as_str(), Some("custom" | "function")))
            && serde_json::to_value(function_tool(&mut old.clone()))
                .ok()
                .as_ref()
                == Some(&wire)
    });
    if let Some(old) = catalog.get(&name) {
        anyhow::ensure!(
            old.get("type")
                .is_none_or(|kind| matches!(kind.as_str(), Some("custom" | "function"))),
            "temporal tool type collision"
        );
    }
    catalog.insert(name.clone(), definition);
    Ok((active.insert(name) || !identical).then_some(wire))
}

fn codex_tool_description(name: &str, description: Option<String>) -> Option<String> {
    if name != "Read" {
        return description;
    }

    let base = description.unwrap_or_else(|| "Reads a file from the local filesystem.".to_string());
    Some(format!("{base}\n\n{}", read_offset_guidance()))
}

fn codex_tool_parameters(name: &str, mut parameters: Value) -> Value {
    if name != "Read" {
        return parameters;
    }

    let Some(props) = parameters
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    else {
        return parameters;
    };

    if let Some(offset) = props.get_mut("offset").and_then(Value::as_object_mut) {
        offset.insert(
            "description".to_string(),
            Value::String(
                "Optional continuation index. Use only after a prior Read of the same file returned content and more lines are needed. Compute as prior offset plus returned line count. Displayed line numbers, grep line numbers, byte counts, token counts, file sizes, and guessed positions are invalid offsets. Omit when unsure.".to_string(),
            ),
        );
    }

    if let Some(limit) = props.get_mut("limit").and_then(Value::as_object_mut) {
        limit.insert(
            "description".to_string(),
            Value::String(
                "Optional number of lines to read. Omit when opening a file. Use with offset only when continuing a large file."
                    .to_string(),
            ),
        );
    }

    parameters
}

fn map_tool_choice(
    req: &MessagesRequest,
    lite: bool,
) -> Result<Option<ResponsesToolChoice>, anyhow::Error> {
    let choice = match req.extra.get("tool_choice") {
        Some(Value::Object(m)) => m,
        Some(Value::String(s)) => {
            return Ok(Some(match s.as_str() {
                "auto" => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
                "none" => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::None),
                "any" | "required" => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Required),
                _ => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
            }));
        }
        _ => return Ok(None),
    };

    let choice_type = choice
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("auto");
    match choice_type {
        "auto" => Ok(Some(ResponsesToolChoice::Mode(
            ResponsesToolChoiceMode::Auto,
        ))),
        "none" => Ok(Some(ResponsesToolChoice::Mode(
            ResponsesToolChoiceMode::None,
        ))),
        "any" | "required" => Ok(Some(ResponsesToolChoice::Mode(
            ResponsesToolChoiceMode::Required,
        ))),
        "tool" => {
            let name = choice.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let tools = req.extra.get("tools").and_then(|v| v.as_array());
            let is_web_search = tools.is_some_and(|t| {
                t.iter().any(|tool| {
                    (tool.get("type").and_then(|v| v.as_str()) == Some("web_search_20250305"))
                        && tool.get("name").and_then(|v| v.as_str()) == Some(name)
                })
            });
            if is_web_search && lite {
                // Orchestration restricts the initial catalog to web, then restores
                // the caller's catalog after executing the required private call.
                Ok(Some(ResponsesToolChoice::Mode(
                    ResponsesToolChoiceMode::Required,
                )))
            } else if is_web_search {
                Ok(Some(ResponsesToolChoice::AllowedTools {
                    r#type: "allowed_tools".to_string(),
                    mode: "required".to_string(),
                    tools: vec![serde_json::json!({"type": "web_search"})],
                }))
            } else {
                Ok(Some(ResponsesToolChoice::Function {
                    r#type: "function".to_string(),
                    name: name.to_string(),
                }))
            }
        }
        _ => Ok(None),
    }
}

/// Translate only new messages, retaining the inherited catalog's activation history.
/// The caller must validate/translate the complete request first.
pub(crate) fn thread_input_suffix(
    req: &MessagesRequest,
    lite: bool,
    messages: usize,
) -> anyhow::Result<Vec<ResponsesInputItem>> {
    anyhow::ensure!(
        messages <= req.messages.len(),
        "invalid thread message boundary"
    );
    build_input(req, lite, Some(messages))
}

fn build_input(
    req: &MessagesRequest,
    lite: bool,
    boundary: Option<usize>,
) -> anyhow::Result<Vec<ResponsesInputItem>> {
    let mut out: Vec<ResponsesInputItem> = Vec::new();
    let mut read_tool_uses_with_offset = HashSet::new();
    // A continuation cannot retroactively move an inherited prefix declaration
    // to a new load. Walk the retained history to recover its current catalog,
    // then discard portable items at the explicit message boundary.
    let history = &req.messages[..boundary.unwrap_or(req.messages.len())];
    let temporal = temporal_tool_names(history);
    let loads = tool_reference_loads(&req.messages);
    let initial_loads = boundary.map(|_| tool_reference_loads(history));
    let loaded: HashSet<&str> = initial_loads
        .as_ref()
        .unwrap_or(&loads)
        .values()
        .flatten()
        .map(String::as_str)
        .collect();
    let mut catalog = HashMap::new();
    let mut active = HashSet::new();
    for tool in req
        .extra
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = tool["name"]
            .as_str()
            .expect("validated tool name")
            .to_string();
        let deferred = tool.get("defer_loading") == Some(&Value::Bool(true));
        if !deferred || (!temporal.contains(&name) && !loaded.contains(name.as_str())) {
            active.insert(name.clone());
        }
        anyhow::ensure!(
            catalog.insert(name, tool.clone()).is_none(),
            "duplicate tool names are ambiguous"
        );
    }

    for (message_index, msg) in req.messages.iter().enumerate() {
        let blocks = normalize_content(&msg.content)
            .map_err(|error| anyhow::anyhow!("messages[{message_index}].{error}"))?;
        match msg.role.as_str() {
            "user" => {
                let mut parts: Vec<ResponsesContentPart> = Vec::new();
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            parts.push(ResponsesContentPart::InputText {
                                text: text.to_string(),
                            });
                        }
                        ContentBlock::Image { image_url } => {
                            parts.push(ResponsesContentPart::InputImage {
                                image_url,
                                detail: None,
                            });
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            if !parts.is_empty() {
                                out.push(ResponsesInputItem::Message {
                                    metadata: Default::default(),
                                    role: "user".to_string(),
                                    content: std::mem::take(&mut parts),
                                });
                            }
                            let mut rendered = render_tool_result(&content)?;
                            if is_error.unwrap_or(false) {
                                rendered.prepend_text("[tool execution error]".to_string());
                            }
                            if let Some(note) = rewritten_read_offset_note(&rendered, tool_use_id) {
                                rendered.push_text(format!("\n{note}"));
                            }
                            if should_append_read_offset_guidance(
                                &rendered,
                                read_tool_uses_with_offset.contains(tool_use_id),
                                is_error.unwrap_or(false),
                            ) {
                                rendered.push_text(format!("\n{}", read_offset_guidance()));
                            }
                            let joined_text = rendered.joined_text();
                            out.push(ResponsesInputItem::FunctionCallOutput {
                                call_id: tool_use_id.to_string(),
                                output: function_call_output(rendered, joined_text),
                            });
                            if lite {
                                let mut tools = Vec::new();
                                for name in loads.get(tool_use_id).into_iter().flatten() {
                                    let reference =
                                        serde_json::json!({"type":"tool_reference","name":name});
                                    if let Some(tool) =
                                        add_temporal_tool(&reference, &mut catalog, &mut active)?
                                    {
                                        tools.push(tool);
                                    }
                                }
                                if !tools.is_empty() {
                                    out.push(ResponsesInputItem::AdditionalTools {
                                        id: None,
                                        role: "developer".into(),
                                        tools,
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    out.push(ResponsesInputItem::Message {
                        metadata: Default::default(),
                        role: "user".to_string(),
                        content: parts,
                    });
                }
            }
            "system" => {
                let mut parts = Vec::new();
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            parts.push(ResponsesContentPart::InputText {
                                text: text.to_string(),
                            })
                        }
                        ContentBlock::ToolAddition { tool } => {
                            anyhow::ensure!(
                                lite,
                                "temporal tool additions/replacements require the verified Responses Lite lane; full-lane temporal tools are unavailable"
                            );
                            if !parts.is_empty() {
                                out.push(ResponsesInputItem::Message {
                                    metadata: Default::default(),
                                    role: "developer".into(),
                                    content: std::mem::take(&mut parts),
                                });
                            }
                            if let Some(tool) = add_temporal_tool(tool, &mut catalog, &mut active)?
                            {
                                out.push(ResponsesInputItem::AdditionalTools {
                                    id: None,
                                    role: "developer".into(),
                                    tools: vec![tool],
                                });
                            }
                        }
                        _ => unreachable!("validated system content"),
                    }
                }
                if !parts.is_empty() {
                    out.push(ResponsesInputItem::Message {
                        metadata: Default::default(),
                        role: "developer".into(),
                        content: parts,
                    });
                }
            }
            _ => {
                let mut text_parts: Vec<ResponsesContentPart> = Vec::new();
                let flush_text =
                    |out: &mut Vec<ResponsesInputItem>,
                     text_parts: &mut Vec<ResponsesContentPart>| {
                        if !text_parts.is_empty() {
                            out.push(ResponsesInputItem::Message {
                                metadata: Default::default(),
                                role: "assistant".to_string(),
                                content: std::mem::take(text_parts),
                            });
                        }
                    };
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            text_parts.push(ResponsesContentPart::OutputText {
                                annotations: None,
                                logprobs: None,
                                text: text.to_string(),
                            });
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            flush_text(&mut out, &mut text_parts);
                            if is_read_tool_use_with_offset(name, input) {
                                read_tool_uses_with_offset.insert(id);
                            }
                            let args =
                                serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                            out.push(ResponsesInputItem::FunctionCall {
                                call_id: id.to_string(),
                                name: name.to_string(),
                                namespace: None,
                                arguments: args,
                            });
                        }
                        ContentBlock::Thinking { signature } => {
                            let Some(replay) = signature.and_then(decode_reasoning_signature)
                            else {
                                continue;
                            };
                            flush_text(&mut out, &mut text_parts);
                            out.push(ResponsesInputItem::Reasoning {
                                id: replay.id,
                                summary: Vec::new(),
                                encrypted_content: replay.encrypted_content,
                            });
                        }
                        _ => {}
                    }
                }
                flush_text(&mut out, &mut text_parts);
            }
        }
        if boundary.is_some_and(|boundary| message_index < boundary) {
            out.clear();
        }
    }

    Ok(out)
}

fn is_read_tool_use_with_offset(name: &str, input: &Value) -> bool {
    name == "Read" && input.get("offset").is_some()
}

fn rewritten_read_offset_note(
    output: &RenderedToolResult<'_>,
    tool_use_id: &str,
) -> Option<String> {
    if output.contains("Proxy Read offset note:") {
        return None;
    }
    read_offset_rewrite(tool_use_id)
        .as_ref()
        .map(read_offset_rewrite_note)
}

fn read_offset_rewrite_note(rewrite: &ReadOffsetRewrite) -> String {
    let file = rewrite
        .file_path
        .as_deref()
        .map(|path| format!(" for {path}"))
        .unwrap_or_default();
    format!(
        "Proxy Read offset note:\n\
         - Requested Read offset {}{} exceeds the proxy rewrite threshold of 1000000.\n\
         - This Read starts at the beginning of the file.\n\
         - For continuation reads, use offset after a prior Read of the same file returned content and more lines are needed.\n\
         - Compute offset as prior offset plus the number of lines returned by that prior Read.",
        rewrite.offset, file
    )
}

fn should_append_read_offset_guidance(
    output: &RenderedToolResult<'_>,
    read_call_had_offset: bool,
    is_error: bool,
) -> bool {
    if !read_call_had_offset || output.contains("Codex Read guidance:") {
        return false;
    }
    // Fold each part once, then reuse optimized substring searches.
    let lowercase: Vec<_> = output.text_parts().map(str::to_ascii_lowercase).collect();
    let contains = |needle: &str| lowercase.iter().any(|text| text.contains(needle));
    contains("offset")
        && (contains("file has")
            || contains("out of range")
            || (contains("line") && contains("requested")))
        && (is_error || contains("warning") || contains("system-reminder"))
}

fn read_offset_guidance() -> &'static str {
    "Codex Read guidance:\n\
     - offset is an optional zero based continuation index, not a line number lookup.\n\
     - Use offset only after a prior Read of the same file returned content and more lines are needed.\n\
     - Compute offset as prior offset plus the number of lines returned by that prior Read.\n\
     - Displayed line numbers, grep line numbers, byte counts, token counts, file sizes, and guessed positions are invalid offsets.\n\
     - Omit offset and limit when opening a file or when unsure."
}

// ---------------------------------------------------------------------------
// Tool result rendering
// ---------------------------------------------------------------------------

enum RenderedToolResultPart<'a> {
    Text(Cow<'a, str>),
    Image(String),
}

struct RenderedToolResult<'a> {
    parts: Vec<RenderedToolResultPart<'a>>,
}

impl RenderedToolResult<'_> {
    fn has_images(&self) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, RenderedToolResultPart::Image(_)))
    }

    fn text_parts(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(|part| match part {
            RenderedToolResultPart::Text(text) => Some(text.as_ref()),
            RenderedToolResultPart::Image(_) => None,
        })
    }

    // All recognition needles exclude newlines, so joining text parts with '\n'
    // cannot create a match that is absent from the individual parts.
    fn contains(&self, needle: &str) -> bool {
        self.text_parts().any(|text| text.contains(needle))
    }

    fn joined_text(&self) -> String {
        self.text_parts().collect::<Vec<_>>().join("\n")
    }

    fn prepend_text(&mut self, text: String) {
        self.parts
            .insert(0, RenderedToolResultPart::Text(text.into()));
    }

    fn push_text(&mut self, text: String) {
        self.parts.push(RenderedToolResultPart::Text(text.into()));
    }
}

fn render_tool_result(content: &Value) -> anyhow::Result<RenderedToolResult<'_>> {
    let parts = match content {
        Value::String(text) => vec![RenderedToolResultPart::Text(Cow::Borrowed(text))],
        Value::Array(blocks) => blocks
            .iter()
            .map(render_tool_result_block)
            .collect::<anyhow::Result<_>>()?,
        _ => anyhow::bail!("tool_result.content must be text or content blocks"),
    };
    Ok(RenderedToolResult { parts })
}

fn render_tool_result_block(block: &Value) -> anyhow::Result<RenderedToolResultPart<'_>> {
    let part = match block.get("type").and_then(Value::as_str) {
        Some("text") => RenderedToolResultPart::Text(Cow::Borrowed(
            block
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("tool_result text must be a string"))?,
        )),
        Some("image") => {
            let source = block
                .get("source")
                .ok_or_else(|| anyhow::anyhow!("tool_result image requires source"))?;
            anyhow::ensure!(
                source.get("type").and_then(Value::as_str) == Some("base64"),
                "tool_result images require base64; remote image URLs have no verified Codex fetching contract"
            );
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("image media_type must be a string"))?;
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("image data must be a string"))?;
            RenderedToolResultPart::Image(validated_image_data_url(media_type, data)?)
        }
        Some("tool_reference") => {
            let name = block
                .get("tool_name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("tool_reference requires tool_name"))?;
            RenderedToolResultPart::Text(format!("Tool loaded: {name}").into())
        }
        _ => anyhow::bail!(
            "unsupported nested tool_result content; use text, base64 images or tool_reference"
        ),
    };
    Ok(part)
}

fn function_call_output(
    rendered: RenderedToolResult<'_>,
    joined_text: String,
) -> ResponsesFunctionCallOutput {
    if !rendered.has_images() {
        return ResponsesFunctionCallOutput::Text(joined_text);
    }

    ResponsesFunctionCallOutput::ContentItems(
        rendered
            .parts
            .into_iter()
            .map(|part| match part {
                RenderedToolResultPart::Text(text) => {
                    ResponsesFunctionCallOutputContentPart::InputText {
                        text: text.into_owned(),
                    }
                }
                RenderedToolResultPart::Image(image_url) => {
                    ResponsesFunctionCallOutputContentPart::InputImage {
                        image_url,
                        detail: None,
                    }
                }
            })
            .collect(),
    )
}
