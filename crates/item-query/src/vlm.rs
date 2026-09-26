//! VLM sidecar client: any OpenAI-compatible /v1/chat/completions endpoint
//! (llama.cpp server, Ollama, or a cloud API). The Rust core never embeds a
//! VLM — swap backends by changing base_url/model.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Per-request budget. Without one a stalled sidecar holds the caller (a CLI
/// invocation, or an axum request in item-web) open forever. Same 60s as the
/// ingest side's `defaults::VLM_TIMEOUT_SECS`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
}

#[derive(Debug, Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ReplyMessage,
}

#[derive(Debug, Deserialize)]
struct ReplyMessage {
    /// Servers emit `"content": null` for a refusal or a truncated reply;
    /// that is an empty answer, not a deserialization failure.
    #[serde(default)]
    content: Option<String>,
}

pub struct VlmClient {
    base_url: String,
    model: String,
    http: reqwest::Client,
}

impl VlmClient {
    /// `base_url` like "http://127.0.0.1:8080/v1".
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    pub async fn ask(&self, prompt: &str) -> anyhow::Result<String> {
        let body = ChatRequest {
            model: &self.model,
            messages: vec![
                Message {
                    role: "system",
                    content: "You are an assistant that answers where household \
                              items were last seen, using ONLY the sighting log \
                              provided by the user. If the log lacks the item, \
                              say so plainly.",
                },
                Message {
                    role: "user",
                    content: prompt,
                },
            ],
        };
        let resp: ChatResponse = self
            .http
            .post(format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let answer = resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("vlm returned no choices"))?
            .message
            .content
            .unwrap_or_default();
        if answer.trim().is_empty() {
            anyhow::bail!("vlm returned an empty answer");
        }
        Ok(answer)
    }
}
