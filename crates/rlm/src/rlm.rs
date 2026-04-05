use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::lambda_rlm::{LambdaOptions, run_lambda_completion as execute_lambda_completion};
use crate::llm::{LlmClient, LlmClientImpl, Message};
use crate::logger::{Logger, ReplEnvLogger};
use crate::prompts::{DEFAULT_QUERY, REPL_SYSTEM_PROMPT, build_system_prompt, next_action_prompt};
use crate::repl::{RecursiveRunner, ReplHandle, ReplResult, SharedProgramState};
use crate::utils::{
    ContextInput, check_for_final_answer, convert_context_for_repl, find_code_blocks,
    process_code_execution_blocks, stringify_context_value,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RlmMethod {
    #[default]
    Rlm,
    LambdaRlm,
}

impl RlmMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rlm => "rlm",
            Self::LambdaRlm => "lambda_rlm",
        }
    }
}

impl fmt::Display for RlmMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RlmMethod {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "rlm" => Ok(Self::Rlm),
            "lambda_rlm" => Ok(Self::LambdaRlm),
            _ => Err(format!("invalid rlm method: {value}")),
        }
    }
}

#[derive(Clone)]
pub struct RlmConfig {
    pub method: RlmMethod,
    pub api_key: Option<String>,
    pub base_url: String,
    pub model: String,
    pub recursive_model: String,
    pub lambda_options: LambdaOptions,
    pub max_iterations: usize,
    pub depth: usize,
    pub enable_logging: bool,
    pub disable_recursive: bool,
}

pub struct RlmRepl {
    llm: Arc<dyn LlmClient>,
    recursive_llm: Arc<dyn LlmClient>,
    method: RlmMethod,
    depth: usize,
    max_iterations: usize,
    lambda_options: LambdaOptions,
    logger: Logger,
    repl_env_logger: ReplEnvLogger,
    messages: Vec<Message>,
    repl_env: Option<ReplHandle>,
    query: Option<String>,
    focused_context: bool,
    disable_recursive: bool,
    recursive_runner: Option<Arc<dyn RecursiveRunner>>,
    shared_state: SharedProgramState,
}

impl RlmRepl {
    pub fn new(config: RlmConfig) -> anyhow::Result<Self> {
        Self::new_with_shared_state(config, SharedProgramState::new())
    }

    pub(crate) fn new_with_shared_state(
        config: RlmConfig,
        shared_state: SharedProgramState,
    ) -> anyhow::Result<Self> {
        let llm = make_client(
            &config.model,
            config.api_key.clone(),
            config.base_url.clone(),
        )?;
        let recursive_llm = make_client(
            &config.recursive_model,
            config.api_key.clone(),
            config.base_url.clone(),
        )?;
        let recursive_runner: Option<Arc<dyn RecursiveRunner>> = if config.depth > 0 {
            Some(Arc::new(RlmRecursiveRunner::new(
                config.clone(),
                shared_state.clone(),
            )))
        } else {
            None
        };
        Ok(Self {
            llm,
            recursive_llm,
            method: config.method,
            depth: config.depth,
            max_iterations: config.max_iterations,
            lambda_options: config.lambda_options.clone(),
            logger: Logger::new(config.enable_logging),
            repl_env_logger: ReplEnvLogger::new(config.enable_logging),
            messages: Vec::new(),
            repl_env: None,
            query: None,
            focused_context: false,
            disable_recursive: config.disable_recursive,
            recursive_runner,
            shared_state,
        })
    }

    pub async fn setup_context(
        &mut self,
        context: impl Into<ContextInput>,
        query: Option<&str>,
    ) -> anyhow::Result<Vec<Message>> {
        let query = query.unwrap_or(DEFAULT_QUERY).to_owned();
        self.query = Some(query.clone());
        self.logger.log_query_start(&query);

        self.reset_messages_to_system_prompt();
        self.logger.log_initial_messages(&self.messages);

        let context_data = convert_context_for_repl(context.into(), &query);
        self.focused_context = context_data.full_text.is_some();
        if self.repl_env.is_none() {
            self.repl_env = Some(ReplHandle::new(
                self.recursive_llm.clone(),
                self.recursive_runner.clone(),
                self.depth,
                self.shared_state.clone(),
            )?);
        }
        let repl_env = self
            .repl_env
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("repl env not initialized"))?;
        repl_env.init(context_data, None).await?;

        Ok(self.messages.clone())
    }

    pub async fn completion(
        &mut self,
        context: impl Into<ContextInput>,
        query: Option<&str>,
    ) -> anyhow::Result<String> {
        self.setup_context(context, query).await?;

        let query = self
            .query
            .clone()
            .unwrap_or_else(|| DEFAULT_QUERY.to_owned());
        match self.method {
            RlmMethod::Rlm => self.run_completion_loop(&query).await,
            RlmMethod::LambdaRlm => self.run_lambda_completion(&query).await,
        }
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
        self.reset_messages_to_system_prompt();
        self.logger.log_initial_messages(&self.messages);
        match self.method {
            RlmMethod::Rlm => self.run_completion_loop(&query).await,
            RlmMethod::LambdaRlm => self.run_lambda_completion(&query).await,
        }
    }

    pub async fn execute_code(&self, code: &str) -> anyhow::Result<ReplResult> {
        let repl_env = self
            .repl_env
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("repl env not initialized"))?;
        repl_env.execute(code.to_owned()).await
    }

    async fn current_context_text(&self) -> anyhow::Result<String> {
        let repl_env = self
            .repl_env
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("repl env not initialized"))?;
        let context_value = repl_env.export_context().await?;
        Ok(stringify_context_value(&context_value))
    }

    async fn run_completion_loop(&mut self, query: &str) -> anyhow::Result<String> {
        let repl_env = self
            .repl_env
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("repl env not initialized"))?;

        for iteration in 0..self.max_iterations {
            let prompt = next_action_prompt(query, iteration, false, self.focused_context);
            self.messages.push(prompt);

            let response = self.llm.completion(&self.messages, None).await?;
            let _ = self.messages.pop();
            let code_blocks = find_code_blocks(&response);
            self.logger
                .log_model_response(&response, !code_blocks.is_empty());

            if !code_blocks.is_empty() {
                process_code_execution_blocks(
                    &code_blocks,
                    &mut self.messages,
                    &repl_env,
                    &mut self.repl_env_logger,
                    &self.logger,
                    self.disable_recursive,
                )
                .await;
            } else {
                self.messages.push(Message::assistant(format!(
                    "You responded with:\n{response}"
                )));
            }

            if let Some(final_answer) =
                check_for_final_answer(&response, &repl_env, &self.logger).await
            {
                self.logger.log_final_response(&final_answer);
                return Ok(final_answer);
            }
        }

        println!("No final answer found in any iteration");
        let final_prompt =
            next_action_prompt(query, self.max_iterations, true, self.focused_context);
        self.messages.push(final_prompt);
        let final_answer = self.llm.completion(&self.messages, None).await?;
        self.logger.log_final_response(&final_answer);
        Ok(final_answer)
    }

    async fn run_lambda_completion(&mut self, query: &str) -> anyhow::Result<String> {
        let context_text = self.current_context_text().await?;
        self.logger
            .log_lambda_phase("context_refresh", &format!("chars={}", context_text.len()));
        let final_answer = execute_lambda_completion(
            self.llm.clone(),
            context_text,
            query,
            &self.lambda_options,
            &mut self.logger,
        )
        .await?;
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
        self.focused_context = false;
        self.repl_env_logger.clear();
        self.shared_state.clear();
    }

    fn reset_messages_to_system_prompt(&mut self) {
        if let Some(first) = self.messages.first()
            && first.role == "system"
            && first.content == REPL_SYSTEM_PROMPT
        {
            self.messages.truncate(1);
            return;
        }
        self.messages = build_system_prompt();
    }
}

#[derive(Clone)]
struct RlmRecursiveRunner {
    config: RlmConfig,
    shared_state: SharedProgramState,
}

impl RlmRecursiveRunner {
    fn new(config: RlmConfig, shared_state: SharedProgramState) -> Self {
        Self {
            config,
            shared_state,
        }
    }

    fn child_config(&self) -> RlmConfig {
        let depth = self.config.depth.saturating_sub(1);
        RlmConfig {
            method: self.config.method,
            api_key: self.config.api_key.clone(),
            base_url: self.config.base_url.clone(),
            model: self.config.recursive_model.clone(),
            recursive_model: self.config.recursive_model.clone(),
            lambda_options: self.config.lambda_options.clone(),
            max_iterations: self.config.max_iterations,
            depth,
            enable_logging: self.config.enable_logging,
            disable_recursive: self.config.disable_recursive,
        }
    }
}

#[async_trait::async_trait]
impl RecursiveRunner for RlmRecursiveRunner {
    async fn completion(&self, query: String, context: ContextInput) -> anyhow::Result<String> {
        let mut repl =
            RlmRepl::new_with_shared_state(self.child_config(), self.shared_state.clone())?;
        repl.completion(context, Some(&query)).await
    }
}

fn make_client(
    model: &str,
    api_key: Option<String>,
    base_url: String,
) -> anyhow::Result<Arc<dyn LlmClient>> {
    let api_key = api_key.ok_or(crate::llm::LlmError::MissingApiKey)?;
    let client = LlmClientImpl::new(api_key, base_url, model.to_owned())?;
    Ok(Arc::new(client))
}
