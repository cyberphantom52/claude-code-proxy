use std::collections::{HashMap, HashSet};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::anthropic::schema::MessagesRequest;
use crate::config;
use crate::providers::translate_shared::{
    ContentBlock, flatten_system_text, image_source_to_url, normalize_content, parallel_tool_calls,
};

use super::model_allowlist::{
    KNOWN_EFFORTS, model_parallel_tool_calls, model_reasoning_summary, model_verbosity,
    resolve_model_effort, resolve_service_tier_value, validate_hosted_web_search,
    validate_input_modalities,
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
    Function {
        r#type: String,
        name: String,
    },
    WebSearch {
        r#type: String,
    },
    AllowedTools {
        r#type: String,
        mode: String,
        tools: Vec<Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<ResponsesInputItem>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(default)]
        call_id: String,
        name: String,
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
        id: String,
        summary: Vec<Value>,
        encrypted_content: String,
    },
    #[serde(rename = "compaction")]
    Compaction { encrypted_content: String },
    #[serde(rename = "compaction_trigger")]
    CompactionTrigger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesFunctionCallOutput {
    Text(String),
    ContentItems(Vec<ResponsesFunctionCallOutputContentPart>),
}

impl ResponsesFunctionCallOutput {
    #[cfg(test)]
    fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::ContentItems(_) => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsesContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "output_text")]
    OutputText { text: String },
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesWebSearchFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<Vec<String>>,
}

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
        return Ok(to_codex_effort(Some(val)));
    }
    Ok(effort)
}

/// Whether the resolved effort asks the upstream for reasoning output.
/// `Effort::None` still names the effort so the wire request overrides the
/// upstream default, but it must not request a summary or encrypted
/// continuation content, which are reasoning artifacts.
pub(crate) fn reasoning_requested(effort: Option<&Effort>) -> bool {
    effort.is_some_and(|effort| {
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
    is_compact_request(flatten_system_text(request.extra.get("system")).as_deref())
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

pub fn normalize_strict_json_schema(schema: &Value) -> Value {
    match schema {
        Value::Array(arr) => Value::Array(arr.iter().map(normalize_strict_json_schema).collect()),
        Value::Object(map) => {
            let mut out = map.clone();
            if let Some(properties) = out.get("properties").and_then(|v| v.as_object()) {
                let keys: Vec<String> = properties.keys().cloned().collect();
                out.insert(
                    "required".into(),
                    Value::Array(keys.into_iter().map(Value::String).collect()),
                );
            }
            for (key, val) in out.clone().iter() {
                out.insert(key.clone(), normalize_strict_json_schema(val));
            }
            Value::Object(out)
        }
        _ => schema.clone(),
    }
}

/// Hosted tools (web_search) are rejected by the Responses Lite lane, which
/// only supports function and custom tools. Requests carrying them must use
/// the full Responses API.
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
    req: &MessagesRequest,
    opts: TranslateOptions,
) -> Result<ResponsesRequest, anyhow::Error> {
    if has_hosted_web_search(req) {
        validate_hosted_web_search(&opts.model)?;
    }
    let instructions = flatten_system_text(req.extra.get("system"));
    let is_compact = is_compact_messages_request(req);
    let input = build_input(req)?;
    let tools = read_tools(req)?;
    let tool_choice = map_tool_choice(req)?;
    validate_input_modalities(&opts.model, &serde_json::to_value(&req.messages)?)?;
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

    if let Some(fmt) = read_output_format(req) {
        text.format = Some(fmt);
    }

    let mut out = ResponsesRequest {
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
        if let Some(ref tools) = tools
            && !tools.is_empty()
        {
            let loads = tool_reference_loads(req);
            let (prefix_tools, loaded_tools) = split_loaded_deferred_tools(req, tools, &loads)?;
            if !prefix_tools.is_empty() {
                prefix.push(ResponsesInputItem::AdditionalTools {
                    id: None,
                    role: "developer".to_string(),
                    tools: prefix_tools,
                });
            }
            append_loaded_tools(&mut out.input, &loads, loaded_tools);
        }
        if let Some(instructions) = out.instructions.take()
            && !instructions.is_empty()
        {
            prefix.push(ResponsesInputItem::Message {
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
    let mut resolved_effort =
        resolve_effort_override(to_codex_effort(effort), config::codex_effort().as_deref())?;
    let resolved_string = resolved_effort.as_ref().map(ToString::to_string);
    resolved_effort =
        resolve_model_effort(&out.model, resolved_string.as_deref())?.map(Effort::from);
    if is_compact {
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
    if resolved_effort.is_some() || opts.use_responses_lite {
        let summary = if wants_reasoning && !req.omit_reasoning_artifacts {
            model_reasoning_summary(
                &out.model,
                config::codex_reasoning_summary().as_deref(),
                Some("auto"),
            )?
        } else {
            None
        };
        out.reasoning = Some(ResponsesReasoning {
            effort: resolved_effort.clone(),
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

fn read_output_format(req: &MessagesRequest) -> Option<ResponsesTextFormat> {
    let output_config = req.extra.get("output_config")?.as_object()?;
    let format = output_config.get("format")?.as_object()?;
    let kind = format.get("type")?.as_str()?;
    match kind {
        "json_schema" => {
            let name = format
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("response")
                .to_string();
            let schema = format.get("schema")?;
            let normalized = normalize_strict_json_schema(schema);
            Some(ResponsesTextFormat::JsonSchema {
                name,
                schema: normalized,
                strict: Some(true),
            })
        }
        "json_object" => Some(ResponsesTextFormat::JsonObject),
        _ => Some(ResponsesTextFormat::Text),
    }
}

/// Claude Code sends this deferred stub only to keep Anthropic's deferred loading on.
const DEFERRED_TOOL_PLACEHOLDER: &str = "DeferredToolPlaceholder";

fn responses_tool_name(tool: &ResponsesTool) -> Option<&str> {
    match tool {
        ResponsesTool::Function(function) => Some(function.name.as_str()),
        ResponsesTool::WebSearch(_) => None,
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
fn tool_reference_loads(req: &MessagesRequest) -> HashMap<String, Vec<String>> {
    let mut loads: HashMap<String, Vec<String>> = HashMap::new();
    for msg in req.messages.iter().filter(|msg| msg.role == "user") {
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
    tools: &[ResponsesTool],
    loads: &HashMap<String, Vec<String>>,
) -> Result<(Vec<Value>, HashMap<String, Value>), anyhow::Error> {
    let deferred = deferred_tool_names(req);
    let loaded: HashSet<&str> = loads.values().flatten().map(String::as_str).collect();
    let mut prefix = Vec::new();
    let mut appended = HashMap::new();
    for tool in tools {
        if is_deferred_placeholder(tool, &deferred) {
            continue;
        }
        let value = serde_json::to_value(tool)?;
        match responses_tool_name(tool) {
            Some(name) if deferred.contains(name) && loaded.contains(name) => {
                appended.insert(name.to_string(), value);
            }
            _ => prefix.push(value),
        }
    }
    Ok((prefix, appended))
}

fn append_loaded_tools(
    input: &mut Vec<ResponsesInputItem>,
    loads: &HashMap<String, Vec<String>>,
    mut loaded_tools: HashMap<String, Value>,
) {
    if loaded_tools.is_empty() {
        return;
    }
    let mut out = Vec::with_capacity(input.len() + loads.len());
    for item in input.drain(..) {
        let names = match &item {
            ResponsesInputItem::FunctionCallOutput { call_id, .. } => loads.get(call_id),
            _ => None,
        };
        out.push(item);
        let tools: Vec<Value> = names
            .into_iter()
            .flatten()
            .filter_map(|name| loaded_tools.remove(name))
            .collect();
        if !tools.is_empty() {
            out.push(ResponsesInputItem::AdditionalTools {
                id: None,
                role: "developer".to_string(),
                tools,
            });
        }
    }
    debug_assert!(
        loaded_tools.is_empty(),
        "every loaded tool should follow the tool_result that loaded it"
    );
    *input = out;
}

fn read_tools(req: &MessagesRequest) -> Result<Option<Vec<ResponsesTool>>, anyhow::Error> {
    let Some(tools) = req.extra.get("tools") else {
        return Ok(None);
    };
    let tools_arr = match tools {
        Value::Array(a) => a,
        _ => return Ok(None),
    };
    let mut out = Vec::new();
    for tool in tools_arr {
        let tool_type = tool
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("function");
        if tool_type == "web_search_20250305" {
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
            }));
        } else {
            let name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let parameters = tool
                .get("input_schema")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            let description = codex_tool_description(&name, description);
            let parameters = codex_tool_parameters(&name, parameters);
            out.push(ResponsesTool::Function(ResponsesFunctionTool {
                kind: "function".to_string(),
                name,
                description,
                parameters,
                strict: false,
            }));
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

fn codex_tool_description(name: &str, description: Option<String>) -> Option<String> {
    if name != "Read" {
        return description;
    }

    let base = description.unwrap_or_else(|| "Reads a file from the local filesystem.".to_string());
    Some(format!("{base}\n\n{}", read_offset_guidance()))
}

// OpenAI's regex dialect differs from schemas emitted by clients. Drop pattern
// constraints rather than guessing backend compatibility with a local regex engine.
// This relaxes tool argument validation, including otherwise compatible patterns.
// Only visit schema-bearing keywords: defaults, examples and other literal data
// must remain intact, as must names in property and definition maps.
fn strip_tool_schema_patterns(schema: &mut Value) {
    let Some(schema) = schema.as_object_mut() else {
        return;
    };
    schema.remove("pattern");

    for (keyword, value) in schema {
        match keyword.as_str() {
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas"
            | "dependencies" => {
                if let Some(schemas) = value.as_object_mut() {
                    for schema in schemas.values_mut() {
                        strip_tool_schema_patterns(schema);
                    }
                }
            }
            "allOf" | "anyOf" | "oneOf" | "prefixItems" | "items" => {
                if let Some(schemas) = value.as_array_mut() {
                    for schema in schemas {
                        strip_tool_schema_patterns(schema);
                    }
                } else if keyword == "items" {
                    strip_tool_schema_patterns(value);
                }
            }
            "additionalProperties"
            | "additionalItems"
            | "unevaluatedProperties"
            | "unevaluatedItems"
            | "contains"
            | "propertyNames"
            | "not"
            | "if"
            | "then"
            | "else"
            | "contentSchema" => strip_tool_schema_patterns(value),
            _ => {}
        }
    }
}

fn codex_tool_parameters(name: &str, mut parameters: Value) -> Value {
    strip_tool_schema_patterns(&mut parameters);
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

fn map_tool_choice(req: &MessagesRequest) -> Result<Option<ResponsesToolChoice>, anyhow::Error> {
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
            if is_web_search {
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

fn build_input(req: &MessagesRequest) -> anyhow::Result<Vec<ResponsesInputItem>> {
    let mut out: Vec<ResponsesInputItem> = Vec::new();
    let mut read_tool_uses_with_offset = HashSet::new();

    for (message_index, msg) in req.messages.iter().enumerate() {
        anyhow::ensure!(
            matches!(msg.role.as_str(), "user" | "assistant" | "system"),
            "messages[{message_index}].role: unsupported role '{}'",
            msg.role
        );
        let blocks = normalize_content(&msg.content)
            .map_err(|error| anyhow::anyhow!("messages[{message_index}].{error}"))?;
        for (block_index, block) in blocks.iter().enumerate() {
            anyhow::ensure!(
                matches!(
                    (msg.role.as_str(), block),
                    (_, ContentBlock::Text { .. })
                        | (
                            "user",
                            ContentBlock::Image { .. } | ContentBlock::ToolResult { .. }
                        )
                        | (
                            "assistant",
                            ContentBlock::ToolUse { .. } | ContentBlock::Thinking { .. }
                        )
                ),
                "messages[{message_index}].content[{block_index}]: unsupported content block type '{}' for role '{}'",
                msg.content[block_index]["type"].as_str().unwrap_or("text"),
                msg.role
            );
        }
        match msg.role.as_str() {
            "user" => {
                let mut parts: Vec<ResponsesContentPart> = Vec::new();
                for block in &blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            parts.push(ResponsesContentPart::InputText { text: text.clone() });
                        }
                        ContentBlock::Image { source } => {
                            parts.push(ResponsesContentPart::InputImage {
                                image_url: image_source_to_url(source),
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
                                    role: "user".to_string(),
                                    content: std::mem::take(&mut parts),
                                });
                            }
                            let mut rendered = render_tool_result(content);
                            if is_error.unwrap_or(false) {
                                rendered.prepend_text("[tool execution error]".to_string());
                            }
                            if let Some(note) =
                                rewritten_read_offset_note(&rendered.joined_text(), tool_use_id)
                            {
                                rendered.push_text(format!("\n{note}"));
                            }
                            if should_append_read_offset_guidance(
                                &rendered.joined_text(),
                                read_tool_uses_with_offset.contains(tool_use_id),
                                is_error.unwrap_or(false),
                            ) {
                                rendered.push_text(format!("\n{}", read_offset_guidance()));
                            }
                            out.push(ResponsesInputItem::FunctionCallOutput {
                                call_id: tool_use_id.clone(),
                                output: function_call_output(rendered),
                            });
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    out.push(ResponsesInputItem::Message {
                        role: "user".to_string(),
                        content: parts,
                    });
                }
            }
            "system" => {
                let parts: Vec<ResponsesContentPart> = blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => {
                            Some(ResponsesContentPart::InputText { text: text.clone() })
                        }
                        _ => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    out.push(ResponsesInputItem::Message {
                        role: "developer".to_string(),
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
                                role: "assistant".to_string(),
                                content: std::mem::take(text_parts),
                            });
                        }
                    };
                for block in &blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            text_parts
                                .push(ResponsesContentPart::OutputText { text: text.clone() });
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            flush_text(&mut out, &mut text_parts);
                            if is_read_tool_use_with_offset(name, input) {
                                read_tool_uses_with_offset.insert(id.clone());
                            }
                            let args =
                                serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                            out.push(ResponsesInputItem::FunctionCall {
                                call_id: id.clone(),
                                name: name.clone(),
                                arguments: args,
                            });
                        }
                        ContentBlock::Thinking { signature, .. } => {
                            let Some(replay) =
                                signature.as_deref().and_then(decode_reasoning_signature)
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
    }

    Ok(out)
}

fn is_read_tool_use_with_offset(name: &str, input: &Value) -> bool {
    name == "Read" && input.get("offset").is_some()
}

fn rewritten_read_offset_note(output: &str, tool_use_id: &str) -> Option<String> {
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
    output: &str,
    read_call_had_offset: bool,
    is_error: bool,
) -> bool {
    read_call_had_offset
        && !output.contains("Codex Read guidance:")
        && looks_like_read_offset_result(output)
        && (is_error || looks_like_read_offset_warning(output))
}

fn looks_like_read_offset_result(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("offset")
        && (lower.contains("file has")
            || lower.contains("out of range")
            || (lower.contains("line") && lower.contains("requested")))
}

fn looks_like_read_offset_warning(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("warning") || lower.contains("system-reminder")
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

enum RenderedToolResultPart {
    Text(String),
    Image(String),
}

struct RenderedToolResult {
    parts: Vec<RenderedToolResultPart>,
}

impl RenderedToolResult {
    fn has_images(&self) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, RenderedToolResultPart::Image(_)))
    }

    fn joined_text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                RenderedToolResultPart::Text(text) => Some(text.as_str()),
                RenderedToolResultPart::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn prepend_text(&mut self, text: String) {
        self.parts.insert(0, RenderedToolResultPart::Text(text));
    }

    fn push_text(&mut self, text: String) {
        self.parts.push(RenderedToolResultPart::Text(text));
    }
}

fn render_tool_result(content: &Value) -> RenderedToolResult {
    let parts = match content {
        Value::String(text) => vec![RenderedToolResultPart::Text(text.clone())],
        Value::Array(blocks) => blocks.iter().map(render_tool_result_block).collect(),
        _ => Vec::new(),
    };
    RenderedToolResult { parts }
}

fn render_tool_result_block(block: &Value) -> RenderedToolResultPart {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .map(|text| RenderedToolResultPart::Text(text.to_string()))
            .unwrap_or_else(|| {
                RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block))
            }),
        Some("image") => render_tool_result_image(block),
        Some("tool_reference") => match block.get("tool_name").and_then(Value::as_str) {
            Some(name) => RenderedToolResultPart::Text(format!("Tool loaded: {name}")),
            None => RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block)),
        },
        Some(other) => {
            RenderedToolResultPart::Text(format!("[unsupported content block omitted: {other}]"))
        }
        None => RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block)),
    }
}

fn render_tool_result_image(block: &Value) -> RenderedToolResultPart {
    let Some(source) = block.get("source").and_then(Value::as_object) else {
        return RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block));
    };
    match source.get("type").and_then(Value::as_str) {
        Some("url") if source.get("url").and_then(Value::as_str).is_some() => {
            RenderedToolResultPart::Text("[image omitted: url]".to_string())
        }
        Some("base64") => {
            let media_type = source.get("media_type").and_then(Value::as_str);
            let data = source.get("data").and_then(Value::as_str);
            match media_type
                .zip(data)
                .and_then(|(media_type, data)| validated_image_data_url(media_type, data))
            {
                Some(image_url) => RenderedToolResultPart::Image(image_url),
                None => {
                    RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block))
                }
            }
        }
        _ => RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block)),
    }
}

fn validated_image_data_url(media_type: &str, data: &str) -> Option<String> {
    if !matches!(
        media_type,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    ) {
        return None;
    }

    let compact: String = data
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    if compact.is_empty() {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&compact)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&compact))
        .ok()?;
    let canonical = base64::engine::general_purpose::STANDARD.encode(decoded);
    Some(format!("data:{media_type};base64,{canonical}"))
}

fn function_call_output(rendered: RenderedToolResult) -> ResponsesFunctionCallOutput {
    if !rendered.has_images() {
        return ResponsesFunctionCallOutput::Text(rendered.joined_text());
    }

    ResponsesFunctionCallOutput::ContentItems(
        rendered
            .parts
            .into_iter()
            .map(|part| match part {
                RenderedToolResultPart::Text(text) => {
                    ResponsesFunctionCallOutputContentPart::InputText { text }
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

fn unsupported_tool_result_block_to_string(block: &Value) -> String {
    let kind = block
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    format!("[unsupported content block omitted: {kind}]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numeric_effort_defaults_use_unsigned_wire_values_and_named_efforts_stay_strings() {
        super::super::model_allowlist::with_test_catalog(
            json!([{
                "slug":"numeric", "default_reasoning_level":"4096",
                "supported_reasoning_levels":[{"effort":"4096"},{"effort":"low"}]
            }]),
            || {
                let mut req: MessagesRequest =
                    serde_json::from_value(json!({"messages":[{"role":"user","content":"hello"}]}))
                        .unwrap();
                let options = || TranslateOptions {
                    model: "numeric".to_string(),
                    ..opts()
                };
                let wire =
                    serde_json::to_value(translate_request(&req, options()).unwrap()).unwrap();
                assert_eq!(wire["reasoning"]["effort"], json!(4096));
                req.extra
                    .insert("output_config".to_string(), json!({"effort":"low"}));
                let wire =
                    serde_json::to_value(translate_request(&req, options()).unwrap()).unwrap();
                assert_eq!(wire["reasoning"]["effort"], json!("low"));
            },
        );
        for value in [json!(4096), json!(u64::MAX), json!("low")] {
            let effort: Effort = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(effort).unwrap(), value);
        }
        for value in [
            json!(-1),
            json!(1.5),
            json!("-1"),
            json!("1.5"),
            json!("18446744073709551616"),
        ] {
            assert!(serde_json::from_value::<Effort>(value).is_err());
        }
    }

    #[test]
    fn interactive_fast_mode_changes_tier_not_the_selected_family_model() {
        use super::super::model_allowlist::{
            resolve_model_request_with_config_override, with_test_catalog,
        };
        with_test_catalog(
            json!([
                {"slug":"gpt-6-astra","service_tiers":[{"id":"priority"}]},
                {"slug":"gpt-6.1-sol","service_tiers":[{"id":"priority"}]},
                {"slug":"gpt-6-sol","service_tiers":[{"id":"priority"}]},
                {"slug":"gpt-6-luna","service_tiers":[{"id":"priority"}]}
            ]),
            || {
                for (alias, model) in [
                    ("fable", "gpt-6-astra"),
                    ("opus", "gpt-6.1-sol"),
                    ("sonnet", "gpt-6-sol"),
                    ("haiku", "gpt-6-luna"),
                ] {
                    let mut req: MessagesRequest = serde_json::from_value(json!({"model":alias,"messages":[{"role":"user","content":"hello"}],"speed":"fast"})).unwrap();
                    let resolved = resolve_model_request_with_config_override(alias, false);
                    let options = || TranslateOptions {
                        model: resolved.model.clone(),
                        use_responses_lite: true,
                        ..opts()
                    };
                    let out = translate_request(&req, options()).unwrap();
                    assert_eq!(out.model, model);
                    assert_eq!(out.service_tier, Some(ServiceTier::Priority));
                    req.extra.insert("speed".to_string(), json!("standard"));
                    assert!(
                        translate_request(&req, options())
                            .unwrap()
                            .service_tier
                            .is_none()
                    );
                    req.extra.remove("speed");
                    assert!(
                        translate_request(&req, options())
                            .unwrap()
                            .service_tier
                            .is_none()
                    );
                    req.extra.insert("speed".to_string(), json!("invalid"));
                    assert!(
                        translate_request(&req, options())
                            .unwrap_err()
                            .to_string()
                            .contains("Invalid speed")
                    );
                }
            },
        );
        with_test_catalog(json!([{"slug":"no-fast","service_tiers":[]}]), || {
            let req: MessagesRequest =
                serde_json::from_value(json!({"model":"no-fast","messages":[],"speed":"fast"}))
                    .unwrap();
            assert!(
                translate_request(
                    &req,
                    TranslateOptions {
                        model: "no-fast".to_string(),
                        ..opts()
                    }
                )
                .unwrap_err()
                .to_string()
                .contains("service_tier 'priority'")
            );
        });
    }

    #[test]
    fn catalog_defaults_and_capabilities_shape_anthropic_requests() {
        super::super::model_allowlist::with_test_catalog(
            json!([{
                "slug":"future", "default_reasoning_level":"low",
                "supported_reasoning_levels":[{"effort":"low"},{"effort":"future-effort"}],
                "supports_reasoning_summary_parameter":true, "default_reasoning_summary":"none",
                "support_verbosity":true, "default_verbosity":"medium",
                "supports_parallel_tool_calls":false, "input_modalities":["text"]
            }]),
            || {
                let mut req: MessagesRequest =
                    serde_json::from_value(json!({"messages":[{"role":"user","content":"hello"}]}))
                        .unwrap();
                let options = || TranslateOptions {
                    model: "future".to_string(),
                    ..opts()
                };
                let out = translate_request(&req, options()).unwrap();
                assert_eq!(out.reasoning.as_ref().unwrap().effort, Some(Effort::Low));
                assert_eq!(
                    out.reasoning.as_ref().unwrap().summary.as_deref(),
                    Some("none")
                );
                assert_eq!(out.text.verbosity.as_deref(), Some("medium"));
                assert!(!out.parallel_tool_calls);
                assert!(
                    out.include.is_some(),
                    "summary:none must not disable encrypted reasoning replay"
                );
                req.extra.insert(
                    "output_config".to_string(),
                    json!({"effort":"future-effort"}),
                );
                assert_eq!(
                    translate_request(&req, options())
                        .unwrap()
                        .reasoning
                        .unwrap()
                        .effort,
                    Some(Effort::Other("future-effort".to_string()))
                );
                req.extra
                    .insert("output_config".to_string(), json!({"effort":"none"}));
                assert!(
                    translate_request(&req, options())
                        .unwrap_err()
                        .to_string()
                        .contains("Allowed: low, future-effort")
                );
                req.extra.remove("output_config");
                req.extra
                    .insert("parallel_tool_calls".to_string(), json!(true));
                assert!(
                    translate_request(&req, options())
                        .unwrap_err()
                        .to_string()
                        .contains("parallel_tool_calls")
                );
                req.extra.remove("parallel_tool_calls");
                req.messages[0].content = json!([{"type":"image","source":{"type":"url","url":"https://example.test/image.png"}}]);
                assert!(
                    translate_request(&req, options())
                        .unwrap_err()
                        .to_string()
                        .contains("image inputs")
                );
            },
        );
    }

    #[test]
    fn unsupported_summary_and_verbosity_are_omitted_without_losing_json_format() {
        super::super::model_allowlist::with_test_catalog(
            json!([{
                "slug":"future", "default_reasoning_level":"low", "supports_reasoning_summary_parameter":false,
                "support_verbosity":false
            }]),
            || {
                let req: MessagesRequest = serde_json::from_value(json!({"messages":[{"role":"user","content":"hello"}],"output_config":{"format":{"type":"json_object"}}})).unwrap();
                let out = translate_request(
                    &req,
                    TranslateOptions {
                        model: "future".to_string(),
                        ..opts()
                    },
                )
                .unwrap();
                assert!(out.reasoning.unwrap().summary.is_none());
                assert!(out.text.verbosity.is_none());
                assert!(matches!(
                    out.text.format,
                    Some(ResponsesTextFormat::JsonObject)
                ));
                assert!(model_reasoning_summary("future", Some("detailed"), None).is_err());
                assert!(model_verbosity("future", Some("high")).is_err());
            },
        );
    }

    const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    fn opts() -> TranslateOptions {
        TranslateOptions {
            session_id: None,
            service_tier: None,
            model: "gpt-5.5".to_string(),
            use_responses_lite: false,
        }
    }

    #[test]
    fn input_validation_rejects_lossy_blocks_with_message_and_block_paths() {
        for (role, block, detail) in [
            (
                "user",
                json!({"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"YQ=="}}),
                "document",
            ),
            ("user", json!({"type":"future_block"}), "future_block"),
            ("user", json!({}), "type"),
            ("user", json!(null), "type"),
            ("user", json!({"type":"text"}), "text"),
            ("user", json!({"type":"text","text":42}), "text"),
            ("user", json!({"type":"image"}), "source"),
            ("user", json!({"type":"image","source":{}}), "type"),
            (
                "user",
                json!({"type":"image","source":{"type":"file","data":"a"}}),
                "file",
            ),
            (
                "user",
                json!({"type":"image","source":{"type":"url","url":""}}),
                "empty",
            ),
            (
                "user",
                json!({"type":"image","source":{"type":"base64","media_type":"image/png"}}),
                "data",
            ),
            ("assistant", json!({"type":"tool_use","name":"Read"}), "id"),
            (
                "assistant",
                json!({"type":"tool_use","id":"tu_1","name":""}),
                "name",
            ),
            ("user", json!({"type":"tool_result"}), "tool_use_id"),
            (
                "user",
                json!({"type":"tool_result","tool_use_id":"tu_1","content":null}),
                "content",
            ),
            (
                "user",
                json!({"type":"tool_result","tool_use_id":"tu_1","content":{}}),
                "content",
            ),
            (
                "user",
                json!({"type":"tool_result","tool_use_id":"tu_1","is_error":"true"}),
                "is_error",
            ),
            (
                "assistant",
                json!({"type":"thinking","thinking":false}),
                "thinking",
            ),
            (
                "assistant",
                json!({"type":"thinking","thinking":"summary","signature":42}),
                "signature",
            ),
            (
                "assistant",
                json!({"type":"image","source":{"type":"url","url":"https://example.invalid/a.png"}}),
                "image",
            ),
            (
                "user",
                json!({"type":"tool_use","id":"tu_1","name":"Read"}),
                "tool_use",
            ),
            (
                "system",
                json!({"type":"tool_result","tool_use_id":"tu_1"}),
                "tool_result",
            ),
        ] {
            let req: MessagesRequest = serde_json::from_value(json!({"messages":[
                {"role":"user","content":"valid first message"},
                {"role":role,"content":[{"type":"text","text":"valid first block"},block]}
            ]}))
            .unwrap();
            for use_responses_lite in [false, true] {
                let error = translate_request(
                    &req,
                    TranslateOptions {
                        use_responses_lite,
                        ..opts()
                    },
                )
                .unwrap_err()
                .to_string();
                assert!(error.contains("messages[1].content[1]"), "{error}");
                assert!(error.contains(detail), "{error}");
            }
        }
        for content in [json!(null), json!({"type":"text","text":"lost"}), json!(42)] {
            let req: MessagesRequest =
                serde_json::from_value(json!({"messages":[{"role":"user","content":content}]}))
                    .unwrap();
            let error = translate_request(&req, opts()).unwrap_err().to_string();
            assert!(
                error.contains("messages[0].content must be a string or an array"),
                "{error}"
            );
        }
    }

    #[test]
    fn input_validation_preserves_tool_defaults_and_explicit_nested_placeholders() {
        let req: MessagesRequest = serde_json::from_value(json!({"messages":[
            {"role":"system","content":[{"type":"text","text":"instructions"}]},
            {"role":"assistant","content":[
                {"type":"tool_use","id":"tu_1","name":"Read","client_extension":true},
                {"type":"thinking","thinking":"foreign summary","signature":"anthropic-signature"},
                {"type":"tool_use","id":"tu_2","name":"Read","input":null}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu_1","content":[
                    {"type":"text","text":"before"},
                    {"type":"tool_reference","tool_name":"Read"},
                    {"type":"document","source":{"media_type":"application/pdf"}},
                    {"type":"text"},
                    {"type":"image","source":{"type":"url","url":"https://example.invalid/a.png"}},
                    {"type":"text","text":"after"}
                ]},
                {"type":"tool_result","tool_use_id":"tu_2"}
            ]}
        ]}))
        .unwrap();
        let wire = serde_json::to_value(translate_request(&req, opts()).unwrap()).unwrap();
        assert_eq!(wire["input"][0]["role"], "developer");
        assert_eq!(wire["input"][1]["arguments"], "null");
        assert_eq!(wire["input"][2]["arguments"], "null");
        assert_eq!(
            wire["input"][3]["output"],
            "before\nTool loaded: Read\n[unsupported content block omitted: document]\n[unsupported content block omitted: text]\n[image omitted: url]\nafter"
        );
        assert_eq!(wire["input"][4]["output"], "");
    }

    #[test]
    fn service_tier_override_values_normalize_to_wire_tiers() {
        for (value, tier, wire) in [
            ("fast", ServiceTier::Priority, "priority"),
            ("priority", ServiceTier::Priority, "priority"),
            ("ultrafast", ServiceTier::Ultrafast, "ultrafast"),
            ("flex", ServiceTier::Flex, "flex"),
        ] {
            let normalized = resolve_service_tier_value("unknown-model", value).unwrap();
            assert_eq!(normalized, tier, "{value}");
            assert_eq!(serde_json::to_value(normalized).unwrap(), json!(wire));
        }
        for value in ["", "bad tier", "control\n"] {
            assert!(resolve_service_tier_value("unknown-model", value).is_err());
        }
    }

    #[test]
    fn service_tier_override_wins_and_narrows_to_the_model() {
        let cases = [
            (None, None, "gpt-6-astra", None),
            (
                None,
                Some(ServiceTier::Ultrafast),
                "gpt-6-astra",
                Some(ServiceTier::Ultrafast),
            ),
            (
                None,
                Some(ServiceTier::Ultrafast),
                "gpt-6-sol",
                Some(ServiceTier::Ultrafast),
            ),
            (
                Some("ultrafast"),
                None,
                "gpt-6-astra",
                Some(ServiceTier::Ultrafast),
            ),
            (
                Some("ultrafast"),
                Some(ServiceTier::Priority),
                "gpt-6-astra",
                Some(ServiceTier::Ultrafast),
            ),
            (
                Some("ultrafast"),
                None,
                "gpt-6-luna",
                Some(ServiceTier::Ultrafast),
            ),
            (
                Some("fast"),
                Some(ServiceTier::Ultrafast),
                "gpt-6-astra",
                Some(ServiceTier::Priority),
            ),
            (
                Some("flex"),
                Some(ServiceTier::Ultrafast),
                "gpt-6-astra",
                Some(ServiceTier::Flex),
            ),
        ];
        for (override_tier, model_tier, model, expected) in cases {
            let picked = pick_service_tier(override_tier, model_tier.clone(), model).unwrap();
            assert_eq!(picked, expected, "{override_tier:?} {model_tier:?} {model}");
        }
        assert!(pick_service_tier(Some("bad tier"), None, "gpt-6-astra").is_err());
    }

    #[test]
    fn responses_tool_choice_modes_serialize_as_openai_strings() {
        for (mode, expected) in [
            (ResponsesToolChoiceMode::Auto, json!("auto")),
            (ResponsesToolChoiceMode::None, json!("none")),
            (ResponsesToolChoiceMode::Required, json!("required")),
        ] {
            assert_eq!(
                serde_json::to_value(ResponsesToolChoice::Mode(mode)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn translate_tool_choice_preserves_wire_and_parallel_semantics() {
        for (tool_choice, expected_choice, expected_parallel) in [
            (json!({"type":"auto"}), json!("auto"), true),
            (
                json!({"type":"auto","disable_parallel_tool_use":false}),
                json!("auto"),
                true,
            ),
            (json!({"type":"none"}), json!("none"), true),
            (json!({"type":"any"}), json!("required"), true),
            (
                json!({"type":"any","disable_parallel_tool_use":true}),
                json!("required"),
                false,
            ),
            (
                json!({
                    "type":"tool",
                    "name":"test",
                    "disable_parallel_tool_use":true
                }),
                json!({"type":"function","name":"test"}),
                false,
            ),
        ] {
            let req: MessagesRequest = serde_json::from_value(json!({
                "model": "gpt-5.5",
                "messages": [{"role":"user", "content":"use the tool"}],
                "tools": [{
                    "name":"test",
                    "input_schema":{"type":"object","properties":{}}
                }],
                "tool_choice": tool_choice
            }))
            .unwrap();
            let wire = serde_json::to_value(translate_request(&req, opts()).unwrap()).unwrap();

            assert_eq!(wire["tool_choice"], expected_choice);
            assert_eq!(wire["parallel_tool_calls"], expected_parallel);
        }
    }

    #[test]
    fn responses_lite_keeps_parallel_tool_calls_disabled() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"use the tool"}],
            "tools": [{
                "name":"test",
                "input_schema":{"type":"object","properties":{}}
            }],
            "tool_choice": {
                "type":"any",
                "disable_parallel_tool_use":false
            }
        }))
        .unwrap();
        let wire = serde_json::to_value(
            translate_request(
                &req,
                TranslateOptions {
                    model: "gpt-5.6-luna".to_string(),
                    use_responses_lite: true,
                    ..opts()
                },
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(wire["tool_choice"], json!("required"));
        assert_eq!(wire["parallel_tool_calls"], false);
    }

    #[test]
    fn translate_web_search_tool_to_codex_tool() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["example.com"]
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                session_id: Some("s".into()),
                service_tier: None,
                model: "gpt-5.5".to_string(),
                use_responses_lite: false,
            },
        )
        .unwrap();
        assert_eq!(out.prompt_cache_key.as_deref(), Some("s"));
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::AllowedTools { .. })
        ));
        let tool_choice = serde_json::to_value(out.tool_choice.as_ref().unwrap()).unwrap();
        assert_eq!(tool_choice["type"], "allowed_tools");
        assert_eq!(tool_choice["mode"], "required");
        assert_eq!(tool_choice["tools"], json!([{"type":"web_search"}]));
        let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
            panic!("expected web_search tool");
        };
        assert!(tool.external_web_access);
        assert_eq!(
            tool.filters.as_ref().unwrap().allowed_domains.as_deref(),
            Some(&["example.com".to_string()][..])
        );
        assert!(out.instructions.is_none());
    }

    #[test]
    fn automatic_filtered_web_search_keeps_native_filters() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["example.com"],
                "blocked_domains":["spam.example"]
            }],
            "tool_choice": {"type":"auto"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
            panic!("expected web_search tool");
        };
        assert!(tool.external_web_access);
        let filters = tool.filters.as_ref().unwrap();
        assert_eq!(
            filters.allowed_domains.as_deref(),
            Some(&["example.com".to_string()][..])
        );
        assert_eq!(
            filters.blocked_domains.as_deref(),
            Some(&["spam.example".to_string()][..])
        );
        assert!(out.instructions.is_none());
    }

    #[test]
    fn forced_filtered_web_search_keeps_native_filters() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"find it"}],
            "system": "Be brief.",
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["a.example", "b.example"],
                "blocked_domains":["spam.example"]
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
            panic!("expected web_search tool");
        };
        let filters = tool.filters.as_ref().unwrap();
        assert_eq!(
            filters.allowed_domains.as_deref(),
            Some(&["a.example".to_string(), "b.example".to_string()][..])
        );
        assert_eq!(
            filters.blocked_domains.as_deref(),
            Some(&["spam.example".to_string()][..])
        );
        assert_eq!(out.instructions.as_deref(), Some("Be brief."));
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::AllowedTools { .. })
        ));
    }

    #[test]
    fn unfiltered_web_search_adds_no_domain_instructions() {
        for tool_choice in [None, Some(json!({"type":"tool", "name":"web_search"}))] {
            let mut body = json!({
                "model": "gpt-5.5",
                "messages": [{"role":"user", "content":"find it"}],
                "tools": [{"type":"web_search_20250305", "name":"web_search"}]
            });
            if let Some(tool_choice) = tool_choice {
                body["tool_choice"] = tool_choice;
            }
            let req: MessagesRequest = serde_json::from_value(body).unwrap();
            let out = translate_request(&req, opts()).unwrap();
            let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
                panic!("expected web_search tool");
            };
            assert!(tool.external_web_access);
            assert!(tool.filters.is_none());
            assert!(out.instructions.is_none());
        }
    }

    #[test]
    fn has_hosted_web_search_detects_web_search_tool() {
        let with: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [
                {"name":"Bash", "input_schema":{}},
                {"type":"web_search_20250305", "name":"web_search"}
            ]
        }))
        .unwrap();
        assert!(has_hosted_web_search(&with));

        let without: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"run it"}],
            "tools": [{"name":"Bash", "input_schema":{}}]
        }))
        .unwrap();
        assert!(!has_hosted_web_search(&without));
    }

    #[test]
    fn responses_lite_downgrades_unregistered_web_search_tool_choice() {
        // On the lite lane tools travel in the AdditionalTools developer
        // prefix, so a top-level web_search tool_choice would reference a
        // tool upstream doesn't know about and 502.
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search"
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: true,
            },
        )
        .unwrap();
        assert!(out.tools.is_none());
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto))
        ));
        assert_eq!(
            serde_json::to_value(&out).unwrap()["tool_choice"],
            json!("auto")
        );
    }

    #[test]
    fn full_lane_keeps_web_search_tool_choice_registered() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search"
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: false,
            },
        )
        .unwrap();
        assert!(out.tools.as_ref().is_some_and(|t| {
            t.iter()
                .any(|tool| matches!(tool, ResponsesTool::WebSearch(_)))
        }));
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::AllowedTools { .. })
        ));
    }

    #[test]
    fn translate_artifact_tool_strips_exact_incompatible_pattern() {
        let req: MessagesRequest = serde_json::from_str(
            r#"{
                "model": "gpt-5.5",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{
                    "name": "Artifact",
                    "description": "Manage artifacts.",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "field": {
                                "type": "string",
                                "pattern": "^(?!__.*__$)[^\\p{Cc}\\p{Cf}\\p{Zl}\\p{Zp}\"\\\\./[\\]]{1,200}$",
                                "minLength": 1,
                                "maxLength": 200
                            }
                        },
                        "required": ["field"],
                        "additionalProperties": false
                    }
                }]
            }"#,
        )
        .unwrap();
        let original_tools = req.extra["tools"].clone();
        assert_eq!(
            original_tools[0]["input_schema"]["properties"]["field"]["pattern"],
            r#"^(?!__.*__$)[^\p{Cc}\p{Cf}\p{Zl}\p{Zp}"\\./[\]]{1,200}$"#
        );
        let mut expected = original_tools[0]["input_schema"].clone();
        expected["properties"]["field"]
            .as_object_mut()
            .unwrap()
            .remove("pattern");

        let normal = translate_request(&req, opts()).unwrap();
        let lite = translate_request(
            &req,
            TranslateOptions {
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        for out in [normal, lite] {
            let wire = serde_json::to_value(out).unwrap();
            let tool = if wire["tools"].is_array() {
                &wire["tools"][0]
            } else {
                &wire["input"][0]["tools"][0]
            };
            assert_eq!(
                tool,
                &json!({
                    "type": "function",
                    "name": "Artifact",
                    "description": "Manage artifacts.",
                    "parameters": expected,
                    "strict": false
                })
            );
        }
        assert_eq!(req.extra["tools"], original_tools);
    }

    #[test]
    fn tool_schema_patterns_are_removed_only_from_schema_locations() {
        let literal = json!({
            "pattern": "literal pattern",
            "properties": {"pattern": {"pattern": "also literal"}},
            "items": [{"pattern": "literal item"}]
        });
        let leaf = json!({
            "type": "string",
            "pattern": "^[a-z]+$",
            "description": "Keep metadata and literal data.",
            "default": literal,
            "examples": [literal],
            "const": literal,
            "enum": [literal],
            "x-custom": literal
        });
        let mut clean_leaf = leaf.clone();
        clean_leaf.as_object_mut().unwrap().remove("pattern");

        let mut schema = json!({"pattern": "^root$", "required": ["pattern"]});
        let mut expected = json!({"required": ["pattern"]});
        for keyword in [
            "properties",
            "patternProperties",
            "$defs",
            "definitions",
            "dependentSchemas",
            "dependencies",
        ] {
            schema[keyword] = json!({
                "pattern": leaf,
                "properties": {"properties": {"pattern": leaf}},
                "booleanSchema": false
            });
            expected[keyword] = json!({
                "pattern": clean_leaf,
                "properties": {"properties": {"pattern": clean_leaf}},
                "booleanSchema": false
            });
        }
        for keyword in ["allOf", "anyOf", "oneOf", "prefixItems", "items"] {
            schema[keyword] = json!([leaf, true, false]);
            expected[keyword] = json!([clean_leaf, true, false]);
        }
        for keyword in [
            "additionalProperties",
            "additionalItems",
            "unevaluatedProperties",
            "unevaluatedItems",
            "contains",
            "propertyNames",
            "not",
            "if",
            "then",
            "else",
            "contentSchema",
        ] {
            schema[keyword] = leaf.clone();
            expected[keyword] = clean_leaf.clone();
        }
        schema["dependencies"]["literalDependency"] = json!(["pattern"]);
        expected["dependencies"]["literalDependency"] = json!(["pattern"]);
        schema["$ref"] = json!("#/$defs/pattern");
        expected["$ref"] = json!("#/$defs/pattern");
        assert_eq!(codex_tool_parameters("Custom", schema), expected);

        let nested = json!({"items": {"properties": {"pattern": leaf}}});
        let expected = json!({"items": {"properties": {"pattern": clean_leaf}}});
        assert_eq!(codex_tool_parameters("Custom", nested), expected);
        for unchanged in [json!({}), json!(true), json!(false)] {
            assert_eq!(
                codex_tool_parameters("Custom", unchanged.clone()),
                unchanged
            );
        }
    }

    #[test]
    fn translate_read_tool_adds_codex_offset_guidance() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"read it"}],
            "tools": [{
                "name": "Read",
                "description": "Reads a file from the local filesystem.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string", "pattern": "^/"},
                        "offset": {"type": "integer", "description": "old offset"},
                        "limit": {"type": "integer", "description": "old limit"}
                    },
                    "required": ["file_path"]
                }
            }]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let tools = out.tools.as_ref().unwrap();
        let ResponsesTool::Function(tool) = &tools[0] else {
            panic!("expected function tool");
        };
        let description = tool.description.as_deref().unwrap();
        assert!(description.contains("Codex Read guidance"));
        assert!(description.contains("zero based continuation index"));
        assert!(description.contains("guessed positions are invalid offsets"));

        let props = tool
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(props["file_path"], json!({"type": "string"}));
        assert_eq!(
            props
                .get("offset")
                .and_then(|v| v.get("description"))
                .and_then(Value::as_str),
            Some(
                "Optional continuation index. Use only after a prior Read of the same file returned content and more lines are needed. Compute as prior offset plus returned line count. Displayed line numbers, grep line numbers, byte counts, token counts, file sizes, and guessed positions are invalid offsets. Omit when unsure."
            )
        );
        assert_eq!(
            props
                .get("limit")
                .and_then(|v| v.get("description"))
                .and_then(Value::as_str),
            Some(
                "Optional number of lines to read. Omit when opening a file. Use with offset only when continuing a large file."
            )
        );
    }

    #[test]
    fn translate_non_read_tool_preserves_tool_metadata() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"search"}],
            "tools": [{
                "name": "Search",
                "description": "Find matching records.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "offset": {"type": "integer", "description": "record offset"}
                    }
                }
            }]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let tools = out.tools.as_ref().unwrap();
        let ResponsesTool::Function(tool) = &tools[0] else {
            panic!("expected function tool");
        };
        assert_eq!(tool.description.as_deref(), Some("Find matching records."));
        assert!(!tool.strict);
        assert_eq!(
            serde_json::to_value(tool).unwrap()["strict"],
            Value::Bool(false)
        );
        assert_eq!(
            tool.parameters
                .get("properties")
                .and_then(|v| v.get("offset"))
                .and_then(|v| v.get("description"))
                .and_then(Value::as_str),
            Some("record offset")
        );
    }

    #[test]
    fn translate_omits_reasoning_when_not_enabled() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(out.reasoning.is_none());
        assert!(out.include.is_none());
    }

    #[test]
    fn translate_includes_reasoning_when_enabled() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "medium"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let reasoning = out.reasoning.unwrap();
        assert!(matches!(reasoning.effort, Some(Effort::Medium)));
        assert_eq!(reasoning.summary.as_deref(), Some("auto"));
        assert_eq!(
            out.include,
            Some(vec!["reasoning.encrypted_content".to_string()])
        );
    }

    #[test]
    fn translate_effort_max_maps_to_max() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "max"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Max)));
    }

    #[test]
    fn translate_effort_override_max_maps_to_max() {
        let effort = resolve_effort_override(Some(Effort::Low), Some("max")).unwrap();
        assert!(matches!(effort, Some(Effort::Max)));
    }

    #[test]
    fn compact_request_detected_from_system_marker() {
        assert!(is_compact_request(Some(
            "You are a helpful AI assistant tasked with summarizing conversations."
        )));
        assert!(!is_compact_request(Some("You are Claude Code.")));
        assert!(!is_compact_request(None));
    }

    #[test]
    fn compact_request_detected_from_final_user_message() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [
                {"role": "user", "content": "prior turn"},
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "tool-1",
                            "content": "result"
                        },
                        {
                            "type": "text",
                            "text": concat!(
                                "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\n",
                                "Your task is to create a detailed summary of the conversation so far, ",
                                "paying close attention to the user's explicit requests."
                            )
                        }
                    ]
                }
            ],
            "system": "You are Claude Code."
        }))
        .unwrap();

        assert!(is_compact_messages_request(&req));
    }

    #[test]
    fn compact_request_detected_when_context_follows_compact_message() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [
                {
                    "role": "user",
                    "content": concat!(
                        "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n",
                        "Your task is to create a detailed summary of the conversation so far."
                    )
                },
                {"role": "assistant", "content": "context appended after compact prompt"}
            ],
            "system": "You are Claude Code."
        }))
        .unwrap();

        assert!(is_compact_messages_request(&req));
    }

    #[test]
    fn historical_compact_message_outside_detection_tail_is_ignored() {
        let mut messages = vec![
            serde_json::from_value(json!({
                "role": "user",
                "content": concat!(
                    "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n",
                    "Your task is to create a detailed summary of the conversation so far."
                )
            }))
            .unwrap(),
        ];
        messages.extend((0..COMPACT_DETECTION_TAIL_MESSAGES).map(|index| {
            serde_json::from_value(json!({
                "role": "user",
                "content": format!("ordinary message {index}")
            }))
            .unwrap()
        }));

        let req = MessagesRequest {
            model: Some("gpt-5.6-sol".to_string()),
            messages,
            ..serde_json::from_value(json!({
                "model": "gpt-5.6-sol",
                "messages": []
            }))
            .unwrap()
        };

        assert!(!is_compact_messages_request(&req));
    }

    #[test]
    fn compact_message_requires_both_markers() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{
                "role": "user",
                "content": "Your task is to create a detailed summary of the conversation so far."
            }],
            "system": "You are Claude Code."
        }))
        .unwrap();

        assert!(!is_compact_messages_request(&req));
    }

    #[test]
    fn compact_effort_cap_parses_env_values() {
        assert!(matches!(compact_effort_cap_from(None), Some(Effort::Low)));
        assert!(matches!(
            compact_effort_cap_from(Some("")),
            Some(Effort::Low)
        ));
        assert!(compact_effort_cap_from(Some("off")).is_none());
        assert!(matches!(
            compact_effort_cap_from(Some("none")),
            Some(Effort::None)
        ));
        assert!(matches!(
            compact_effort_cap_from(Some("medium")),
            Some(Effort::Medium)
        ));
        // Unrecognized values fall back to the safe default.
        assert!(matches!(
            compact_effort_cap_from(Some("bogus")),
            Some(Effort::Low)
        ));
    }

    #[test]
    fn compact_effort_cap_defaults_a_missing_effort() {
        // No effort named: take the cap instead of the upstream default.
        assert!(matches!(
            apply_compact_effort_cap(None, Some(Effort::Low)),
            Some(Effort::Low)
        ));
        assert!(matches!(
            apply_compact_effort_cap(None, Some(Effort::None)),
            Some(Effort::None)
        ));
        // An explicit effort at or below the cap survives.
        assert!(matches!(
            apply_compact_effort_cap(Some(Effort::None), Some(Effort::Low)),
            Some(Effort::None)
        ));
        assert!(matches!(
            apply_compact_effort_cap(Some(Effort::Low), Some(Effort::Low)),
            Some(Effort::Low)
        ));
        // Above the cap is lowered.
        assert!(matches!(
            apply_compact_effort_cap(Some(Effort::High), Some(Effort::Low)),
            Some(Effort::Low)
        ));
        // Cap disabled: the request is left exactly as it asked.
        assert!(apply_compact_effort_cap(None, None).is_none());
        assert!(matches!(
            apply_compact_effort_cap(Some(Effort::High), None),
            Some(Effort::High)
        ));
    }

    #[test]
    fn only_a_non_none_effort_requests_reasoning_artifacts() {
        assert!(!reasoning_requested(None));
        assert!(!reasoning_requested(Some(&Effort::None)));
        for effort in [
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::Xhigh,
            Effort::Max,
        ] {
            assert!(reasoning_requested(Some(&effort)));
        }
    }

    #[test]
    fn compact_request_downgrades_effort_to_cap() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"summarize"}],
            "system": "You are a helpful AI assistant tasked with summarizing conversations.",
            "output_config": {"effort": "medium"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Low)));
    }

    #[test]
    fn compact_cap_never_raises_effort() {
        // A compact request already at or below the cap is left alone.
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"summarize"}],
            "system": "You are a helpful AI assistant tasked with summarizing conversations.",
            "output_config": {"effort": "low"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Low)));
    }

    #[test]
    fn non_compact_request_keeps_requested_effort() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "system": "You are Claude Code.",
            "output_config": {"effort": "high"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::High)));
    }

    #[test]
    fn effort_ordering_matches_variant_order() {
        assert!(Effort::None < Effort::Low);
        assert!(Effort::Low < Effort::Medium);
        assert!(Effort::Medium < Effort::High);
        assert!(Effort::High < Effort::Xhigh);
        assert!(Effort::Xhigh < Effort::Max);
    }

    #[test]
    fn max_tokens_is_not_serialized_for_codex() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "max_tokens": 4096,
            "messages": [{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let value = serde_json::to_value(out).unwrap();
        assert!(value.get("max_output_tokens").is_none());
    }

    #[test]
    fn translate_effort_xhigh_maps_to_xhigh() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "xhigh"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Xhigh)));
        assert_eq!(
            out.include,
            Some(vec!["reasoning.encrypted_content".to_string()])
        );
    }

    #[test]
    fn reasoning_summary_override_values() {
        for (requested, expected) in [
            (None, "auto"),
            (Some("auto"), "auto"),
            (Some("detailed"), "detailed"),
            (Some("off"), "none"),
            (Some("none"), "none"),
        ] {
            assert_eq!(
                model_reasoning_summary("unknown-model", requested, Some("auto"))
                    .unwrap()
                    .as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn translate_user_text_and_image() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [
                {"type":"text", "text":"describe"},
                {"type":"image", "source": {"type":"base64", "media_type":"image/jpeg", "data":"xyz"}}
            ]}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 1);
        if let ResponsesInputItem::Message { role, content } = &out.input[0] {
            assert_eq!(role, "user");
            assert_eq!(content.len(), 2);
        } else {
            panic!("expected Message");
        }
    }

    #[test]
    fn translate_assistant_with_text_and_tool_use() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"assistant", "content": [
                {"type":"text", "text":"answer"},
                {"type":"tool_use", "id":"tu_1", "name":"search", "input": {"q":"rust"}}
            ]}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
    }

    #[test]
    fn translate_strict_json_schema_normalization() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hi"}],
            "output_config": {"format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": {"ok": {"type": "boolean"}, "reason": {"type": "string"}},
                    "required": ["ok"]
                }
            }}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        if let Some(ResponsesTextFormat::JsonSchema { schema, .. }) = &out.text.format {
            let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
            assert!(required.iter().any(|v| v == "ok"));
            assert!(required.iter().any(|v| v == "reason"));
        } else {
            panic!("expected JsonSchema format");
        }
    }

    fn tool_search_request(load: bool, extra_turn: bool) -> MessagesRequest {
        let mut tools = vec![
            json!({"name":"Bash","description":"run","input_schema":{"type":"object","properties":{}}}),
            json!({"name":"ToolSearch","description":"search","input_schema":{"type":"object","properties":{}}}),
            json!({"name":"DeferredToolPlaceholder","description":"never call","defer_loading":true,
                   "input_schema":{"type":"object","properties":{}}}),
        ];
        let mut messages = vec![json!({"role":"user","content":"find the mail tool"})];
        if load {
            tools.push(json!({"name":"mcp__mail__search","description":"search mail","defer_loading":true,
                              "input_schema":{"type":"object","properties":{"q":{"type":"string"}}}}));
            messages.push(
                json!({"role":"assistant","content":[{"type":"tool_use","id":"tu_search",
                "name":"ToolSearch","input":{"query":"select:mcp__mail__search"}}]}),
            );
            messages.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu_search",
                 "content":[{"type":"tool_reference","tool_name":"mcp__mail__search"}]},
                {"type":"text","text":"Tool loaded."}]}));
        }
        if extra_turn {
            messages.push(
                json!({"role":"assistant","content":[{"type":"tool_use","id":"tu_bash",
                "name":"Bash","input":{"command":"ls"}}]}),
            );
            messages.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu_bash","content":"a.txt"}]}));
        }
        serde_json::from_value(json!({"model":"gpt-6-astra","messages":messages,"tools":tools}))
            .unwrap()
    }

    fn lite_wire(req: &MessagesRequest) -> Value {
        serde_json::to_value(
            translate_request(
                req,
                TranslateOptions {
                    model: "gpt-6-astra".to_string(),
                    use_responses_lite: true,
                    ..opts()
                },
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn additional_tool_names(item: &Value) -> Vec<String> {
        item["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn loaded_deferred_tool_is_appended_after_its_tool_search_result() {
        let wire = lite_wire(&tool_search_request(true, false));
        let input = wire["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "additional_tools");
        assert_eq!(additional_tool_names(&input[0]), vec!["Bash", "ToolSearch"]);
        let output_at = input
            .iter()
            .position(|item| {
                item["type"] == "function_call_output" && item["call_id"] == "tu_search"
            })
            .unwrap();
        assert_eq!(input[output_at]["output"], "Tool loaded: mcp__mail__search");
        assert_eq!(input[output_at + 1]["type"], "additional_tools");
        assert_eq!(input[output_at + 1]["role"], "developer");
        assert_eq!(
            additional_tool_names(&input[output_at + 1]),
            vec!["mcp__mail__search"]
        );
        assert!(!wire.to_string().contains("DeferredToolPlaceholder"));
    }

    #[test]
    fn tool_load_keeps_the_earlier_request_as_a_prefix() {
        let before = lite_wire(&tool_search_request(false, false));
        let after = lite_wire(&tool_search_request(true, false));
        let later = lite_wire(&tool_search_request(true, true));
        let before_input = before["input"].as_array().unwrap();
        let after_input = after["input"].as_array().unwrap();
        let later_input = later["input"].as_array().unwrap();
        assert_eq!(&after_input[..before_input.len()], &before_input[..]);
        assert_eq!(&later_input[..after_input.len()], &after_input[..]);
    }

    #[test]
    fn deferred_tool_without_a_visible_load_stays_in_the_prefix() {
        let mut req = tool_search_request(false, false);
        req.extra["tools"].as_array_mut().unwrap().push(json!({
            "name":"mcp__mail__search","description":"search mail","defer_loading":true,
            "input_schema":{"type":"object","properties":{}}
        }));
        let wire = lite_wire(&req);
        let input = wire["input"].as_array().unwrap();
        assert_eq!(
            additional_tool_names(&input[0]),
            vec!["Bash", "ToolSearch", "mcp__mail__search"]
        );
        assert_eq!(
            input
                .iter()
                .filter(|item| item["type"] == "additional_tools")
                .count(),
            1
        );
    }

    #[test]
    fn full_lane_keeps_loaded_tools_and_drops_the_placeholder() {
        let out = translate_request(&tool_search_request(true, false), opts()).unwrap();
        let wire = serde_json::to_value(&out).unwrap();
        let names: Vec<&str> = wire["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["Bash", "ToolSearch", "mcp__mail__search"]);
        assert!(
            !wire["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "additional_tools")
        );
    }

    #[test]
    fn translate_tool_result_content() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [{
                "type": "tool_result",
                "tool_use_id": "tu_1",
                "content": [{"type":"text", "text":"result"}]
            }]}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 1);
        if let ResponsesInputItem::FunctionCallOutput { call_id, output } = &out.input[0] {
            assert_eq!(call_id, "tu_1");
            assert_eq!(output.as_text(), Some("result"));
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_read_offset_error_adds_guidance() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "offset": 2952, "limit": 200}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "is_error": true,
                    "content": [{"type":"text", "text":"File has 331 lines, but offset 2952 was requested."}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            let output = output.as_text().expect("text tool output");
            assert!(output.contains("[tool execution error]"));
            assert!(output.contains("File has 331 lines"));
            assert!(output.contains("Codex Read guidance:"));
            assert!(output.contains("zero based continuation index"));
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_read_unrelated_error_keeps_original_output() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "offset": 10, "limit": 20}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "is_error": true,
                    "content": [{"type":"text", "text":"File does not exist."}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            assert_eq!(
                output.as_text(),
                Some("[tool execution error]\nFile does not exist.")
            );
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_rewritten_read_result_adds_proxy_note() {
        crate::providers::codex::translate::read_rewrite::sanitize_read_args(
            "Read",
            r#"{"file_path":"/tmp/a","offset":1300000,"limit":20}"#,
            Some("tu_rewritten_read"),
        );
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_rewritten_read",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "limit": 20}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_rewritten_read",
                    "content": [{"type":"text", "text":"1\tcontent"}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            let output = output.as_text().expect("text tool output");
            assert!(output.contains("1\tcontent"));
            assert!(output.contains("Proxy Read offset note:"));
            assert!(output.contains("1300000"));
            assert!(output.contains("/tmp/a"));
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_read_success_with_offset_words_keeps_original_output() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "offset": 10, "limit": 20}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "content": [{"type":"text", "text":"File has 331 lines, and the requested offset is shown in this fixture."}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            assert_eq!(
                output.as_text(),
                Some("File has 331 lines, and the requested offset is shown in this fixture.")
            );
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_tool_result_preserves_mixed_content_order() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [{
                "type": "tool_result",
                "tool_use_id": "tu_image",
                "content": [
                    {"type": "text", "text": "before"},
                    {"type": "image", "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": PNG_BASE64
                    }},
                    {"type": "text", "text": "after"}
                ]
            }]}]
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(
            serde_json::to_value(&out.input[0]).unwrap(),
            json!({
                "type": "function_call_output",
                "call_id": "tu_image",
                "output": [
                    {"type": "input_text", "text": "before"},
                    {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_BASE64}")},
                    {"type": "input_text", "text": "after"}
                ]
            })
        );
    }

    #[test]
    fn translate_tool_result_preserves_image_then_text_order() {
        let rendered = render_tool_result(&json!([
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": PNG_BASE64
            }},
            {"type": "text", "text": "caption"}
        ]));

        assert_eq!(
            serde_json::to_value(function_call_output(rendered)).unwrap(),
            json!([
                {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_BASE64}")},
                {"type": "input_text", "text": "caption"}
            ])
        );
    }

    #[test]
    fn unsupported_tool_result_images_become_in_place_text_placeholders() {
        let rendered = render_tool_result(&json!([
            {"type": "text", "text": "before"},
            {"type": "image", "source": {
                "type": "url",
                "url": "https://example.invalid/a.png"
            }},
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "text/plain",
                "data": "aGVsbG8="
            }},
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": "not base64"
            }},
            {"type": "text", "text": "after"}
        ]));

        assert_eq!(
            serde_json::to_value(function_call_output(rendered)).unwrap(),
            json!(
                "before\n[image omitted: url]\n[unsupported content block omitted: image]\n[unsupported content block omitted: image]\nafter"
            )
        );
    }

    #[test]
    fn supported_tool_result_image_media_types_pass_validation() {
        for media_type in ["image/jpeg", "image/png", "image/gif", "image/webp"] {
            assert_eq!(
                validated_image_data_url(media_type, "YQ"),
                Some(format!("data:{media_type};base64,YQ=="))
            );
        }
        assert!(validated_image_data_url("image/svg+xml", "YQ==").is_none());
        assert!(validated_image_data_url("image/png", "").is_none());
    }

    #[test]
    fn text_only_tool_result_keeps_string_wire_format() {
        let rendered = render_tool_result(&json!([
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"}
        ]));

        assert_eq!(
            serde_json::to_value(function_call_output(rendered)).unwrap(),
            json!("first\nsecond")
        );
    }

    #[test]
    fn tool_result_error_prefix_precedes_image_content() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [{
                "type": "tool_result",
                "tool_use_id": "tu_error_image",
                "is_error": true,
                "content": [{"type": "image", "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": PNG_BASE64
                }}]
            }]}]
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(
            serde_json::to_value(&out.input[0]).unwrap()["output"],
            json!([
                {"type": "input_text", "text": "[tool execution error]"},
                {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_BASE64}")}
            ])
        );
    }

    #[test]
    fn malformed_tool_result_blocks_still_become_text_placeholders() {
        let rendered = render_tool_result(&json!([
            {"type": "text"},
            {"type": "image"},
            {}
        ]));

        assert_eq!(
            rendered.joined_text(),
            "[unsupported content block omitted: text]\n[unsupported content block omitted: image]\n[unsupported content block omitted: unknown]"
        );
        assert!(!rendered.has_images());
    }

    #[test]
    fn luna_preserves_high_effort() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "high"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::High)));
    }

    #[test]
    fn sol_preserves_high_effort() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "high"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::High)));
    }

    #[test]
    fn responses_lite_moves_instructions_and_tools_into_input() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"hello"}],
            "system": "be helpful",
            "tools": [{"name":"test","input_schema":{"type":"object"}}]
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(out.instructions.is_none());
        assert!(out.tools.is_none());
        assert!(!out.parallel_tool_calls);
        assert!(out.client_metadata.is_some());
        assert_eq!(out.input.len(), 3);
        assert!(matches!(
            out.input[0],
            ResponsesInputItem::AdditionalTools { .. }
        ));
        if let ResponsesInputItem::Message { role, content } = &out.input[1] {
            assert_eq!(role, "developer");
            assert!(matches!(content[0], ResponsesContentPart::InputText { .. }));
        } else {
            panic!("expected developer message");
        }
    }

    #[test]
    fn responses_lite_without_effort_uses_all_turns_context() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-haiku-4-5",
            "messages": [{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        let reasoning = out.reasoning.unwrap();
        assert!(reasoning.effort.is_none());
        assert!(reasoning.summary.is_none());
        assert_eq!(reasoning.context.as_deref(), Some("all_turns"));
        assert!(out.include.is_none());
    }

    #[test]
    fn responses_lite_reasoning_uses_all_turns_context() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "medium"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert_eq!(out.reasoning.unwrap().context.as_deref(), Some("all_turns"));
    }

    #[test]
    fn translate_returns_only_expected_top_level_fields() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role":"user", "content":"hello"}],
            "system": "be helpful",
            "tools": [{"name":"test","input_schema":{"type":"object"}}],
            "tool_choice": {"type":"tool", "name":"test"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.4".to_string(),
                ..opts()
            },
        )
        .unwrap();
        assert_eq!(out.model, "gpt-5.4");
        let out_value = serde_json::to_value(&out).unwrap();
        let keys: std::collections::BTreeSet<String> =
            out_value.as_object().unwrap().keys().cloned().collect();
        for key in &[
            "model",
            "input",
            "store",
            "stream",
            "parallel_tool_calls",
            "text",
        ] {
            assert!(keys.contains(*key), "missing key: {key}");
        }
    }

    #[test]
    fn assistant_thinking_signature_replays_codex_reasoning_item() {
        let replay = super::super::reasoning_signature::ReasoningReplay {
            id: "rs_1".to_string(),
            encrypted_content: "opaque".to_string(),
        };
        let signature =
            super::super::reasoning_signature::encode_reasoning_signature(&replay).unwrap();
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"user","content":"start"},
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"visible summary","signature":signature},
                    {"type":"text","text":"done"}
                ]},
                {"role":"user","content":"continue"}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let reasoning_index = out
            .input
            .iter()
            .position(|item| matches!(item, ResponsesInputItem::Reasoning { .. }))
            .unwrap();
        let ResponsesInputItem::Reasoning {
            id,
            summary,
            encrypted_content,
        } = &out.input[reasoning_index]
        else {
            unreachable!();
        };
        assert_eq!(id, "rs_1");
        assert!(summary.is_empty());
        assert_eq!(encrypted_content, "opaque");
        assert!(matches!(
            out.input.get(reasoning_index + 1),
            Some(ResponsesInputItem::Message { role, .. }) if role == "assistant"
        ));
    }
}
