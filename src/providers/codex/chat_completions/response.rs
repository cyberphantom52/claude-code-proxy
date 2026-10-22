use http::StatusCode;
use serde_json::{Value, json};

use crate::anthropic::sse::parse_sse_events;
use crate::providers::codex::events::classify_event_failure;

use super::ChatError;

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    details: serde_json::Map<String, Value>,
}

impl Usage {
    pub fn value(&self) -> Value {
        let mut value = Value::Object(self.details.clone());
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
    pub text: String,
    pub usage: Usage,
    pub finish_reason: &'static str,
    pub completed: bool,
    pub service_tier: Option<String>,
    identity_frozen: bool,
}

impl CompletionState {
    pub fn new(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            created: unix_seconds(),
            model: model.to_string(),
            text: String::new(),
            usage: Usage::default(),
            finish_reason: "stop",
            completed: false,
            service_tier: None,
            identity_frozen: false,
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
        match kind {
            Some("response.created" | "response.in_progress") => {
                if let Some(response) = event.get("response") {
                    self.update_metadata(response);
                }
            }
            Some("response.output_text.delta") => {
                let delta = event.get("delta").and_then(Value::as_str).ok_or_else(|| {
                    ChatError::upstream("Codex output-text delta did not contain text")
                })?;
                if self.text.len().saturating_add(delta.len())
                    > crate::traffic::MAX_SSE_CAPTURE_BYTES
                {
                    return Err(ChatError::upstream(
                        "Codex output text exceeded the response byte limit",
                    ));
                }
                self.text.push_str(delta);
                return Ok(Some(delta.to_string()));
            }
            Some("response.completed" | "response.done") => {
                let response = event.get("response").ok_or_else(|| {
                    ChatError::upstream("Codex completion event did not contain a response")
                })?;
                self.update_metadata(response);
                if response.get("status").and_then(Value::as_str) == Some("failed") {
                    return Err(event_error(event));
                }
                self.completed = true;
            }
            _ => {
                if event.get("error").is_some_and(|error| !error.is_null()) {
                    return Err(event_error(event));
                }
            }
        }
        Ok(None)
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

pub fn aggregate_sse(body: &[u8], requested_model: &str) -> Result<Value, ChatError> {
    if body.len() > crate::traffic::MAX_SSE_CAPTURE_BYTES {
        return Err(ChatError::upstream(
            "Codex response exceeded the response byte limit",
        ));
    }
    let events = parse_sse_events(body);
    if events.is_empty() {
        return Err(ChatError::upstream(
            "Codex returned an empty or malformed event stream",
        ));
    }
    let mut state = CompletionState::new(requested_model);
    for event in events {
        if event.data == "[DONE]" {
            continue;
        }
        let value: Value = serde_json::from_str(&event.data).map_err(|_| {
            ChatError::upstream("Codex returned malformed JSON in its event stream")
        })?;
        state.observe(&value)?;
    }
    if !state.completed {
        return Err(ChatError::upstream(
            "Codex event stream ended before completion",
        ));
    }
    if state.text.is_empty() {
        return Err(ChatError::upstream("Codex completed without output text"));
    }
    Ok(completion_value(&state))
}

pub fn completion_value(state: &CompletionState) -> Value {
    let mut value = json!({
        "id": state.id,
        "object": "chat.completion",
        "created": state.created,
        "model": state.model,
        "choices": [{
            "index": 0,
            "message": {"role":"assistant", "content":state.text},
            "finish_reason": state.finish_reason,
        }],
        "usage": state.usage.value(),
    });
    if let Some(tier) = &state.service_tier {
        value["service_tier"] = json!(tier);
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
