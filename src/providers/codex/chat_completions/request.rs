use http::StatusCode;
use serde_json::{Map, Value, json};

use crate::config;
use crate::providers::codex::translate::{
    model_allowlist::{
        assert_allowed_model, model_parallel_tool_calls, model_reasoning_summary, model_verbosity,
        resolve_model_effort, resolve_model_request, resolve_service_tier_value,
        uses_responses_lite, validate_input_modalities,
    },
    request::{Effort, ServiceTier, reasoning_requested, resolve_effort_override, to_codex_effort},
};

use super::ChatError;

const SUPPORTED_FIELDS: &[&str] = &[
    "model",
    "messages",
    "stream",
    "stream_options",
    "reasoning_effort",
    "verbosity",
    "service_tier",
    "prompt_cache_key",
    "parallel_tool_calls",
    "response_format",
    "temperature",
    "top_p",
    "user",
];

#[derive(Debug, Clone)]
pub struct TranslatedRequest {
    pub upstream: Value,
    pub model: String,
    pub effort: Option<String>,
    pub stream: bool,
    pub include_usage: bool,
    pub use_responses_lite: bool,
}

pub fn translate_request(mut body: Value) -> Result<TranslatedRequest, ChatError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| ChatError::invalid("Request body must be a JSON object", None, None))?;
    reject_unsupported_fields(object)?;

    let requested_model = required_string(object, "model")?;
    let resolved = resolve_model_request(requested_model);
    assert_allowed_model(&resolved.model).map_err(|error| {
        ChatError::invalid(error.to_string(), Some("model"), Some("invalid_value"))
    })?;
    let use_responses_lite = uses_responses_lite(&resolved.model);

    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ChatError::invalid("Missing or invalid 'messages'", Some("messages"), None)
        })?;
    if messages.is_empty() {
        return Err(ChatError::invalid(
            "'messages' must contain at least one message",
            Some("messages"),
            None,
        ));
    }
    for (index, message) in messages.iter().enumerate() {
        validate_message(index, message)?;
    }

    let stream = optional_bool(object, "stream")?.unwrap_or(false);
    let include_usage = translate_stream_options(object.get("stream_options"))?;
    if include_usage && !stream {
        return Err(ChatError::invalid(
            "'stream_options' is only supported when 'stream' is true",
            Some("stream_options"),
            None,
        ));
    }

    let request_effort = match object.get("reasoning_effort") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => to_codex_effort(Some(value)),
        Some(_) => {
            return Err(ChatError::invalid(
                "'reasoning_effort' must be a string",
                Some("reasoning_effort"),
                None,
            ));
        }
    };
    let effort = resolve_effort_override(request_effort, config::codex_effort().as_deref())
        .map_err(|error| {
            ChatError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                error.to_string(),
                None,
                None,
            )
        })?;

    let effort_string = effort.as_ref().map(ToString::to_string);
    let effort = resolve_model_effort(&resolved.model, effort_string.as_deref())
        .map_err(|error| {
            ChatError::invalid(
                error.to_string(),
                Some("reasoning_effort"),
                Some("unsupported_value"),
            )
        })?
        .map(Effort::from)
        .or_else(|| {
            crate::providers::codex::translate::model_catalog::catalog_model(&resolved.model)
                .is_none_or(|model| model.supported_reasoning_levels.is_none())
                .then_some(Effort::Medium)
        });
    validate_input_modalities(&resolved.model, &object["messages"]).map_err(|error| {
        ChatError::invalid(
            error.to_string(),
            Some("messages"),
            Some("unsupported_value"),
        )
    })?;
    let requested_verbosity = optional_string(object, "verbosity")?;
    let verbosity = model_verbosity(&resolved.model, requested_verbosity).map_err(|error| {
        ChatError::invalid(
            error.to_string(),
            Some("verbosity"),
            Some("unsupported_value"),
        )
    })?;
    let parallel = model_parallel_tool_calls(
        &resolved.model,
        use_responses_lite,
        optional_bool(object, "parallel_tool_calls")?,
    )
    .map_err(|error| {
        ChatError::invalid(
            error.to_string(),
            Some("parallel_tool_calls"),
            Some("unsupported_value"),
        )
    })?;
    let mut text = Map::new();
    if let Some(verbosity) = verbosity {
        text.insert("verbosity".to_string(), json!(verbosity));
    }
    if let Some(format) = translate_response_format(object.get_mut("response_format"))? {
        text.insert("format".to_string(), format);
    }

    let mut upstream = Map::from_iter([
        ("model".to_string(), json!(&resolved.model)),
        ("input".to_string(), Value::Null),
        ("store".to_string(), json!(false)),
        ("stream".to_string(), json!(true)),
        ("parallel_tool_calls".to_string(), json!(parallel)),
        ("text".to_string(), Value::Object(text)),
    ]);
    if let Some(key) = optional_string(object, "prompt_cache_key")? {
        upstream.insert("prompt_cache_key".to_string(), json!(key));
    }
    let tier = optional_string(object, "service_tier")?
        .map(str::to_string)
        .or_else(|| resolved.service_tier.map(String::from));
    if let Some(tier) = tier {
        let tier = resolve_service_tier_value(&resolved.model, &tier).map_err(|error| {
            ChatError::invalid(
                error.to_string(),
                Some("service_tier"),
                Some("unsupported_value"),
            )
        })?;
        if !matches!(&tier, ServiceTier::Other(value) if value == "default") {
            upstream.insert("service_tier".to_string(), json!(tier));
        }
    }
    let mut reasoning = Map::new();
    if use_responses_lite {
        upstream.insert("client_metadata".to_string(), json!({"lite":"true"}));
        reasoning.insert("context".to_string(), json!("all_turns"));
    }
    if let Some(effort) = effort.as_ref() {
        reasoning.insert("effort".to_string(), json!(effort));
        if reasoning_requested(Some(effort)) {
            let summary = model_reasoning_summary(
                &resolved.model,
                config::codex_reasoning_summary().as_deref(),
                None,
            )
            .map_err(|error| {
                ChatError::invalid(
                    error.to_string(),
                    Some("reasoning_effort"),
                    Some("unsupported_value"),
                )
            })?;
            if let Some(summary) = summary {
                reasoning.insert("summary".to_string(), json!(summary));
            }
        }
    }
    upstream.insert("reasoning".to_string(), Value::Object(reasoning));

    for param in ["temperature", "top_p"] {
        if let Some(value) = object.get(param).filter(|value| !value.is_null()) {
            if use_responses_lite {
                return Err(ChatError::unsupported(param));
            }
            validate_sampling_value(param, value)?;
            upstream.insert(param.to_string(), value.clone());
        }
    }
    if let Some(user) = object.get("user").filter(|value| !value.is_null()) {
        let user = user
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= 64)
            .ok_or_else(|| {
                ChatError::invalid(
                    "'user' must be a non-empty string of at most 64 bytes",
                    Some("user"),
                    None,
                )
            })?;
        upstream.insert("safety_identifier".to_string(), json!(user));
    }

    let messages = object.remove("messages").expect("messages validated");
    let Value::Array(messages) = messages else {
        unreachable!("messages validated as an array");
    };
    upstream.insert(
        "input".to_string(),
        Value::Array(messages.into_iter().map(translate_message).collect()),
    );

    Ok(TranslatedRequest {
        upstream: Value::Object(upstream),
        model: resolved.model,
        effort: effort.map(|effort| effort.to_string()),
        stream,
        include_usage,
        use_responses_lite,
    })
}

fn reject_unsupported_fields(object: &Map<String, Value>) -> Result<(), ChatError> {
    for key in object.keys() {
        if !SUPPORTED_FIELDS.contains(&key.as_str()) {
            return Err(ChatError::unsupported(key));
        }
    }
    Ok(())
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &'static str,
) -> Result<&'a str, ChatError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ChatError::invalid(format!("Missing or invalid '{key}'"), Some(key), None))
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    key: &'static str,
) -> Result<Option<&'a str>, ChatError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(ChatError::invalid(
            format!("'{key}' must be a string"),
            Some(key),
            None,
        )),
    }
}

fn optional_bool(
    object: &Map<String, Value>,
    key: &'static str,
) -> Result<Option<bool>, ChatError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ChatError::invalid(
            format!("'{key}' must be a boolean"),
            Some(key),
            None,
        )),
    }
}

fn validate_message(index: usize, message: &Value) -> Result<(), ChatError> {
    let param = format!("messages[{index}]");
    let object = message
        .as_object()
        .ok_or_else(|| ChatError::invalid("Each message must be an object", Some(&param), None))?;
    for key in object.keys() {
        if !matches!(key.as_str(), "role" | "content" | "name") {
            return Err(ChatError::unsupported(format!("{param}.{key}")));
        }
    }
    let role = object.get("role").and_then(Value::as_str).ok_or_else(|| {
        ChatError::invalid(
            "Each message requires a role",
            Some(&format!("{param}.role")),
            None,
        )
    })?;
    match role {
        "system" | "developer" | "user" | "assistant" => {}
        _ => {
            return Err(ChatError::invalid(
                format!("Unsupported message role: {role}"),
                Some(&format!("{param}.role")),
                Some("unsupported_value"),
            ));
        }
    };
    validate_content(object.get("content"), index)
}

fn validate_content(content: Option<&Value>, index: usize) -> Result<(), ChatError> {
    let param = format!("messages[{index}].content");
    match content {
        Some(Value::String(text)) if !text.is_empty() => Ok(()),
        Some(Value::Array(parts)) if !parts.is_empty() => {
            parts.iter().enumerate().try_for_each(|(part_index, part)| {
                let part_param = format!("{param}[{part_index}]");
                let object = part.as_object().ok_or_else(|| {
                    ChatError::invalid(
                        "Message content parts must be objects",
                        Some(&part_param),
                        None,
                    )
                })?;
                if object.get("type").and_then(Value::as_str) == Some("image_url") {
                    let image = object
                        .get("image_url")
                        .and_then(Value::as_object)
                        .ok_or_else(|| {
                            ChatError::invalid(
                                "image_url must be an object",
                                Some(&part_param),
                                None,
                            )
                        })?;
                    image
                        .get("url")
                        .and_then(Value::as_str)
                        .filter(|url| !url.is_empty())
                        .ok_or_else(|| {
                            ChatError::invalid("image_url.url is required", Some(&part_param), None)
                        })?;
                    return Ok(());
                }
                if object.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(ChatError::invalid(
                        "Only text message content is supported",
                        Some(&format!("{part_param}.type")),
                        Some("unsupported_value"),
                    ));
                }
                object
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| {
                        ChatError::invalid(
                            "Text content must not be empty",
                            Some(&format!("{part_param}.text")),
                            None,
                        )
                    })?;
                Ok(())
            })
        }
        _ => Err(ChatError::invalid(
            "Message content must contain text",
            Some(&param),
            None,
        )),
    }
}

fn translate_message(mut message: Value) -> Value {
    let role = match message["role"].as_str().expect("message role validated") {
        "system" | "developer" => "developer",
        "user" => "user",
        "assistant" => "assistant",
        _ => unreachable!("message role validated"),
    };
    let parts = match message["content"].take() {
        Value::String(text) => vec![Value::Object(Map::from_iter([
            ("type".to_string(), json!("input_text")),
            ("text".to_string(), Value::String(text)),
        ]))],
        Value::Array(parts) => parts
            .into_iter()
            .map(|mut part| {
                if part["type"] == "image_url" {
                    let image = part["image_url"].as_object_mut().expect("image validated");
                    let mut translated = json!({"type":"input_image"});
                    translated["image_url"] = image.remove("url").expect("image URL validated");
                    if let Some(detail) = image.remove("detail") {
                        translated["detail"] = detail;
                    }
                    translated
                } else {
                    let mut translated = json!({"type":"input_text"});
                    translated["text"] = part["text"].take();
                    translated
                }
            })
            .collect(),
        _ => unreachable!("message content validated"),
    };
    let mut translated = json!({"type":"message", "role":role});
    translated["content"] = Value::Array(parts);
    translated
}

fn translate_response_format(value: Option<&mut Value>) -> Result<Option<Value>, ChatError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = value.as_object_mut().ok_or_else(|| {
        ChatError::invalid(
            "'response_format' must be an object",
            Some("response_format"),
            None,
        )
    })?;
    match object.get("type").and_then(Value::as_str) {
        Some("text") => Ok(None),
        Some("json_object") => Ok(Some(json!({"type":"json_object"}))),
        Some("json_schema") => {
            let format = object
                .get_mut("json_schema")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| {
                    ChatError::invalid(
                        "'response_format.json_schema' must be an object",
                        Some("response_format.json_schema"),
                        None,
                    )
                })?;
            format
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    ChatError::invalid(
                        "JSON Schema output requires a name",
                        Some("response_format.json_schema.name"),
                        None,
                    )
                })?;
            format
                .get("schema")
                .filter(|schema| schema.is_object())
                .ok_or_else(|| {
                    ChatError::invalid(
                        "JSON Schema output requires an object schema",
                        Some("response_format.json_schema.schema"),
                        None,
                    )
                })?;
            let strict = match format.get("strict") {
                None | Some(Value::Null) => None,
                Some(Value::Bool(value)) => Some(*value),
                Some(_) => {
                    return Err(ChatError::invalid(
                        "JSON Schema strict must be a boolean",
                        Some("response_format.json_schema.strict"),
                        None,
                    ));
                }
            };
            let mut translated = Map::from_iter([
                ("type".to_string(), json!("json_schema")),
                (
                    "name".to_string(),
                    format.remove("name").expect("schema name validated"),
                ),
                (
                    "schema".to_string(),
                    format.remove("schema").expect("schema validated"),
                ),
            ]);
            if let Some(strict) = strict {
                translated.insert("strict".to_string(), json!(strict));
            }
            Ok(Some(Value::Object(translated)))
        }
        Some(kind) => Err(ChatError::invalid(
            format!("Unsupported response format: {kind}"),
            Some("response_format.type"),
            Some("unsupported_value"),
        )),
        None => Err(ChatError::invalid(
            "'response_format.type' is required",
            Some("response_format.type"),
            None,
        )),
    }
}

fn translate_stream_options(value: Option<&Value>) -> Result<bool, ChatError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(false);
    };
    let object = value.as_object().ok_or_else(|| {
        ChatError::invalid(
            "'stream_options' must be an object",
            Some("stream_options"),
            None,
        )
    })?;
    for key in object.keys() {
        if key != "include_usage" {
            return Err(ChatError::unsupported(format!("stream_options.{key}")));
        }
    }
    match object.get("include_usage") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ChatError::invalid(
            "'stream_options.include_usage' must be a boolean",
            Some("stream_options.include_usage"),
            None,
        )),
    }
}

fn validate_sampling_value(param: &'static str, value: &Value) -> Result<(), ChatError> {
    let number = value
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| {
            ChatError::invalid(format!("'{param}' must be a number"), Some(param), None)
        })?;
    let valid = match param {
        "temperature" => (0.0..=2.0).contains(&number),
        _ => (0.0..=1.0).contains(&number),
    };
    if valid {
        Ok(())
    } else {
        Err(ChatError::invalid(
            format!("'{param}' is out of range"),
            Some(param),
            Some("invalid_value"),
        ))
    }
}
