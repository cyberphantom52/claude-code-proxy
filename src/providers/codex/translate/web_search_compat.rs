use serde_json::Value;

pub struct WebSearchCompatBlock {
    pub index: usize,
    pub content: WebSearchCompatContent,
}

pub enum WebSearchCompatContent {
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    WebSearchToolResult {
        tool_use_id: String,
        content: Vec<WebSearchResult>,
    },
}

#[derive(Clone)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
}

pub fn server_tool_use_id_from_codex_web_search_id(id: &str) -> String {
    let suffix: String = id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("srvtoolu_{suffix}")
}

pub(super) fn web_search_query(item: &Value) -> String {
    let Some(action) = item.get("action") else {
        return String::new();
    };
    action
        .get("query")
        .and_then(Value::as_str)
        .or_else(|| {
            action
                .get("queries")
                .and_then(Value::as_array)
                .and_then(|queries| queries.iter().find_map(Value::as_str))
        })
        .unwrap_or("")
        .to_string()
}

pub fn build_web_search_compat_blocks(
    searches: &[super::reducer::ReducerEvent],
) -> Vec<WebSearchCompatBlock> {
    // Answer links are not search provenance. No verified search-call association
    // exists for output annotations, so keep these result lists empty.
    let mut blocks = Vec::new();

    for event in searches {
        if let super::reducer::ReducerEvent::WebSearch {
            index,
            result_index,
            id,
            query,
        } = event
        {
            blocks.push(WebSearchCompatBlock {
                index: *index,
                content: WebSearchCompatContent::ServerToolUse {
                    id: id.clone(),
                    name: "web_search".to_string(),
                    input: serde_json::json!({"query": query}),
                },
            });
            blocks.push(WebSearchCompatBlock {
                index: *result_index,
                content: WebSearchCompatContent::WebSearchToolResult {
                    tool_use_id: id.clone(),
                    content: Vec::new(),
                },
            });
        }
    }

    blocks
}
