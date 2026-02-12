use regex::Regex;
use serde_json::Value;

use crate::llm::Message;
use crate::logger::{Logger, ReplEnvLogger};
use crate::repl::{ReplEnv, ReplResult};

#[derive(Clone, Debug)]
pub enum ContextInput {
    Json(Value),
    Text(String),
    Messages(Vec<Message>),
    Strings(Vec<String>),
}

impl From<String> for ContextInput {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for ContextInput {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<Vec<String>> for ContextInput {
    fn from(value: Vec<String>) -> Self {
        Self::Strings(value)
    }
}

impl From<Vec<Message>> for ContextInput {
    fn from(value: Vec<Message>) -> Self {
        Self::Messages(value)
    }
}

impl From<Value> for ContextInput {
    fn from(value: Value) -> Self {
        Self::Json(value)
    }
}

#[derive(Clone, Debug)]
pub struct ContextData {
    pub json: Option<Value>,
    pub text: Option<String>,
}

pub fn context_from_value(value: Option<Value>) -> ContextInput {
    match value {
        None => ContextInput::Text(String::new()),
        Some(Value::String(text)) => ContextInput::Text(text),
        Some(Value::Array(items)) => {
            if let Some(strings) = array_to_strings(&items) {
                return ContextInput::Strings(strings);
            }
            if let Some(messages) = array_to_messages(&items) {
                return ContextInput::Messages(messages);
            }
            ContextInput::Json(Value::Array(items))
        }
        Some(other) => ContextInput::Json(other),
    }
}

pub fn convert_context_for_repl(context: ContextInput) -> ContextData {
    match context {
        ContextInput::Json(value) => ContextData {
            json: Some(normalize_context_json(value)),
            text: None,
        },
        ContextInput::Text(value) => ContextData {
            json: None,
            text: Some(value),
        },
        ContextInput::Messages(messages) => {
            let items: Vec<String> = messages.into_iter().map(|msg| msg.content).collect();
            ContextData {
                json: Some(Value::Array(items.into_iter().map(Value::String).collect())),
                text: None,
            }
        }
        ContextInput::Strings(items) => ContextData {
            json: Some(Value::Array(items.into_iter().map(Value::String).collect())),
            text: None,
        },
    }
}

fn array_to_strings(items: &[Value]) -> Option<Vec<String>> {
    let mut strings = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Value::String(text) => strings.push(text.clone()),
            _ => return None,
        }
    }
    Some(strings)
}

fn array_to_messages(items: &[Value]) -> Option<Vec<Message>> {
    let mut messages = Vec::with_capacity(items.len());
    for item in items {
        let map = match item {
            Value::Object(map) => map,
            _ => return None,
        };
        let content_value = map.get("content")?;
        let content = match content_value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let role = map
            .get("role")
            .and_then(|value| value.as_str())
            .unwrap_or("user")
            .to_owned();
        messages.push(Message { role, content });
    }
    Some(messages)
}

fn normalize_context_json(value: Value) -> Value {
    match value {
        Value::Array(items) => {
            let use_content = items
                .first()
                .and_then(|item| match item {
                    Value::Object(map) => map.get("content"),
                    _ => None,
                })
                .is_some();
            if use_content {
                let mapped = items
                    .into_iter()
                    .map(|item| {
                        if let Value::Object(mut map) = item {
                            map.remove("content")
                                .and_then(|value| value.as_str().map(|text| text.to_owned()))
                                .unwrap_or_default()
                        } else {
                            String::new()
                        }
                    })
                    .map(Value::String)
                    .collect();
                Value::Array(mapped)
            } else {
                Value::Array(items)
            }
        }
        other => other,
    }
}

pub fn find_code_blocks(text: &str) -> Vec<String> {
    let pattern = Regex::new(r"```repl\s*\n(?s:(.*?))\n```").expect("regex");
    pattern
        .captures_iter(text)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().trim().to_owned()))
        .collect()
}

pub enum FinalAnswerKind {
    Final,
    FinalVar,
}

pub fn find_final_answer(text: &str) -> Option<(FinalAnswerKind, String)> {
    let final_var_re = Regex::new(r"(?ms)^\s*FINAL_VAR\((.*?)\)").expect("regex");
    if let Some(cap) = final_var_re.captures(text) {
        return Some((FinalAnswerKind::FinalVar, cap[1].trim().to_owned()));
    }
    let final_re = Regex::new(r"(?ms)^\s*FINAL\((.*?)\)").expect("regex");
    if let Some(cap) = final_re.captures(text) {
        return Some((FinalAnswerKind::Final, cap[1].trim().to_owned()));
    }
    None
}

pub fn add_execution_result_to_messages(
    messages: &mut Vec<Message>,
    code: &str,
    result: &str,
    max_character_length: usize,
) {
    let mut output = result.to_owned();
    if output.len() > max_character_length {
        output.truncate(max_character_length);
        output.push_str("...");
    }
    messages.push(Message::user(format!(
        "Code executed:\n```python\n{code}\n```\n\nREPL output:\n{output}"
    )));
}

pub fn format_execution_result(result: &ReplResult) -> String {
    let mut parts = Vec::new();
    if !result.stdout.is_empty() {
        parts.push(format!("\n{}", result.stdout));
    }
    if !result.stderr.is_empty() {
        parts.push(format!("\n{}", result.stderr));
    }
    if !result.locals.is_empty() || !result.locals_map.is_empty() {
        let mut vars = Vec::new();
        for local in &result.locals {
            if should_skip_var_name(&local.name) || !local.is_simple {
                continue;
            }
            let display = if let Some(value) = &local.string_value {
                let (truncated, did_truncate) = truncate_string(value, 100);
                if did_truncate {
                    format!("'{}...'", escape_string(&truncated))
                } else {
                    local.repr.clone()
                }
            } else {
                local.repr.clone()
            };
            vars.push(format!("{}={}", local.name, display));
        }
        if vars.is_empty() {
            for (name, repr) in &result.locals_map {
                if should_skip_var_name(name) {
                    continue;
                }
                vars.push(format!("{name}={repr}"));
            }
        }
        if !vars.is_empty() {
            parts.push(format!("REPL variables: [{}]\n", vars.join(", ")));
        }
    }
    if parts.is_empty() {
        "No output".to_owned()
    } else {
        parts.join("\n")
    }
}

fn should_skip_var_name(name: &str) -> bool {
    name.starts_with('_') || matches!(name, "__builtins__" | "__name__" | "__doc__")
}

fn truncate_string(value: &str, max_len: usize) -> (String, bool) {
    if value.len() <= max_len {
        return (value.to_owned(), false);
    }
    let mut end = max_len.min(value.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (value[..end].to_owned(), true)
}

fn escape_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

pub fn execute_code(
    repl_env: &mut ReplEnv,
    code: &str,
    repl_env_logger: &mut ReplEnvLogger,
    logger: &Logger,
) -> String {
    match repl_env.execute(code) {
        Ok(result) => {
            let output = format_execution_result(&result);
            repl_env_logger.log_execution(
                code,
                &result.stdout,
                &result.stderr,
                result.execution_time,
            );
            repl_env_logger.display_last();

            logger.log_tool_execution(code, &output);
            output
        }
        Err(err) => format!("Error executing code: {err}"),
    }
}

pub fn process_code_execution(
    response: &str,
    messages: &mut Vec<Message>,
    repl_env: &mut ReplEnv,
    repl_env_logger: &mut ReplEnvLogger,
    logger: &Logger,
    disable_recursive: bool,
) {
    let code_blocks = find_code_blocks(response);
    for code in code_blocks {
        let execution_result = execute_code(repl_env, &code, repl_env_logger, logger);
        let max_len = if disable_recursive {
            usize::MAX
        } else {
            100_000
        };
        add_execution_result_to_messages(messages, &code, &execution_result, max_len);
    }
}

pub fn check_for_final_answer(
    response: &str,
    repl_env: &ReplEnv,
    logger: &Logger,
) -> Option<String> {
    let (kind, content) = find_final_answer(response)?;
    match kind {
        FinalAnswerKind::Final => Some(content),
        FinalAnswerKind::FinalVar => {
            let variable_name = content
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim_matches('\n')
                .trim_matches('\r');
            match repl_env.get_variable(variable_name) {
                Ok(Some(value)) => Some(value),
                Ok(None) => {
                    let msg = format!("Variable '{}' not found in REPL environment", variable_name);
                    logger.log_tool_execution("FINAL_VAR", &msg);
                    None
                }
                Err(err) => {
                    let msg = format!("Error retrieving variable '{}': {err}", variable_name);
                    logger.log_tool_execution("FINAL_VAR", &msg);
                    None
                }
            }
        }
    }
}
