use std::borrow::Cow;

use super::translate::request::{
    ResponsesContentPart, ResponsesFunctionCallOutput, ResponsesFunctionCallOutputContentPart,
    ResponsesInputItem, ResponsesRequest, ResponsesTool,
};
use tiktoken_rs::o200k_base_singleton;

/// Approximate token counter for Codex translated requests.
/// Text uses the OpenAI `o200k_base` tokenizer. Images, encrypted reasoning,
/// and protocol framing still use local estimates, so the result is not a
/// provider billing count.
pub fn count_translated_tokens(translated: &ResponsesRequest) -> u64 {
    let mut total = 0u64;
    let mut scratch = Vec::new();

    // Instructions
    if let Some(ref instructions) = translated.instructions {
        total += approx_token_count(instructions);
    }

    // Input items
    for item in &translated.input {
        total += count_input_item_tokens(item, &mut scratch);
    }

    // Tools
    if let Some(ref tools) = translated.tools {
        total += count_tool_tokens(tools, &mut scratch);
    }

    // Overhead
    total += translated.input.len() as u64 * 4;
    total += translated.tools.as_ref().map_or(0, |t| t.len() as u64 * 4);

    // Model name
    total += approx_token_count(&translated.model);

    total.max(1)
}

fn count_input_item_tokens(item: &ResponsesInputItem, scratch: &mut Vec<u8>) -> u64 {
    match item {
        ResponsesInputItem::AdditionalTools { tools, .. } => tools
            .iter()
            .map(|tool| count_serialized_tokens(tool, scratch))
            .sum(),
        ResponsesInputItem::Message { content, .. } => {
            let mut total = 0u64;
            for part in content {
                total += count_content_part_tokens(part);
            }
            total
        }
        ResponsesInputItem::FunctionCall {
            namespace,
            name,
            arguments,
            ..
        } => {
            namespace.as_deref().map_or(0, approx_token_count)
                + approx_token_count(name)
                + approx_token_count(arguments)
        }
        ResponsesInputItem::FunctionCallOutput { output, .. } => {
            count_function_call_output_tokens(output)
        }
        ResponsesInputItem::Reasoning {
            encrypted_content, ..
        }
        | ResponsesInputItem::Compaction { encrypted_content } => {
            approx_reasoning_token_count(encrypted_content)
        }
        ResponsesInputItem::WebSearchCall {
            encrypted_content,
            action,
            ..
        } => {
            let opaque = encrypted_content
                .as_deref()
                .map_or(0, approx_reasoning_token_count);
            let visible = action
                .as_ref()
                .map_or(0, |action| count_serialized_tokens(action, scratch));
            opaque.saturating_add(visible)
        }
        ResponsesInputItem::CompactionTrigger => 0,
    }
}

fn count_function_call_output_tokens(output: &ResponsesFunctionCallOutput) -> u64 {
    match output {
        ResponsesFunctionCallOutput::Text(text) => approx_token_count(text),
        ResponsesFunctionCallOutput::ContentItems(content) => content
            .iter()
            .map(|part| match part {
                ResponsesFunctionCallOutputContentPart::InputText { text } => {
                    approx_token_count(text)
                }
                ResponsesFunctionCallOutputContentPart::InputImage { .. } => 2000,
            })
            .sum(),
    }
}

fn approx_reasoning_token_count(encoded_content: &str) -> u64 {
    let model_visible_bytes = encoded_content
        .len()
        .saturating_mul(3)
        .checked_div(4)
        .unwrap_or(0)
        .saturating_sub(650);
    u64::try_from(model_visible_bytes.saturating_add(3) / 4).unwrap_or(u64::MAX)
}

fn count_content_part_tokens(part: &ResponsesContentPart) -> u64 {
    match part {
        ResponsesContentPart::InputText { text } => approx_token_count(text),
        ResponsesContentPart::OutputText { text, .. } => approx_token_count(text),
        ResponsesContentPart::InputImage { .. } => 2000, // Image token estimate
    }
}

fn count_tool_tokens(tools: &[ResponsesTool], scratch: &mut Vec<u8>) -> u64 {
    let mut total = 0u64;
    for tool in tools {
        match tool {
            ResponsesTool::Function(f) => {
                total += approx_token_count(&f.name);
                if let Some(ref desc) = f.description {
                    total += approx_token_count(desc);
                }
                total += count_serialized_tokens(&f.parameters, scratch);
            }
            ResponsesTool::WebSearch(_) => {
                total += 10; // fixed overhead for web search tool
            }
            ResponsesTool::Namespace { .. } => {
                total += count_serialized_tokens(tool, scratch);
            }
        }
    }
    total
}

fn count_serialized_tokens<T: serde::Serialize>(value: &T, scratch: &mut Vec<u8>) -> u64 {
    scratch.clear();
    if serde_json::to_writer(&mut *scratch, value).is_err() {
        return 0;
    }
    // Count the complete serialization, never its chunks: JSON write boundaries
    // are not token boundaries.
    approx_token_count(std::str::from_utf8(scratch).expect("serialized JSON is UTF-8"))
}

pub(crate) fn approx_token_count(text: &str) -> u64 {
    u64::try_from(o200k_base_singleton().count_ordinary(text)).unwrap_or(u64::MAX)
}

pub(crate) fn truncate_to_token_budget(text: Cow<'_, str>, max_tokens: u64) -> Cow<'_, str> {
    let max_tokens = usize::try_from(max_tokens).unwrap_or(usize::MAX);
    if max_tokens == 0 {
        return Cow::Borrowed("");
    }

    let tokenizer = o200k_base_singleton();
    let tokens = tokenizer.encode_ordinary(&text);
    if tokens.len() <= max_tokens {
        return text;
    }

    // A token boundary may split a multi-byte UTF-8 scalar. Back up until the
    // decoded prefix is valid rather than replacing bytes or splitting text.
    for end in (1..=max_tokens).rev() {
        if let Ok(prefix) = tokenizer.decode(&tokens[..end]) {
            return Cow::Owned(prefix);
        }
    }

    Cow::Borrowed("")
}
