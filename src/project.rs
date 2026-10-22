use std::path::{Path, PathBuf};

use serde_json::Value;

const WORKING_DIRECTORY_PREFIXES: [&str; 3] = [
    "Primary working directory:",
    "Working directory:",
    "Current working directory:",
];

pub fn name_from_system(system: Option<&Value>) -> Option<String> {
    system.and_then(name_from_value)
}

pub fn name_from_request<'a>(
    system: Option<&Value>,
    message_contents: impl IntoIterator<Item = &'a Value>,
) -> Option<String> {
    name_from_system(system).or_else(|| message_contents.into_iter().find_map(name_from_value))
}

fn name_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => name_from_text(text),
        Value::Array(values) => values.iter().find_map(name_from_value),
        Value::Object(object) => object
            .get("text")
            .and_then(Value::as_str)
            .and_then(name_from_text)
            .or_else(|| object.get("content").and_then(name_from_value)),
        _ => None,
    }
}

fn name_from_text(text: &str) -> Option<String> {
    text.lines()
        .find_map(working_directory_from_line)
        .and_then(name_from_working_directory)
}

fn working_directory_from_line(line: &str) -> Option<&str> {
    let line = line.trim().strip_prefix("- ").unwrap_or(line.trim());
    WORKING_DIRECTORY_PREFIXES.iter().find_map(|prefix| {
        line.strip_prefix(prefix)
            .map(str::trim)
            .filter(|path| !path.is_empty())
    })
}

fn name_from_working_directory(path: &str) -> Option<String> {
    let working_directory = Path::new(path);
    let repository_root = working_directory
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists());

    repository_root
        .and_then(repository_name)
        .or_else(|| path_name(working_directory))
}

fn repository_name(root: &Path) -> Option<String> {
    let git_marker = root.join(".git");
    if git_marker.is_dir() {
        return path_name(root);
    }

    let contents = std::fs::read_to_string(git_marker).ok()?;
    let git_dir = contents.trim().strip_prefix("gitdir:")?.trim();
    let git_dir = if Path::new(git_dir).is_absolute() {
        PathBuf::from(git_dir)
    } else {
        root.join(git_dir)
    };
    git_dir
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == ".git"))
        .and_then(Path::parent)
        .and_then(path_name)
        .or_else(|| path_name(root))
}

fn path_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}
