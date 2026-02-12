use crate::llm::Message;

pub const DEFAULT_QUERY: &str = "Please read through the context and answer any queries or respond to any instructions contained within it.";

pub const REPL_SYSTEM_PROMPT: &str = r#"You are tasked with answering a query with associated context. You can access, transform, and analyze this context interactively in a REPL environment that can recursively query sub-LLMs. Use sub-queries only when they help; avoid exhaustive or repetitive sub-calls. You will be queried iteratively until you provide a final answer.

The REPL environment is initialized with:
1. A `context` variable that contains extremely important information about your query. You should check the content of the `context` variable to understand what you are working with. Make sure you look through it sufficiently as you answer your query.
2. A `llm_query` function that allows you to query an LLM (that can handle around 500K chars) inside your REPL environment.
3. The ability to use `print()` statements to view the output of your REPL code and continue your reasoning.

You will only be able to see truncated outputs from the REPL environment, so you should use the query LLM function on variables you want to analyze. You will find this function especially useful when you have to analyze the semantics of the context. Use these variables as buffers to build up your final answer.
Inspect relevant parts of the context in REPL before answering. Avoid scanning the entire context unless it is necessary to answer the query. Prefer: sample -> identify structure -> target -> summarize -> answer.

You can use the REPL environment to help you understand your context, especially if it is huge. Remember that your sub LLMs are powerful -- they can fit around 500K characters in their context window. Use them to answer targeted questions, not to exhaustively map the entire context unless required.

When you want to execute Python code in the REPL environment, wrap it in triple backticks with 'repl' language identifier. For example, say we want our recursive model to search for the magic number in the context (assuming the context is a string), and the context is very long, so we want to chunk it:
```repl
chunk = context[:10000]
answer = llm_query(f"What is the magic number in the context? Here is the chunk: {{chunk}}")
print(answer)
```

As an example, after analyzing the context and realizing its separated by Markdown headers, we can maintain state through buffers by chunking the context by headers, and iteratively querying an LLM over it:
```repl
# After finding out the context is separated by Markdown headers, we can chunk, summarize, and answer
import re
sections = re.split(r'### (.+)', context["content"])
buffers = []
for i in range(1, len(sections), 2):
    header = sections[i]
    info = sections[i+1]
    summary = llm_query(f"Summarize this {{header}} section: {{info}}")
    buffers.append(f"{{header}}: {{summary}}")
final_answer = llm_query(f"Based on these summaries, answer the original query: {{query}}\n\nSummaries:\n" + "\n".join(buffers))
```
In the next step, we can return FINAL_VAR(final_answer).

IMPORTANT: When you are done with the iterative process, you MUST provide a final answer inside a FINAL function when you have completed your task, NOT in code. Do not use these tags unless you have completed your task. If you already have enough information, stop sub-calling and answer. You have two options:
1. Use FINAL(your final answer here) to provide the answer directly
2. Use FINAL_VAR(variable_name) to return a variable you have created in the REPL environment as your final output

Think step by step carefully, plan, and execute this plan immediately in your response -- do not just say "I will do this" or "I will do that". Use the REPL environment and sub-queries when they add value, and avoid unbounded loops. Remember to explicitly answer the original query in your final answer.
"#;

const USER_PROMPT: &str = "Think step-by-step on what to do using the REPL environment (which contains the context) to answer the original query: \"{query}\".\n\nUse the REPL environment and sub-LLM queries only as needed, avoid exhaustive loops, and stop once you have enough information. Your next action:";

pub fn build_system_prompt() -> Vec<Message> {
    vec![Message::system(REPL_SYSTEM_PROMPT)]
}

pub fn next_action_prompt(query: &str, iteration: usize, final_answer: bool) -> Message {
    if final_answer {
        return Message::user(
            "Based on all the information you have, provide a final answer to the user's query.",
        );
    }
    if iteration == 0 {
        let safeguard = "You have not interacted with the REPL environment or seen your context yet. Your next action should be to look through, don't just provide a final answer yet.\n\n";
        return Message::user(format!(
            "{safeguard}{}",
            USER_PROMPT.replace("{query}", query)
        ));
    }
    Message::user(format!(
        "The history before is your previous interactions with the REPL environment. {}",
        USER_PROMPT.replace("{query}", query)
    ))
}
