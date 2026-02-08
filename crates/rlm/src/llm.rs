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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmUsage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub cached_input_tokens: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct LlmCompletion {
    pub content: String,
    pub usage: Option<LlmUsage>,
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn completion(
        &self,
        messages: &[Message],
        max_completion_tokens: Option<u32>,
    ) -> Result<LlmCompletion, LlmError>;
}

pub struct LlmClientImpl {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
    prompt_cache_key: Option<String>,
    prompt_cache_retention: Option<String>,
}

impl LlmClientImpl {
    pub fn new(
        api_key: String,
        base_url: String,
        model: String,
        prompt_cache_key: Option<String>,
        prompt_cache_retention: Option<String>,
    ) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("reqwest client");
        Self {
            client,
            api_key,
            base_url,
            model,
            prompt_cache_key,
            prompt_cache_retention,
        }
    }
}

#[derive(Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<InputMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<String>,
}

#[derive(Deserialize)]
struct ResponsesResponse {
    output: Vec<ResponseOutputItem>,
    #[serde(default)]
    output_text: Option<String>,
    usage: Option<ResponseUsage>,
}

#[derive(Deserialize)]
struct ResponseOutputItem {
    content: Option<Vec<ResponseContent>>,
}

#[derive(Deserialize)]
struct ResponseContent {
    #[serde(rename = "type")]
    content_type: String,
    text: Option<String>,
}

#[derive(Serialize)]
struct InputMessage {
    role: String,
    content: Vec<InputContent>,
}

#[derive(Serialize)]
struct InputContent {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

#[derive(Deserialize)]
struct ResponseUsage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    total_tokens: Option<u32>,
    input_tokens_details: Option<InputTokenDetails>,
}

#[derive(Deserialize)]
struct InputTokenDetails {
    cached_tokens: Option<u32>,
}

#[async_trait]
impl LlmClient for LlmClientImpl {
    async fn completion(
        &self,
        messages: &[Message],
        max_completion_tokens: Option<u32>,
    ) -> Result<LlmCompletion, LlmError> {
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let body = ResponsesRequest {
            model: self.model.clone(),
            input: build_input_messages(messages),
            max_output_tokens: max_completion_tokens,
            prompt_cache_key: self.prompt_cache_key.clone(),
            prompt_cache_retention: self.prompt_cache_retention.clone(),
        };

        let response = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let body_text = response.text().await?;
        if !status.is_success() {
            let msg = format!("Error making LLM query: HTTP {status} {body_text}");
            return Ok(LlmCompletion {
                content: msg,
                usage: None,
            });
        }

        let parsed: ResponsesResponse =
            serde_json::from_str(&body_text).map_err(|_| LlmError::InvalidResponse)?;
        let content = extract_response_text(&parsed).ok_or(LlmError::InvalidResponse)?;
        let usage = parsed.usage.map(|usage| LlmUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_input_tokens: usage
                .input_tokens_details
                .and_then(|details| details.cached_tokens),
        });
        Ok(LlmCompletion { content, usage })
    }
}

fn build_input_messages(messages: &[Message]) -> Vec<InputMessage> {
    messages
        .iter()
        .map(|message| InputMessage {
            role: message.role.clone(),
            content: vec![InputContent {
                content_type: "input_text".to_owned(),
                text: message.content.clone(),
            }],
        })
        .collect()
}

fn extract_response_text(response: &ResponsesResponse) -> Option<String> {
    if let Some(text) = response.output_text.as_ref()
        && !text.is_empty()
    {
        return Some(text.to_owned());
    }
    for item in &response.output {
        let Some(content) = item.content.as_ref() else {
            continue;
        };
        for part in content {
            if (part.content_type == "output_text" || part.content_type == "text")
                && let Some(text) = part.text.as_ref()
                && !text.is_empty()
            {
                return Some(text.to_owned());
            }
        }
    }
    None
}
