use std::borrow::Cow;

use serde_json::Value;

use crate::anthropic::schema::MessagesRequest;

#[derive(Debug)]
pub enum ContentBlock<'a> {
    Text {
        text: &'a str,
    },
    Image {
        source: ImageSource<'a>,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a Value,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: Cow<'a, Value>,
        is_error: Option<bool>,
    },
    Thinking {
        signature: Option<&'a str>,
    },
}

#[derive(Debug)]
pub struct ImageSource<'a> {
    pub media_type: &'a str,
    pub data: &'a str,
    pub source_type: &'a str,
}

pub fn flatten_system_text(system_val: Option<&Value>) -> Option<String> {
    let system = system_val?;
    let texts: Vec<&str> = match system {
        Value::String(s) => return Some(s.clone()),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| {
                let text = b.get("text").and_then(|v| v.as_str())?;
                if text.starts_with("x-anthropic-billing-header:") {
                    None
                } else {
                    Some(text)
                }
            })
            .collect(),
        _ => return None,
    };
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n\n"))
    }
}

pub fn parallel_tool_calls(req: &MessagesRequest) -> Option<bool> {
    req.extra
        .get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|choice| choice.get("disable_parallel_tool_use"))
        .and_then(Value::as_bool)
        .map(|disabled| !disabled)
}

pub fn normalize_content(content: &Value) -> anyhow::Result<Vec<ContentBlock<'_>>> {
    match content {
        Value::String(s) => Ok(vec![ContentBlock::Text { text: s }]),
        Value::Array(arr) => arr
            .iter()
            .enumerate()
            .map(|(index, item)| {
                parse_content_block(item)
                    .map_err(|error| anyhow::anyhow!("content[{index}]: {error}"))
            })
            .collect(),
        _ => anyhow::bail!("content must be a string or an array of content blocks"),
    }
}

pub fn image_source_to_url(source: &ImageSource<'_>) -> String {
    if source.source_type == "url" {
        source.data.to_string()
    } else {
        format!("data:{};base64,{}", source.media_type, source.data)
    }
}

fn string_field<'a>(value: &'a Value, field: &str) -> anyhow::Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{field} must be a string"))
}

fn parse_content_block(value: &Value) -> anyhow::Result<ContentBlock<'_>> {
    let kind = string_field(value, "type")?;
    let parsed = || -> anyhow::Result<ContentBlock<'_>> {
        Ok(match kind {
            "text" => ContentBlock::Text {
                text: string_field(value, "text")?,
            },
            "image" => {
                let source = value
                    .get("source")
                    .filter(|source| source.is_object())
                    .ok_or_else(|| anyhow::anyhow!("source must be an object"))?;
                let source_type = string_field(source, "type")?;
                let (media_type, data) = match source_type {
                    "url" => ("", string_field(source, "url")?),
                    "base64" => (
                        string_field(source, "media_type")?,
                        string_field(source, "data")?,
                    ),
                    _ => anyhow::bail!("unsupported image source type '{source_type}'"),
                };
                anyhow::ensure!(!data.is_empty(), "image source data/url must not be empty");
                anyhow::ensure!(
                    source_type != "base64" || !media_type.is_empty(),
                    "image source media_type must not be empty"
                );
                ContentBlock::Image {
                    source: ImageSource {
                        media_type,
                        data,
                        source_type,
                    },
                }
            }
            "tool_use" => {
                let id = string_field(value, "id")?;
                let name = string_field(value, "name")?;
                anyhow::ensure!(
                    !id.is_empty() && !name.is_empty(),
                    "id and name must not be empty"
                );
                ContentBlock::ToolUse {
                    id,
                    name,
                    input: value.get("input").unwrap_or(&Value::Null),
                }
            }
            "tool_result" => {
                let tool_use_id = string_field(value, "tool_use_id")?;
                anyhow::ensure!(!tool_use_id.is_empty(), "tool_use_id must not be empty");
                let content = value
                    .get("content")
                    .map(Cow::Borrowed)
                    .unwrap_or_else(|| Cow::Owned(Value::String(String::new())));
                // Nested blocks keep the renderer's explicit placeholders and client
                // extensions (tool_reference); only an invalid container was silent loss.
                anyhow::ensure!(
                    content.is_string() || content.is_array(),
                    "tool_result.content must be a string or an array of content blocks"
                );
                let is_error = match value.get("is_error") {
                    None | Some(Value::Null) => None,
                    Some(Value::Bool(is_error)) => Some(*is_error),
                    Some(_) => anyhow::bail!("is_error must be a boolean"),
                };
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                }
            }
            "thinking" => {
                let signature = match value.get("signature") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(signature)) => Some(signature.as_str()),
                    Some(_) => anyhow::bail!("signature must be a string"),
                };
                string_field(value, "thinking")?;
                ContentBlock::Thinking { signature }
            }
            _ => anyhow::bail!("unsupported content block type '{kind}'"),
        })
    };
    parsed().map_err(|error| anyhow::anyhow!("{kind}: {error}"))
}
