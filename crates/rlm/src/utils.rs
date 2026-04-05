use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::llm::Message;
use crate::logger::{Logger, ReplEnvLogger};
use crate::repl::{ReplHandle, ReplResult};

static CODE_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"```repl\s*\n(?s:(.*?))\n```").expect("regex"));
static FINAL_VAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?ms)^\s*FINAL_VAR\((.*?)\)").expect("regex"));
static FINAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?ms)^\s*FINAL\((.*?)\)").expect("regex"));

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
    pub full_text: Option<String>,
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

pub fn split_embedded_context_question(text: &str) -> Option<(&str, &str)> {
    let body = text.trim().strip_prefix("Context:\n")?;
    let (context, question) = body.split_once("\n\nQuestion:\n")?;
    let context = context.trim();
    let question = question.trim();
    (!context.is_empty() && !question.is_empty()).then_some((context, question))
}

pub fn convert_context_for_repl(context: ContextInput, query: &str) -> ContextData {
    match context {
        ContextInput::Json(value) => ContextData {
            json: Some(normalize_context_json(value)),
            text: None,
            full_text: None,
        },
        ContextInput::Text(value) => build_text_context(value, query),
        ContextInput::Messages(messages) => {
            let items: Vec<String> = messages.into_iter().map(|msg| msg.content).collect();
            ContextData {
                json: Some(Value::Array(items.into_iter().map(Value::String).collect())),
                text: None,
                full_text: None,
            }
        }
        ContextInput::Strings(items) => ContextData {
            json: Some(Value::Array(items.into_iter().map(Value::String).collect())),
            text: None,
            full_text: None,
        },
    }
}

fn build_text_context(value: String, query: &str) -> ContextData {
    let excerpt = retrieve_relevant_text(&value, query);
    let full_text = (excerpt != value).then_some(value);
    ContextData {
        json: None,
        text: Some(excerpt),
        full_text,
    }
}

pub fn stringify_context_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => {
            if let Some(strings) = array_to_strings(items) {
                return strings.join("\n\n");
            }
            if let Some(messages) = array_to_messages(items) {
                return messages
                    .iter()
                    .map(|message| message.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n\n");
            }
            items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        other => other.to_string(),
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

const MIN_RETRIEVAL_CONTEXT_CHARS: usize = 8_192;
const MAX_RETRIEVAL_EXCERPT_CHARS: usize = 12_000;
const LINE_WINDOW_RADIUS: usize = 2;
const MAX_LINE_SPANS: usize = 8;
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "any", "are", "be", "by", "can", "do", "for", "from", "how", "i", "in", "is",
    "it", "looking", "of", "on", "or", "please", "read", "respond", "the", "through", "to", "what",
    "with", "you", "your",
];

fn retrieve_relevant_text(text: &str, query: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= MIN_RETRIEVAL_CONTEXT_CHARS {
        return trimmed.to_owned();
    }

    let terms = query_terms(query);
    if terms.is_empty() {
        return trimmed.to_owned();
    }

    let excerpt = retrieve_relevant_lines(trimmed, &terms).unwrap_or_else(|| trimmed.to_owned());

    if excerpt.trim().is_empty() {
        trimmed.to_owned()
    } else {
        excerpt
    }
}

fn retrieve_relevant_lines(text: &str, terms: &[String]) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 4 {
        return None;
    }

    let mut scored = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let score = match_count(line, terms);
        if score > 0 {
            scored.push((score, idx));
        }
    }
    if scored.is_empty() {
        return None;
    }
    scored.sort_by(|left, right| right.cmp(left));

    let mut spans = Vec::new();
    for &(_, idx) in scored.iter().take(MAX_LINE_SPANS) {
        let start = idx.saturating_sub(LINE_WINDOW_RADIUS);
        let end = (idx + LINE_WINDOW_RADIUS + 1).min(lines.len());
        spans.push((start, end));
    }

    let excerpt = join_line_spans(&lines, &merge_spans(spans), MAX_RETRIEVAL_EXCERPT_CHARS);
    (!excerpt.trim().is_empty()).then_some(excerpt)
}

fn query_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for token in query.split(|ch: char| !ch.is_ascii_alphanumeric()) {
        let token = token.trim().to_ascii_lowercase();
        if token.len() < 2 || STOPWORDS.contains(&token.as_str()) || terms.contains(&token) {
            continue;
        }
        terms.push(token);
    }
    terms
}

fn match_count(text: &str, terms: &[String]) -> usize {
    let lower = text.to_ascii_lowercase();
    terms
        .iter()
        .filter(|term| lower.contains(term.as_str()))
        .count()
}

fn merge_spans(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if spans.is_empty() {
        return spans;
    }
    spans.sort_unstable();
    let mut merged = vec![spans[0]];
    for (start, end) in spans.into_iter().skip(1) {
        let last = merged.last_mut().expect("merged has at least one span");
        if start <= last.1 {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

fn join_line_spans(lines: &[&str], spans: &[(usize, usize)], max_chars: usize) -> String {
    let mut out = String::new();
    let mut out_chars = 0;
    for (idx, (start, end)) in spans.iter().enumerate() {
        let separator = (idx > 0 && !out.is_empty()).then_some("\n...\n");
        let separator_chars = separator.map_or(0, char_count);
        let segment = lines[*start..*end].join("\n");
        let segment_chars = char_count(&segment);
        if out_chars + separator_chars + segment_chars > max_chars {
            if let Some(separator) = separator
                && out_chars + separator_chars <= max_chars
            {
                out.push_str(separator);
                out_chars += separator_chars;
            }
            let remaining = max_chars.saturating_sub(out_chars);
            if remaining > 0 {
                let truncated = truncate_text(&segment, remaining);
                out.push_str(&truncated);
            }
            break;
        }
        if let Some(separator) = separator {
            out.push_str(separator);
            out_chars += separator_chars;
        }
        out.push_str(&segment);
        out_chars += segment_chars;
    }
    out
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

pub fn find_code_blocks(text: &str) -> Vec<String> {
    CODE_BLOCK_RE
        .captures_iter(text)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().trim().to_owned()))
        .collect()
}

pub enum FinalAnswerKind {
    Final,
    FinalVar,
}

pub fn find_final_answer(text: &str) -> Option<(FinalAnswerKind, String)> {
    if let Some(cap) = FINAL_VAR_RE.captures(text) {
        return Some((FinalAnswerKind::FinalVar, cap[1].trim().to_owned()));
    }
    if let Some(cap) = FINAL_RE.captures(text) {
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
            for local in &result.locals {
                if should_skip_var_name(&local.name) {
                    continue;
                }
                vars.push(format!("{}={}", local.name, local.repr));
            }
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

pub async fn execute_code(
    repl_env: &ReplHandle,
    code: &str,
    repl_env_logger: &mut ReplEnvLogger,
    logger: &Logger,
) -> String {
    match repl_env.execute(code.to_owned()).await {
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

pub async fn process_code_execution(
    response: &str,
    messages: &mut Vec<Message>,
    repl_env: &ReplHandle,
    repl_env_logger: &mut ReplEnvLogger,
    logger: &Logger,
    disable_recursive: bool,
) {
    let code_blocks = find_code_blocks(response);
    process_code_execution_blocks(
        &code_blocks,
        messages,
        repl_env,
        repl_env_logger,
        logger,
        disable_recursive,
    )
    .await;
}

pub async fn process_code_execution_blocks(
    code_blocks: &[String],
    messages: &mut Vec<Message>,
    repl_env: &ReplHandle,
    repl_env_logger: &mut ReplEnvLogger,
    logger: &Logger,
    disable_recursive: bool,
) {
    for code in code_blocks {
        let execution_result = execute_code(repl_env, code, repl_env_logger, logger).await;
        let max_len = if disable_recursive {
            usize::MAX
        } else {
            100_000
        };
        add_execution_result_to_messages(messages, code, &execution_result, max_len);
    }
}

pub async fn check_for_final_answer(
    response: &str,
    repl_env: &ReplHandle,
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
            match repl_env.get_variable(variable_name.to_owned()).await {
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

#[cfg(test)]
mod tests {
    use super::{char_count, join_line_spans, split_embedded_context_question};

    #[test]
    fn join_line_spans_respects_unicode_char_limit() {
        let lines = ["alpha", "beta", "piñata", "omega"];
        let excerpt = join_line_spans(&lines, &[(1, 4)], 7);
        assert_eq!(excerpt, "beta\npi");
        assert_eq!(char_count(&excerpt), 7);
    }

    #[test]
    fn split_embedded_context_question_requires_goose_form() {
        assert_eq!(
            split_embedded_context_question("Context:\nctx\n\nQuestion:\nwhat?"),
            Some(("ctx", "what?"))
        );
        assert_eq!(
            split_embedded_context_question("Context:\nctx\nQuestion:\nwhat?"),
            None
        );
    }
}
