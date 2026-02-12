use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_owned(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_owned(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_owned(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("missing api key")]
    MissingApiKey,
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid response")]
    InvalidResponse,
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn completion(
        &self,
        messages: &[Message],
        max_completion_tokens: Option<u32>,
    ) -> Result<String, LlmError>;
}

pub struct LlmClientImpl {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl LlmClientImpl {
    pub fn new(api_key: String, base_url: String, model: String) -> Result<Self, LlmError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(300))
            .build()?;
        Ok(Self {
            client,
            api_key,
            base_url,
            model,
        })
    }
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: Option<String>,
}

#[async_trait]
impl LlmClient for LlmClientImpl {
    async fn completion(
        &self,
        messages: &[Message],
        max_completion_tokens: Option<u32>,
    ) -> Result<String, LlmError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = ChatRequest {
            model: self.model.clone(),
            messages: messages.to_vec(),
            max_completion_tokens,
        };

        let response = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let parsed: ChatResponse = response.json().await?;
        let content = parsed
            .choices
            .first()
            .and_then(|choice| choice.message.content.clone())
            .ok_or(LlmError::InvalidResponse)?;

        Ok(content)
    }
}
