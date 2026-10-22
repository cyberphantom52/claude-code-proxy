use super::translate::request::{
    ResponsesContentPart, ResponsesFunctionCallOutput, ResponsesFunctionCallOutputContentPart,
    ResponsesInputItem, ResponsesRequest, ResponsesTool,
};

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CodexRequestSizeSummary {
    pub body_json_bytes: u64,
    pub instructions_bytes: u64,
    pub input_json_bytes: u64,
    pub tools_json_bytes: u64,
    pub text_json_bytes: u64,
    pub reasoning_json_bytes: u64,
    pub include_json_bytes: u64,
    pub client_metadata_json_bytes: u64,
    pub input_item_count: usize,
    pub tool_count: usize,
    pub input_image_part_count: usize,
    pub input_image_data_url_bytes: u64,
    pub input_type_counts: std::collections::BTreeMap<String, usize>,
    pub role_counts: std::collections::BTreeMap<String, usize>,
    pub largest_input_items: Vec<InputItemSummary>,
    pub largest_input_images: Vec<InputImageSummary>,
    pub largest_tools: Vec<ToolSummary>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct InputItemSummary {
    pub index: usize,
    pub r#type: String,
    pub role: Option<String>,
    pub json_bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct InputImageSummary {
    pub item_index: usize,
    pub part_index: usize,
    pub json_bytes: u64,
    pub image_url_bytes: u64,
    pub data_url: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolSummary {
    pub index: usize,
    pub name: String,
    pub json_bytes: u64,
}

pub(crate) fn serialized_json_bytes(value: &(impl serde::Serialize + ?Sized)) -> u64 {
    struct Counter(u64);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len() as u64);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    if serde_json::to_writer(&mut counter, value).is_err() {
        return u64::MAX;
    }
    counter.0
}

fn byte_length(s: &str) -> u64 {
    s.len() as u64
}

fn input_image_parts(input: &[ResponsesInputItem]) -> Vec<(usize, usize, &str)> {
    let mut parts = Vec::new();
    for (item_idx, item) in input.iter().enumerate() {
        match item {
            ResponsesInputItem::Message { content, .. } => {
                for (part_idx, part) in content.iter().enumerate() {
                    if let ResponsesContentPart::InputImage { image_url, .. } = part {
                        parts.push((item_idx, part_idx, image_url.as_str()));
                    }
                }
            }
            ResponsesInputItem::FunctionCallOutput {
                output: ResponsesFunctionCallOutput::ContentItems(content),
                ..
            } => {
                for (part_idx, part) in content.iter().enumerate() {
                    if let ResponsesFunctionCallOutputContentPart::InputImage {
                        image_url, ..
                    } = part
                    {
                        parts.push((item_idx, part_idx, image_url.as_str()));
                    }
                }
            }
            _ => {}
        }
    }
    parts
}

pub fn summarize_codex_request_size(body: &ResponsesRequest) -> CodexRequestSizeSummary {
    let image_parts = input_image_parts(&body.input);

    let input_type_counts = count_items_by(&body.input, |item| match item {
        ResponsesInputItem::AdditionalTools { .. } => Some("additional_tools".to_string()),
        ResponsesInputItem::Message { .. } => Some("message".to_string()),
        ResponsesInputItem::FunctionCall { .. } => Some("function_call".to_string()),
        ResponsesInputItem::FunctionCallOutput { .. } => Some("function_call_output".to_string()),
        ResponsesInputItem::Reasoning { .. } => Some("reasoning".to_string()),
        ResponsesInputItem::Compaction { .. } => Some("compaction".to_string()),
        ResponsesInputItem::CompactionTrigger => Some("compaction_trigger".to_string()),
    });

    let role_counts = count_items_by(&body.input, |item| match item {
        ResponsesInputItem::AdditionalTools { role, .. } => Some(role.clone()),
        ResponsesInputItem::Message { role, .. } => Some(role.clone()),
        _ => None,
    });

    let largest_input_items = {
        let mut items: Vec<InputItemSummary> = body
            .input
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let (r#type, role) = match item {
                    ResponsesInputItem::AdditionalTools { role, .. } => {
                        ("additional_tools".to_string(), Some(role.clone()))
                    }
                    ResponsesInputItem::Message { role, .. } => {
                        ("message".to_string(), Some(role.clone()))
                    }
                    ResponsesInputItem::FunctionCall { .. } => ("function_call".to_string(), None),
                    ResponsesInputItem::FunctionCallOutput { .. } => {
                        ("function_call_output".to_string(), None)
                    }
                    ResponsesInputItem::Reasoning { .. } => ("reasoning".to_string(), None),
                    ResponsesInputItem::Compaction { .. } => ("compaction".to_string(), None),
                    ResponsesInputItem::CompactionTrigger => {
                        ("compaction_trigger".to_string(), None)
                    }
                };
                let json_bytes_val = serialized_json_bytes(item);
                InputItemSummary {
                    index: i,
                    r#type,
                    role,
                    json_bytes: json_bytes_val,
                }
            })
            .collect();
        items.sort_by_key(|item| std::cmp::Reverse(item.json_bytes));
        items.truncate(5);
        items
    };

    let largest_input_images = {
        let mut items: Vec<InputImageSummary> = image_parts
            .iter()
            .map(|&(item_idx, part_idx, url)| {
                let json_bytes_val = serialized_json_bytes(url)
                    + b"{\"type\":\"input_image\",\"image_url\":}".len() as u64;
                InputImageSummary {
                    item_index: item_idx,
                    part_index: part_idx,
                    json_bytes: json_bytes_val,
                    image_url_bytes: byte_length(url),
                    data_url: url.starts_with("data:"),
                }
            })
            .collect();
        items.sort_by_key(|item| std::cmp::Reverse(item.image_url_bytes));
        items.truncate(5);
        items
    };

    let largest_tools = {
        let mut items: Vec<ToolSummary> = Vec::new();
        if let Some(ref tools) = body.tools {
            for (i, tool) in tools.iter().enumerate() {
                let name = match tool {
                    ResponsesTool::Function(f) => f.name.chars().take(128).collect(),
                    ResponsesTool::WebSearch(_) => "web_search".to_string(),
                };
                let json_bytes_val = serialized_json_bytes(tool);
                items.push(ToolSummary {
                    index: i,
                    name,
                    json_bytes: json_bytes_val,
                });
            }
        }
        items.sort_by_key(|item| std::cmp::Reverse(item.json_bytes));
        items.truncate(5);
        items
    };

    CodexRequestSizeSummary {
        body_json_bytes: serialized_json_bytes(body),
        instructions_bytes: body.instructions.as_ref().map_or(0, |s| byte_length(s)),
        input_json_bytes: serialized_json_bytes(&body.input),
        tools_json_bytes: body.tools.as_ref().map_or(0, serialized_json_bytes),
        text_json_bytes: serialized_json_bytes(&body.text),
        reasoning_json_bytes: body.reasoning.as_ref().map_or(0, serialized_json_bytes),
        include_json_bytes: body.include.as_ref().map_or(0, serialized_json_bytes),
        client_metadata_json_bytes: body
            .client_metadata
            .as_ref()
            .map_or(0, serialized_json_bytes),
        input_item_count: body.input.len(),
        tool_count: body.tools.as_ref().map_or(0, |t| t.len()),
        input_image_part_count: image_parts.len(),
        input_image_data_url_bytes: image_parts
            .iter()
            .filter(|(_, _, url)| url.starts_with("data:"))
            .map(|(_, _, url)| byte_length(url))
            .sum(),
        input_type_counts,
        role_counts,
        largest_input_items,
        largest_input_images,
        largest_tools,
    }
}

fn count_items_by<T, F>(items: &[T], f: F) -> std::collections::BTreeMap<String, usize>
where
    F: Fn(&T) -> Option<String>,
{
    let mut counts = std::collections::BTreeMap::new();
    for item in items {
        if let Some(key) = f(item) {
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarize_simple_request() {
        let input = vec![ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![ResponsesContentPart::InputText {
                text: "hello".to_string(),
            }],
        }];
        let req = ResponsesRequest {
            model: "gpt-5.5".to_string(),
            instructions: None,
            input,
            tools: None,
            tool_choice: None,
            store: false,
            stream: true,
            parallel_tool_calls: true,
            include: None,
            client_metadata: None,
            service_tier: None,
            prompt_cache_key: None,
            text: super::super::translate::request::ResponsesText {
                verbosity: Some("low".to_string()),
                format: None,
            },
            reasoning: None,
        };
        let summary = summarize_codex_request_size(&req);
        assert_eq!(
            summary.body_json_bytes,
            serde_json::to_vec(&req).unwrap().len() as u64
        );
        assert_eq!(
            summary.input_json_bytes,
            serde_json::to_vec(&req.input).unwrap().len() as u64
        );
        assert_eq!(summary.input_item_count, 1);
        assert_eq!(summary.tool_count, 0);
        assert!(summary.body_json_bytes > 0);
    }

    #[test]
    fn summarize_with_tools_and_images() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "describe"},
                        {"type": "input_image", "image_url": "data:image/png;base64,abc"}
                    ]
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": [
                        {"type": "input_text", "text": "tool image"},
                        {"type": "input_image", "image_url": "data:image/jpeg;base64,def"}
                    ]
                }
            ],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"}
        }))
        .unwrap();
        let summary = summarize_codex_request_size(&req);
        assert_eq!(summary.input_image_part_count, 2);
        assert!(summary.input_image_data_url_bytes > 0);
    }
}
