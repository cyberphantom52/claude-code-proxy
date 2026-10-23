use std::borrow::Cow;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::anthropic::schema::MessagesRequest;
use crate::anthropic::sse::encode_sse_event;
use crate::request_identity::ConversationIdentity;
use crate::traffic::TrafficCapture;

use super::count_tokens::{approx_token_count, truncate_to_token_budget};

const SEARCH_OUTPUT_TOKEN_BUDGET: u64 = 2_500;
const SEARCH_ASSISTANT_CONTEXT_TOKEN_BUDGET: u64 = 1_000;
const SEARCH_USER_CONTEXT_MESSAGES: usize = 2;
const CLAUDE_SEARCH_PROMPT_PREFIX: &str = "Perform a web search for the query:";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchRequest {
    pub id: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    pub commands: SearchCommands,
    pub settings: SearchSettings,
    pub max_output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchCommands {
    pub search_query: Vec<SearchQuery>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchQuery {
    pub q: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<SearchFilters>,
    pub allowed_callers: Vec<&'static str>,
    pub external_web_access: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchFilters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SearchResponse {
    pub output: String,
    #[serde(default)]
    pub results: Option<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchResult<'a> {
    title: &'a str,
    url: &'a str,
}

pub fn is_standalone_search_request(req: &MessagesRequest) -> bool {
    let Some(choice) = req.extra.get("tool_choice").and_then(Value::as_object) else {
        return false;
    };
    if choice.get("type").and_then(Value::as_str) != Some("tool") {
        return false;
    }
    let Some(selected_name) = choice.get("name").and_then(Value::as_str) else {
        return false;
    };
    req.extra
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.get("type").and_then(Value::as_str) == Some("web_search_20250305")
                    && tool.get("name").and_then(Value::as_str) == Some(selected_name)
            })
        })
}

pub fn build_search_request(
    req: &MessagesRequest,
    model: &str,
    owner: Option<&ConversationIdentity>,
) -> Result<(SearchRequest, String), anyhow::Error> {
    let query = extract_search_query(req)
        .ok_or_else(|| anyhow::anyhow!("web_search request does not contain a text query"))?;
    let input = search_input(req);
    let filters = search_filters(req);
    let id = owner
        .map(ConversationIdentity::search_session_id)
        .map(Into::into)
        .unwrap_or_else(|| format!("search-{}", uuid::Uuid::new_v4()));

    Ok((
        SearchRequest {
            id,
            model: model.to_string(),
            input,
            commands: SearchCommands {
                search_query: vec![SearchQuery { q: query.clone() }],
            },
            settings: SearchSettings {
                filters,
                allowed_callers: vec!["direct"],
                external_web_access: true,
            },
            max_output_tokens: SEARCH_OUTPUT_TOKEN_BUDGET,
        },
        query,
    ))
}

pub fn search_request_input_tokens(request: &SearchRequest) -> u64 {
    let mut tokens = approx_token_count(&request.model);
    tokens += request
        .commands
        .search_query
        .iter()
        .map(|query| approx_token_count(&query.q))
        .sum::<u64>();
    tokens += request.input.as_ref().map(value_text_tokens).unwrap_or(0);
    if let Some(filters) = &request.settings.filters {
        tokens += filters
            .allowed_domains
            .iter()
            .flatten()
            .chain(filters.blocked_domains.iter().flatten())
            .map(|domain| approx_token_count(domain))
            .sum::<u64>();
    }
    tokens.max(1)
}

pub fn search_response_output_tokens(response: &SearchResponse) -> u64 {
    (approx_token_count(&response.output)
        + response
            .results
            .as_ref()
            .map(|results| results.iter().map(value_text_tokens).sum())
            .unwrap_or(0))
    .max(1)
}

fn value_text_tokens(value: &Value) -> u64 {
    match value {
        Value::String(text) => approx_token_count(text),
        Value::Array(values) => values.iter().map(value_text_tokens).sum(),
        Value::Object(values) => values.values().map(value_text_tokens).sum(),
        _ => 0,
    }
}

pub fn anthropic_search_response(
    response: &SearchResponse,
    query: &str,
    message_id: &str,
    model: &str,
    stream: bool,
    input_tokens: u64,
    traffic: Option<&TrafficCapture>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let tool_use_id = format!("srvtoolu_ws_{}", uuid::Uuid::new_v4().simple());
    let results = search_results(response);
    let output_tokens = search_response_output_tokens(response);
    let usage = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
        "server_tool_use": {"web_search_requests": 1}
    });

    if !stream {
        let content = response_content(response, query, &tool_use_id, &results);
        return (
            http::StatusCode::OK,
            axum::Json(json!({
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": content,
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "usage": usage,
            })),
        )
            .into_response();
    }

    let mut body = Vec::new();
    emit(
        &mut body,
        traffic,
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": input_tokens, "output_tokens": 0}
            }
        }),
    );
    emit(
        &mut body,
        traffic,
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "server_tool_use",
                "id": tool_use_id,
                "name": "web_search",
                "input": {}
            }
        }),
    );
    emit(
        &mut body,
        traffic,
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "input_json_delta",
                "partial_json": json!({"query": query}).to_string()
            }
        }),
    );
    emit_block_stop(&mut body, traffic, 0);
    emit(
        &mut body,
        traffic,
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": tool_use_id,
                "content": web_search_result_values(&results)
            }
        }),
    );
    emit_block_stop(&mut body, traffic, 1);
    if !response.output.is_empty() {
        emit(
            &mut body,
            traffic,
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": 2,
                "content_block": {"type": "text", "text": ""}
            }),
        );
        emit(
            &mut body,
            traffic,
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": {"type": "text_delta", "text": response.output}
            }),
        );
        emit_block_stop(&mut body, traffic, 2);
    }
    emit(
        &mut body,
        traffic,
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": usage
        }),
    );
    emit(
        &mut body,
        traffic,
        "message_stop",
        &json!({"type": "message_stop"}),
    );

    let headers = [
        (http::header::CONTENT_TYPE, "text/event-stream"),
        (http::header::CACHE_CONTROL, "no-cache"),
        (http::header::CONNECTION, "keep-alive"),
    ];
    (headers, body).into_response()
}

fn extract_search_query(req: &MessagesRequest) -> Option<String> {
    req.messages
        .iter()
        .rev()
        .filter(|message| message.role == "user")
        .find_map(|message| {
            let text = content_text(&message.content);
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            Some(
                text.strip_prefix(CLAUDE_SEARCH_PROMPT_PREFIX)
                    .map(str::trim)
                    .filter(|query| !query.is_empty())
                    .unwrap_or(text)
                    .to_string(),
            )
        })
}

fn search_input(req: &MessagesRequest) -> Option<Value> {
    let latest_user = req
        .messages
        .iter()
        .rposition(|message| message.role == "user" && has_content_text(&message.content))?;
    let first_user = req.messages[..=latest_user]
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| message.role == "user" && has_content_text(&message.content))
        .take(SEARCH_USER_CONTEXT_MESSAGES)
        .last()
        .map(|(index, _)| index)
        .unwrap_or(latest_user);

    let mut assistant_budget = SEARCH_ASSISTANT_CONTEXT_TOKEN_BUDGET;
    let items: Vec<Value> = req.messages[first_user..=latest_user]
        .iter()
        .filter(|message| matches!(message.role.as_str(), "user" | "assistant"))
        .filter_map(|message| {
            let role = message.role.as_str();
            if role == "assistant" && assistant_budget == 0 {
                return None;
            }
            let text = content_text(&message.content);
            if text.is_empty() {
                return None;
            }
            let (content_type, text) = if role == "assistant" {
                let text = truncate_to_token_budget(text, assistant_budget);
                assistant_budget = assistant_budget.saturating_sub(approx_token_count(&text));
                ("output_text", text)
            } else {
                ("input_text", text)
            };
            (!text.is_empty()).then(|| {
                let mut item = json!({
                    "type": "message",
                    "role": role,
                    "content": [{"type": content_type, "text": null}]
                });
                item["content"][0]["text"] = Value::String(text.into_owned());
                item
            })
        })
        .collect();
    (!items.is_empty()).then_some(Value::Array(items))
}

fn has_content_text(content: &Value) -> bool {
    match content {
        Value::String(text) => !text.is_empty(),
        Value::Array(blocks) => {
            let mut texts = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str));
            // Even two empty text blocks join to a nonempty newline.
            texts
                .next()
                .is_some_and(|first| !first.is_empty() || texts.next().is_some())
        }
        _ => false,
    }
}

fn content_text(content: &Value) -> Cow<'_, str> {
    match content {
        Value::String(text) => Cow::Borrowed(text),
        Value::Array(blocks) => Cow::Owned(
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => Cow::Borrowed(""),
    }
}

fn search_filters(req: &MessagesRequest) -> Option<SearchFilters> {
    let tool = req
        .extra
        .get("tools")?
        .as_array()?
        .iter()
        .find(|tool| tool.get("type").and_then(Value::as_str) == Some("web_search_20250305"))?;
    let allowed_domains = string_array(tool.get("allowed_domains"));
    let blocked_domains = string_array(tool.get("blocked_domains"));
    (allowed_domains.is_some() || blocked_domains.is_some()).then_some(SearchFilters {
        allowed_domains,
        blocked_domains,
    })
}

fn string_array(value: Option<&Value>) -> Option<Vec<String>> {
    let values = value?.as_array()?;
    let values: Vec<String> = values
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    (!values.is_empty()).then_some(values)
}

fn search_results(response: &SearchResponse) -> Vec<SearchResult<'_>> {
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    for result in response.results.iter().flatten() {
        let Some(url) = result.get("url").and_then(Value::as_str) else {
            continue;
        };
        if !seen.insert(url) {
            continue;
        }
        let title = result
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| result.get("ref_id").and_then(Value::as_str))
            .unwrap_or(url);
        results.push(SearchResult { title, url });
    }
    results
}

fn response_content(
    response: &SearchResponse,
    query: &str,
    tool_use_id: &str,
    results: &[SearchResult<'_>],
) -> Vec<Value> {
    let mut content = vec![
        json!({
            "type": "server_tool_use",
            "id": tool_use_id,
            "name": "web_search",
            "input": {"query": query}
        }),
        json!({
            "type": "web_search_tool_result",
            "tool_use_id": tool_use_id,
            "content": web_search_result_values(results)
        }),
    ];
    if !response.output.is_empty() {
        content.push(json!({"type": "text", "text": response.output}));
    }
    content
}

fn web_search_result_values(results: &[SearchResult<'_>]) -> Vec<Value> {
    results
        .iter()
        .map(|result| {
            json!({
                "type": "web_search_result",
                "title": result.title,
                "url": result.url,
            })
        })
        .collect()
}

fn emit_block_stop(out: &mut Vec<u8>, traffic: Option<&TrafficCapture>, index: usize) {
    emit(
        out,
        traffic,
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": index}),
    );
}

fn emit(out: &mut Vec<u8>, traffic: Option<&TrafficCapture>, event: &str, data: &Value) {
    if let Some(traffic) = traffic {
        traffic.write_json_event(
            "050-downstream-event",
            &json!({"event": event, "data": data}),
        );
    }
    out.extend_from_slice(&encode_sse_event(Some(event), &data.to_string()));
}
