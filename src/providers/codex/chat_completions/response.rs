use http::StatusCode;
use serde_json::{Value, json};

use crate::providers::codex::events::{classify_event_failure, response_is_incomplete_terminal};
use crate::providers::codex::translate::reducer::{CompletedOutputs, TextParts};

use super::ChatError;

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    details: serde_json::Map<String, Value>,
}

impl Usage {
    pub fn value(&self) -> Value {
        self.clone().into_value()
    }

    fn into_value(self) -> Value {
        let mut value = Value::Object(self.details);
        value["prompt_tokens"] = json!(self.prompt_tokens);
        value["completion_tokens"] = json!(self.completion_tokens);
        if value.get("total_tokens").is_none() {
            value["total_tokens"] =
                json!(self.prompt_tokens.saturating_add(self.completion_tokens));
        }
        value
    }
}

#[derive(Debug, Clone)]
pub struct CompletionState {
    pub id: String,
    pub created: u64,
    pub model: String,
    text: Option<String>,
    pub output_bytes: usize,
    pub usage: Usage,
    pub completed: bool,
    pub finish_reason: &'static str,
    pub service_tier: Option<String>,
    identity_frozen: bool,
    completed_outputs: CompletedOutputs,
    text_parts: TextParts,
    next_text_part: usize,
}

impl CompletionState {
    pub fn new(model: &str, retain_text: bool) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            created: unix_seconds(),
            model: model.to_string(),
            text: retain_text.then(String::new),
            output_bytes: 0,
            usage: Usage::default(),
            completed: false,
            finish_reason: "stop",
            service_tier: None,
            identity_frozen: false,
            completed_outputs: CompletedOutputs::default(),
            text_parts: TextParts::linear(),
            next_text_part: 0,
        }
    }

    pub fn freeze_identity(&mut self) {
        self.identity_frozen = true;
    }

    pub fn observe(&mut self, event: &Value) -> Result<Option<String>, ChatError> {
        let kind = event.get("type").and_then(Value::as_str);
        if let Some(failure) = classify_event_failure(event) {
            let status =
                StatusCode::from_u16(failure.client_status()).unwrap_or(StatusCode::BAD_GATEWAY);
            return Err(ChatError::new(
                status,
                failure.client_error_type(),
                failure.message,
                None,
                None,
            ));
        }
        let mut suffix = String::new();
        for reconciled in self
            .completed_outputs
            .accept(std::borrow::Cow::Borrowed(event))
            .map_err(ChatError::upstream)?
        {
            let output_index = reconciled["output_index"].as_u64().unwrap_or(0) as usize;
            let item_closed = !matches!(
                reconciled["type"].as_str(),
                Some(
                    "response.output_item.done"
                        | "response.completed"
                        | "response.done"
                        | "response.incomplete"
                )
            ) && self.completed_outputs.is_done(output_index);
            for delta in self
                .text_parts
                .accept(&reconciled, &mut self.next_text_part, output_index)
                .map_err(ChatError::upstream)?
            {
                if delta.refusal {
                    continue;
                }
                if (self.completed || item_closed) && !delta.text.is_empty() {
                    return Err(ChatError::upstream(
                        "Codex output text changed after completion",
                    ));
                }
                let output_bytes = self.output_bytes.saturating_add(delta.text.len());
                if output_bytes > crate::traffic::MAX_SSE_CAPTURE_BYTES {
                    return Err(ChatError::upstream(
                        "Codex output text exceeded the response byte limit",
                    ));
                }
                self.output_bytes = output_bytes;
                suffix.push_str(&delta.text);
            }
            if reconciled["type"] == "response.output_item.done"
                && reconciled["item"]["status"] != "incomplete"
            {
                self.text_parts.close_item(Some(output_index));
            }
        }
        match kind {
            Some("response.created" | "response.in_progress") => {
                if let Some(response) = event.get("response") {
                    self.update_metadata(response);
                }
            }
            Some("response.completed" | "response.done" | "response.incomplete") => {
                let response = event.get("response").ok_or_else(|| {
                    ChatError::upstream("Codex completion event did not contain a response")
                })?;
                self.update_metadata(response);
                self.finish_reason = if response_is_incomplete_terminal(event) {
                    "length"
                } else {
                    "stop"
                };
                self.text_parts.close_item(None);
                self.completed = true;
            }
            _ => {
                if event.get("error").is_some_and(|error| !error.is_null()) {
                    return Err(event_error(event));
                }
            }
        }
        if let Some(text) = self.text.as_mut() {
            text.push_str(&suffix);
        }
        Ok((!suffix.is_empty()).then_some(suffix))
    }

    fn update_metadata(&mut self, response: &Value) {
        if !self.identity_frozen {
            if let Some(id) = response.get("id").and_then(Value::as_str) {
                self.id = chat_completion_id(id);
            }
            if let Some(model) = response.get("model").and_then(Value::as_str) {
                self.model = model.to_string();
            }
            if let Some(created) = response
                .get("created_at")
                .or_else(|| response.get("created"))
                .and_then(Value::as_f64)
                .filter(|created| *created >= 0.0)
            {
                self.created = created as u64;
            }
        }
        if let Some(tier) = response.get("service_tier").and_then(Value::as_str) {
            self.service_tier = Some(tier.to_string());
        }
        if let Some(usage) = response.get("usage") {
            self.usage.prompt_tokens = usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(self.usage.prompt_tokens);
            self.usage.completion_tokens = usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(self.usage.completion_tokens);
            if let Some(fields) = usage.as_object() {
                for (name, value) in fields {
                    let name = match name.as_str() {
                        "input_tokens" | "output_tokens" => continue,
                        "input_tokens_details" => "prompt_tokens_details",
                        "output_tokens_details" => "completion_tokens_details",
                        name => name,
                    };
                    self.usage.details.insert(name.to_string(), value.clone());
                }
            }
        }
    }
}

pub fn finish_completion(state: CompletionState) -> Result<Value, ChatError> {
    if !state.completed {
        return Err(ChatError::upstream(
            "Codex event stream ended before completion",
        ));
    }
    if state.output_bytes == 0 && state.finish_reason != "length" {
        return Err(ChatError::upstream("Codex completed without output text"));
    }
    Ok(completion_value(state))
}

pub fn completion_value(state: CompletionState) -> Value {
    let mut value = json!({
        "object": "chat.completion",
        "created": state.created,
        "choices": [{
            "index": 0,
            "message": {"role":"assistant"},
            "finish_reason": state.finish_reason,
        }],
    });
    value["id"] = Value::String(state.id);
    value["model"] = Value::String(state.model);
    value["choices"][0]["message"]["content"] =
        Value::String(state.text.expect("buffered completion retains text"));
    value["usage"] = state.usage.into_value();
    if let Some(tier) = state.service_tier {
        value["service_tier"] = Value::String(tier);
    }
    value
}

pub fn event_error(event: &Value) -> ChatError {
    let message = event
        .pointer("/response/error/message")
        .or_else(|| event.pointer("/error/message"))
        .or_else(|| event.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Codex response generation failed");
    ChatError::upstream(message)
}

pub fn chat_completion_id(response_id: &str) -> String {
    if response_id.starts_with("chatcmpl-") {
        response_id.to_string()
    } else if let Some(suffix) = response_id.strip_prefix("resp_") {
        format!("chatcmpl-{suffix}")
    } else {
        format!("chatcmpl-{response_id}")
    }
}

fn unix_seconds() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
