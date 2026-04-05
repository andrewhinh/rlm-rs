use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;

use crate::llm::{LlmClient, Message};
use crate::logger::Logger;
use crate::utils::split_embedded_context_question;

const DEFAULT_CONTEXT_WINDOW_CHARS: usize = 100_000;
const DEFAULT_ACCURACY_TARGET: f64 = 0.80;
const DEFAULT_LEAF_ACCURACY: f64 = 0.95;
const DEFAULT_COMPOSE_ACCURACY: f64 = 0.90;
const MAX_BRANCHING_FACTOR: usize = 20;

const TASK_DETECTION_PROMPT: &str =
    "Based on the metadata below, select the single most appropriate task type.\n\nMetadata: \
     {metadata}\n\nReply with ONLY a single digit (no other text):\n1. summarization - \
     condense/summarize content\n2. qa - answer a question using context\n3. translation - \
     translate text\n4. classification - categorize/label text\n5. extraction - extract specific \
     facts or entities\n6. analysis - deep analysis or evaluation\n7. general - mixed or \
     other\n\nSingle digit:";
const TASK_DETECTION_RETRY_SUFFIX: &str =
    "\n\nYour previous reply was invalid. Reply with ONLY one digit from 1 to 7.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskType {
    Summarization,
    Qa,
    Translation,
    Classification,
    Extraction,
    Analysis,
    General,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ComposeOp {
    Concatenate,
    MergeSummaries,
    SelectRelevant,
    MajorityVote,
    MergeExtractions,
    CombineAnalysis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PipelineFlags {
    use_filter: bool,
}

#[derive(Clone, Debug)]
pub struct LambdaOptions {
    pub context_window_chars: usize,
    pub accuracy_target: f64,
    pub a_leaf: f64,
    pub a_compose: f64,
}

impl Default for LambdaOptions {
    fn default() -> Self {
        Self {
            context_window_chars: DEFAULT_CONTEXT_WINDOW_CHARS,
            accuracy_target: DEFAULT_ACCURACY_TARGET,
            a_leaf: DEFAULT_LEAF_ACCURACY,
            a_compose: DEFAULT_COMPOSE_ACCURACY,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LambdaPlan {
    task_type: TaskType,
    compose_op: ComposeOp,
    pipeline: PipelineFlags,
    k_star: usize,
    tau_star: usize,
    depth: usize,
    expected_accuracy: f64,
}

#[derive(Clone, Debug)]
struct ContextSnapshot {
    text: String,
    query: String,
    preview: String,
    len: usize,
}

pub(crate) async fn run_lambda_completion(
    detector_llm: Arc<dyn LlmClient>,
    context_text: String,
    query: &str,
    options: &LambdaOptions,
    logger: &mut Logger,
) -> anyhow::Result<String> {
    let snapshot = normalize_inputs(context_text, query);
    logger.log_lambda_phase(
        "task_detect_start",
        &format!(
            "context_chars={} query={:?} preview={:?}",
            snapshot.len,
            truncate_chars(&snapshot.query, 120),
            truncate_chars(&snapshot.preview, 160)
        ),
    );
    let (task_type, task_detect_response) = detect_task_type(&detector_llm, &snapshot).await?;
    logger.log_model_response(&task_detect_response, false);
    logger.log_lambda_phase("task_detect_done", task_type.as_str());
    let plan = plan(task_type, snapshot.len, options);
    logger.log_lambda_phase("plan", &describe_plan(&plan));
    logger.log_lambda_phase("execute", "run deterministic lambda executor");
    let final_answer = execute_phi(&detector_llm, &snapshot.text, &plan, &snapshot.query).await?;
    let final_answer = if final_answer.trim().is_empty() {
        "No result produced.".to_owned()
    } else {
        final_answer
    };
    logger.log_lambda_phase("result", &truncate_chars(&final_answer, 200));
    Ok(final_answer)
}

fn normalize_inputs(context_text: String, query: &str) -> ContextSnapshot {
    let raw_text = context_text.trim().to_owned();
    let mut text = raw_text.clone();
    let mut query = query.trim().to_owned();
    if let Some((parsed_context, parsed_query)) = split_embedded_context_question(&raw_text) {
        text = parsed_context.to_owned();
        if query.is_empty() {
            query = parsed_query.to_owned();
        }
    }
    ContextSnapshot {
        preview: truncate_chars(&text, 500),
        len: text.len(),
        text,
        query,
    }
}

async fn detect_task_type(
    llm: &Arc<dyn LlmClient>,
    snapshot: &ContextSnapshot,
) -> anyhow::Result<(TaskType, String)> {
    let metadata = format!(
        "length={}, query={:?}, preview={:?}",
        snapshot.len,
        truncate_chars(&snapshot.query, 100),
        truncate_chars(&snapshot.preview, 150)
    );
    let prompt = TASK_DETECTION_PROMPT.replace("{metadata}", &metadata);
    let response = llm
        .completion(&[Message::user(prompt.clone())], Some(8))
        .await
        .context("lambda task detection failed")?;
    if let Some(task) = parse_detected_task_type(&response) {
        return Ok((task, response));
    }

    let retry_prompt = format!("{prompt}{TASK_DETECTION_RETRY_SUFFIX}");
    let retry_response = llm
        .completion(&[Message::user(retry_prompt)], Some(8))
        .await
        .context("lambda task detection retry failed")?;
    Ok((parse_task_type(&retry_response), retry_response))
}

fn parse_detected_task_type(response: &str) -> Option<TaskType> {
    for ch in response.chars() {
        if let Some(digit) = ch.to_digit(10) {
            return Some(match digit {
                1 => TaskType::Summarization,
                2 => TaskType::Qa,
                3 => TaskType::Translation,
                4 => TaskType::Classification,
                5 => TaskType::Extraction,
                6 => TaskType::Analysis,
                7 => TaskType::General,
                _ => TaskType::General,
            });
        }
    }
    None
}

fn parse_task_type(response: &str) -> TaskType {
    parse_detected_task_type(response).unwrap_or(TaskType::General)
}

fn plan(task_type: TaskType, n: usize, options: &LambdaOptions) -> LambdaPlan {
    let context_window = options.context_window_chars.max(1);
    let compose_op = composition_for(task_type);
    let pipeline = pipeline_for(task_type);
    let compose_cost = compose_cost(compose_op);

    if n <= context_window {
        return LambdaPlan {
            task_type,
            compose_op,
            pipeline,
            k_star: 1,
            tau_star: n.max(1),
            depth: 0,
            expected_accuracy: 1.0,
        };
    }

    let max_k = n.div_ceil(context_window).clamp(2, MAX_BRANCHING_FACTOR);
    let heuristic_k = if compose_cost > 0.1 {
        ((n as f64 / compose_cost).sqrt().ceil() as usize).clamp(2, MAX_BRANCHING_FACTOR)
    } else {
        n.div_ceil(context_window).clamp(2, MAX_BRANCHING_FACTOR)
    };
    let mut k_star = heuristic_k.min(max_k);
    let mut depth = plan_depth(n, context_window, k_star);

    while (options.a_leaf.powi(depth as i32) * options.a_compose.powi(depth as i32))
        < options.accuracy_target
        && k_star < max_k
    {
        k_star += 1;
        depth = plan_depth(n, context_window, k_star);
    }

    let tau_star = (n / k_star).max(1).min(context_window);
    let expected_accuracy =
        options.a_leaf.powi(depth as i32) * options.a_compose.powi(depth as i32);
    LambdaPlan {
        task_type,
        compose_op,
        pipeline,
        k_star,
        tau_star,
        depth,
        expected_accuracy,
    }
}

fn plan_depth(n: usize, context_window: usize, k_star: usize) -> usize {
    if n <= context_window || k_star <= 1 {
        return 0;
    }
    let ratio = n as f64 / context_window as f64;
    ratio.log(k_star as f64).ceil().max(1.0) as usize
}

fn execute_phi<'a>(
    llm: &'a Arc<dyn LlmClient>,
    text: &'a str,
    plan: &'a LambdaPlan,
    query: &'a str,
) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'a>> {
    Box::pin(async move {
        if text.len() <= plan.tau_star.max(1) || plan.k_star <= 1 {
            return complete_prompt(llm, leaf_prompt(plan.task_type, text, query)).await;
        }

        let mut chunks = split_text(text, plan.k_star);
        if should_filter(plan, &chunks, query) {
            chunks = filter_relevant(llm, chunks, query, plan.tau_star).await?;
        }

        let mut outputs = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            outputs.push(execute_phi(llm, chunk, plan, query).await?);
        }
        reduce_outputs(llm, &outputs, plan.compose_op, query).await
    })
}

fn should_filter(plan: &LambdaPlan, chunks: &[String], query: &str) -> bool {
    plan.pipeline.use_filter && !query.trim().is_empty() && !chunks.is_empty()
}

async fn complete_prompt(llm: &Arc<dyn LlmClient>, prompt: String) -> anyhow::Result<String> {
    llm.completion(&[Message::user(prompt)], None)
        .await
        .context("lambda llm completion failed")
}

fn leaf_prompt(task_type: TaskType, text: &str, query: &str) -> String {
    match task_type {
        TaskType::Summarization => format!("Summarize the following text concisely:\n\n{text}"),
        TaskType::Qa => {
            if query.trim().is_empty() {
                format!("Answer based on the following context:\n\n{text}")
            } else {
                format!("Using the following context, answer: {query}\n\nContext:\n{text}")
            }
        }
        TaskType::Translation => format!("Translate the following text:\n\n{text}"),
        TaskType::Classification => format!("Classify the following text:\n\n{text}"),
        TaskType::Extraction => format!("Extract all key information from:\n\n{text}"),
        TaskType::Analysis => {
            format!("Analyze the following text and provide insights:\n\n{text}")
        }
        TaskType::General => format!("Process the following and provide a response:\n\n{text}"),
    }
}

async fn filter_relevant(
    llm: &Arc<dyn LlmClient>,
    chunks: Vec<String>,
    query: &str,
    tau_star: usize,
) -> anyhow::Result<Vec<String>> {
    let mut kept = Vec::new();
    let preview_len = (tau_star / 10).max(50);
    for chunk in &chunks {
        let excerpt = truncate_chars(chunk, preview_len);
        let prompt = format!(
            "Question: {query}\n\nDoes this excerpt contain information relevant to answering the \
             question?\nReply YES or NO only.\n\nExcerpt:\n{excerpt}"
        );
        let response = llm
            .completion(&[Message::user(prompt)], Some(16))
            .await
            .context("lambda relevance filter failed")?;
        if response.trim().to_ascii_uppercase().starts_with('Y') {
            kept.push(chunk.clone());
        }
    }
    if kept.is_empty() {
        Ok(chunks)
    } else {
        Ok(kept)
    }
}

async fn reduce_outputs(
    llm: &Arc<dyn LlmClient>,
    outputs: &[String],
    compose_op: ComposeOp,
    query: &str,
) -> anyhow::Result<String> {
    match compose_op {
        ComposeOp::Concatenate => Ok(outputs.join("\n\n")),
        ComposeOp::MergeSummaries => {
            if outputs.len() <= 1 {
                return Ok(outputs.first().cloned().unwrap_or_default());
            }
            let merged = outputs.join("\n\n---\n\n");
            complete_prompt(
                llm,
                format!(
                    "Merge these partial summaries into one concise, coherent summary. Preserve \
                     all key facts and findings:\n\n{merged}"
                ),
            )
            .await
        }
        ComposeOp::SelectRelevant => {
            let mut candidates: Vec<String> = outputs
                .iter()
                .filter(|output| is_informative_answer(output))
                .cloned()
                .collect();
            if candidates.is_empty() {
                candidates = outputs.to_vec();
            }
            if candidates.len() == 1 {
                return Ok(candidates[0].clone());
            }
            let merged = candidates.join("\n\n---\n\n");
            complete_prompt(
                llm,
                format!(
                    "Question: {query}\n\nSynthesise these partial answers into one complete, \
                     accurate answer:\n\n{merged}"
                ),
            )
            .await
        }
        ComposeOp::MajorityVote => Ok(majority_vote(outputs)),
        ComposeOp::MergeExtractions => Ok(merge_extractions(outputs)),
        ComposeOp::CombineAnalysis => {
            if outputs.len() <= 1 {
                return Ok(outputs.first().cloned().unwrap_or_default());
            }
            let merged = outputs.join("\n\n---\n\n");
            complete_prompt(
                llm,
                format!(
                    "Combine these partial analyses into one comprehensive, well-structured \
                     analysis:\n\n{merged}"
                ),
            )
            .await
        }
    }
}

fn majority_vote(outputs: &[String]) -> String {
    let mut counts = std::collections::HashMap::<String, usize>::new();
    for output in outputs {
        let key = output.trim().to_ascii_lowercase();
        *counts.entry(key).or_default() += 1;
    }
    let winner = counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(key, _)| key)
        .unwrap_or_default();
    outputs
        .iter()
        .find(|output| output.trim().eq_ignore_ascii_case(&winner))
        .cloned()
        .or_else(|| outputs.first().cloned())
        .unwrap_or_default()
}

fn merge_extractions(outputs: &[String]) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();
    for output in outputs {
        for line in output.lines() {
            let item = line.trim();
            if item.is_empty() || !seen.insert(item.to_owned()) {
                continue;
            }
            lines.push(item.to_owned());
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n")
    }
}

fn is_informative_answer(output: &str) -> bool {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    !lower.contains("not found")
        && !lower.contains("no information")
        && !lower.contains("not mentioned")
}

fn split_text(text: &str, k: usize) -> Vec<String> {
    if k <= 1 || text.is_empty() {
        return vec![text.to_owned()];
    }

    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let chunk_size = (n / k).max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut start = 0usize;

    for index in 0..k {
        if start >= n {
            break;
        }
        if index == k - 1 {
            chunks.push(chars[start..].iter().collect());
            break;
        }

        let mut end = (start + chunk_size).min(n);
        if end < n {
            let margin = (chunk_size / 5).max(1);
            let search_start = start.max(end.saturating_sub(margin));
            let search_end = (end + margin).min(n);
            if let Some(boundary) = (search_start..search_end)
                .rev()
                .find(|idx| chars[*idx].is_whitespace())
                && boundary > start
            {
                end = boundary + 1;
            }
        }
        chunks.push(chars[start..end].iter().collect());
        start = end;
    }

    chunks
        .into_iter()
        .filter(|chunk| !chunk.trim().is_empty())
        .collect()
}

fn composition_for(task_type: TaskType) -> ComposeOp {
    match task_type {
        TaskType::Summarization => ComposeOp::MergeSummaries,
        TaskType::Qa => ComposeOp::SelectRelevant,
        TaskType::Translation => ComposeOp::Concatenate,
        TaskType::Classification => ComposeOp::MajorityVote,
        TaskType::Extraction => ComposeOp::MergeExtractions,
        TaskType::Analysis => ComposeOp::CombineAnalysis,
        TaskType::General => ComposeOp::MergeSummaries,
    }
}

fn pipeline_for(task_type: TaskType) -> PipelineFlags {
    PipelineFlags {
        use_filter: matches!(task_type, TaskType::Qa | TaskType::Extraction),
    }
}

fn compose_cost(compose_op: ComposeOp) -> f64 {
    match compose_op {
        ComposeOp::Concatenate => 0.01,
        ComposeOp::MergeSummaries => 2.0,
        ComposeOp::SelectRelevant => 1.5,
        ComposeOp::MajorityVote => 0.05,
        ComposeOp::MergeExtractions => 0.05,
        ComposeOp::CombineAnalysis => 2.0,
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

impl TaskType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Summarization => "summarization",
            Self::Qa => "qa",
            Self::Translation => "translation",
            Self::Classification => "classification",
            Self::Extraction => "extraction",
            Self::Analysis => "analysis",
            Self::General => "general",
        }
    }
}

impl ComposeOp {
    fn as_str(self) -> &'static str {
        match self {
            Self::Concatenate => "concatenate",
            Self::MergeSummaries => "merge_summaries",
            Self::SelectRelevant => "select_relevant",
            Self::MajorityVote => "majority_vote",
            Self::MergeExtractions => "merge_extractions",
            Self::CombineAnalysis => "combine_analysis",
        }
    }
}

fn describe_plan(plan: &LambdaPlan) -> String {
    format!(
        "task={} compose={} use_filter={} k={} tau={} depth={} expected_accuracy={:.3}",
        plan.task_type.as_str(),
        plan.compose_op.as_str(),
        plan.pipeline.use_filter,
        plan.k_star,
        plan.tau_star,
        plan.depth,
        plan.expected_accuracy
    )
}
