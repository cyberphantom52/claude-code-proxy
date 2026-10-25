use serde_json::Value;

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
