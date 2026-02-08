use std::sync::Arc;
use std::time::Instant;

use crate::llm::{LlmClient, LlmClientImpl, Message};
use crate::logger::{LlmMetricsLogger, Logger, ReplEnvLogger};
use crate::prompts::{DEFAULT_QUERY, build_system_prompt, next_action_prompt};
use crate::repl::{ReplEnv, ReplResult};
use crate::utils::{
    ContextInput, check_for_final_answer, convert_context_for_repl, find_code_blocks,
    process_code_execution,
};

pub struct RlmConfig {
    pub api_key: Option<String>,
    pub base_url: String,
    pub model: String,
    pub recursive_model: String,
    pub max_iterations: usize,
    pub depth: usize,
    pub enable_logging: bool,
    pub disable_recursive: bool,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<String>,
}

pub struct RlmRepl {
    llm: Arc<dyn LlmClient>,
    recursive_llm: Arc<dyn LlmClient>,
    #[allow(dead_code)]
    depth: usize,
    max_iterations: usize,
    logger: Logger,
    repl_env_logger: ReplEnvLogger,
    llm_metrics: LlmMetricsLogger,
    messages: Vec<Message>,
    repl_env: Option<ReplEnv>,
    query: Option<String>,
    disable_recursive: bool,
}

impl RlmRepl {
    pub fn new(config: RlmConfig) -> anyhow::Result<Self> {
        let llm = make_client(
            &config.model,
            config.api_key.clone(),
            config.base_url.clone(),
            config.prompt_cache_key.clone(),
            config.prompt_cache_retention.clone(),
        )?;
        let recursive_llm = make_client(
            &config.recursive_model,
            config.api_key,
            config.base_url,
            config.prompt_cache_key.clone(),
            config.prompt_cache_retention.clone(),
        )?;
        Ok(Self {
            llm,
            recursive_llm,
            depth: config.depth,
            max_iterations: config.max_iterations,
            logger: Logger::new(config.enable_logging),
            repl_env_logger: ReplEnvLogger::new(config.enable_logging),
            llm_metrics: LlmMetricsLogger::new(config.enable_logging),
            messages: Vec::new(),
            repl_env: None,
            query: None,
            disable_recursive: config.disable_recursive,
        })
    }

    pub fn setup_context(
        &mut self,
        context: impl Into<ContextInput>,
        query: Option<&str>,
    ) -> anyhow::Result<Vec<Message>> {
        let query = query.unwrap_or(DEFAULT_QUERY).to_owned();
        self.query = Some(query.clone());
        self.logger.log_query_start(&query);

        self.messages = build_system_prompt();
        self.logger.log_initial_messages(&self.messages);

        let context_data = convert_context_for_repl(context.into());
        self.repl_env = Some(ReplEnv::new(
            context_data,
            self.recursive_llm.clone(),
            self.llm_metrics.clone(),
            None,
        )?);

        Ok(self.messages.clone())
    }

    pub async fn completion(
        &mut self,
        context: impl Into<ContextInput>,
        query: Option<&str>,
    ) -> anyhow::Result<String> {
        self.setup_context(context, query)?;

        let query = self
            .query
            .clone()
            .unwrap_or_else(|| DEFAULT_QUERY.to_owned());
        self.run_completion_loop(&query).await
    }

    pub async fn completion_with_existing(
        &mut self,
        query: Option<&str>,
    ) -> anyhow::Result<String> {
        if self.repl_env.is_none() {
            anyhow::bail!("repl env not initialized");
        }
        let query = query.unwrap_or(DEFAULT_QUERY).to_owned();
        self.query = Some(query.clone());
        self.logger.log_query_start(&query);
        self.messages = build_system_prompt();
        self.logger.log_initial_messages(&self.messages);
        self.run_completion_loop(&query).await
    }

    pub fn execute_code(&mut self, code: &str) -> anyhow::Result<ReplResult> {
        let repl_env = self
            .repl_env
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("repl env not initialized"))?;
        repl_env.execute(code)
    }

    async fn run_completion_loop(&mut self, query: &str) -> anyhow::Result<String> {
        let repl_env = self
            .repl_env
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("repl env not initialized"))?;

        for iteration in 0..self.max_iterations {
            let prompt = next_action_prompt(query, iteration, false);
            let mut messages = self.messages.clone();
            messages.push(prompt);

            let start = Instant::now();
            let response = self.llm.completion(&messages, None).await?;
            let elapsed = start.elapsed().as_secs_f64();
            self.llm_metrics
                .log_call("root", elapsed, response.usage.as_ref());
            let response = response.content;
            let code_blocks = find_code_blocks(&response);
            self.logger
                .log_model_response(&response, !code_blocks.is_empty());

            if !code_blocks.is_empty() {
                process_code_execution(
                    &response,
                    &mut self.messages,
                    repl_env,
                    &mut self.repl_env_logger,
                    &self.logger,
                    self.disable_recursive,
                );
            } else {
                self.messages.push(Message::assistant(format!(
                    "You responded with:\n{response}"
                )));
            }

            if let Some(final_answer) = check_for_final_answer(&response, repl_env, &self.logger) {
                self.logger.log_final_response(&final_answer);
                return Ok(final_answer);
            }
        }

        println!("No final answer found in any iteration");
        let final_prompt = next_action_prompt(query, self.max_iterations, true);
        self.messages.push(final_prompt);
        let start = Instant::now();
        let final_answer = self.llm.completion(&self.messages, None).await?;
        let elapsed = start.elapsed().as_secs_f64();
        self.llm_metrics
            .log_call("root_final", elapsed, final_answer.usage.as_ref());
        let final_answer = final_answer.content;
        self.logger.log_final_response(&final_answer);
        Ok(final_answer)
    }

    pub fn cost_summary(&self) -> anyhow::Result<()> {
        anyhow::bail!("Cost tracking not implemented for RLM REPL.")
    }

    pub fn reset(&mut self) {
        self.messages.clear();
        self.repl_env = None;
        self.query = None;
        self.repl_env_logger.clear();
    }
}

fn make_client(
    model: &str,
    api_key: Option<String>,
    base_url: String,
    prompt_cache_key: Option<String>,
    prompt_cache_retention: Option<String>,
) -> anyhow::Result<Arc<dyn LlmClient>> {
    let api_key = api_key.ok_or(crate::llm::LlmError::MissingApiKey)?;
    let client = LlmClientImpl::new(
        api_key,
        base_url,
        model.to_owned(),
        prompt_cache_key,
        prompt_cache_retention,
    );
    Ok(Arc::new(client))
}
