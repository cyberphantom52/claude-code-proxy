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
            let required = map
                .get("properties")
                .and_then(Value::as_object)
                .map(|properties| {
                    Value::Array(properties.keys().cloned().map(Value::String).collect())
                });
            let mut out: serde_json::Map<String, Value> = map
                .iter()
                .map(|(key, value)| (key.clone(), normalize_strict_json_schema(value)))
                .collect();
            if let Some(required) = required {
                out.insert("required".into(), required);
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
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            parts.push(ResponsesContentPart::InputText { text });
                        }
                        ContentBlock::Image { source } => {
                            parts.push(ResponsesContentPart::InputImage {
                                image_url: image_source_to_url(&source),
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
                            let mut rendered = render_tool_result(&content);
                            if is_error.unwrap_or(false) {
                                rendered.prepend_text("[tool execution error]".to_string());
                            }
                            if let Some(note) =
                                rewritten_read_offset_note(&rendered.joined_text(), &tool_use_id)
                            {
                                rendered.push_text(format!("\n{note}"));
                            }
                            if should_append_read_offset_guidance(
                                &rendered.joined_text(),
                                read_tool_uses_with_offset.contains(&tool_use_id),
                                is_error.unwrap_or(false),
                            ) {
                                rendered.push_text(format!("\n{}", read_offset_guidance()));
                            }
                            out.push(ResponsesInputItem::FunctionCallOutput {
                                call_id: tool_use_id,
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
                    .into_iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => {
                            Some(ResponsesContentPart::InputText { text })
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
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            text_parts.push(ResponsesContentPart::OutputText { text });
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            flush_text(&mut out, &mut text_parts);
                            if is_read_tool_use_with_offset(&name, &input) {
                                read_tool_uses_with_offset.insert(id.clone());
                            }
                            let args =
                                serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                            out.push(ResponsesInputItem::FunctionCall {
                                call_id: id,
                                name,
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
