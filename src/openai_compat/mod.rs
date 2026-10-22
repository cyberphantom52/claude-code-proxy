pub mod request;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

pub const MAX_OPENAI_REQUEST_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct OpenAiError {
    pub message: String,
    pub param: Option<String>,
}

impl OpenAiError {
    pub fn invalid(message: impl Into<String>, param: Option<impl Into<String>>) -> Self {
        Self {
            message: message.into(),
            param: param.map(Into::into),
        }
    }

    pub fn response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": self.message,
                    "type": "invalid_request_error",
                    "param": self.param,
                    "code": null,
                }
            })),
        )
            .into_response()
    }
}
