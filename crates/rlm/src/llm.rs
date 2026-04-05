use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::time::sleep;

const MAX_ATTEMPTS: usize = 4;
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);

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
    #[error("http status {0}: {1}")]
    HttpStatus(reqwest::StatusCode, String),
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
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
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
            model: &self.model,
            messages,
            max_completion_tokens,
        };
        let mut delay = INITIAL_RETRY_DELAY;

        for attempt in 0..MAX_ATTEMPTS {
            let response = match self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    if attempt + 1 < MAX_ATTEMPTS && is_retryable_transport_error(&err) {
                        sleep(delay).await;
                        delay *= 2;
                        continue;
                    }
                    return Err(LlmError::Http(err));
                }
            };

            let retry_after = retry_after_delay(response.headers());
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                if attempt + 1 < MAX_ATTEMPTS && is_retryable_status(Some(status)) {
                    sleep(retry_after.unwrap_or(delay)).await;
                    if retry_after.is_none() {
                        delay *= 2;
                    }
                    continue;
                }
                return Err(LlmError::HttpStatus(status, truncate_error_body(&body)));
            }

            let parsed: ChatResponse = response.json().await?;
            let content = parsed
                .choices
                .into_iter()
                .next()
                .and_then(|choice| choice.message.content)
                .ok_or(LlmError::InvalidResponse)?;

            return Ok(content);
        }

        Err(LlmError::InvalidResponse)
    }
}

fn is_retryable_transport_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect()
}

fn is_retryable_status(status: Option<reqwest::StatusCode>) -> bool {
    matches!(
        status,
        Some(reqwest::StatusCode::TOO_MANY_REQUESTS)
            | Some(reqwest::StatusCode::BAD_GATEWAY)
            | Some(reqwest::StatusCode::SERVICE_UNAVAILABLE)
            | Some(reqwest::StatusCode::GATEWAY_TIMEOUT)
            | Some(reqwest::StatusCode::INTERNAL_SERVER_ERROR)
    )
}

fn retry_after_delay(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

fn truncate_error_body(body: &str) -> String {
    const MAX_ERROR_BODY_CHARS: usize = 2_000;
    let mut truncated: String = body.chars().take(MAX_ERROR_BODY_CHARS).collect();
    if truncated.chars().count() < body.chars().count() {
        truncated.push_str("...");
    }
    truncated
}
