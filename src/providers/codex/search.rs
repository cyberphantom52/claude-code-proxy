use std::borrow::Cow;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::anthropic::schema::MessagesRequest;
use crate::request_identity::ConversationIdentity;

use super::count_tokens::{approx_token_count, truncate_to_token_budget};
use super::translate::request::{ResponsesContentPart, ResponsesInputItem};

mod tool;
pub(crate) use tool::native_search_tool;

const SEARCH_OUTPUT_TOKEN_BUDGET: u64 = 2_500;

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

// Match the pinned codex-api search commands, but reject unknown fields instead
// of silently discarding model instructions before forwarding them to alpha/search.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SearchCommands {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_query: Option<Vec<SearchQuery>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_query: Option<Vec<SearchQuery>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<Vec<OpenOperation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub click: Option<Vec<ClickOperation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub find: Option<Vec<FindOperation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<Vec<ScreenshotOperation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finance: Option<Vec<FinanceOperation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weather: Option<Vec<WeatherOperation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sports: Option<Vec<SportsOperation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<Vec<TimeOperation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_length: Option<SearchResponseLength>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recency: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OpenOperation {
    pub ref_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineno: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClickOperation {
    pub ref_id: String,
    pub id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FindOperation {
    pub ref_id: String,
    pub pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotOperation {
    pub ref_id: String,
    pub pageno: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FinanceOperation {
    pub ticker: String,
    pub r#type: FinanceAssetType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FinanceAssetType {
    Equity,
    Fund,
    Crypto,
    Index,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WeatherOperation {
    pub location: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SportsOperation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<SportsToolName>,
    pub r#fn: SportsFunction,
    pub league: SportsLeague,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opponent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_games: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SportsToolName {
    Sports,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SportsFunction {
    Schedule,
    Standings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SportsLeague {
    Nba,
    Wnba,
    Nfl,
    Nhl,
    Mlb,
    Epl,
    Ncaamb,
    Ncaawb,
    Ipl,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TimeOperation {
    pub utc_offset: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SearchResponseLength {
    Short,
    Medium,
    Long,
}

impl SearchCommands {
    pub(crate) fn parse(arguments: &str) -> anyhow::Result<Self> {
        if arguments.trim().is_empty() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_str(arguments)?)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<SearchFilters>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_location: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_context_size: Option<String>,
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

pub(crate) struct SearchPolicy {
    pub(crate) forced: bool,
    pub(crate) allow_search: bool,
    pub(crate) max_uses: u64,
    id: String,
    settings: SearchSettings,
}

impl SearchPolicy {
    pub(crate) fn from_request(
        req: &MessagesRequest,
        owner: Option<&ConversationIdentity>,
    ) -> Option<Self> {
        let tool =
            req.extra.get("tools")?.as_array()?.iter().find(|tool| {
                tool.get("type").and_then(Value::as_str) == Some("web_search_20250305")
            })?;
        let choice = req.extra.get("tool_choice");
        let choice_kind = choice.and_then(|choice| {
            choice
                .as_str()
                .or_else(|| choice.get("type").and_then(Value::as_str))
        });
        let forced = choice.is_some_and(|choice| {
            choice["type"] == "tool" && choice.get("name") == tool.get("name")
        });
        let base = owner
            .map(|owner| owner.search_session_id().into_owned())
            .unwrap_or_else(|| format!("search-{}", uuid::Uuid::new_v4()));
        let id = if let Some(branch) = super::threads::branch() {
            let mut digest = Sha256::new();
            for value in [base.as_str(), branch.as_str()] {
                digest.update((value.len() as u64).to_be_bytes());
                digest.update(value.as_bytes());
            }
            format!("search-{:x}", digest.finalize())
        } else {
            base
        };
        let allowed_domains = string_array(tool.get("allowed_domains"));
        let blocked_domains = string_array(tool.get("blocked_domains"));
        Some(Self {
            forced,
            allow_search: choice_kind != Some("none") && (choice_kind != Some("tool") || forced),
            max_uses: tool
                .get("max_uses")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX),
            id,
            settings: SearchSettings {
                filters: (allowed_domains.is_some() || blocked_domains.is_some()).then_some(
                    SearchFilters {
                        allowed_domains,
                        blocked_domains,
                    },
                ),
                user_location: tool.get("user_location").cloned(),
                search_context_size: tool
                    .get("search_context_size")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                allowed_callers: vec!["direct"],
                external_web_access: true,
            },
        })
    }

    pub(crate) fn request(
        &self,
        model: &str,
        commands: SearchCommands,
        history: &[ResponsesInputItem],
    ) -> SearchRequest {
        SearchRequest {
            id: self.id.clone(),
            model: model.to_owned(),
            input: recent_input(history),
            commands,
            settings: self.settings.clone(),
            max_output_tokens: SEARCH_OUTPUT_TOKEN_BUDGET,
        }
    }
}

// Pinned ext/web-search/history.rs policy: the last two visible user text
// messages, assistant text between them (shared 1k-token budget), nothing after
// the current user. Never send private tools, reasoning, images, or declarations.
fn recent_input(history: &[ResponsesInputItem]) -> Option<Value> {
    let mut messages: Vec<_> = history
        .iter()
        .filter_map(|item| {
            let ResponsesInputItem::Message {
                role,
                content,
                metadata,
            } = item
            else {
                return None;
            };
            if role != "user" && role != "assistant" {
                return None;
            }
            let content: Vec<_> = content
                .iter()
                .filter(|part| {
                    matches!(
                        (role.as_str(), part),
                        ("user", ResponsesContentPart::InputText { .. })
                            | ("assistant", ResponsesContentPart::OutputText { .. })
                    )
                })
                .cloned()
                .collect();
            if content.is_empty() {
                return None;
            }
            let mut metadata = metadata.clone();
            metadata.id = None;
            metadata.status = None;
            metadata.encrypted_content = None;
            Some(ResponsesInputItem::Message {
                role: role.clone(),
                content,
                metadata,
            })
        })
        .collect();
    let users: Vec<_> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            matches!(item, ResponsesInputItem::Message { role, .. } if role == "user").then_some(i)
        })
        .collect();
    let last = *users.last()?;
    let first = users[users.len().saturating_sub(2)];
    messages.truncate(last + 1);
    messages.drain(..first);
    let mut budget = 1_000;
    messages.retain_mut(|item| {
        if let ResponsesInputItem::Message { role, content, .. } = item
            && role == "assistant"
        {
            content.retain_mut(|part| {
                let ResponsesContentPart::OutputText { text, .. } = part else {
                    return false;
                };
                if budget == 0 {
                    return false;
                }
                *text = truncate_to_token_budget(Cow::Borrowed(text), budget).into_owned();
                budget = budget.saturating_sub(approx_token_count(text));
                true
            });
            return !content.is_empty();
        }
        true
    });
    serde_json::to_value(messages).ok()
}

fn string_array(value: Option<&Value>) -> Option<Vec<String>> {
    value?.as_array().map(|values| {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    })
}

pub(crate) fn compatibility_blocks(
    call_id: &str,
    arguments: &Value,
    response: Result<&SearchResponse, &str>,
) -> Vec<Value> {
    let id = format!("srvtoolu_{call_id}");
    let content = match response {
        Ok(response) => {
            let mut seen = HashSet::new();
            Value::Array(
                response
                    .results
                    .iter()
                    .flatten()
                    .filter_map(|result| {
                        let url = result.get("url")?.as_str()?;
                        if !seen.insert(url) {
                            return None;
                        }
                        Some(json!({"type":"web_search_result", "url":url,
                    "title":result.get("title").and_then(Value::as_str).unwrap_or(url)}))
                    })
                    .collect(),
            )
        }
        Err(message) => {
            let code = if message == "web search max_uses limit reached" {
                "max_uses_exceeded"
            } else if message.starts_with("invalid web.run commands:") {
                "invalid_input"
            } else {
                "unavailable"
            };
            json!({"type":"web_search_tool_result_error", "error_code":code, "message":message})
        }
    };
    vec![
        json!({"type":"server_tool_use", "id":id, "name":"web_search", "input":arguments}),
        json!({"type":"web_search_tool_result", "tool_use_id":id, "content":content}),
    ]
}
