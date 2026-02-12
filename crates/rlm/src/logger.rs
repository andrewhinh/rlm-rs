use std::time::Instant;

use crate::llm::Message;

#[derive(Clone, Debug)]
struct CodeExecution {
    code: String,
    stdout: String,
    stderr: String,
    execution_number: usize,
    execution_time: f64,
}

#[derive(Clone, Debug)]
pub struct Logger {
    enabled: bool,
    conversation_step: usize,
    last_messages_length: usize,
    current_query: String,
    session_start_time: Option<Instant>,
    current_depth: usize,
}

impl Logger {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            conversation_step: 0,
            last_messages_length: 0,
            current_query: String::new(),
            session_start_time: None,
            current_depth: 0,
        }
    }

    fn _print_separator(&self, ch: char) {
        if self.enabled {
            let line: String = std::iter::repeat_n(ch, 80).collect();
            println!("{line}");
        }
    }

    pub fn log_query_start(&mut self, query: &str) {
        if !self.enabled {
            return;
        }
        self.current_query = query.to_owned();
        self.conversation_step = 0;
        self.last_messages_length = 0;
        self.session_start_time = Some(Instant::now());
        self.current_depth = 0;

        self._print_separator('=');
        println!("STARTING NEW QUERY");
        self._print_separator('=');
        println!("QUERY: {query}");
        println!();
    }

    pub fn log_initial_messages(&mut self, messages: &[Message]) {
        if !self.enabled {
            return;
        }
        println!("INITIAL MESSAGES SETUP:");
        for (idx, msg) in messages.iter().enumerate() {
            let content = truncate(msg.content.as_str(), 2000);
            println!("  [{}] {}: {}", idx + 1, msg.role.to_uppercase(), content);
        }
        println!();
        self.last_messages_length = messages.len();
    }

    pub fn log_model_response(&mut self, response: &str, has_tool_calls: bool) {
        if !self.enabled {
            return;
        }
        self.conversation_step += 1;
        println!("MODEL RESPONSE (Step {}):", self.conversation_step);
        println!("  Response: {}", truncate(response, 500));
        if has_tool_calls {
            println!("  Contains tool calls - will execute them");
        } else {
            println!("  No tool calls - final response");
        }
        println!();
    }

    pub fn log_tool_execution(&self, tool_call_str: &str, tool_result: &str) {
        if !self.enabled {
            return;
        }
        println!("TOOL EXECUTION:");
        println!("  Call: {}", truncate(tool_call_str, 300));
        println!("  Result: {}", truncate(tool_result, 300));
        println!();
    }

    pub fn log_final_response(&self, response: &str) {
        if !self.enabled {
            return;
        }
        self._print_separator('=');
        println!("FINAL RESPONSE:");
        self._print_separator('=');
        println!("{response}");
        self._print_separator('=');
        println!();
    }
}

#[derive(Clone, Debug)]
pub struct ReplEnvLogger {
    enabled: bool,
    executions: Vec<CodeExecution>,
    execution_count: usize,
    max_output_length: usize,
}

impl ReplEnvLogger {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            executions: Vec::new(),
            execution_count: 0,
            max_output_length: 2000,
        }
    }

    fn _truncate_output(&self, text: &str) -> String {
        if text.len() <= self.max_output_length {
            return text.to_owned();
        }
        let half_len = self.max_output_length / 2;
        let first_part = slice_to_boundary(text, half_len);
        let mut last_start = text.len().saturating_sub(half_len);
        while !text.is_char_boundary(last_start) {
            last_start = last_start.saturating_sub(1);
        }
        let last_part = &text[last_start..];
        let truncated_chars = text.len() - self.max_output_length;
        format!("{first_part}\n\n... [TRUNCATED {truncated_chars} characters] ...\n\n{last_part}")
    }

    pub fn log_execution(&mut self, code: &str, stdout: &str, stderr: &str, elapsed_secs: f64) {
        self.execution_count += 1;
        let execution = CodeExecution {
            code: code.to_owned(),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            execution_number: self.execution_count,
            execution_time: elapsed_secs,
        };
        self.executions.push(execution.clone());
    }

    pub fn display_last(&self) {
        if !self.enabled {
            return;
        }
        if let Some(last) = self.executions.last() {
            self._display_single_execution(last);
        }
    }

    pub fn display_all(&self) {
        if !self.enabled {
            return;
        }
        for (idx, execution) in self.executions.iter().enumerate() {
            self._display_single_execution(execution);
            if idx + 1 < self.executions.len() {
                println!("{}", "─".repeat(80));
                println!();
            }
        }
    }

    fn _display_single_execution(&self, execution: &CodeExecution) {
        println!("REPL EXECUTION [{}]:", execution.execution_number);
        println!("  Code:\n{}", self._truncate_output(&execution.code));
        if !execution.stderr.is_empty() {
            println!("  Stderr:\n{}", self._truncate_output(&execution.stderr));
        } else if !execution.stdout.is_empty() {
            println!("  Stdout:\n{}", self._truncate_output(&execution.stdout));
        } else {
            println!("  Output: No output");
        }
        println!("  Execution time: {:.4}s", execution.execution_time);
        println!();
    }

    pub fn clear(&mut self) {
        self.executions.clear();
        self.execution_count = 0;
    }
}

fn truncate(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        return text.to_owned();
    }
    format!("{}...", slice_to_boundary(text, max_len))
}

fn slice_to_boundary(text: &str, max_len: usize) -> &str {
    let mut end = max_len.min(text.len());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &text[..end]
}
