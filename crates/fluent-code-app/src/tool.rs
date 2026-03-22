use std::fs;
use std::path::{Path, PathBuf};

use fluent_code_provider::{ProviderTool, ProviderToolCall};
use regex::Regex;
use serde_json::Value;

use crate::error::{FluentCodeError, Result};

const UPPERCASE_TEXT_TOOL_NAME: &str = "uppercase_text";
const READ_TOOL_NAME: &str = "read";
const GLOB_TOOL_NAME: &str = "glob";
const GREP_TOOL_NAME: &str = "grep";

const DEFAULT_READ_OFFSET: usize = 1;
const DEFAULT_READ_LIMIT: usize = 200;
const MAX_READ_LIMIT: usize = 500;
const MAX_GLOB_MATCHES: usize = 200;
const DEFAULT_GREP_HEAD_LIMIT: usize = 50;
const MAX_GREP_HEAD_LIMIT: usize = 200;

pub fn built_in_tools() -> Vec<ProviderTool> {
    vec![uppercase_text_tool(), read_tool(), glob_tool(), grep_tool()]
}

pub fn execute_built_in_tool(tool_call: &ProviderToolCall) -> Result<String> {
    let workspace_root = std::env::current_dir().map_err(FluentCodeError::Io)?;

    match tool_call.name.as_str() {
        UPPERCASE_TEXT_TOOL_NAME => execute_uppercase_text(&tool_call.arguments),
        READ_TOOL_NAME => execute_read(&workspace_root, &tool_call.arguments),
        GLOB_TOOL_NAME => execute_glob(&workspace_root, &tool_call.arguments),
        GREP_TOOL_NAME => execute_grep(&workspace_root, &tool_call.arguments),
        other => Err(FluentCodeError::Provider(format!(
            "unsupported built-in tool '{other}'"
        ))),
    }
}

fn uppercase_text_tool() -> ProviderTool {
    ProviderTool {
        name: UPPERCASE_TEXT_TOOL_NAME.to_string(),
        description: "Convert the provided text to uppercase.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The text to convert to uppercase"
                }
            },
            "required": ["text"],
            "additionalProperties": false
        }),
    }
}

fn read_tool() -> ProviderTool {
    ProviderTool {
        name: READ_TOOL_NAME.to_string(),
        description: "Read a file with line numbers or list a directory within the workspace."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative file or directory path"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based line offset for file reads"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of lines to return"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    }
}

fn glob_tool() -> ProviderTool {
    ProviderTool {
        name: GLOB_TOOL_NAME.to_string(),
        description: "Find files by glob pattern within the workspace.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern such as **/*.rs or src/**/*.toml"
                },
                "path": {
                    "type": "string",
                    "description": "Optional workspace-relative directory to search within"
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    }
}

fn grep_tool() -> ProviderTool {
    ProviderTool {
        name: GREP_TOOL_NAME.to_string(),
        description: "Search file contents with a regex inside the workspace.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "include": {
                    "type": "string",
                    "description": "Optional glob to filter matched files, for example *.rs"
                },
                "path": {
                    "type": "string",
                    "description": "Optional workspace-relative directory to search within"
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["files_with_matches", "content", "count"],
                    "description": "How to format the search result"
                },
                "head_limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of matches or files to return"
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    }
}

fn execute_uppercase_text(arguments: &Value) -> Result<String> {
    let text = get_required_str(arguments, "text", "uppercase_text")?;
    Ok(text.to_uppercase())
}

fn execute_read(workspace_root: &Path, arguments: &Value) -> Result<String> {
    let path = get_required_str(arguments, "path", READ_TOOL_NAME)?;
    let target = resolve_workspace_path(workspace_root, path)?;
    let metadata = fs::metadata(&target).map_err(FluentCodeError::Io)?;

    if metadata.is_dir() {
        return read_directory(workspace_root, &target);
    }

    let offset = get_optional_usize(arguments, "offset")?.unwrap_or(DEFAULT_READ_OFFSET);
    let limit = get_optional_usize(arguments, "limit")?
        .unwrap_or(DEFAULT_READ_LIMIT)
        .min(MAX_READ_LIMIT);
    let content = fs::read_to_string(&target).map_err(|error| {
        FluentCodeError::Provider(format!(
            "read could not open '{}' as UTF-8 text: {error}",
            display_relative(workspace_root, &target)
        ))
    })?;

    Ok(format_file_slice(
        &display_relative(workspace_root, &target),
        &content,
        offset,
        limit,
    ))
}

fn execute_glob(workspace_root: &Path, arguments: &Value) -> Result<String> {
    let pattern = get_required_str(arguments, "pattern", GLOB_TOOL_NAME)?;
    let search_root = resolve_optional_search_root(workspace_root, arguments)?;
    let absolute_pattern = search_root.join(pattern);
    let absolute_pattern = absolute_pattern.to_string_lossy().replace('\\', "/");

    let mut matches = glob::glob(&absolute_pattern)
        .map_err(|error| FluentCodeError::Provider(format!("invalid glob pattern: {error}")))?
        .filter_map(std::result::Result::ok)
        .filter_map(|candidate| canonicalize_within_workspace(workspace_root, &candidate).ok())
        .collect::<Vec<_>>();

    matches.sort();
    matches.dedup();

    if matches.len() > MAX_GLOB_MATCHES {
        matches.truncate(MAX_GLOB_MATCHES);
    }

    if matches.is_empty() {
        return Ok("Found 0 path(s)".to_string());
    }

    let mut lines = vec![format!("Found {} path(s)", matches.len())];
    for path in matches {
        let mut display = display_relative(workspace_root, &path);
        if path.is_dir() {
            display.push('/');
        }
        lines.push(display);
    }

    Ok(lines.join("\n"))
}

fn execute_grep(workspace_root: &Path, arguments: &Value) -> Result<String> {
    let pattern = get_required_str(arguments, "pattern", GREP_TOOL_NAME)?;
    let regex = Regex::new(pattern)
        .map_err(|error| FluentCodeError::Provider(format!("invalid regex pattern: {error}")))?;
    let include = get_optional_str(arguments, "include")?;
    let output_mode = get_optional_str(arguments, "output_mode")?
        .unwrap_or_else(|| "files_with_matches".to_string());
    let head_limit = get_optional_usize(arguments, "head_limit")?
        .unwrap_or(DEFAULT_GREP_HEAD_LIMIT)
        .min(MAX_GREP_HEAD_LIMIT);
    let search_root = resolve_optional_search_root(workspace_root, arguments)?;

    let mut matched_files = Vec::new();
    let mut content_matches = Vec::new();
    let mut count_matches = Vec::new();

    for path in collect_files(&search_root, workspace_root)? {
        if let Some(include_pattern) = include.as_deref()
            && !matches_include_pattern(workspace_root, &path, include_pattern)?
        {
            continue;
        }

        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };

        let mut match_count = 0usize;
        for (index, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                match_count += 1;
                if output_mode == "content" && content_matches.len() < head_limit {
                    content_matches.push(format!(
                        "{}:{}: {}",
                        display_relative(workspace_root, &path),
                        index + 1,
                        line
                    ));
                }
            }
        }

        if match_count > 0 {
            matched_files.push(display_relative(workspace_root, &path));
            count_matches.push(format!(
                "{}: {}",
                display_relative(workspace_root, &path),
                match_count
            ));
        }
    }

    matched_files.sort();
    matched_files.dedup();
    count_matches.sort();
    content_matches.sort();

    let output = match output_mode.as_str() {
        "files_with_matches" => {
            let lines = matched_files
                .into_iter()
                .take(head_limit)
                .collect::<Vec<_>>();
            if lines.is_empty() {
                "Found 0 matching file(s)".to_string()
            } else {
                lines.join("\n")
            }
        }
        "count" => {
            let lines = count_matches
                .into_iter()
                .take(head_limit)
                .collect::<Vec<_>>();
            if lines.is_empty() {
                "Found 0 matching file(s)".to_string()
            } else {
                lines.join("\n")
            }
        }
        "content" => {
            let lines = content_matches
                .into_iter()
                .take(head_limit)
                .collect::<Vec<_>>();
            if lines.is_empty() {
                "Found 0 matching line(s)".to_string()
            } else {
                lines.join("\n")
            }
        }
        other => {
            return Err(FluentCodeError::Provider(format!(
                "grep output_mode must be one of files_with_matches, content, or count, got '{other}'"
            )));
        }
    };

    Ok(output)
}

fn read_directory(workspace_root: &Path, directory: &Path) -> Result<String> {
    let mut entries = fs::read_dir(directory)
        .map_err(FluentCodeError::Io)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    entries.sort();

    let mut lines = Vec::new();
    for entry in entries {
        let mut display = display_relative(workspace_root, &entry);
        if entry.is_dir() {
            display.push('/');
        }
        lines.push(display);
    }

    if lines.is_empty() {
        return Ok(format!(
            "Directory is empty: {}",
            display_relative(workspace_root, directory)
        ));
    }

    Ok(lines.join("\n"))
}

fn format_file_slice(path: &str, content: &str, offset: usize, limit: usize) -> String {
    let start = offset.max(1);
    let mut lines = Vec::new();
    lines.push(format!("<path>{path}</path>"));

    for (index, line) in content.lines().enumerate().skip(start - 1).take(limit) {
        lines.push(format!("{}: {}", index + 1, line));
    }

    if lines.len() == 1 {
        lines.push(format!("{start}: "));
    }

    lines.join("\n")
}

fn resolve_optional_search_root(workspace_root: &Path, arguments: &Value) -> Result<PathBuf> {
    match get_optional_str(arguments, "path")? {
        Some(path) => resolve_workspace_path(workspace_root, &path),
        None => Ok(workspace_root.to_path_buf()),
    }
}

fn resolve_workspace_path(workspace_root: &Path, path: &str) -> Result<PathBuf> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        workspace_root.join(path)
    };

    canonicalize_within_workspace(workspace_root, &candidate)
}

fn canonicalize_within_workspace(workspace_root: &Path, candidate: &Path) -> Result<PathBuf> {
    let canonical_root = workspace_root.canonicalize().map_err(FluentCodeError::Io)?;
    let canonical_candidate = candidate.canonicalize().map_err(|error| {
        FluentCodeError::Provider(format!(
            "path '{}' is not accessible: {error}",
            candidate.display()
        ))
    })?;

    if canonical_candidate == canonical_root || canonical_candidate.starts_with(&canonical_root) {
        Ok(canonical_candidate)
    } else {
        Err(FluentCodeError::Provider(format!(
            "path '{}' escapes the workspace root",
            candidate.display()
        )))
    }
}

fn display_relative(workspace_root: &Path, path: &Path) -> String {
    path.strip_prefix(workspace_root)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

fn collect_files(root: &Path, workspace_root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(path) = stack.pop() {
        let metadata = fs::metadata(&path).map_err(FluentCodeError::Io)?;
        if metadata.is_dir() {
            let mut entries = fs::read_dir(&path)
                .map_err(FluentCodeError::Io)?
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .collect::<Vec<_>>();
            entries.sort();
            for entry in entries.into_iter().rev() {
                if canonicalize_within_workspace(workspace_root, &entry).is_ok() {
                    stack.push(entry);
                }
            }
        } else if metadata.is_file() {
            files.push(path);
        }
    }

    files.sort();
    Ok(files)
}

fn matches_include_pattern(workspace_root: &Path, path: &Path, pattern: &str) -> Result<bool> {
    let path = display_relative(workspace_root, path);
    let escaped = regex::escape(pattern).replace("\\*", ".*");
    let regex = Regex::new(&format!("^{escaped}$"))
        .map_err(|error| FluentCodeError::Provider(format!("invalid include pattern: {error}")))?;
    Ok(regex.is_match(&path))
}

fn get_required_str<'a>(arguments: &'a Value, key: &str, tool_name: &str) -> Result<&'a str> {
    arguments.get(key).and_then(Value::as_str).ok_or_else(|| {
        FluentCodeError::Provider(format!("{tool_name} requires a string '{key}' argument"))
    })
}

fn get_optional_str(arguments: &Value, key: &str) -> Result<Option<String>> {
    match arguments.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(FluentCodeError::Provider(format!(
            "argument '{key}' must be a string"
        ))),
    }
}

fn get_optional_usize(arguments: &Value, key: &str) -> Result<Option<usize>> {
    match arguments.get(key) {
        None => Ok(None),
        Some(Value::Number(number)) => {
            let value = number.as_u64().ok_or_else(|| {
                FluentCodeError::Provider(format!("argument '{key}' must be a positive integer"))
            })?;
            usize::try_from(value)
                .map(Some)
                .map_err(|_| FluentCodeError::Provider(format!("argument '{key}' is too large")))
        }
        Some(_) => Err(FluentCodeError::Provider(format!(
            "argument '{key}' must be a positive integer"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    use fluent_code_provider::ProviderToolCall;

    use super::{built_in_tools, execute_built_in_tool};

    fn current_dir_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn built_in_tools_include_read_glob_and_grep() {
        let tools = built_in_tools();
        let names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"uppercase_text"));
        assert!(names.contains(&"read"));
        assert!(names.contains(&"glob"));
        assert!(names.contains(&"grep"));
    }

    #[test]
    fn read_returns_line_numbered_file_content() {
        let _guard = current_dir_lock().lock().expect("lock current dir");
        let workspace = unique_test_dir();
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::write(workspace.join("notes.txt"), "alpha\nbeta\ngamma\n").expect("write file");

        let original_dir = std::env::current_dir().expect("get current dir");
        std::env::set_current_dir(&workspace).expect("set current dir");

        let result = execute_built_in_tool(&ProviderToolCall {
            id: "call-1".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({ "path": "notes.txt", "offset": 2, "limit": 2 }),
        })
        .expect("read tool result");

        std::env::set_current_dir(&original_dir).expect("restore current dir");

        assert!(result.contains("<path>notes.txt</path>"));
        assert!(result.contains("2: beta"));
        assert!(result.contains("3: gamma"));

        cleanup(workspace);
    }

    #[test]
    fn read_rejects_path_outside_workspace() {
        let _guard = current_dir_lock().lock().expect("lock current dir");
        let workspace = unique_test_dir();
        fs::create_dir_all(&workspace).expect("create workspace");
        let outside_dir = std::env::temp_dir();

        let original_dir = std::env::current_dir().expect("get current dir");
        std::env::set_current_dir(&workspace).expect("set current dir");

        let error = execute_built_in_tool(&ProviderToolCall {
            id: "call-1".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({ "path": outside_dir.to_string_lossy().to_string() }),
        })
        .expect_err("read should reject outside path");

        std::env::set_current_dir(&original_dir).expect("restore current dir");
        assert!(error.to_string().contains("escapes the workspace root"));

        cleanup(workspace);
    }

    #[test]
    fn read_missing_path_reports_not_accessible() {
        let _guard = current_dir_lock().lock().expect("lock current dir");
        let workspace = unique_test_dir();
        fs::create_dir_all(&workspace).expect("create workspace");

        let original_dir = std::env::current_dir().expect("get current dir");
        std::env::set_current_dir(&workspace).expect("set current dir");

        let error = execute_built_in_tool(&ProviderToolCall {
            id: "call-missing-read".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({ "path": "missing.txt" }),
        })
        .expect_err("read should fail for missing path");

        std::env::set_current_dir(&original_dir).expect("restore current dir");

        assert!(error.to_string().contains("is not accessible"));

        cleanup(workspace);
    }

    #[test]
    fn glob_returns_matching_relative_paths() {
        let _guard = current_dir_lock().lock().expect("lock current dir");
        let workspace = unique_test_dir();
        fs::create_dir_all(workspace.join("src/nested")).expect("create dirs");
        fs::write(workspace.join("src/main.rs"), "fn main() {}\n").expect("write main");
        fs::write(workspace.join("src/nested/lib.rs"), "pub fn run() {}\n").expect("write lib");

        let original_dir = std::env::current_dir().expect("get current dir");
        std::env::set_current_dir(&workspace).expect("set current dir");

        let result = execute_built_in_tool(&ProviderToolCall {
            id: "call-1".to_string(),
            name: "glob".to_string(),
            arguments: serde_json::json!({ "pattern": "src/**/*.rs" }),
        })
        .expect("glob tool result");

        std::env::set_current_dir(&original_dir).expect("restore current dir");

        assert!(result.contains("src/main.rs"));
        assert!(result.contains("src/nested/lib.rs"));

        cleanup(workspace);
    }

    #[test]
    fn grep_returns_matching_content_lines() {
        let _guard = current_dir_lock().lock().expect("lock current dir");
        let workspace = unique_test_dir();
        fs::create_dir_all(workspace.join("src")).expect("create dirs");
        fs::write(
            workspace.join("src/main.rs"),
            "fn alpha() {}\nfn beta() {}\n",
        )
        .expect("write file");

        let original_dir = std::env::current_dir().expect("get current dir");
        std::env::set_current_dir(&workspace).expect("set current dir");

        let result = execute_built_in_tool(&ProviderToolCall {
            id: "call-1".to_string(),
            name: "grep".to_string(),
            arguments: serde_json::json!({
                "pattern": "beta",
                "include": "src/*.rs",
                "output_mode": "content"
            }),
        })
        .expect("grep tool result");

        std::env::set_current_dir(&original_dir).expect("restore current dir");

        assert!(result.contains("src/main.rs:2: fn beta() {}"));

        cleanup(workspace);
    }

    #[test]
    fn grep_rejects_invalid_regex() {
        let _guard = current_dir_lock().lock().expect("lock current dir");
        let workspace = unique_test_dir();
        fs::create_dir_all(&workspace).expect("create workspace");

        let original_dir = std::env::current_dir().expect("get current dir");
        std::env::set_current_dir(&workspace).expect("set current dir");

        let error = execute_built_in_tool(&ProviderToolCall {
            id: "call-1".to_string(),
            name: "grep".to_string(),
            arguments: serde_json::json!({ "pattern": "(" }),
        })
        .expect_err("grep should reject invalid regex");

        std::env::set_current_dir(&original_dir).expect("restore current dir");
        assert!(error.to_string().contains("invalid regex pattern"));

        cleanup(workspace);
    }

    fn unique_test_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();

        std::env::temp_dir().join(format!("fluent-code-tool-test-{nanos}"))
    }

    fn cleanup(path: PathBuf) {
        let _ = fs::remove_dir_all(path);
    }
}
