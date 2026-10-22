use serde_json::Value;

use crate::registry::normalize_incoming_model;

use super::OpenAiError;

pub fn extract_model(body: &Value) -> Result<(String, String), OpenAiError> {
    let object = body.as_object().ok_or_else(|| {
        OpenAiError::invalid("Request body must be a JSON object", None::<String>)
    })?;
    let requested = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| OpenAiError::invalid("Missing or invalid 'model'", Some("model")))?
        .to_string();
    Ok((requested.clone(), normalize_incoming_model(&requested)))
}
