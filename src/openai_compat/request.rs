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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_model_and_normalizes_context_hint() {
        assert_eq!(
            extract_model(&json!({"model":"gpt-5.5[1m]"})).unwrap(),
            ("gpt-5.5[1m]".to_string(), "gpt-5.5".to_string()),
        );
    }

    #[test]
    fn rejects_invalid_model_or_request_shape() {
        for body in [
            json!(null),
            json!([]),
            json!({}),
            json!({"model":7}),
            json!({"model":""}),
        ] {
            assert!(extract_model(&body).is_err(), "{body}");
        }
    }
}
